//! Linux process metrics measured externally: no sampler inside the proxy.
use anyhow::{Context, ensure};
use serde::Serialize;
use std::{collections::BTreeSet, fs, time::Instant};

pub fn parse_cpus(list: &str) -> anyhow::Result<Vec<usize>> {
    let mut cpus = BTreeSet::new();
    for part in list.trim().split(',') {
        let (start, end): (usize, usize) = if let Some((a, b)) = part.split_once('-') {
            (a.parse()?, b.parse()?)
        } else {
            let n = part.parse()?;
            (n, n)
        };
        ensure!(start <= end && end < 65536, "invalid CPU range");
        cpus.extend(start..=end);
    }
    Ok(cpus.into_iter().collect())
}

pub fn allowed_cpus() -> anyhow::Result<Vec<usize>> {
    let status = fs::read_to_string("/proc/self/status")?;
    let list = status
        .lines()
        .find_map(|s| s.strip_prefix("Cpus_allowed_list:"))
        .context("no CPU affinity in proc")?;
    parse_cpus(list)
}

pub fn ticks_per_second() -> anyhow::Result<u64> {
    let output = std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .output()?;
    ensure!(output.status.success(), "getconf CLK_TCK failed");
    let ticks: u64 = std::str::from_utf8(&output.stdout)?.trim().parse()?;
    ensure!(ticks > 0, "invalid CLK_TCK");
    Ok(ticks)
}

#[derive(Clone, Default, Serialize)]
pub struct Snapshot {
    pub rss_kib: u64,
    pub lifetime_hwm_kib: u64,
    pub user_ticks: u64,
    pub system_ticks: u64,
    pub minor_faults: u64,
    pub major_faults: u64,
    pub logical_read_bytes: u64,
    pub logical_write_bytes: u64,
    pub storage_read_bytes: u64,
    pub storage_write_bytes: u64,
    pub read_syscalls: u64,
    pub write_syscalls: u64,
    /// Sum of context switches of live threads from `/proc/PID/task/*/status`.
    /// Exited threads are absent, so deltas are lower bounds.
    pub voluntary_context_switches: u64,
    pub involuntary_context_switches: u64,
    /// Sum over threads of `/proc/PID/task/*/schedstat` field 2: ns spent
    /// runnable but waiting for a CPU (run-queue wait).
    pub run_queue_wait_ns: u64,
}

fn stat_fields(stat: &str) -> anyhow::Result<[u64; 4]> {
    // comm may contain spaces or ')', so fields cannot be split from byte zero.
    let tail = stat.rsplit_once(')').context("invalid proc stat")?.1;
    let fields: Vec<_> = tail.split_whitespace().collect();
    let field = |i: usize| -> anyhow::Result<u64> {
        fields
            .get(i)
            .context("short proc stat")?
            .parse()
            .map_err(Into::into)
    };
    Ok([field(11)?, field(12)?, field(7)?, field(9)?])
}

/// Threads that exited during the window drop out of the sum, so this is a
/// lower bound for short-lived threads (Tokio workers live for the whole run).
fn thread_scheduler_counters(root: &str) -> anyhow::Result<(u64, u64, u64)> {
    let tasks = fs::read_dir(format!("{root}/task"))?;
    let mut totals = (0, 0, 0);
    for task in tasks.flatten() {
        // Threads may exit between enumeration and read.
        let Ok(status) = fs::read_to_string(task.path().join("status")) else {
            continue;
        };
        let get = |key: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0)
        };
        totals.0 += get("voluntary_ctxt_switches:");
        totals.1 += get("nonvoluntary_ctxt_switches:");
        if let Ok(s) = fs::read_to_string(task.path().join("schedstat")) {
            totals.2 += s
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
        }
    }
    Ok(totals)
}

/// Host-wide TCP counters from `/proc/net/netstat` and `/proc/net/snmp`.
/// Includes every process on the host: a delta is attributable to the
/// benchmark only when nothing else is using TCP heavily at the same time.
#[derive(Clone, Default, Serialize)]
pub struct TcpCounters {
    pub listen_overflows: u64,
    pub listen_drops: u64,
    pub syn_retransmits: u64,
    pub retransmitted_segments: u64,
    pub active_opens: u64,
    pub passive_opens: u64,
    pub attempt_fails: u64,
    pub established_resets: u64,
}

fn proc_net_pairs(text: &str) -> Vec<(String, u64)> {
    let lines: Vec<_> = text.lines().collect();
    let mut out = Vec::new();
    for pair in lines.chunks(2) {
        let [names, values] = pair else { continue };
        let (Some((prefix, names)), Some((_, values))) =
            (names.split_once(':'), values.split_once(':'))
        else {
            continue;
        };
        for (name, value) in names.split_whitespace().zip(values.split_whitespace()) {
            if let Ok(v) = value.parse() {
                out.push((format!("{prefix}.{name}"), v));
            }
        }
    }
    out
}

impl TcpCounters {
    pub fn read() -> anyhow::Result<Self> {
        let mut text = fs::read_to_string("/proc/net/netstat")?;
        text.push_str(&fs::read_to_string("/proc/net/snmp")?);
        let pairs = proc_net_pairs(&text);
        let get = |key: &str| pairs.iter().find(|(k, _)| k == key).map_or(0, |(_, v)| *v);
        Ok(Self {
            listen_overflows: get("TcpExt.ListenOverflows"),
            listen_drops: get("TcpExt.ListenDrops"),
            syn_retransmits: get("TcpExt.TCPSynRetrans"),
            retransmitted_segments: get("Tcp.RetransSegs"),
            active_opens: get("Tcp.ActiveOpens"),
            passive_opens: get("Tcp.PassiveOpens"),
            attempt_fails: get("Tcp.AttemptFails"),
            established_resets: get("Tcp.EstabResets"),
        })
    }

    pub fn delta(&self, before: &Self) -> Self {
        let d = |a: u64, b: u64| a.saturating_sub(b);
        Self {
            listen_overflows: d(self.listen_overflows, before.listen_overflows),
            listen_drops: d(self.listen_drops, before.listen_drops),
            syn_retransmits: d(self.syn_retransmits, before.syn_retransmits),
            retransmitted_segments: d(self.retransmitted_segments, before.retransmitted_segments),
            active_opens: d(self.active_opens, before.active_opens),
            passive_opens: d(self.passive_opens, before.passive_opens),
            attempt_fails: d(self.attempt_fails, before.attempt_fails),
            established_resets: d(self.established_resets, before.established_resets),
        }
    }
}

impl Snapshot {
    pub fn read(pid: u32) -> anyhow::Result<Self> {
        let root = format!("/proc/{pid}");
        let stat = fs::read_to_string(format!("{root}/stat"))?;
        let [user_ticks, system_ticks, minor_faults, major_faults] = stat_fields(&stat)?;
        let status = fs::read_to_string(format!("{root}/status"))?;
        let value = |key: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse().ok())
                .context("missing RSS metric")
        };
        let (voluntary, involuntary, run_queue_wait_ns) = thread_scheduler_counters(&root)?;
        let mut snapshot = Self {
            rss_kib: value("VmRSS:")?,
            lifetime_hwm_kib: value("VmHWM:")?,
            user_ticks,
            system_ticks,
            minor_faults,
            major_faults,
            voluntary_context_switches: voluntary,
            involuntary_context_switches: involuntary,
            run_queue_wait_ns,
            ..Self::default()
        };
        for line in fs::read_to_string(format!("{root}/io"))?.lines() {
            let Some((key, number)) = line.split_once(':') else {
                continue;
            };
            let value = number.trim().parse()?;
            match key {
                "rchar" => snapshot.logical_read_bytes = value,
                "wchar" => snapshot.logical_write_bytes = value,
                "read_bytes" => snapshot.storage_read_bytes = value,
                "write_bytes" => snapshot.storage_write_bytes = value,
                "syscr" => snapshot.read_syscalls = value,
                "syscw" => snapshot.write_syscalls = value,
                _ => {}
            }
        }
        Ok(snapshot)
    }

    pub fn delta(&self, before: &Self) -> Self {
        Self {
            rss_kib: self.rss_kib.saturating_sub(before.rss_kib),
            lifetime_hwm_kib: self.lifetime_hwm_kib,
            user_ticks: self.user_ticks.saturating_sub(before.user_ticks),
            system_ticks: self.system_ticks.saturating_sub(before.system_ticks),
            minor_faults: self.minor_faults.saturating_sub(before.minor_faults),
            major_faults: self.major_faults.saturating_sub(before.major_faults),
            logical_read_bytes: self
                .logical_read_bytes
                .saturating_sub(before.logical_read_bytes),
            logical_write_bytes: self
                .logical_write_bytes
                .saturating_sub(before.logical_write_bytes),
            storage_read_bytes: self
                .storage_read_bytes
                .saturating_sub(before.storage_read_bytes),
            storage_write_bytes: self
                .storage_write_bytes
                .saturating_sub(before.storage_write_bytes),
            read_syscalls: self.read_syscalls.saturating_sub(before.read_syscalls),
            write_syscalls: self.write_syscalls.saturating_sub(before.write_syscalls),
            voluntary_context_switches: self
                .voluntary_context_switches
                .saturating_sub(before.voluntary_context_switches),
            involuntary_context_switches: self
                .involuntary_context_switches
                .saturating_sub(before.involuntary_context_switches),
            run_queue_wait_ns: self
                .run_queue_wait_ns
                .saturating_sub(before.run_queue_wait_ns),
        }
    }
}

#[derive(Serialize)]
pub struct ProcessReport {
    pub pid: u32,
    pub baseline: Snapshot,
    pub after_client: Snapshot,
    pub after_persistence: Snapshot,
    pub delta_including_persistence: Snapshot,
    pub peak_sampled_rss_kib: u64,
    pub sample_count: u64,
    pub sampler_errors: u64,
    pub cpu_seconds: f64,
    pub window_seconds_including_persistence: f64,
    pub cpu_percent_of_one_core: f64,
}

pub struct Sampler {
    pids: Vec<u32>,
    baseline: Vec<Snapshot>,
    start: Instant,
    stop: std::sync::mpsc::Sender<()>,
    task: Option<std::thread::JoinHandle<Vec<(u64, u64, u64)>>>,
}

impl Sampler {
    pub fn start(pids: Vec<u32>) -> anyhow::Result<Self> {
        let baseline = pids
            .iter()
            .map(|&p| Snapshot::read(p))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let (stop, rx) = std::sync::mpsc::channel();
        let worker_pids = pids.clone();
        let mut peaks: Vec<_> = baseline.iter().map(|s| (s.rss_kib, 0, 0)).collect();
        let task = std::thread::spawn(move || {
            loop {
                for (pid, (peak, count, errors)) in worker_pids.iter().zip(&mut peaks) {
                    let rss = fs::read_to_string(format!("/proc/{pid}/status"))
                        .ok()
                        .and_then(|s| {
                            s.lines().find_map(|l| {
                                l.strip_prefix("VmRSS:")
                                    .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
                            })
                        });
                    match rss {
                        Some(rss) => {
                            *peak = (*peak).max(rss);
                            *count += 1;
                        }
                        None => *errors += 1,
                    }
                }
                if rx.recv_timeout(std::time::Duration::from_millis(5)).is_ok() {
                    break;
                }
            }
            peaks
        });
        Ok(Self {
            pids,
            baseline,
            start: Instant::now(),
            stop,
            task: Some(task),
        })
    }

    pub fn snapshot(&self) -> anyhow::Result<Vec<Snapshot>> {
        self.pids.iter().map(|&p| Snapshot::read(p)).collect()
    }

    pub fn finish(
        mut self,
        after_client: Vec<Snapshot>,
        ticks: u64,
    ) -> anyhow::Result<Vec<ProcessReport>> {
        let final_snapshots = self.snapshot()?;
        let elapsed = self.start.elapsed().as_secs_f64();
        let _ = self.stop.send(());
        let peaks = self
            .task
            .take()
            .context("missing sampler")?
            .join()
            .map_err(|_| anyhow::anyhow!("sampler panicked"))?;
        Ok(self
            .pids
            .iter()
            .copied()
            .zip(self.baseline.iter().cloned())
            .zip(after_client)
            .zip(final_snapshots)
            .zip(peaks)
            .map(
                |((((pid, baseline), after_client), after_persistence), (peak, count, errors))| {
                    let delta = after_persistence.delta(&baseline);
                    let cpu_seconds = (delta.user_ticks + delta.system_ticks) as f64 / ticks as f64;
                    ProcessReport {
                        pid,
                        baseline,
                        after_client,
                        after_persistence,
                        delta_including_persistence: delta,
                        peak_sampled_rss_kib: peak,
                        sample_count: count,
                        sampler_errors: errors,
                        cpu_seconds,
                        window_seconds_including_persistence: elapsed,
                        cpu_percent_of_one_core: 100.0 * cpu_seconds / elapsed,
                    }
                },
            )
            .collect())
    }
}
impl Drop for Sampler {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

pub fn self_test() -> anyhow::Result<()> {
    ensure!(
        parse_cpus("0-2,2,4")? == vec![0, 1, 2, 4],
        "CPU list parser"
    );
    let stat = "42 (tricky ) name) R 1 2 3 4 5 6 7 8 9 10 11 12 13 14";
    ensure!(stat_fields(stat)? == [11, 12, 7, 9], "proc stat indexes");
    ensure!(Snapshot::read(std::process::id())?.rss_kib > 0, "RSS read");
    let pairs = proc_net_pairs("TcpExt: A B\nTcpExt: 1 2\nTcp: C\nTcp: 3\n");
    ensure!(
        pairs
            == vec![
                ("TcpExt.A".into(), 1),
                ("TcpExt.B".into(), 2),
                ("Tcp.C".into(), 3)
            ],
        "proc net parser"
    );
    TcpCounters::read()?;
    Ok(())
}
