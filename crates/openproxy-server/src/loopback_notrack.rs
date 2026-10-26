//! Opt-in conntrack bypass for loopback traffic to openproxy's own listeners.
//!
//! Enabled with `OPENPROXY_LOOPBACK_NOTRACK=1`. Every loopback packet to or from
//! the listening ports normally walks conntrack's per-packet lookup, and each
//! new flow allocates an entry. Local clients (agents on the same host) pay
//! that lookup per packet. This installs `notrack` for those ports on `lo`.
//! It does NOT bypass filter chains or change the host's accept/drop policy.
//!
//! Lifetime is owned by the kernel, not by cleanup code: the table carries
//! `flags owner`, so it lives exactly as long as the netlink socket of the
//! `nft -i` child that created it. The child holds that socket while its stdin
//! stays open, and openproxy holds the other end of the pipe. A clean stop,
//! a panic, `kill -9` or a crash all close the pipe, the child exits and the
//! kernel deletes the table. No other process can delete or edit it meanwhile.
//!
//! Scope is deliberately narrow: only `oifname "lo"` and only these ports.
//! Loopback packets are generated locally, so the raw `output` hook sees both
//! directions before conntrack does. Traffic to upstreams leaves through real
//! interfaces and keeps full tracking. Failure never blocks startup.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

/// Environment switch. Accepts `1` or `true`.
pub const ENV_VAR: &str = "OPENPROXY_LOOPBACK_NOTRACK";

const INSTALL_TIMEOUT: Duration = Duration::from_secs(3);

/// Whether the operator asked for the loopback bypass.
#[must_use]
pub fn requested() -> bool {
    std::env::var(ENV_VAR).is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

/// Name of the owner table for this process. The pid keeps two instances in
/// the same network namespace from colliding.
#[must_use]
pub fn table_name(pid: u32) -> String {
    format!("openproxy_loopback_notrack_{pid}")
}

/// Create the owner table and BOTH directions in one atomic command.
/// The interactive child must keep the owning netlink socket open afterward.
#[must_use]
pub fn script(table: &str, ports: &[u16]) -> String {
    let mut ports: Vec<u16> = ports.to_vec();
    ports.sort_unstable();
    ports.dedup();
    let set = ports
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "add table inet {table} {{ flags owner; \
         chain output {{ type filter hook output priority raw; policy accept; \
         oifname \"lo\" tcp dport {{ {set} }} notrack; \
         oifname \"lo\" tcp sport {{ {set} }} notrack; }}; }}\n"
    )
}

/// Keeps the owner table alive. Dropping it closes the pipe and the kernel
/// removes the rules.
pub struct LoopbackNotrack {
    table: String,
    stdin: Option<ChildStdin>,
    child: Child,
}

impl LoopbackNotrack {
    /// Install the bypass for `ports`. Requires `CAP_NET_ADMIN` and an `nft`
    /// binary on `PATH`.
    pub async fn install(ports: &[u16]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !ports.is_empty() && !ports.contains(&0),
            "invalid listener ports"
        );
        let table = table_name(std::process::id());
        let mut child = Command::new("nft")
            .arg("-i")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| anyhow::anyhow!("cannot start `nft -i`: {e}"))?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("nft stdin unavailable"))?;
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::warn!(target: "openproxy::loopback_notrack", "nft: {line}");
                }
            });
        }
        stdin.write_all(script(&table, ports).as_bytes()).await?;
        stdin.flush().await?;

        let mut guard = Self {
            table,
            stdin: Some(stdin),
            child,
        };
        tokio::time::timeout(INSTALL_TIMEOUT, guard.wait_installed(ports))
            .await
            .map_err(|_| anyhow::anyhow!("nft did not install the table in time"))??;
        Ok(guard)
    }

    /// Owner table name, for logs.
    #[must_use]
    pub fn table(&self) -> &str {
        &self.table
    }

    async fn wait_installed(&mut self, ports: &[u16]) -> anyhow::Result<()> {
        loop {
            if let Some(status) = self.child.try_wait()? {
                anyhow::bail!("`nft -i` exited early with {status}");
            }
            let listed = Command::new("nft")
                .args(["-j", "list", "table", "inet", &self.table])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .output()
                .await?;
            if listed.status.success() {
                let ruleset: serde_json::Value = serde_json::from_slice(&listed.stdout)?;
                anyhow::ensure!(
                    installed_exactly(&ruleset, &self.table, ports),
                    "nft installed table does not match the requested rules"
                );
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Close the pipe and wait for `nft` to exit, which removes the table.
    pub async fn remove(mut self) {
        drop(self.stdin.take());
        if tokio::time::timeout(INSTALL_TIMEOUT, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

/// Validate actual nft JSON, not substrings in table names or diagnostic text.
fn installed_exactly(json: &serde_json::Value, table: &str, ports: &[u16]) -> bool {
    use serde_json::json;
    let Some(entries) = json["nftables"].as_array() else {
        return false;
    };
    let mut ports = ports.to_vec();
    ports.sort_unstable();
    ports.dedup();
    let right = match ports.as_slice() {
        [port] => json!(port),
        _ => json!({"set": ports}),
    };
    let rule = |field| {
        json!([
            {"match": {"op": "==", "left": {"meta": {"key": "oifname"}}, "right": "lo"}},
            {"match": {"op": "==", "left": {"payload": {"protocol": "tcp", "field": field}}, "right": right}},
            {"notrack": null}
        ])
    };
    let tables: Vec<_> = entries.iter().filter_map(|e| e.get("table")).collect();
    let chains: Vec<_> = entries.iter().filter_map(|e| e.get("chain")).collect();
    let rules: Vec<_> = entries.iter().filter_map(|e| e.get("rule")).collect();
    tables.len() == 1
        && tables[0]["family"] == "inet"
        && tables[0]["name"] == table
        && tables[0]["flags"] == json!(["owner"])
        && chains.len() == 1
        && chains[0]["family"] == "inet"
        && chains[0]["table"] == table
        && chains[0]["name"] == "output"
        && chains[0]["type"] == "filter"
        && chains[0]["hook"] == "output"
        && chains[0]["prio"] == -300
        && chains[0]["policy"] == "accept"
        && rules.len() == 2
        && rules
            .iter()
            .all(|r| r["family"] == "inet" && r["table"] == table && r["chain"] == "output")
        && rules.iter().any(|r| r["expr"] == rule("dport"))
        && rules.iter().any(|r| r["expr"] == rule("sport"))
}

#[cfg(test)]
#[path = "loopback_notrack_tests.rs"]
mod tests;
