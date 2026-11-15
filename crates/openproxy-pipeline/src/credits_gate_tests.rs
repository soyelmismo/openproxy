//! Admission and full-lifecycle tests for the prepaid-credit gate.
//!
//! The unit tests exercise the lane primitive; the lifecycle tests drive the
//! real `Pipeline::run` against a local HTTP upstream fixture, a real SQLite
//! ledger and the real usage journal, so the assertions cover what actually
//! reaches the database: billed amount, `credits_debited` marker, balance and
//! whether a lane is left held. Every pipeline call is wrapped in a timeout:
//! a regression in the gate must fail, never hang the suite.

use super::*;
use openproxy_types::CancelReason;
use openproxy_types::CoreError;
use openproxy_types::ids::{ApiKeyId, ComboId, ComboTargetId, RequestId};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Large enough for the conservative pre-dispatch quote of one fixture request
/// (`credit_budget` bounds the prompt by payload bytes at 1000 USD / 1M tokens),
/// yet still smaller than that request's real cost (1000 prompt tokens = 1 USD),
/// so one admitted request always drains the grant.
const KEY_GRANT_UNITS: i64 = 500_000;

// ── lane primitive ───────────────────────────────────────────────────

#[tokio::test]
async fn same_key_requests_are_serialized() {
    let gate = std::sync::Arc::new(CreditsAdmissionGate::new());
    let first = gate.acquire(0, 7).await;

    let waiting_gate = std::sync::Arc::clone(&gate);
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let waiter = tokio::spawn(async move {
        let guard = waiting_gate.acquire(0, 7).await;
        let _ = tx.send(guard.key_id()).await;
        drop(guard);
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        rx.try_recv().is_err(),
        "a second request for the same key must wait for the lane"
    );

    drop(first);
    let acquired = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("dropping the guard must release the lane")
        .expect("waiter reports acquisition");
    assert_eq!(acquired, 7);
    waiter.await.unwrap();
}

#[tokio::test]
async fn different_keys_run_in_parallel() {
    let gate = std::sync::Arc::new(CreditsAdmissionGate::new());
    let a = gate.acquire(0, 1).await;
    let b = tokio::time::timeout(Duration::from_secs(1), gate.acquire(0, 2))
        .await
        .expect("a different key must not wait for another key's lane");
    assert_eq!(b.key_id(), 2);
    drop(a);
    drop(b);
}

// ── upstream fixture ────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum UpstreamMode {
    /// SSE completion: one content frame, a terminal `usage` frame, `[DONE]`.
    /// The harness advertises `streaming: true` for the fixture model, and the
    /// pipeline always streams upstream for such a model, so even a unary
    /// client request arrives here as a stream and is accumulated downstream.
    Ok,
    /// 500 with a JSON error body.
    ServerError,
    /// SSE headers, then silence: the client stays in flight until aborted.
    Hang,
}

struct Upstream {
    url: String,
    hits: std::sync::Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Upstream {
    async fn start(mode: UpstreamMode) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&hits);
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let counter = std::sync::Arc::clone(&counter);
                tokio::spawn(async move {
                    serve_connection(socket, mode, counter).await;
                });
            }
        });
        Self { url, hits, task }
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_connection(
    mut socket: tokio::net::TcpStream,
    mode: UpstreamMode,
    hits: std::sync::Arc<AtomicUsize>,
) {
    if read_http_request(&mut socket).await.is_none() {
        return;
    }
    hits.fetch_add(1, Ordering::SeqCst);

    let response = match mode {
        UpstreamMode::Ok => {
            let body = concat!(
                "data: {\"id\":\"credit-gate\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
                "data: {\"id\":\"credit-gate\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1000,\"completion_tokens\":0,\"total_tokens\":1000}}\n\n",
                "data: [DONE]\n\n"
            );
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
        }
        UpstreamMode::ServerError => {
            let body = "{\"error\":{\"message\":\"fixture upstream failure\"}}";
            format!(
                "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
        }
        UpstreamMode::Hang => {
            let _ = socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                )
                .await;
            let _ = socket.flush().await;
            // Hold the response open; the test aborts the pipeline future.
            tokio::time::sleep(Duration::from_secs(30)).await;
            let _ = socket.shutdown().await;
            return;
        }
    };
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.flush().await;
    let _ = socket.shutdown().await;
}

async fn read_http_request(socket: &mut tokio::net::TcpStream) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 4096];
    let header_end = loop {
        let n = socket.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        bytes.extend_from_slice(&buf[..n]);
        if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if bytes.len() > 1024 * 1024 {
            return None;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
    let length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.trim() == "content-length")
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let n = socket.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    Some(bytes)
}

// ── harness ─────────────────────────────────────────────────────────

struct Harness {
    pipeline: crate::Pipeline,
    pool: openproxy_db::conn::DbPool,
    combo_id: ComboId,
    target_id: ComboTargetId,
    key_id: i64,
    hits: std::sync::Arc<AtomicUsize>,
    worker: Option<crate::worker::WorkerHandle>,
}

impl Harness {
    /// `grant_units = None` seeds a key with no ledger rows at all (not
    /// credit-governed). `with_worker = false` leaves the usage journal with
    /// nobody draining it, which lets a test observe admission's own drain.
    fn start(upstream: &Upstream, grant_units: Option<i64>, with_worker: bool) -> Self {
        let (pool, seed_conn, _path) = crate::test_utils::fresh_pool();
        let master_key =
            std::sync::Arc::new(openproxy_db::secrets::MasterKey::generate().expect("master key"));

        let (combo_id, key_id, target_id) = {
            let conn = seed_conn.lock();
            let (combo_id, _account_id) = crate::test_utils::seed_solo_combo_at_url(
                &conn,
                "mock-openai",
                &upstream.url,
                master_key.as_ref(),
            );
            // A request that reports 1000 prompt tokens costs 1000 * 1000 / 1e6
            // = 1.0 USD, i.e. far more than the seeded grant, so one admitted
            // request always drains it.
            conn.execute(
                "UPDATE combos SET pricing_input_per_1m = 1000.0, pricing_output_per_1m = 0.0 WHERE id = ?1",
                [combo_id.0],
            )
            .expect("price combo");
            conn.execute(
                "UPDATE models SET capabilities_json = '{\"streaming\":true}' WHERE provider_id = 'mock-openai'",
                [],
            )
            .expect("enable streaming capability");
            conn.execute(
                "INSERT INTO api_keys (key_hash, label) VALUES ('credit-gate-fixture', 'credit-gate')",
                [],
            )
            .expect("insert api key");
            let key_id: i64 = conn
                .query_row("SELECT last_insert_rowid()", [], |row| row.get(0))
                .expect("api key id");
            if let Some(units) = grant_units {
                conn.execute(
                    "INSERT INTO credit_transactions (api_key_id, kind, amount_units, remaining_units) \
                     VALUES (?1, 'grant', ?2, ?2)",
                    rusqlite::params![key_id, units],
                )
                .expect("grant credit");
            }
            let target_id: i64 = conn
                .query_row(
                    "SELECT id FROM combo_targets WHERE combo_id = ?1",
                    [combo_id.0],
                    |row| row.get(0),
                )
                .expect("combo target id");
            (combo_id, key_id, ComboTargetId(target_id))
        };

        let mut config = crate::test_utils::test_config_with_mock(master_key, &upstream.url);
        config.max_attempts = 1;
        config.retries.max_attempts = 1;
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        config.background_tx = tx;
        let pipeline = crate::test_utils::test_pipeline_with_pool(pool.clone(), config);
        let worker = with_worker.then(|| crate::worker::spawn_worker(pool.writer_arc(), rx));

        Self {
            pipeline,
            pool,
            combo_id,
            target_id,
            key_id,
            hits: std::sync::Arc::clone(&upstream.hits),
            worker,
        }
    }

    fn request(
        &self,
    ) -> (
        crate::PipelineRequest,
        tokio::sync::watch::Sender<Option<CancelReason>>,
    ) {
        let (mut req, cancel) = crate::test_utils::make_request(self.combo_id);
        req.api_key_id = Some(ApiKeyId(self.key_id));
        (req, cancel)
    }

    fn streaming_request(
        &self,
    ) -> (
        crate::PipelineRequest,
        tokio::sync::mpsc::Receiver<bytes::Bytes>,
        tokio::sync::watch::Sender<Option<CancelReason>>,
    ) {
        let (mut req, cancel) = self.request();
        std::sync::Arc::make_mut(&mut req.openai_request).stream = true;
        let (sink, rx) = tokio::sync::mpsc::channel(64);
        req.stream_sink = Some(crate::StreamSink::direct(sink));
        (req, rx, cancel)
    }

    /// Identity the gate keys lanes by: the shared writer connection.
    fn db_id(&self) -> usize {
        std::sync::Arc::as_ptr(&self.pipeline.conn) as usize
    }

    fn balance(&self) -> i64 {
        let conn = self.pool.writer();
        openproxy_db::credits_ledger::balance_units(&conn, self.key_id).expect("balance")
    }

    fn usage_row(&self, request_id: &str) -> Option<(i64, i64, f64)> {
        use rusqlite::OptionalExtension;
        let conn = self.pool.writer();
        conn.query_row(
            "SELECT client_response, credits_debited, COALESCE(cost_usd, 0) FROM usage \
             WHERE request_id = ?1 ORDER BY id DESC LIMIT 1",
            [request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .expect("usage lookup")
    }

    fn debited_rows(&self) -> i64 {
        let conn = self.pool.writer();
        conn.query_row(
            "SELECT COUNT(*) FROM usage WHERE api_key_id = ?1 AND credits_debited = 1",
            [self.key_id],
            |row| row.get(0),
        )
        .expect("count debits")
    }

    async fn run_with_timeout(&self, req: crate::PipelineRequest) -> crate::PipelineResult {
        tokio::time::timeout(Duration::from_secs(10), self.pipeline.run(req))
            .await
            .expect("pipeline run timed out: the credit gate leaked a lane")
    }

    /// Proof that the key's lane is free right now, independent of balances.
    async fn assert_lane_free(&self) {
        let guard = tokio::time::timeout(
            Duration::from_secs(2),
            self.pipeline
                .credits_gate()
                .acquire(self.db_id(), self.key_id),
        )
        .await
        .expect("the credit lane is still held after the request finished");
        drop(guard);
    }

    /// Insert usage before the deferred winner mark. Recording a row already
    /// flagged CLIENT_RESPONSE debits synchronously and cannot model a pending charge.
    fn record_delivered_usage(&self, request_id: RequestId) {
        let usage = openproxy_types::usage::UsageInput {
            request_id,
            trace_id: "credit-gate-journal".into(),
            attempt: 1,
            provider_id: openproxy_types::ids::ProviderId::new("mock-openai"),
            account_id: None,
            combo_id: Some(self.combo_id),
            combo_target_id: Some(self.target_id),
            model_row_id: None,
            upstream_model_id: "m".into(),
            prompt_tokens: Some(1000),
            completion_tokens: Some(0),
            cached_tokens: None,
            connect_ms: None,
            ttft_ms: None,
            total_ms: 1,
            status_code: 200,
            error_msg: None,
            race_total: 1,
            api_key_id: Some(ApiKeyId(self.key_id)),
            request_body_json: None,
            response_body_json: None,
            request_headers: None,
            response_headers: None,
            error_message: None,
            race_attempts: 1,
            stop_reason: None,
            compression_savings_pct: None,
            compression_techniques: None,
            pii_redacted: None,
            endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
            proxy_url: None,
            proxy_status: None,
            flags: 0,
            audio_ms: None,
            session_hash: None,
            max_chunk_gap_ms: None,
            avg_chunk_gap_ms: None,
        };
        let conn = self.pool.writer();
        openproxy_db::cost::record(&conn, &usage).expect("insert billable usage");
    }

    /// Queue the deferred debit for an already recorded row.
    fn journal_mark(&self, request_id: RequestId) {
        let writer = self.pool.writer_arc();
        crate::worker::admit_job(
            &writer,
            &crate::worker::BackgroundJob::MarkClientResponse {
                request_id: request_id.to_string(),
                attempt: 1,
                target_id: self.target_id,
            },
        )
        .expect("journal the deferred mark");
    }

    fn pending_debit(&self) -> Option<PendingDebit> {
        self.pipeline
            .credits_gate()
            .pending_debit(self.db_id(), self.key_id)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        drop(self.worker.take());
    }
}

// ── lifecycle ───────────────────────────────────────────────────────

#[tokio::test]
async fn concurrent_requests_cannot_spend_the_same_balance() {
    let upstream = Upstream::start(UpstreamMode::Ok).await;
    let harness = Harness::start(&upstream, Some(KEY_GRANT_UNITS), true);

    let first = harness.request().0;
    let second = harness.request().0;
    let (a, b) = tokio::join!(
        harness.run_with_timeout(first),
        harness.run_with_timeout(second)
    );

    let mut codes = [a.status_code, b.status_code];
    codes.sort_unstable();
    assert_eq!(
        codes,
        [200, 402],
        "exactly one request may be admitted against the last of the balance: first={:?} second={:?}",
        a.error,
        b.error
    );
    let denied = if a.status_code == 402 { &a } else { &b };
    assert!(
        matches!(
            &denied.error,
            Some(CoreError::InsufficientCredits { balance_usd }) if *balance_usd == 0.0
        ),
        "the denied request must report an exhausted prepaid balance: {:?}",
        denied.error
    );
    assert_eq!(harness.balance(), 0, "the granted balance is fully spent");
    assert_eq!(harness.debited_rows(), 1, "only the admitted request bills");
    assert_eq!(harness.hits.load(Ordering::SeqCst), 1, "one dispatch only");
}

#[tokio::test]
async fn streaming_request_bills_before_releasing_its_lane() {
    let upstream = Upstream::start(UpstreamMode::Ok).await;
    let harness = Harness::start(&upstream, Some(KEY_GRANT_UNITS), true);

    let (mut req, mut chunks, _cancel) = harness.streaming_request();
    // A prompt long enough that even a char-based token estimate costs more
    // than the seeded grant: the debit must happen whatever the usage parse.
    std::sync::Arc::make_mut(&mut req.openai_request).messages[0].content = Some(
        serde_json::json!("explain prepaid credit admission in one paragraph"),
    );
    let request_id = req.request_id.to_string();
    let result = harness.run_with_timeout(req).await;
    assert_eq!(
        result.status_code, 200,
        "streaming failed: {:?}",
        result.error
    );

    let mut wire = Vec::new();
    while let Ok(chunk) = chunks.try_recv() {
        wire.extend_from_slice(&chunk);
    }
    assert!(
        String::from_utf8_lossy(&wire).contains("[DONE]"),
        "the fixture stream must reach the client"
    );

    let (client_response, debited, cost) = harness
        .usage_row(&request_id)
        .expect("streaming request must record a usage row");
    assert_eq!(client_response, 1);
    assert_eq!(debited, 1, "the debit is persisted before `run` returns");
    assert!(cost > 0.0, "a priced stream must carry a cost");
    assert_eq!(harness.balance(), 0);
    harness.assert_lane_free().await;

    let next = harness.request().0;
    let denied = harness.run_with_timeout(next).await;
    assert_eq!(denied.status_code, 402);
    assert_eq!(harness.hits.load(Ordering::SeqCst), 1, "no second dispatch");
}

#[tokio::test]
async fn upstream_error_is_not_billed_and_releases_the_lane() {
    let upstream = Upstream::start(UpstreamMode::ServerError).await;
    let harness = Harness::start(&upstream, Some(KEY_GRANT_UNITS), true);

    let req = harness.request().0;
    let request_id = req.request_id.to_string();
    let result = harness.run_with_timeout(req).await;
    assert!(result.status_code >= 500, "fixture must fail upstream");
    let (_client_response, _debited, cost) =
        harness.usage_row(&request_id).expect("failure is recorded");
    assert!(
        cost.abs() < f64::EPSILON,
        "a failed attempt has no billable cost, got {cost}"
    );
    assert_eq!(
        harness.balance(),
        KEY_GRANT_UNITS,
        "an upstream error must not spend prepaid credit"
    );
    harness.assert_lane_free().await;

    // The upstream failure deliberately opens health/cooldown gates. Reset
    // those independent protections so the retry tests CREDIT admission,
    // rather than expecting a known-unhealthy target to dispatch again.
    {
        let conn = harness.pool.writer();
        conn.execute("DELETE FROM target_cooldowns", []).unwrap();
        let account: i64 = conn
            .query_row(
                "SELECT account_id FROM combo_targets WHERE id = ?1",
                [harness.target_id.0],
                |r| r.get(0),
            )
            .unwrap();
        harness.pipeline.circuit_breaker.record_success(
            crate::circuit_breaker::CircuitBreakerKey::Account(openproxy_types::ids::AccountId(
                account,
            )),
        );
    }
    // A later request is dispatched again, not rejected by a stale balance.
    let retry = harness.run_with_timeout(harness.request().0).await;
    assert!(retry.status_code >= 500);
    assert_eq!(harness.hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cancelled_request_releases_the_lane_without_billing() {
    let upstream = Upstream::start(UpstreamMode::Ok).await;
    let harness = Harness::start(&upstream, Some(KEY_GRANT_UNITS), true);

    let (req, cancel) = harness.request();
    let request_id = req.request_id.to_string();
    cancel.send(Some(CancelReason::ClientDisconnected)).unwrap();
    let result = harness.run_with_timeout(req).await;

    assert_eq!(result.status_code, 499, "client disconnect is a 499");
    assert_eq!(
        harness.hits.load(Ordering::SeqCst),
        0,
        "nothing was dispatched"
    );
    assert!(harness.usage_row(&request_id).is_none(), "nothing to bill");
    assert_eq!(harness.balance(), KEY_GRANT_UNITS);
    harness.assert_lane_free().await;
}

/// Un cancel del cliente CON fila de usage parcial (stream cortado en vuelo)
/// SÍ factura: el seller no aborta su upstream y reclama input completo por
/// NeedAuth, así que el usuario asume lo entregado + prompt a precio uncached.
/// La fila 499 se cobra con cached_tokens = None (no llegó el bloque de usage).
#[tokio::test]
async fn client_cancel_with_partial_usage_row_bills_the_key() {
    let upstream = Upstream::start(UpstreamMode::Ok).await;
    let harness = Harness::start(&upstream, Some(KEY_GRANT_UNITS), true);

    let request_id = RequestId::new();
    // Fila parcial de un stream cortado: status 499, prompt estimado completo
    // (uncached: sin bloque final de usage), tokens entregados en vuelo.
    let usage = openproxy_types::usage::UsageInput {
        request_id,
        trace_id: "credit-gate-cancel".into(),
        attempt: 1,
        provider_id: openproxy_types::ids::ProviderId::new("mock-openai"),
        account_id: None,
        combo_id: Some(harness.combo_id),
        combo_target_id: Some(harness.target_id),
        model_row_id: None,
        upstream_model_id: "m".into(),
        prompt_tokens: Some(1000),
        completion_tokens: Some(500),
        cached_tokens: None,
        connect_ms: None,
        ttft_ms: None,
        total_ms: 1,
        status_code: 499,
        error_msg: None,
        race_total: 1,
        api_key_id: Some(ApiKeyId(harness.key_id)),
        request_body_json: None,
        response_body_json: None,
        request_headers: None,
        response_headers: None,
        error_message: None,
        race_attempts: 1,
        stop_reason: None,
        compression_savings_pct: None,
        compression_techniques: None,
        pii_redacted: None,
        endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
        proxy_url: None,
        proxy_status: None,
        flags: 0,
        audio_ms: None,
        session_hash: None,
        max_chunk_gap_ms: None,
        avg_chunk_gap_ms: None,
    };
    {
        let conn = harness.pool.writer();
        openproxy_db::cost::record(&conn, &usage).expect("insert partial cancel usage");
    }
    let row = harness
        .usage_row(&request_id.to_string())
        .expect("partial cancel usage row exists");
    assert!(
        row.2 > 0.0,
        "a 499 row with estimated tokens must carry a real cost_usd, got {}",
        row.2
    );

    let crate::credits_gate::CreditAdmission::Open(Some(lane)) = harness
        .pipeline
        .admit_credit_request(&harness.request().0)
        .await
    else {
        panic!("a granted key with a fresh balance must admit");
    };
    let result = crate::PipelineResult {
        status_code: 499,
        error: Some(openproxy_types::error::CoreError::Cancelled(
            openproxy_types::CancelReason::ClientDisconnected,
        )),
        final_response: None,
        attempts: 1,
        usage_tuple: Some((request_id, 1, harness.target_id)),
    };
    harness.pipeline.settle_credit_request(lane, &result).await;
    harness.assert_lane_free().await;
    assert_eq!(
        harness.balance(),
        0,
        "the partial-cancel debit must be posted against the key"
    );
    assert_eq!(
        harness.usage_row(&request_id.to_string()).map(|row| row.1),
        Some(1),
        "the 499 usage row is now debited"
    );
}

#[path = "credits_gate/recovery_tests.rs"]
mod recovery_tests;
