//! Entrypoint tests for the pre-dispatch budget gate.
//!
//! These drive the real executors (`execute_embeddings`, `execute_transcribe`,
//! `execute_image_generation`, `execute_system_one`) against a real SQLite
//! ledger and a real HTTP upstream fixture, so they assert what the customer is
//! actually charged and whether the upstream was contacted at all — not the
//! shape of an internal call.
//!
//! Every rejection test asserts the same three things: the request fails, the
//! upstream saw **zero** hits, and the balance is untouched.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use bytes::Bytes;
use httpmock::prelude::*;
use openproxy_adapters::adapters::{AdapterFormat, MockAdapter, ProviderAdapterEnum};
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry;
use openproxy_types::CoreError;
use openproxy_types::config::CircuitBreakerConfig;
use openproxy_types::embeddings::{EmbeddingInput, EmbeddingRequest};
use openproxy_types::ids::{ApiKeyId, ModelRowId, ProviderId};
use openproxy_types::images::ImageGenerationRequest;
use openproxy_types::systemone::{SystemOneRequest, SystemOneResponse};

use crate::audio::ParsedAudioBody;
use crate::unary::{HostLocal, UnaryCharge, UnaryCreditLease, UnaryDeps, UnaryTarget};

/// Fresh key ids so the process-wide lane/marker tables never collide with a
/// previous test's pool address.
static NEXT_KEY_ID: AtomicI64 = AtomicI64::new(50_000);
static NEXT_COMBO_SEQ: AtomicI64 = AtomicI64::new(1);

struct Fixture {
    pool: DbPool,
    master_key: Arc<MasterKey>,
    adapters: Vec<ProviderAdapterEnum>,
    key_id: i64,
    /// Model name that routes to this fixture's combo.
    route: String,
    upstream: MockServer,
}

impl Fixture {
    /// `upstream_model` becomes `models.model_id`; `pricing` sets the combo's
    /// own rate (USD per 1M tokens / per 1M images); `grant` seeds the prepaid
    /// ledger (`None` = free/unlimited key).
    async fn new(
        upstream_model: &str,
        pricing: Option<(f64, f64)>,
        grant: Option<i64>,
        max_output_tokens: Option<i64>,
    ) -> Self {
        let upstream = MockServer::start_async().await;
        let pool = DbPool::test_pool_with_prefix("openproxy-unary-budget").expect("test db pool");
        let master_key = Arc::new(MasterKey::generate().expect("master key"));
        let seq = NEXT_COMBO_SEQ.fetch_add(1, Ordering::Relaxed);
        let route = format!("budget-combo-{seq}");
        let key_id = NEXT_KEY_ID.fetch_add(1, Ordering::Relaxed);

        {
            let conn = pool.writer();
            crate::providers::create(
                &conn,
                crate::providers::NewProvider {
                    id: &ProviderId::new("mock-openai"),
                    name: "Mock OpenAI",
                    base_url: &upstream.base_url(),
                    auth_type: crate::providers::AuthType::Bearer,
                    format: crate::providers::ProviderFormat::Openai,
                    extra_headers_json: None,
                    auto_activate_keyword: None,
                    rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
                },
            )
            .expect("seed provider");
            conn.execute(
                "INSERT INTO models (provider_id, model_id, target_format, max_output_tokens) \
                 VALUES ('mock-openai', ?1, 'openai', ?2)",
                rusqlite::params![upstream_model, max_output_tokens],
            )
            .expect("seed model");
            let model_row_id: i64 = conn
                .query_row("SELECT last_insert_rowid()", [], |row| row.get(0))
                .expect("model row id");
            let account_id = openproxy_db::accounts::create(
                &conn,
                &ProviderId::new("mock-openai"),
                Some("sk-test"),
                master_key.as_ref(),
                Some("budget"),
                10,
                None,
            )
            .expect("seed account");
            let combo_id = openproxy_db::combos::create_combo(
                &conn,
                &route,
                openproxy_types::combos::Strategy::Priority,
                1,
            )
            .expect("seed combo");
            if let Some((input, output)) = pricing {
                conn.execute(
                    "UPDATE combos SET pricing_input_per_1m = ?1, pricing_output_per_1m = ?2 \
                     WHERE id = ?3",
                    rusqlite::params![input, output, combo_id.0],
                )
                .expect("price combo");
            }
            openproxy_db::combos::add_target(
                &conn,
                openproxy_types::combos::AddTargetInput {
                    combo_id,
                    provider_id: ProviderId::new("mock-openai"),
                    account_id: Some(account_id),
                    model_row_id: Some(ModelRowId(model_row_id)),
                    sub_combo_id: None,
                    priority_order: 1,
                    description: None,
                    selected_accounts: None,
                    client_profile: None,
                },
            )
            .expect("add target");
            conn.execute(
                "INSERT INTO api_keys (id, key_hash, label) VALUES (?1, ?2, 'budget')",
                rusqlite::params![key_id, format!("budget-fixture-{key_id}")],
            )
            .expect("seed api key");
            if let Some(units) = grant {
                conn.execute(
                    "INSERT INTO credit_transactions (api_key_id, kind, amount_units, remaining_units) \
                     VALUES (?1, 'grant', ?2, ?2)",
                    rusqlite::params![key_id, units],
                )
                .expect("grant credit");
            }
        }

        // A pool address can be reused by a later test; the key id is unique per
        // fixture, so no lane or pending marker from another test can be hit.

        let adapter = MockAdapter::new("mock-openai", &upstream.base_url(), AdapterFormat::Openai);
        Self {
            pool,
            master_key,
            adapters: vec![ProviderAdapterEnum::Mock(Box::new(adapter))],
            key_id,
            route,
            upstream,
        }
    }

    // Deliberately `Option`: the APIs under test take `Option<ApiKeyId>`.
    #[allow(clippy::unnecessary_wraps)]
    fn key(&self) -> Option<ApiKeyId> {
        Some(ApiKeyId(self.key_id))
    }

    fn free_key(&self) -> Option<ApiKeyId> {
        None
    }

    fn deps<'a>(
        &'a self,
        client: &'a Arc<UpstreamClient>,
        breaker: &'a CircuitBreakerRegistry,
    ) -> UnaryDeps<'a> {
        UnaryDeps {
            db_pool: &self.pool,
            adapters: &self.adapters,
            upstream_client: client,
            circuit_breaker: breaker,
            master_key: self.master_key.as_ref(),
        }
    }

    fn balance(&self) -> i64 {
        let conn = self.pool.reader();
        openproxy_db::credits_ledger::balance_units(&conn, self.key_id).expect("balance")
    }

    fn usage_rows(&self) -> i64 {
        let conn = self.pool.reader();
        conn.query_row(
            "SELECT COUNT(*) FROM usage WHERE api_key_id = ?1",
            [self.key_id],
            |row| row.get(0),
        )
        .expect("count usage")
    }

    /// `(prompt_tokens, completion_tokens, audio_ms, cost_usd, credits_debited)`
    /// of the newest usage row for this key.
    fn last_usage(&self) -> (Option<i64>, Option<i64>, Option<i64>, f64, i64) {
        let conn = self.pool.reader();
        conn.query_row(
            "SELECT prompt_tokens, completion_tokens, audio_ms, COALESCE(cost_usd, -1), \
                    credits_debited \
             FROM usage WHERE api_key_id = ?1 ORDER BY id DESC LIMIT 1",
            [self.key_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("last usage row")
    }

    /// The upstream mock answering `path`; created lazily per test.
    async fn respond(&self, path: &str, body: &str) -> httpmock::Mock<'_> {
        self.upstream
            .mock_async(|when, then| {
                when.method(POST).path(path);
                then.status(200)
                    .header("content-type", "application/json")
                    .body(body);
            })
            .await
    }
}

fn embedding_request(model: &str, text: &str) -> EmbeddingRequest {
    EmbeddingRequest {
        model: model.to_string(),
        input: EmbeddingInput::Single(text.to_string()),
        encoding_format: None,
        dimensions: None,
        user: None,
    }
}

fn image_request(model: &str, images: u32) -> ImageGenerationRequest {
    ImageGenerationRequest {
        prompt: "a cat".to_string(),
        model: model.to_string(),
        n: Some(images),
        quality: None,
        response_format: None,
        size: None,
        style: None,
        user: None,
        aspect_ratio: None,
        seed: None,
        negative_prompt: None,
        post_processing: None,
    }
}

fn system_one_request(model: &str) -> SystemOneRequest {
    SystemOneRequest {
        state: serde_json::json!({ "ticket": "x" }),
        model: Some(model.to_string()),
        questions: BTreeMap::new(),
    }
}

fn audio_body(model: &str, bytes: Vec<u8>) -> ParsedAudioBody {
    ParsedAudioBody {
        model_name: model.to_string(),
        file_bytes: Bytes::from(bytes),
        file_name: "clip.wav".to_string(),
        file_content_type: "audio/wav".to_string(),
        form_fields: Vec::new(),
    }
}

fn breaker() -> CircuitBreakerRegistry {
    CircuitBreakerRegistry::new(&CircuitBreakerConfig::default())
}

const EMBEDDING_OK: &str = r#"{"object":"list","data":[{"object":"embedding","index":0,"embedding":[0.1]}],
    "model":"mock-model","usage":{"prompt_tokens":0,"total_tokens":0}}"#;

// ── embeddings ──────────────────────────────────────────────────────

#[tokio::test]
async fn prepaid_embedding_over_the_balance_runs_the_last_draw() {
    // "hello" is a 5-byte input: the bound is 5 + 8 framing = 13 tokens, i.e.
    // 13_000 micro-USD at 1000 USD/1M. The grant is 1_000: the LAST-DRAW
    // policy admits it anyway and the debit clamps to the grant, closing the
    // balance at exactly 0 instead of stranding an unspendable remainder.
    let fx = Fixture::new("mock-model", Some((1000.0, 0.0)), Some(1_000), None).await;
    let mock = fx.respond("/embeddings", EMBEDDING_OK).await;
    let client = UpstreamClient::new();
    let cb = breaker();

    crate::embeddings::execute_embeddings(
        fx.deps(&client, &cb),
        embedding_request(&fx.route, "hello"),
        fx.key(),
        false,
    )
    .await
    .expect("a positive balance is admitted (last-draw)");
    assert_eq!(mock.calls_async().await, 1, "the request dispatched");
    assert_eq!(fx.balance(), 0, "the debit consumed the whole grant");

    // The balance is gone: a second attempt is refused with a clean 402 and
    // the lane is not wedged.
    let again = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        crate::embeddings::execute_embeddings(
            fx.deps(&client, &cb),
            embedding_request(&fx.route, "hello"),
            fx.key(),
            false,
        ),
    )
    .await
    .expect("the refusal must not leave the key lane held");
    let error = again.expect_err("an exhausted balance must be refused");
    assert!(
        matches!(error, CoreError::InsufficientCredits { .. }),
        "expected 402, got {error:?}"
    );
}

#[tokio::test]
async fn prepaid_embedding_within_budget_bills_the_bound_when_usage_is_zero() {
    let fx = Fixture::new("mock-model", Some((1000.0, 0.0)), Some(20_000), None).await;
    let mock = fx.respond("/embeddings", EMBEDDING_OK).await;
    let client = UpstreamClient::new();
    let cb = breaker();

    crate::embeddings::execute_embeddings(
        fx.deps(&client, &cb),
        embedding_request(&fx.route, "hello"),
        fx.key(),
        false,
    )
    .await
    .expect("an affordable request dispatches");

    assert_eq!(mock.calls_async().await, 1);
    let (prompt, completion, audio, cost, debited) = fx.last_usage();
    assert_eq!(
        prompt,
        Some(13),
        "the upstream reported zero tokens; the row must bill the admitted bound"
    );
    assert_eq!(completion, None);
    assert_eq!(audio, None);
    assert!(cost > 0.0, "a bound-billed row can never cost zero");
    assert_eq!(debited, 1);
    assert_eq!(fx.balance(), 7_000, "exactly the bound was debited");
}

#[tokio::test]
async fn reported_usage_above_the_bound_is_kept_as_evidence_not_hidden() {
    // The upstream claims a million tokens; the admitted bound is 13. Clamping
    // the row down to 13 would understate a real overrun and erase the evidence,
    // so the reported number is kept and the ledger's own clamp is what limits
    // the damage: the balance goes to zero, never below it.
    let fx = Fixture::new("mock-model", Some((1000.0, 0.0)), Some(20_000), None).await;
    let body = r#"{"object":"list","data":[],"model":"mock-model",
        "usage":{"prompt_tokens":1000000,"total_tokens":1000000}}"#;
    fx.respond("/embeddings", body).await;
    let client = UpstreamClient::new();
    let cb = breaker();

    crate::embeddings::execute_embeddings(
        fx.deps(&client, &cb),
        embedding_request(&fx.route, "hello"),
        fx.key(),
        false,
    )
    .await
    .expect("the request dispatches");

    let (prompt, _, _, cost, _) = fx.last_usage();
    assert_eq!(
        prompt,
        Some(1_000_000),
        "the reported quantity is the evidence of the overrun"
    );
    assert!(cost >= 1_000.0, "the row records what the response claimed");
    assert_eq!(
        fx.balance(),
        0,
        "the debit is clamped to the balance, never below it"
    );
}

#[tokio::test]
async fn unpriced_prepaid_request_is_refused_but_a_free_key_still_dispatches() {
    let fx = Fixture::new("unpriced-model", None, Some(1_000_000), None).await;
    let mock = fx.respond("/embeddings", EMBEDDING_OK).await;
    let client = UpstreamClient::new();
    let cb = breaker();

    let error = crate::embeddings::execute_embeddings(
        fx.deps(&client, &cb),
        embedding_request(&fx.route, "hello"),
        fx.key(),
        false,
    )
    .await
    .expect_err("missing pricing on a prepaid key must fail closed");
    assert_eq!(
        error.http_status(),
        503,
        "expected an explicit refusal: {error:?}"
    );
    assert_eq!(mock.calls_async().await, 0);
    assert_eq!(fx.balance(), 1_000_000);

    // The same model on a key without ledger rows keeps working: pricing is a
    // prepaid concern, not a free-tier restriction.
    crate::embeddings::execute_embeddings(
        fx.deps(&client, &cb),
        embedding_request(&fx.route, "hello"),
        fx.free_key(),
        false,
    )
    .await
    .expect("a free key must not depend on pricing");
    assert_eq!(mock.calls_async().await, 1);
}

#[tokio::test]
async fn multimodal_embedding_input_is_refused_before_dispatch() {
    // The token cost of an image part belongs to the remote resource, not to
    // the URL that references it: there is no client-side bound, so a prepaid
    // request carrying one is refused instead of dispatched unpriced.
    let fx = Fixture::new("mock-model", Some((1000.0, 0.0)), Some(10_000_000), None).await;
    let mock = fx.respond("/embeddings", EMBEDDING_OK).await;
    let client = UpstreamClient::new();
    let cb = breaker();

    let request = EmbeddingRequest {
        model: fx.route.clone(),
        input: EmbeddingInput::Parts(
            vec![
                openproxy_types::embeddings::EmbeddingPart::Text {
                    text: "describe".to_string(),
                },
                openproxy_types::embeddings::EmbeddingPart::Image {
                    url: "https://example.invalid/cat.png".to_string(),
                },
            ]
            .into(),
        ),
        encoding_format: None,
        dimensions: None,
        user: None,
    };
    let error =
        crate::embeddings::execute_embeddings(fx.deps(&client, &cb), request, fx.key(), false)
            .await
            .expect_err("a remote media part has no pre-dispatch bound");
    assert_eq!(error.http_status(), 503, "got {error:?}");
    assert_eq!(mock.calls_async().await, 0);
    assert_eq!(fx.balance(), 10_000_000);
}

// ── audio ───────────────────────────────────────────────────────────

#[tokio::test]
async fn unreadable_audio_is_refused_before_dispatch() {
    let fx = Fixture::new("whisper-large-v3", None, Some(1_000_000), None).await;
    let mock = fx
        .respond("/audio/transcriptions", r#"{"text":"hi"}"#)
        .await;
    let client = UpstreamClient::new();
    let cb = breaker();

    let error = crate::audio::execute_transcribe(
        &fx.pool,
        &fx.adapters,
        &client,
        &cb,
        fx.master_key.as_ref(),
        audio_body(&fx.route, b"definitely not an audio container".to_vec()),
        fx.key(),
    )
    .await
    .err()
    .expect("an unbounded audio duration must fail closed");
    assert_eq!(
        error.http_status(),
        400,
        "expected a validation refusal, got {error:?}"
    );
    assert_eq!(
        mock.calls_async().await,
        0,
        "the file must never be uploaded"
    );
    assert_eq!(fx.balance(), 1_000_000);
    assert_eq!(fx.usage_rows(), 0);
}

#[tokio::test]
async fn known_audio_duration_is_billed_from_the_pre_dispatch_probe() {
    // whisper-large-v3 is 0.111 USD/hour: one second is ~30.8 micro-units.
    let fx = Fixture::new("whisper-large-v3", None, Some(1_000), None).await;
    let mock = fx
        .respond("/audio/transcriptions", r#"{"text":"hi"}"#)
        .await;
    let client = UpstreamClient::new();
    let cb = breaker();

    crate::audio::execute_transcribe(
        &fx.pool,
        &fx.adapters,
        &client,
        &cb,
        fx.master_key.as_ref(),
        audio_body(&fx.route, crate::audio::generate_test_speech_wav()),
        fx.key(),
    )
    .await
    .expect("a one-second clip fits the balance");

    assert_eq!(mock.calls_async().await, 1);
    let (_, _, audio_ms, cost, debited) = fx.last_usage();
    assert_eq!(audio_ms, Some(1_000), "the probed duration is persisted");
    assert!(cost > 0.0, "speech-to-text must not be recorded at zero");
    assert_eq!(debited, 1);
    let debited_units = (cost * 1_000_000.0).round() as i64;
    assert!(
        (30..=31).contains(&debited_units),
        "one second at 0.111 USD/hour is ~30.8 micro-units, got {debited_units}"
    );
    assert_eq!(fx.balance(), 1_000 - debited_units);
}

#[tokio::test]
async fn audio_priced_by_tokens_is_refused_instead_of_billed_at_zero() {
    let fx = Fixture::new(
        "whisper-large-v3",
        Some((1000.0, 0.0)),
        Some(1_000_000),
        None,
    )
    .await;
    let mock = fx
        .respond("/audio/transcriptions", r#"{"text":"hi"}"#)
        .await;
    let client = UpstreamClient::new();
    let cb = breaker();

    let error = crate::audio::execute_transcribe(
        &fx.pool,
        &fx.adapters,
        &client,
        &cb,
        fx.master_key.as_ref(),
        audio_body(&fx.route, crate::audio::generate_test_speech_wav()),
        fx.key(),
    )
    .await
    .err()
    .expect("a token tariff cannot bill audio");
    assert_eq!(error.http_status(), 503, "got {error:?}");
    assert_eq!(mock.calls_async().await, 0);
    assert_eq!(fx.balance(), 1_000_000);
}

// ── images ──────────────────────────────────────────────────────────

const IMAGE_OK: &str = r#"{"created":1,"data":[{"url":"https://example.invalid/a.png"}]}"#;

#[tokio::test]
async fn prepaid_image_without_a_per_image_tariff_is_refused() {
    // A combo (token-shaped) price is the only price available for this model:
    // multiplying it by an image count would invent a rate per image that no
    // contract describes, so the prepaid request is refused instead.
    let fx = Fixture::new(
        "mock-image",
        Some((1_000_000.0, 0.0)),
        Some(5_000_000),
        None,
    )
    .await;
    let mock = fx.respond("/images/generations", IMAGE_OK).await;
    let client = UpstreamClient::new();
    let cb = breaker();

    let error = crate::images::execute_image_generation(
        &fx.pool,
        &fx.adapters,
        &client,
        &cb,
        fx.master_key.as_ref(),
        image_request(&fx.route, 2),
        fx.key(),
    )
    .await
    .expect_err("a token tariff cannot price an image");
    assert_eq!(error.http_status(), 503, "got {error:?}");
    assert_eq!(
        mock.calls_async().await,
        0,
        "no image may be generated unpaid"
    );
    assert_eq!(fx.balance(), 5_000_000, "the balance must be untouched");
}

#[tokio::test]
async fn free_key_image_still_dispatches_without_pricing() {
    let fx = Fixture::new("mock-image", None, None, None).await;
    let mock = fx.respond("/images/generations", IMAGE_OK).await;
    let client = UpstreamClient::new();
    let cb = breaker();

    crate::images::execute_image_generation(
        &fx.pool,
        &fx.adapters,
        &client,
        &cb,
        fx.master_key.as_ref(),
        image_request(&fx.route, 2),
        fx.free_key(),
    )
    .await
    .expect("a key without ledger rows never depends on pricing");

    assert_eq!(mock.calls_async().await, 1);
}

// ── systemone ───────────────────────────────────────────────────────

#[tokio::test]
async fn system_one_without_usage_bills_the_model_output_bound() {
    // The model row caps output at 64 tokens; the response omits `usage`.
    let fx = Fixture::new(
        "mock-model",
        Some((1000.0, 1000.0)),
        Some(500_000),
        Some(64),
    )
    .await;
    let mock = fx.respond("/systemone", r#"{"answers":{}}"#).await;
    let client = UpstreamClient::new();
    let cb = breaker();

    let response: SystemOneResponse = crate::systemone::execute_system_one(
        fx.deps(&client, &cb),
        system_one_request(&fx.route),
        fx.key(),
        false,
    )
    .await
    .expect("the request fits the balance");
    assert!(response.usage.is_none(), "the fixture omits usage");

    assert_eq!(mock.calls_async().await, 1);
    let (prompt, completion, _, cost, debited) = fx.last_usage();
    assert!(
        prompt.is_some_and(|tokens| tokens > 0),
        "the input bound is persisted instead of NULL"
    );
    assert_eq!(completion, Some(64), "the output bound is persisted");
    assert!(cost > 0.0, "an omitted usage block must not bill zero");
    assert_eq!(debited, 1);
    assert!(fx.balance() < 500_000);
}

#[tokio::test]
async fn system_one_request_over_the_balance_runs_the_last_draw() {
    let fx = Fixture::new("mock-model", Some((1000.0, 1000.0)), Some(1), Some(64)).await;
    let mock = fx.respond("/systemone", r#"{"answers":{}}"#).await;
    let client = UpstreamClient::new();
    let cb = breaker();

    crate::systemone::execute_system_one(
        fx.deps(&client, &cb),
        system_one_request(&fx.route),
        fx.key(),
        false,
    )
    .await
    .expect("a positive balance is admitted (last-draw)");
    assert_eq!(mock.calls_async().await, 1);
    assert_eq!(fx.balance(), 0, "the debit clamped to the 1-unit grant");
}

#[tokio::test]
async fn system_one_with_a_priced_output_and_no_ceiling_is_refused() {
    // Output is billed per token and neither the model row nor the shared
    // catalog declares a limit: the maximum cost is unknown, so the request is
    // refused rather than admitted under an invented ceiling.
    let fx = Fixture::new("mock-model", Some((1000.0, 1000.0)), Some(5_000_000), None).await;
    let mock = fx.respond("/systemone", r#"{"answers":{}}"#).await;
    let client = UpstreamClient::new();
    let cb = breaker();

    let error = crate::systemone::execute_system_one(
        fx.deps(&client, &cb),
        system_one_request(&fx.route),
        fx.key(),
        false,
    )
    .await
    .expect_err("an unbounded output slot must fail closed");
    assert_eq!(error.http_status(), 503, "got {error:?}");
    assert_eq!(mock.calls_async().await, 0);
    assert_eq!(fx.balance(), 5_000_000);
}

// ── native engine ───────────────────────────────────────────────────

/// A host-local engine spends no metered upstream, so it is admitted without a
/// price — the one place where "no price" is not a free ride. The engine itself
/// needs downloaded weights and cannot run in CI, so this test drives the exact
/// policy call the entrypoints make (`HostLocal::confirm`).
#[tokio::test]
async fn local_engine_target_is_admitted_without_pricing() {
    let fx = Fixture::new("mock-model", None, Some(1_000), Some(64)).await;
    let lease = UnaryCreditLease::admit(&fx.pool, fx.key())
        .await
        .expect("admitted");
    assert!(lease.is_prepaid());

    let target = UnaryTarget {
        provider: ProviderId::new("laya"),
        account_id: None,
        model_row_id: None,
        combo_target_id: None,
        upstream_model: "mock-model".to_string(),
        combo_id: None,
    };
    let local = HostLocal::confirm(&target, true).expect("laya runs on this host");
    let quote = lease
        .authorize_dispatch(
            &fx.pool,
            &target,
            UnaryCharge::SystemOne(&system_one_request(&fx.route)),
            Some(local),
        )
        .await
        .expect("a local engine target is not priced")
        .expect("a prepaid key still gets a bound");
    assert_eq!(quote.units(), 0, "local execution costs no upstream money");
    assert!(
        quote.prompt_tokens().is_some_and(|tokens| tokens > 0),
        "the input quantities are still bounded"
    );

    drop(lease);
    assert_eq!(fx.balance(), 1_000, "a local request debits nothing here");
}

/// The local free path must not be reachable for a metered target, and a
/// provider name is not enough on its own: the dispatcher has to have made the
/// host-local decision for *that* target.
#[tokio::test]
async fn host_local_claim_for_a_metered_target_is_rejected() {
    let fx = Fixture::new(
        "mock-model",
        Some((1000.0, 1000.0)),
        Some(1_000_000),
        Some(64),
    )
    .await;
    let lease = UnaryCreditLease::admit(&fx.pool, fx.key())
        .await
        .expect("admitted");

    // A local engine id on a target that is not served on this host: `confirm`
    // refuses to mint the proof at all.
    let remote_laya = UnaryTarget {
        provider: ProviderId::new("laya"),
        account_id: None,
        model_row_id: None,
        combo_target_id: None,
        upstream_model: "mock-model".to_string(),
        combo_id: None,
    };
    assert!(
        HostLocal::confirm(&remote_laya, false).is_none(),
        "a provider name alone never proves local execution"
    );

    // A proof minted for one target cannot be reused for a different provider.
    let metered = UnaryTarget {
        provider: ProviderId::new("mock-openai"),
        account_id: None,
        model_row_id: None,
        combo_target_id: None,
        upstream_model: "mock-model".to_string(),
        combo_id: None,
    };
    let borrowed = HostLocal::confirm(&remote_laya, true).expect("laya proof");
    let error = lease
        .authorize_dispatch(
            &fx.pool,
            &metered,
            UnaryCharge::SystemOne(&system_one_request(&fx.route)),
            Some(borrowed),
        )
        .await
        .expect_err("a host-local proof must not price a metered provider");
    assert!(matches!(error, CoreError::Internal(_)), "got {error:?}");
}
