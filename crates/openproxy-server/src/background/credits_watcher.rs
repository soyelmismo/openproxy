//! On-chain USDC top-up watcher (Base L2).
//!
//! One background task polls a Base JSON-RPC node for USDC `Transfer` logs
//! whose `to` is the single collector address, and feeds each log to
//! `openproxy_core::credits::matcher::apply_payment`, which owns the
//! idempotency (`matched_payments` PK) and the compare-and-swap that makes a
//! payment credit exactly one order exactly once.
//!
//! The scanner is deliberately dumb: it discovers logs, advances the
//! persisted cursor, and hands them over. All assignment logic lives in the
//! core module, so the money path is testable without a network.

use openproxy_core::credits::matcher::{PaymentOutcome, apply_payment};
use openproxy_db::DbPool;
use openproxy_types::config::CreditsConfig;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// ERC-20 `Transfer(address,address,uint256)` topic0 (keccak of the sig).
const TRANSFER_TOPIC: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// A decoded USDC transfer to the collector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsdcTransfer {
    pub tx_hash: String,
    pub log_index: i64,
    pub block_number: i64,
    /// Amount in micro-USDC (6 decimals).
    pub amount_units: i64,
    pub from_address: String,
    pub to_address: String,
}

/// Minimal JSON-RPC client over the workspace HTTP stack.
pub struct BaseRpc {
    url: String,
    client: Arc<openproxy_adapters::upstream::UpstreamClient>,
}

impl BaseRpc {
    #[must_use]
    pub fn new(url: String, client: Arc<openproxy_adapters::upstream::UpstreamClient>) -> Self {
        Self { url, client }
    }

    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let body = bytes::Bytes::from(
            serde_json::to_vec(&payload).map_err(|e| format!("serialize rpc payload: {e}"))?,
        );
        let req = openproxy_adapters::upstream::UpstreamRequest::post_json(&self.url, body)
            .with_header(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
        let cancel = openproxy_adapters::upstream::CancellationToken::new();
        let resp = self
            .client
            .call(
                req,
                openproxy_adapters::upstream::TimeoutProfile::ModelDiscovery,
                cancel,
            )
            .await
            .map_err(|e| format!("rpc {method} transport error: {e}"))?;
        let status = resp.status;
        let raw = resp
            .collect()
            .await
            .map_err(|e| format!("rpc {method} body error: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "rpc {method} HTTP {status}: {}",
                String::from_utf8_lossy(&raw)
            ));
        }
        let parsed: serde_json::Value =
            serde_json::from_slice(&raw).map_err(|e| format!("rpc {method} invalid JSON: {e}"))?;
        if let Some(err) = parsed.get("error") {
            return Err(format!("rpc {method} error: {err}"));
        }
        parsed
            .get("result")
            .cloned()
            .ok_or_else(|| format!("rpc {method} returned no result"))
    }

    /// Latest block tagged `safe` (reorg-resistant on Base).
    ///
    /// # Errors
    /// Transport or RPC failures.
    pub async fn safe_block_number(&self) -> Result<i64, String> {
        let v = self
            .call("eth_getBlockByNumber", serde_json::json!(["safe", false]))
            .await?;
        parse_quantity(
            v.get("number")
                .and_then(serde_json::Value::as_str)
                .ok_or("safe block missing number")?,
        )
    }

    /// `eth_getLogs` for USDC transfers into `collector` in `[from, to]`.
    ///
    /// # Errors
    /// Transport or RPC failures.
    pub async fn transfer_logs(
        &self,
        usdc: &str,
        collector: &str,
        from_block: i64,
        to_block: i64,
    ) -> Result<Vec<UsdcTransfer>, String> {
        let collector_topic = format!(
            "0x{:0>64}",
            collector.trim_start_matches("0x").to_lowercase()
        );
        let filter = serde_json::json!({
            "address": usdc,
            "fromBlock": format!("0x{from_block:x}"),
            "toBlock": format!("0x{to_block:x}"),
            "topics": [TRANSFER_TOPIC, null, collector_topic],
        });
        let v = self.call("eth_getLogs", serde_json::json!([filter])).await?;
        let logs = v.as_array().ok_or("eth_getLogs result is not an array")?;
        let mut out = Vec::with_capacity(logs.len());
        for log in logs {
            if let Some(t) = parse_transfer_log(log, collector) {
                out.push(t);
            }
        }
        Ok(out)
    }
}

/// Parse a hex quantity (`0x1a`) into an integer.
fn parse_quantity(raw: &str) -> Result<i64, String> {
    let trimmed = raw.trim_start_matches("0x");
    i64::from_str_radix(trimmed, 16).map_err(|e| format!("invalid hex quantity {raw}: {e}"))
}

/// Decode one `eth_getLogs` entry into a [`UsdcTransfer`], returning `None`
/// for logs that do not carry exactly one Transfer with a matching `to` and a
/// value that fits the ledger's integer domain.
fn parse_transfer_log(log: &serde_json::Value, collector: &str) -> Option<UsdcTransfer> {
    let topics = log.get("topics")?.as_array()?;
    if topics.len() != 3 {
        return None;
    }
    let topic0 = topics.first()?.as_str()?;
    if !topic0.eq_ignore_ascii_case(TRANSFER_TOPIC) {
        return None;
    }
    // A topic is a 32-byte word: the 20-byte address sits in the LAST 40 hex
    // chars, left-padded with zeroes. Compare those 40 chars, never the whole
    // word, or every real payment would silently fail to match.
    let word_to_addr = |topic: &str| -> String {
        let word = topic.trim_start_matches("0x");
        match word.len() {
            n if n > 40 => word.get(n - 40..).unwrap_or(word).to_string(),
            _ => word.to_string(),
        }
    };
    let from_address = format!("0x{}", word_to_addr(topics.get(1)?.as_str()?));
    let to_topic = topics.get(2)?.as_str()?;
    let to_address = format!("0x{}", word_to_addr(to_topic));
    let collector_hex = collector.trim_start_matches("0x");
    if !to_address
        .trim_start_matches("0x")
        .eq_ignore_ascii_case(collector_hex)
    {
        return None;
    }
    let data = log.get("data")?.as_str()?;
    let raw_value = data.trim_start_matches("0x");
    // ERC-20 value is a 32-byte word. Anything that overflows i64 is nonsense
    // for a top-up and is skipped rather than truncated.
    let value = i128::from_str_radix(raw_value, 16).ok()?;
    let amount_units = i64::try_from(value).ok()?;
    if amount_units <= 0 {
        return None;
    }
    let tx_hash = log.get("transactionHash")?.as_str()?.to_string();
    let log_index = parse_quantity(log.get("logIndex")?.as_str()?).ok()?;
    let block_number = parse_quantity(log.get("blockNumber")?.as_str()?).ok()?;
    Some(UsdcTransfer {
        tx_hash,
        log_index,
        block_number,
        amount_units,
        from_address,
        to_address,
    })
}

/// Outcome summary of one scan tick.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScanReport {
    pub from_block: i64,
    pub to_block: i64,
    pub logs_seen: usize,
    pub credited: usize,
    pub already_seen: usize,
    pub unmatched: usize,
}

/// Background service: poll Base for USDC transfers into the collector and
/// apply each one to the credit ledger.
pub struct CreditsWatcherService {
    pub db_pool: Arc<DbPool>,
    pub config: CreditsConfig,
    pub master_key: Arc<openproxy_db::MasterKey>,
    pub upstream_client: Arc<openproxy_adapters::upstream::UpstreamClient>,
}

impl CreditsWatcherService {
    /// One scan tick: read the cursor, fetch `safe`, process every transfer.
    ///
    /// # Errors
    /// Transport/RPC failures and DB failures. The cursor is only advanced
    /// after a successful pass, so a failure simply retries the same range.
    pub async fn tick(&self) -> Result<ScanReport, String> {
        let pool = Arc::clone(&self.db_pool);
        let mut report = ScanReport::default();

        let last = pool
            .spawn_read(openproxy_db::credits_ledger::last_scanned_block)
            .await
            .map_err(|e| format!("read watcher cursor: {e}"))?;

        let rpc = BaseRpc::new(
            self.config.rpc_url.clone(),
            Arc::clone(&self.upstream_client),
        );
        let safe = rpc.safe_block_number().await?;
        if last == 0 {
            // First run: start at the head instead of replaying all history.
            // Pre-existing deposits are not top-ups from this system.
            pool.spawn_write(move |w| openproxy_db::credits_ledger::set_last_scanned_block(w, safe))
                .await
                .map_err(|e| format!("init watcher cursor: {e}"))?;
            report.from_block = safe;
            report.to_block = safe;
            return Ok(report);
        }
        if safe <= last {
            report.from_block = last;
            report.to_block = safe;
            return Ok(report);
        }

        let from_block = last + 1;
        let transfers = rpc
            .transfer_logs(
                &self.config.usdc_address,
                &self.config.collector_address,
                from_block,
                safe,
            )
            .await?;
        report.from_block = from_block;
        report.to_block = safe;
        report.logs_seen = transfers.len();

        for t in transfers {
            let master = Arc::clone(&self.master_key);
            let tx_hash = t.tx_hash.clone();
            let from = t.from_address.clone();
            let outcome = pool
                .spawn_write(move |w| {
                    apply_payment(
                        w,
                        &master,
                        &tx_hash,
                        t.log_index,
                        t.block_number,
                        t.amount_units,
                        Some(&from),
                    )
                })
                .await
                .map_err(|e| format!("apply payment: {e}"))?;
            match outcome {
                PaymentOutcome::Credited { .. } => report.credited += 1,
                PaymentOutcome::AlreadySeen { .. } => report.already_seen += 1,
                PaymentOutcome::Unmatched { .. } | PaymentOutcome::OrderExpired { .. } => {
                    report.unmatched += 1;
                }
            }
        }

        // Advance the cursor only after the whole range was applied.
        pool.spawn_write(move |w| openproxy_db::credits_ledger::set_last_scanned_block(w, safe))
            .await
            .map_err(|e| format!("advance watcher cursor: {e}"))?;
        Ok(report)
    }
}

impl crate::background::BackgroundService for CreditsWatcherService {
    fn name(&self) -> &'static str {
        "credits_watcher"
    }

    async fn run(&self, cancel: openproxy_adapters::upstream::CancellationToken) {
        let interval = std::time::Duration::from_secs(self.config.scan_interval_secs.max(2));
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!("credits watcher stopping");
                    return;
                }
                _ = ticker.tick() => {
                    match self.tick().await {
                        Ok(report) if report.logs_seen > 0 || report.credited > 0 => {
                            tracing::info!(
                                from = report.from_block,
                                to = report.to_block,
                                logs = report.logs_seen,
                                credited = report.credited,
                                unmatched = report.unmatched,
                                "credits watcher tick"
                            );
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "credits watcher tick failed; will retry");
                        }
                    }
                }
            }
        }
    }
}

/// Background service: kill expired lots and stale orders, and self-audit the
/// ledger invariant once a minute. Any inconsistency is CRITICAL: money
/// appeared or vanished outside the matcher's transaction.
pub struct CreditsExpiryService {
    pub db_pool: Arc<DbPool>,
    pub interval: std::time::Duration,
}

impl crate::background::BackgroundService for CreditsExpiryService {
    fn name(&self) -> &'static str {
        "credits_expiry"
    }

    async fn run(&self, cancel: openproxy_adapters::upstream::CancellationToken) {
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {
                    let pool = Arc::clone(&self.db_pool);
                    match pool.spawn_write(|w| {
                        let lots = openproxy_db::credits_ledger::expire_due(w)?;
                        let orders = openproxy_core::credits::orders::expire_stale_orders(w)?;
                        let audit = openproxy_db::credits_ledger::reconcile(w)?;
                        Ok((lots, orders, audit))
                    }).await {
                        Ok((lots, orders, audit)) => {
                            if lots > 0 || orders > 0 {
                                tracing::info!(lots, orders, "expired credit lots/orders");
                            }
                            if !audit.is_consistent() {
                                tracing::error!(
                                    paid_orders = audit.paid_orders,
                                    topup_rows = audit.topup_rows,
                                    expected_units = audit.credited_expected_units,
                                    actual_units = audit.credited_actual_units,
                                    orphans = audit.orphans,
                                    "CRITICAL: credit ledger invariant violated"
                                );
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "credit expiry tick failed"),
                    }
                }
            }
        }
    }
}

/// Maintenance sweep: fully delete API keys idle for
/// [`SWEEP_IDLE_MONTHS`](openproxy_core::credits::sweep::SWEEP_IDLE_MONTHS).
/// Runs once a day; keys with a positive balance are swept too — a dead key
/// must not keep a balance on the books.
pub struct CreditsSweepService {
    pub db_pool: Arc<DbPool>,
    pub interval: std::time::Duration,
}

impl crate::background::BackgroundService for CreditsSweepService {
    fn name(&self) -> &'static str {
        "credits_sweep"
    }

    async fn run(&self, cancel: openproxy_adapters::upstream::CancellationToken) {
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {
                    let pool = Arc::clone(&self.db_pool);
                    match pool.spawn_write(|w| {
                        openproxy_core::credits::sweep::sweep_idle_keys(w, 500)
                    }).await {
                        Ok(swept) => {
                            if !swept.is_empty() {
                                let dead_units: i64 = swept.iter().map(|k| k.balance_units).sum();
                                tracing::info!(
                                    swept = swept.len(),
                                    dead_balance_units = dead_units,
                                    ids = ?swept.iter().map(|k| k.id).collect::<Vec<_>>(),
                                    "swept idle api keys"
                                );
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "credits sweep tick failed"),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLLECTOR: &str = "0x12c7ab78f21ba0253a21d34a0151d44ddb824950";

    fn log(to: &str, value_hex: &str, idx: &str) -> serde_json::Value {
        serde_json::json!({
            "address": "0xusdc-token-contract",
            "topics": [
                TRANSFER_TOPIC,
                "0x0000000000000000000000001111111111111111111111111111111111111111",
                format!("0x000000000000000000000000{}", to.trim_start_matches("0x"))
            ],
            "data": format!("0x{:0>64}", value_hex.trim_start_matches("0x")),
            "logIndex": idx,
            "blockNumber": "0x10",
            "transactionHash": "0xabc123"
        })
    }

    #[test]
    fn parses_a_usdc_transfer_to_the_collector() {
        let t = parse_transfer_log(&log(COLLECTOR, "989680", "0x0"), COLLECTOR).expect("parse");
        assert_eq!(t.amount_units, 10_000_000, "10 USDC in micro units");
        assert_eq!(t.log_index, 0);
        assert_eq!(t.block_number, 16);
        assert_eq!(t.tx_hash, "0xabc123");
        assert!(t.to_address.eq_ignore_ascii_case(COLLECTOR));
    }

    #[test]
    fn ignores_transfers_to_other_addresses() {
        let other = "0x9999999999999999999999999999999999999999";
        assert!(parse_transfer_log(&log(other, "989680", "0x0"), COLLECTOR).is_none());
    }

    #[test]
    fn ignores_zero_value_and_non_transfer_topics() {
        assert!(parse_transfer_log(&log(COLLECTOR, "0", "0x0"), COLLECTOR).is_none());
        let mut bad = log(COLLECTOR, "989680", "0x0");
        bad["topics"][0] = serde_json::json!("0xdeadbeef");
        assert!(parse_transfer_log(&bad, COLLECTOR).is_none());
    }

    #[test]
    fn ignores_oversized_values_instead_of_wrapping() {
        assert!(
            parse_transfer_log(&log(COLLECTOR, "400000000000000000", "0x0"), COLLECTOR).is_none()
        );
    }

    #[test]
    fn parses_hex_quantities() {
        assert_eq!(parse_quantity("0x0").unwrap(), 0);
        assert_eq!(parse_quantity("0x10").unwrap(), 16);
        assert_eq!(parse_quantity("0x8a2f").unwrap(), 35375);
        assert!(parse_quantity("zz").is_err());
    }

    #[test]
    fn jittered_amount_survives_hex_roundtrip() {
        // The exact amount a buyer must send includes the jitter in the low
        // six decimals; the parser must preserve every digit.
        let amount_units: i64 = 10_000_000 + 987_654;
        let hex = format!("{amount_units:x}");
        let t = parse_transfer_log(&log(COLLECTOR, &hex, "0x1"), COLLECTOR).expect("parse");
        assert_eq!(t.amount_units, amount_units);
    }
}
