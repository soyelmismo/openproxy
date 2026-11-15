//! Lifecycle regressions for the strict prepaid budget and prepaid streams.
//!
//! These drive the real `Pipeline::run` against a local SSE fixture and a real
//! SQLite ledger. They assert what actually reaches the database: no upstream
//! hit when the maximum does not fit, a normal unary/stream bill, a crash after
//! the first chunk charged only for delivered bytes, unpriced models failing
//! closed, free keys untouched and `execute_test_target` not bypassing the gate.

use openproxy_types::ids::{ApiKeyId, ComboId, ComboTargetId};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ── upstream fixture ────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum UpstreamMode {
    /// SSE completion with a real usage frame and `[DONE]`.
    Ok,
    /// One content frame, then silence: the client receives part of the
    /// response and the process is aborted, like a crash mid-stream.
    HangAfterFirstChunk,
    /// SSE headers, then silence; used only by tests that abort the pipeline.
    Hang,
}

struct Upstream {
    url: String,
    hits: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Upstream {
    async fn start(mode: UpstreamMode) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let counter = Arc::clone(&counter);
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
    hits: Arc<AtomicUsize>,
) {
    if read_http_request(&mut socket).await.is_none() {
        return;
    }
    hits.fetch_add(1, Ordering::SeqCst);

    let response = match mode {
        UpstreamMode::Ok => {
            let body = concat!(
                "data: {\"id\":\"budget\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"openai/gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
                "data: {\"id\":\"budget\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"openai/gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":0,\"total_tokens\":100}}\n\n",
                "data: [DONE]\n\n"
            );
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
        }
        UpstreamMode::HangAfterFirstChunk => {
            let head =
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
            let chunk = "data: {\"id\":\"budget\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"openai/gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(chunk.as_bytes()).await;
            let _ = socket.flush().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
            let _ = socket.shutdown().await;
            return;
        }
        UpstreamMode::Hang => {
            let _ = socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                )
                .await;
            let _ = socket.flush().await;
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

// ── lifecycle harness ───────────────────────────────────────────────

struct Harness {
    pipeline: crate::Pipeline,
    pool: openproxy_db::conn::DbPool,
    combo_id: ComboId,
    key_id: i64,
    hits: Arc<AtomicUsize>,
    worker: Option<crate::worker::WorkerHandle>,
}

impl Harness {
    /// `grant = None` seeds a key with no ledger rows (free/unlimited).
    fn start(
        upstream: &Upstream,
        model_id: &str,
        combo_price: Option<(f64, f64)>,
        grant: Option<i64>,
        with_worker: bool,
    ) -> Self {
        let (pool, seed_conn, _path) = crate::test_utils::fresh_pool();
        let master_key =
            Arc::new(openproxy_db::secrets::MasterKey::generate().expect("master key"));

        let (combo_id, key_id, _target_id) = {
            let conn = seed_conn.lock();
            let (combo_id, _account_id) = crate::test_utils::seed_solo_combo_at_url(
                &conn,
                "mock-openai",
                &upstream.url,
                master_key.as_ref(),
            );
            conn.execute(
                "UPDATE models SET model_id = ?1, capabilities_json = '{\"streaming\":true}' \
                 WHERE provider_id = 'mock-openai'",
                [model_id],
            )
            .expect("set fixture model");
            match combo_price {
                Some((input, output)) => {
                    conn.execute(
                        "UPDATE combos SET pricing_input_per_1m = ?1, pricing_output_per_1m = ?2 \
                         WHERE id = ?3",
                        rusqlite::params![input, output, combo_id.0],
                    )
                    .expect("price combo");
                    // Also price the model row: `execute_test_target` formats
                    // with a synthetic combo (id 0), so its quote must resolve
                    // through the target model instead of the combo.
                    conn.execute(
                        "INSERT INTO model_capabilities_sync \
                         (provider_id, model_id, pricing_input_per_1m, pricing_output_per_1m) \
                         VALUES ('mock-openai', ?1, ?2, ?3) \
                         ON CONFLICT(provider_id, model_id) DO UPDATE SET \
                             pricing_input_per_1m = excluded.pricing_input_per_1m, \
                             pricing_output_per_1m = excluded.pricing_output_per_1m",
                        rusqlite::params![model_id, input, output],
                    )
                    .expect("price model row");
                }
                None => {
                    conn.execute(
                        "UPDATE combos SET pricing_input_per_1m = NULL, \
                         pricing_output_per_1m = NULL WHERE id = ?1",
                        [combo_id.0],
                    )
                    .expect("clear combo price");
                }
            }
            conn.execute(
                "INSERT INTO api_keys (key_hash, label) VALUES ('budget-fixture', 'budget')",
                [],
            )
            .expect("insert api key");
            let key_id: i64 = conn
                .query_row("SELECT last_insert_rowid()", [], |row| row.get(0))
                .expect("api key id");
            if let Some(units) = grant {
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
                .expect("target id");
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
            key_id,
            hits: Arc::clone(&upstream.hits),
            worker,
        }
    }

    fn request(&self, max_tokens: Option<u32>) -> crate::PipelineRequest {
        let (mut req, _cancel) = crate::test_utils::make_request(self.combo_id);
        req.api_key_id = Some(ApiKeyId(self.key_id));
        Arc::make_mut(&mut req.openai_request).max_tokens = max_tokens;
        req
    }

    fn streaming_request(
        &self,
        max_tokens: Option<u32>,
    ) -> (
        crate::PipelineRequest,
        tokio::sync::mpsc::Receiver<bytes::Bytes>,
    ) {
        let mut req = self.request(max_tokens);
        Arc::make_mut(&mut req.openai_request).stream = true;
        let (sink, rx) = tokio::sync::mpsc::channel(64);
        req.stream_sink = Some(crate::StreamSink::direct(sink));
        (req, rx)
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

    fn reservation(
        &self,
        request_id: &str,
    ) -> Option<(i64, i64, i64, String, Option<String>, i64)> {
        use rusqlite::OptionalExtension;
        let conn = self.pool.writer();
        conn.query_row(
            "SELECT generated_units, delivery_started, reserved_units, state, outcome, charged_units \
             FROM credit_reservations WHERE request_id = ?1 AND attempt = 1",
            [request_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()
        .expect("reservation lookup")
    }

    async fn run(&self, req: crate::PipelineRequest) -> crate::PipelineResult {
        tokio::time::timeout(Duration::from_secs(10), self.pipeline.run(req))
            .await
            .expect("pipeline run timed out")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        drop(self.worker.take());
    }
}

/// `openai/gpt-4o` is in the static catalog (2.5 input / 10 output USD per
/// million), so a fixture provider with that model id prices without a DB row.
const STATIC_MODEL: &str = "openai/gpt-4o";

#[tokio::test]
async fn the_last_draw_admits_a_request_the_maximum_exceeds() {
    let upstream = Upstream::start(UpstreamMode::Ok).await;
    let harness = Harness::start(&upstream, STATIC_MODEL, Some((2.5, 10.0)), Some(100), true);

    let result = harness.run(harness.request(Some(64))).await;

    assert_eq!(
        result.status_code, 200,
        "a positive balance is always admitted (last-draw): {:?}",
        result.error
    );
    assert_eq!(
        harness.hits.load(Ordering::SeqCst),
        1,
        "the request reached the upstream"
    );
    assert_eq!(
        harness.balance(),
        0,
        "the debit clamped to the balance: the plan closes at exactly 0"
    );
}

#[tokio::test]
async fn affordable_request_is_dispatched_and_billed_once() {
    let upstream = Upstream::start(UpstreamMode::Ok).await;
    let harness = Harness::start(
        &upstream,
        STATIC_MODEL,
        Some((2.5, 10.0)),
        Some(10_000_000),
        true,
    );

    let req = harness.request(Some(64));
    let request_id = req.request_id.to_string();
    let result = harness.run(req).await;

    assert_eq!(result.status_code, 200, "{:?}", result.error);
    assert_eq!(harness.hits.load(Ordering::SeqCst), 1);
    let (client_response, debited, cost) = harness
        .usage_row(&request_id)
        .expect("usage row was recorded");
    assert_eq!(client_response, 1);
    assert_eq!(debited, 1);
    // 100 prompt tokens at 2.5 USD/M = 250 units, plus the delivered "ok"
    // completion (the upstream reported 0, so the recorder estimates 1 token
    // at 10 USD/M = 10 units): 260 units.
    assert!((cost - 0.00026).abs() < 1e-12, "unexpected cost {cost}");
    assert_eq!(harness.balance(), 10_000_000 - 260);
}

#[tokio::test]
async fn streaming_commits_delivery_evidence_and_bills_the_actual_usage() {
    let upstream = Upstream::start(UpstreamMode::Ok).await;
    let harness = Harness::start(
        &upstream,
        STATIC_MODEL,
        Some((2.5, 10.0)),
        Some(10_000_000),
        true,
    );

    let (req, mut chunks) = harness.streaming_request(Some(64));
    let request_id = req.request_id.to_string();
    let result = harness.run(req).await;
    assert_eq!(result.status_code, 200, "{:?}", result.error);
    assert_eq!(harness.hits.load(Ordering::SeqCst), 1);

    let mut wire = Vec::new();
    while let Ok(chunk) = chunks.try_recv() {
        wire.extend_from_slice(&chunk);
    }
    let wire = String::from_utf8_lossy(&wire);
    assert!(wire.contains("ok"), "stream content reached the client");
    assert!(
        wire.contains("[DONE]"),
        "stream terminator reached the client"
    );

    let (client_response, debited, cost) =
        harness.usage_row(&request_id).expect("streaming usage row");
    assert_eq!(client_response, 1);
    assert_eq!(debited, 1);
    // Same arithmetic as the unary test: 250 prompt + 10 for the estimated
    // 1-token completion delivered before the usage frame reported 0.
    assert!((cost - 0.00026).abs() < 1e-12, "unexpected cost {cost}");

    let (generated_units, delivery_started, reserved_units, state, outcome, _charged) = harness
        .reservation(&request_id)
        .expect("prepaid stream must persist a reservation");
    assert!(
        generated_units > 0,
        "the reservation carries delivered-cost evidence"
    );
    assert_eq!(
        delivery_started, 1,
        "delivery is recorded after the first enqueued chunk"
    );
    assert!(reserved_units <= 10_000_000);
    assert_eq!(state, "settled");
    assert_eq!(outcome.as_deref(), Some("billed"));
    assert_eq!(harness.balance(), 10_000_000 - 260);
}

#[tokio::test]
async fn a_crashed_stream_is_charged_from_its_delivered_checkpoint() {
    let upstream = Upstream::start(UpstreamMode::HangAfterFirstChunk).await;
    let harness = Harness::start(
        &upstream,
        STATIC_MODEL,
        Some((2.5, 10.0)),
        Some(10_000_000),
        true,
    );

    let (req, mut chunks) = harness.streaming_request(Some(10_000));
    let request_id = req.request_id.to_string();
    let pipeline = harness.pipeline.clone();
    let task = tokio::spawn(async move { pipeline.run(req).await });

    // The first chunk reaching the client is the delivery point. The sink
    // enqueues first and then checkpoints, so wait for the durable flag before
    // simulating the crash.
    let first = tokio::time::timeout(Duration::from_secs(5), chunks.recv())
        .await
        .expect("the first chunk never reached the client")
        .expect("stream channel closed");
    assert!(
        String::from_utf8_lossy(&first).contains("partial"),
        "unexpected first chunk: {first:?}"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if harness
            .reservation(&request_id)
            .is_some_and(|row| row.1 == 1)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the delivered chunk was never checkpointed"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Crash the request between the first chunk and `record_streaming_success`:
    // no usage row exists, and the process (task) is gone.
    task.abort();
    assert!(task.await.expect_err("aborted task").is_cancelled());

    let (generated_units, delivery_started, reserved_units, state, _outcome, _charged) = harness
        .reservation(&request_id)
        .expect("delivery evidence must survive the crash");
    assert!(generated_units > 0, "the delivered cost was checkpointed");
    assert_eq!(delivery_started, 1);
    assert!(reserved_units > 0, "admission recorded a budget snapshot");
    assert_eq!(state, "held", "the owner never settled, so it stays open");
    assert_eq!(harness.usage_row(&request_id), None, "no usage row landed");

    // The next admission of this key is the recovery point: it must charge the
    // delivered-cost checkpoint instead of treating the crash as \"nothing delivered\".
    let next = harness.request(Some(64));
    let admission = harness.pipeline.admit_credit_request(&next).await;
    drop(admission);

    let (_, _, _, state, outcome, charged_after) = harness
        .reservation(&request_id)
        .expect("the recovered intent still exists");
    assert_eq!(state, "settled");
    assert_eq!(outcome.as_deref(), Some("recovered_partial"));
    assert_eq!(
        charged_after, generated_units,
        "recovery charges exactly the delivered-cost checkpoint"
    );
    assert!(
        charged_after < 5_000,
        "a crash must not charge the unproduced output ceiling (charged {charged_after})"
    );
    assert_eq!(
        harness.balance(),
        10_000_000 - charged_after,
        "the partial delivery was not free"
    );
}

#[tokio::test]
async fn unpriced_model_fails_closed_before_dispatch() {
    let upstream = Upstream::start(UpstreamMode::Ok).await;
    let harness = Harness::start(
        &upstream,
        "no/such-model-budget-fixture",
        None,
        Some(10_000_000),
        true,
    );

    let result = harness.run(harness.request(Some(16))).await;
    assert_ne!(result.status_code, 200, "a missing price cannot be billed");
    assert!(result.status_code >= 500, "got {}", result.status_code);
    assert_eq!(harness.hits.load(Ordering::SeqCst), 0, "no upstream hit");
}

#[tokio::test]
async fn free_keys_keep_full_access_even_with_unpriced_models() {
    let upstream = Upstream::start(UpstreamMode::Ok).await;
    let harness = Harness::start(&upstream, "no/such-model-budget-fixture", None, None, true);

    let result = harness.run(harness.request(None)).await;
    assert_eq!(
        result.status_code, 200,
        "a key without ledger rows is not budget-governed: {:?}",
        result.error
    );
    assert_eq!(harness.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn execute_test_target_runs_the_last_draw() {
    let upstream = Upstream::start(UpstreamMode::Ok).await;
    let harness = Harness::start(&upstream, STATIC_MODEL, Some((2.5, 10.0)), Some(100), true);

    let req = harness.request(Some(64));
    let resolved = crate::context::ResolvedTarget::test("mock-openai", STATIC_MODEL);
    let result = harness.pipeline.execute_test_target(req, resolved).await;

    assert_eq!(
        result.status_code, 200,
        "admin test traffic obeys the same last-draw admission: {:?}",
        result.error
    );
    assert_eq!(harness.hits.load(Ordering::SeqCst), 1);
    // The test-target path dispatches without settling a usage row, so no
    // debit (clamped or not) is asserted here; the admission itself is the
    // last-draw contract under test.
}

#[tokio::test]
async fn aborted_stream_still_leaves_the_lane_free() {
    let upstream = Upstream::start(UpstreamMode::Hang).await;
    let harness = Harness::start(
        &upstream,
        STATIC_MODEL,
        Some((2.5, 10.0)),
        Some(10_000_000),
        true,
    );

    let pipeline = harness.pipeline.clone();
    let (req, _chunks) = harness.streaming_request(Some(64));
    let task = tokio::spawn(async move { pipeline.run(req).await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while harness.hits.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the hanging upstream never saw the request"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    task.abort();
    assert!(task.await.expect_err("aborted task").is_cancelled());

    let db_id = std::sync::Arc::as_ptr(&harness.pipeline.conn) as usize;
    let guard = tokio::time::timeout(
        Duration::from_secs(2),
        harness
            .pipeline
            .credits_gate()
            .acquire(db_id, harness.key_id),
    )
    .await
    .expect("an aborted stream must release its credit lane");
    drop(guard);
}
