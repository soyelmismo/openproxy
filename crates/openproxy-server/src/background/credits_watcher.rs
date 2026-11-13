//! On-chain USDC top-up watcher (Base L2).
//!
//! One background task polls a Base JSON-RPC node for USDC `Transfer` logs
//! whose `to` is the single collector address, and feeds each log to
//! `openproxy_core::credits::matcher::apply_payment_at`, which owns the
//! idempotency (`matched_payments` PK) and the compare-and-swap that makes a
//! payment credit exactly one order exactly once.
//!
//! The scanner only discovers logs and advances the persisted cursor; all
//! assignment logic lives in the core module, so the money path is testable
//! without a network. Hardening rules (money path):
//!
//! - **Bounded chunks, catch-up per tick.** Each `eth_getLogs` call covers at
//!   most [`MAX_BLOCK_CHUNK`] blocks and the cursor advances after that chunk
//!   succeeds. A tick then keeps scanning further chunks (up to
//!   [`MAX_CHUNKS_PER_TICK`]) until the finality head is covered, so a long
//!   downtime catches up in a few intervals instead of one chunk per interval,
//!   while no single request ever asks a provider for a range it rejects.
//! - **Chain identity.** `eth_chainId` must equal `credits.chain_id` before
//!   any log is trusted; the wrong RPC would otherwise mean silent zero logs.
//! - **Finality, fail closed.** `safe` is preferred, then `finalized`. If the
//!   node offers neither, the tick fails and the cursor is held: the watcher
//!   never credits a `latest` head by subtracting a heuristic reorg margin.
//! - **Bootstrap anchored on the oldest unsettled order.** With no cursor yet,
//!   the watcher does not start "from now" nor from a fixed lookback: it starts
//!   at the block that carries the `created_at` of the oldest still unsettled
//!   order (`pending` or `expired`), located by a BOUNDED binary search over
//!   block timestamps (`eth_getBlockByNumber`, at most
//!   [`MAX_BLOCK_TIME_SEARCH_STEPS`] probes). Rationale is the matcher's own
//!   rule: a transfer counts when its BLOCK timestamp falls inside
//!   `[created_at, expires_at + grace]`. A payment made inside that window and
//!   only processed days later — process down, cursor lost, order already
//!   expired — is therefore still credited; filtering the bootstrap on "still
//!   payable NOW" dropped exactly those payments. With no unsettled order the
//!   configured payment window bounds the replay. There is deliberately NO age
//!   cap: a tick already scans at most [`MAX_CHUNKS_PER_TICK`] ×
//!   [`MAX_BLOCK_CHUNK`] blocks and the timestamp search costs at most
//!   [`MAX_BLOCK_TIME_SEARCH_STEPS`] probes, so an order unsettled for months
//!   is caught up incrementally across ticks instead of having its payment
//!   dropped by an arbitrary horizon (`start.max(head - 90 days)` silently
//!   discarded valid, already-paid deposits). A finality head that lags the
//!   wall clock starts the scan AT the head (no block at or below it can carry
//!   a payment for the order yet) and then catches up incrementally, never
//!   above the finality head.
//! - **Strict decoding.** `removed` logs are skipped, contract/topic/data/tx
//!   sizes are validated, and a malformed log aborts the tick WITHOUT
//!   advancing the cursor. Zero-value and > `i64` transfers are VALID ERC-20
//!   transfers: they are ignored terminally (never credited, never truncated,
//!   never cursor-blocking) so a dust attack cannot wedge the checkout.
//! - **Block time, not wall clock.** Every credited payment is matched against
//!   the block timestamp (`eth_getBlockByNumber`), so a delayed confirmation
//!   cannot turn an on-time payment into a late one.
//!
//! ## Configuration is frozen for the process lifetime
//!
//! `rpc_url`, `usdc_address`, `collector_address`, `chain_id` and the payment
//! window are copied at startup. There is no hot reload: the admin switch
//! (`gateway_enabled`) is DB-backed and instant, but changing the collector or
//! the RPC requires a restart. A restart with a new collector makes orders
//! quoted with the old collector **unmatchable** by this watcher, so drain
//! pending orders first (or reconcile them manually). Do not promise hot
//! destination changes.

mod logs;

pub use self::logs::UsdcTransfer;
use self::logs::{DecodedLog, TRANSFER_TOPIC, decode_log, parse_quantity};
use openproxy_core::credits::matcher::{PaymentBlock, PaymentOutcome, apply_payment_at};
use openproxy_core::credits::orders::CONFIRMATION_GRACE_SECS;
use openproxy_db::DbPool;
use openproxy_types::config::CreditsConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Maximum blocks per `eth_getLogs` call. Public Base providers commonly cap
/// ranges/results; 500 stays far inside those limits.
pub const MAX_BLOCK_CHUNK: i64 = 500;

/// Chunks one tick may process back-to-back. A long outage (hours of blocks)
/// is caught up within a few intervals instead of one chunk per interval, and
/// the cap keeps a single tick from monopolising the writer for minutes.
pub const MAX_CHUNKS_PER_TICK: usize = 40;

/// Slack ahead of an order's `created_at` when locating its block: the DB
/// stores wall-clock seconds (`datetime('now')`) and the matcher compares them
/// with block timestamps, so a couple of minutes of skew must not cut the
/// payment out of range.
const BOOTSTRAP_TIME_MARGIN_SECS: i64 = 120;
/// `eth_getBlockByNumber` probes allowed to locate the block at a timestamp.
/// The search interval is `[0, head]`, so `log2(i64::MAX)` (63) steps always
/// converge; 64 bounds a node whose timestamps do not behave monotonically.
const MAX_BLOCK_TIME_SEARCH_STEPS: usize = 64;

/// Wall-clock timestamp a fresh cursor must start from.
///
/// The matcher accepts a transfer while its BLOCK timestamp is inside
/// `[created_at, expires_at + grace]`, so the earliest block that can settle an
/// unsettled order is the one carrying its `created_at` — not "now", and not
/// "now minus the configured window". Anchoring on `now` (or on orders that are
/// still payable at startup) is what lost payments made inside an order's
/// window and only processed after a crash: the order was `expired` by then,
/// yet the payment was valid.
///
/// With no unsettled order there is nothing old to find, so the configured
/// payment window plus the confirmation grace bounds the replay. Both paths are
/// clamped at genesis and never negative.
const fn bootstrap_target_ts(
    now: i64,
    window_secs: i64,
    oldest_unsettled_created_at: Option<i64>,
) -> i64 {
    let target = match oldest_unsettled_created_at {
        Some(created) => created.saturating_sub(BOOTSTRAP_TIME_MARGIN_SECS),
        None => {
            let window = if window_secs > 0 { window_secs } else { 0 };
            now.saturating_sub(window.saturating_add(CONFIRMATION_GRACE_SECS))
                .saturating_sub(BOOTSTRAP_TIME_MARGIN_SECS)
        }
    };
    if target < 0 { 0 } else { target }
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

    /// `eth_chainId` as an integer.
    ///
    /// # Errors
    /// Transport, RPC or malformed-quantity failures.
    pub async fn chain_id(&self) -> Result<u64, String> {
        let v = self.call("eth_chainId", serde_json::json!([])).await?;
        let raw = v.as_str().ok_or("eth_chainId is not a string")?;
        u64::try_from(parse_quantity(raw)?).map_err(|_| format!("negative chain id {raw}"))
    }

    /// Block number for a tag (`safe`, `finalized`) or an exact hex number.
    /// `Ok(None)` when the node returns `null` for the tag.
    ///
    /// # Errors
    /// Transport, RPC or malformed-quantity failures.
    pub async fn block_number(&self, tag: &str) -> Result<Option<i64>, String> {
        let v = self
            .call("eth_getBlockByNumber", serde_json::json!([tag, false]))
            .await?;
        if v.is_null() {
            return Ok(None);
        }
        let raw = v
            .get("number")
            .and_then(serde_json::Value::as_str)
            .ok_or("block missing number")?;
        parse_quantity(raw).map(Some)
    }

    /// Canonical head to scan: `safe`, else `finalized`.
    ///
    /// Both tags are reorg-resistant commitments. `latest` is deliberately NOT
    /// a fallback: "latest minus N blocks" is a heuristic, not finality, and a
    /// reorg after crediting would mean paying out for a transfer that no
    /// longer exists. No safe/finalized head means fail closed.
    ///
    /// # Errors
    /// Neither the `safe` nor the `finalized` tag yields a block.
    pub async fn finality_head(&self) -> Result<(i64, &'static str), String> {
        match self.block_number("safe").await {
            Ok(Some(n)) => return Ok((n, "safe")),
            Ok(None) => tracing::warn!("rpc returned a null `safe` block; trying `finalized`"),
            Err(e) => tracing::warn!(error = %e, "`safe` tag unsupported; trying `finalized`"),
        }
        match self.block_number("finalized").await {
            Ok(Some(n)) => Ok((n, "finalized")),
            Ok(None) => {
                Err("node has neither `safe` nor `finalized` head; refusing to scan".into())
            }
            Err(e) => Err(format!(
                "node has neither `safe` nor `finalized` head ({e}); refusing to scan"
            )),
        }
    }

    /// Unix timestamp (UTC seconds) of a block.
    ///
    /// # Errors
    /// Transport, RPC or malformed-timestamp failures.
    pub async fn block_timestamp(&self, block_number: i64) -> Result<i64, String> {
        let v = self
            .call(
                "eth_getBlockByNumber",
                serde_json::json!([format!("0x{block_number:x}"), false]),
            )
            .await?;
        let raw = v
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("block {block_number} missing timestamp"))?;
        parse_quantity(raw).and_then(|ts| {
            if ts > 0 {
                Ok(ts)
            } else {
                Err(format!("block {block_number} has non-positive timestamp"))
            }
        })
    }

    /// Lowest block in `[0, head]` whose timestamp is `>= target_ts`.
    ///
    /// Bounded binary search over the (monotone) block clock: at most
    /// [`MAX_BLOCK_TIME_SEARCH_STEPS`] `eth_getBlockByNumber` probes, so
    /// locating a days-old order costs ~25 calls, never a scan. A head whose own
    /// timestamp is older than `target_ts` means the chain has not reached that
    /// wall-clock time yet; the answer is the head itself — never a block above
    /// the finality head, which would trust unfinalised data.
    ///
    /// # Errors
    /// Transport/RPC failures, malformed timestamps, or a node whose block
    /// timestamps do not converge within the step budget (fail closed: the
    /// caller keeps its cursor and retries).
    pub async fn first_block_at_or_after(&self, head: i64, target_ts: i64) -> Result<i64, String> {
        if head < 0 {
            return Err(format!("negative head block {head}"));
        }
        if self.block_timestamp(head).await? < target_ts {
            return Ok(head);
        }
        let mut low = 0i64;
        let mut high = head;
        let mut steps = 0usize;
        while low < high {
            if steps >= MAX_BLOCK_TIME_SEARCH_STEPS {
                return Err(format!(
                    "block timestamp search over [0, {head}] did not converge"
                ));
            }
            steps += 1;
            let mid = low + (high - low) / 2;
            if self.block_timestamp(mid).await? >= target_ts {
                high = mid;
            } else {
                low = mid + 1;
            }
        }
        Ok(low)
    }

    /// `eth_getLogs` for USDC transfers into `collector` in `[from, to]`.
    ///
    /// Returns `Err` when any returned log is malformed: a log our own filter
    /// selected but that cannot be decoded is exactly what must NOT be
    /// silently skipped (the range would be lost forever). Valid-but-never-
    /// creditable transfers (zero, > `i64`, reorged) are skipped terminally.
    ///
    /// # Errors
    /// Transport/RPC failures, malformed logs, a malformed configured
    /// address, or out-of-range block numbers.
    pub async fn transfer_logs(
        &self,
        usdc: &str,
        collector: &str,
        from_block: i64,
        to_block: i64,
    ) -> Result<Vec<UsdcTransfer>, String> {
        // Validate once before building the filter: a truncated collector
        // address would otherwise become a topic that matches nothing, and the
        // checkout would look "empty" instead of broken.
        let collector_topic = format!(
            "0x{:0>64}",
            collector
                .strip_prefix("0x")
                .or_else(|| collector.strip_prefix("0X"))
                .filter(|b| { b.len() == 40 && b.bytes().all(|c| c.is_ascii_hexdigit()) })
                .ok_or("configured collector_address is not a 20-byte hex address")?
                .to_lowercase()
        );
        let filter = serde_json::json!({
            "address": usdc,
            "fromBlock": format!("0x{from_block:x}"),
            "toBlock": format!("0x{to_block:x}"),
            "topics": [TRANSFER_TOPIC, null, collector_topic],
        });
        let v = self
            .call("eth_getLogs", serde_json::json!([filter]))
            .await?;
        let logs = v.as_array().ok_or("eth_getLogs result is not an array")?;
        let mut out = Vec::with_capacity(logs.len());
        for log in logs {
            match decode_log(log, usdc, collector, from_block, to_block) {
                Ok(DecodedLog::Transfer(t)) => out.push(t),
                Ok(DecodedLog::Ignored(reason)) => {
                    // Terminal: never credit, never retry, never wedge the
                    // cursor. Debug, not warn: a dust attacker controls this.
                    tracing::debug!(reason, "credits watcher: transfer ignored");
                }
                Err(reason) => {
                    tracing::error!(reason = %reason, log = %log, "credits watcher: malformed log in range; cursor held");
                    return Err(format!(
                        "malformed log in [{from_block}, {to_block}]: {reason}"
                    ));
                }
            }
        }
        Ok(out)
    }
}

/// Inclusive `[from, to]` block window for one tick, or `None` when the
/// cursor already covers the head.
///
/// With no cursor (first boot) the window starts at `boot_from`, the block that
/// carries the oldest unsettled order's `created_at` (see
/// [`bootstrap_target_ts`]). A head older than that timestamp starts the scan AT
/// the head — no block at or below it can carry a payment for that order yet —
/// which also pins the cursor so the following ticks are incremental instead of
/// repeating the timestamp search. From then on the window is strictly
/// incremental and capped at [`MAX_BLOCK_CHUNK`].
const fn scan_window(last: i64, head: i64, boot_from: i64, chunk: i64) -> Option<(i64, i64)> {
    let from_block = if last == 0 {
        let first = if boot_from > 0 { boot_from } else { 0 };
        if head < first { head } else { first }
    } else {
        if head <= last {
            return None;
        }
        last + 1
    };
    if head < from_block {
        return None;
    }
    let chunk = if chunk < 1 { 1 } else { chunk };
    let chunk_end = from_block + chunk - 1;
    Some((from_block, if head < chunk_end { head } else { chunk_end }))
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
    /// Matched an order whose window had run out (deposit needs a human).
    pub order_expired: usize,
    /// Blocks still pending behind this bounded chunk.
    pub backlog_blocks: i64,
    /// Finality tag of the head this tick scanned (`safe` or `finalized`).
    pub head_tag: String,
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
    /// First block a fresh cursor must replay: the block that carries the
    /// `created_at` of the oldest UNSETTLED order, or — with no such order — the
    /// configured payment window. Located with a bounded timestamp binary search
    /// (`O(log head)` probes) instead of a fixed block lookback, so a payment
    /// made inside an order's window and only processed days later (process
    /// down, cursor lost, order expired by then) is still inside the scan.
    ///
    /// There is NO additional age clamp on the origin. A fixed horizon
    /// (`start.max(head - 90 days)`) silently discarded exactly the deposits
    /// this function exists to recover: an order `expired` for a quarter still
    /// owes the transfer made inside its window. Bounding the work is already
    /// the tick's job — at most [`MAX_CHUNKS_PER_TICK`] × [`MAX_BLOCK_CHUNK`]
    /// blocks per tick, cursor advanced chunk by chunk — so an old origin only
    /// means a longer incremental catch-up, never an unbounded request and
    /// never a lost payment.
    async fn bootstrap_from_block(
        &self,
        pool: &Arc<DbPool>,
        rpc: &BaseRpc,
        head: i64,
    ) -> Result<i64, String> {
        let window_secs = i64::from(self.config.payment_window_minutes) * 60;
        // `expired` is deliberately INCLUDED. The matcher settles a transfer
        // while its BLOCK timestamp is inside `[created_at, expires_at +
        // grace]`, so a long-expired order can still be owed a payment made on
        // time. Requiring the window to be open *now* (the previous filter) is
        // exactly what dropped those deposits on a restart without a cursor.
        let oldest_unsettled = pool
            .spawn_read(move |r| {
                r.query_row(
                    "SELECT CAST(strftime('%s', MIN(created_at)) AS INTEGER) \
                     FROM credit_orders WHERE status IN ('pending','expired')",
                    [],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .map_err(openproxy_db::error::map_db_error)
            })
            .await
            .map_err(|e| format!("read oldest unsettled order: {e}"))?;
        let now = chrono::Utc::now().timestamp();
        let target_ts = bootstrap_target_ts(now, window_secs, oldest_unsettled);
        let start = rpc.first_block_at_or_after(head, target_ts).await?;
        Ok(start.min(head))
    }

    /// One scan tick: verify the chain, read the cursor, then scan bounded
    /// chunks until the finality head is covered or [`MAX_CHUNKS_PER_TICK`]
    /// is reached. The cursor advances only after each chunk fully succeeds,
    /// so a failure retries the same range and a long outage catches up in
    /// consecutive increments.
    ///
    /// # Errors
    /// Transport/RPC failures, malformed logs, chain mismatch and DB
    /// failures.
    pub async fn tick(&self) -> Result<ScanReport, String> {
        let pool = Arc::clone(&self.db_pool);
        let mut report = ScanReport::default();

        let mut last = pool
            .spawn_read(openproxy_db::credits_ledger::last_scanned_block)
            .await
            .map_err(|e| format!("read watcher cursor: {e}"))?;

        let rpc = BaseRpc::new(
            self.config.rpc_url.clone(),
            Arc::clone(&self.upstream_client),
        );
        // Wrong network = zero logs forever. Refuse to scan.
        let chain = rpc.chain_id().await?;
        if chain != self.config.chain_id {
            return Err(format!(
                "rpc chain id {chain} != configured chain_id {}",
                self.config.chain_id
            ));
        }
        let (head, head_tag) = rpc.finality_head().await?;
        report.head_tag = head_tag.to_string();
        let boot_from = if last == 0 {
            self.bootstrap_from_block(&pool, &rpc, head).await?
        } else {
            0
        };
        let mut scanned_any = false;
        for _ in 0..MAX_CHUNKS_PER_TICK {
            let chunk = i64::from(self.config.max_block_chunk.max(1));
            let Some((from_block, to_block)) = scan_window(last, head, boot_from, chunk) else {
                break;
            };
            if !scanned_any {
                report.from_block = from_block;
                scanned_any = true;
            }
            self.scan_chunk(&pool, &rpc, from_block, to_block, &mut report)
                .await?;
            report.to_block = to_block;
            last = to_block;
            if to_block >= head {
                break;
            }
        }
        if !scanned_any {
            // Cursor already covers the head (or the node is behind it).
            report.from_block = last;
            report.to_block = head;
        }
        report.backlog_blocks = (head - report.to_block).max(0);
        Ok(report)
    }

    /// Fetch, decode and settle one inclusive block range, then advance the
    /// cursor. Counts accumulate into `report`.
    async fn scan_chunk(
        &self,
        pool: &Arc<DbPool>,
        rpc: &BaseRpc,
        from_block: i64,
        to_block: i64,
        report: &mut ScanReport,
    ) -> Result<(), String> {
        let mut transfers = rpc
            .transfer_logs(
                &self.config.usdc_address,
                &self.config.collector_address,
                from_block,
                to_block,
            )
            .await?;
        // Apply in chain order: a provider that returns logs out of order must
        // not let an OLD payment land after (and expire) a NEWER plan order.
        transfers.sort_by_key(|t| (t.block_number, t.log_index));
        report.logs_seen += transfers.len();

        // Block time (UTC) per block that carries transfers. Matching against
        // the chain clock, not the server wall clock, is what makes a delayed
        // `safe` confirmation fair to the buyer.
        let mut timestamps: HashMap<i64, i64> = HashMap::new();
        for t in &transfers {
            if let std::collections::hash_map::Entry::Vacant(entry) =
                timestamps.entry(t.block_number)
            {
                let ts = rpc.block_timestamp(t.block_number).await?;
                entry.insert(ts);
            }
        }
        for t in transfers {
            let block_timestamp = *timestamps
                .get(&t.block_number)
                .ok_or("missing block timestamp for transfer")?;
            let master = Arc::clone(&self.master_key);
            let tx_hash = t.tx_hash.clone();
            let from = t.from_address.clone();
            let outcome = pool
                .spawn_write(move |w| {
                    apply_payment_at(
                        w,
                        &master,
                        &tx_hash,
                        t.log_index,
                        PaymentBlock {
                            number: t.block_number,
                            timestamp: block_timestamp,
                        },
                        t.amount_units,
                        Some(&from),
                    )
                })
                .await
                .map_err(|e| format!("apply payment: {e}"))?;
            match outcome {
                PaymentOutcome::Credited { .. } => report.credited += 1,
                PaymentOutcome::AlreadySeen { .. } => report.already_seen += 1,
                PaymentOutcome::OrderExpired { .. } => report.order_expired += 1,
                PaymentOutcome::Unmatched { .. } => report.unmatched += 1,
            }
        }

        // Advance the cursor only after this chunk was fully applied.
        pool.spawn_write(move |w| {
            openproxy_db::credits_ledger::set_last_scanned_block(w, to_block)
        })
        .await
        .map_err(|e| format!("advance watcher cursor: {e}"))?;
        Ok(())
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
                () = cancel.cancelled() => {
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
                                already_seen = report.already_seen,
                                unmatched = report.unmatched,
                                order_expired = report.order_expired,
                                backlog_blocks = report.backlog_blocks,
                                head_tag = %report.head_tag,
                                "credits watcher tick"
                            );
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(error = %e, "credits watcher tick failed; cursor held, will retry");
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
                () = cancel.cancelled() => return,
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
                () = cancel.cancelled() => return,
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
mod tests;
