//! Usage queries: list, summary, by-model, by-account, by-status, errors.
//!
//! See docs/mvp-spec.md §7 (Analytics Queries) and §8 (SQLite Schema).
//!
//! This module is the *read* side of the usage table; inserts live in
//! [`crate::cost::record`]. All queries share a common filter shape and are
//! race-aware: `winners` counts rows where `race_lost = 0`, `losers` counts
//! `race_lost = 1`, and `unique_requests` is `COUNT(DISTINCT request_id)` so a
//! race of N losers + 1 winner counts as one logical request.
//!
//! ## Broadcast channel
//!
//! The module also owns the canonical in-process broadcast channel for newly
//! inserted usage rows. After [`crate::cost::record`] inserts a row, it calls
//! [`publish_usage_row`] to push the new row to all connected WebSocket
//! clients. The sender is stored in a `once_cell::sync::OnceCell` so it is

pub mod analytics;

pub use analytics::*;

#[cfg(test)]
mod tests {
    use crate::ids::*;
    use crate::usage::*;

    use rusqlite::{Connection, params};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = base.join(format!(
            "openproxy-usage-test-{}-{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    /// Build a fresh in-memory-style DB on disk, run migrations, and return
    /// the connection plus a cleanup closure (we use a file because rusqlite
    /// doesn't expose `:memory:` across `&Connection` borrows without
    /// `OpenFlags::SQLITE_OPEN_URI`).
    fn fresh_conn() -> (Connection, PathBuf) {
        let dir = tempdir();
        let path = dir.join("usage-test.db");
        let mut conn = Connection::open(&path).expect("open");
        openproxy_db::migrations::run(&mut conn).expect("migrate");
        (conn, path)
    }

    /// Insert one usage row with all defaults driven by the test fixture.
    /// Counts start at 0/200ms ttft/1200ms total to make aggregate assertions
    /// easy to write by inspection.
    // ponytail: [Demasiados argumentos] -> [Refactorizar a struct en el futuro]
    fn insert(
        conn: &Connection,
        request_id: &str,
        trace_id: &str,
        provider: &str,
        model: &str,
        account: Option<i64>,
        status: u16,
        prompt: i64,
        completion: i64,
        cost: f64,
        ttft: Option<i64>,
        total: i64,
        race_lost: bool,
        err: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO usage (\
                request_id, trace_id, attempt, provider_id, account_id, \
                upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
                connect_ms, ttft_ms, total_ms, status_code, error_msg, \
                error_msg_redacted, race_total, race_lost, created_at\
             ) VALUES (\
                ?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, 50, ?9, ?10, ?11, ?12, ?12, 1, ?13, datetime('now')\
             )",
            params![
                request_id,
                trace_id,
                provider,
                account,
                model,
                prompt,
                completion,
                cost,
                ttft,
                total,
                status as i64,
                err,
                race_lost as i64,
            ],
        )
        .expect("insert");
    }

    // -----------------------------------------------------------------------
    // 1. summary_basic — 3 rows sharing one request_id, 1 winner + 2 losers.
    // -----------------------------------------------------------------------
    #[test]
    fn summary_basic() {
        let (conn, _p) = fresh_conn();
        let req = RequestId::new().to_string();
        let t1 = TraceId::new().to_string();
        let t2 = TraceId::new().to_string();
        let t3 = TraceId::new().to_string();
        insert(
            &conn,
            &req,
            &t1,
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            200,
            100,
            50,
            0.01,
            Some(200),
            1200,
            false,
            None,
        );
        insert(
            &conn,
            &req,
            &t2,
            "openrouter",
            "openai/gpt-4o",
            Some(2),
            200,
            100,
            50,
            0.01,
            Some(200),
            1200,
            true,
            None,
        );
        insert(
            &conn,
            &req,
            &t3,
            "openrouter",
            "openai/gpt-4o",
            Some(3),
            200,
            100,
            50,
            0.01,
            Some(200),
            1200,
            true,
            None,
        );

        let s = summary(&conn, &UsageFilter::default()).expect("summary");
        assert_eq!(s.unique_requests, 1, "three rows share one request_id");
        assert_eq!(s.total_rows, 3);
        assert_eq!(s.total_attempts, 3, "three distinct trace_ids");
        assert_eq!(s.winners, 1);
        assert_eq!(s.losers, 2);
        assert_eq!(s.errors, 0);
        assert_eq!(s.total_prompt_tokens, 300);
        assert_eq!(s.total_completion_tokens, 150);
        assert!((s.total_cost_usd - 0.03).abs() < 1e-9);
        assert_eq!(s.avg_ttft_ms, Some(200.0));
        assert!((s.avg_total_ms - 1200.0).abs() < 1e-9);
    }

    // -----------------------------------------------------------------------
    // 2. summary_with_provider_filter
    // -----------------------------------------------------------------------
    #[test]
    fn summary_with_provider_filter() {
        let (conn, _p) = fresh_conn();
        // 2 rows for openrouter, 1 row for anthropic.
        let r1 = RequestId::new().to_string();
        let r2 = RequestId::new().to_string();
        let r3 = RequestId::new().to_string();
        insert(
            &conn,
            &r1,
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            200,
            10,
            5,
            0.001,
            Some(100),
            600,
            false,
            None,
        );
        insert(
            &conn,
            &r2,
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(2),
            200,
            10,
            5,
            0.001,
            Some(100),
            600,
            false,
            None,
        );
        insert(
            &conn,
            &r3,
            &TraceId::new().to_string(),
            "anthropic",
            "claude-3.5-sonnet",
            Some(3),
            200,
            10,
            5,
            0.001,
            Some(100),
            600,
            false,
            None,
        );

        let f = UsageFilter {
            provider_id: Some(ProviderId::new("openrouter")),
            ..Default::default()
        };
        let s = summary(&conn, &f).expect("filtered summary");
        assert_eq!(s.unique_requests, 2);
        assert_eq!(s.total_rows, 2);
        assert_eq!(s.total_cost_usd, 0.002);

        let f_none = UsageFilter::default();
        let s_all = summary(&conn, &f_none).expect("unfiltered summary");
        assert_eq!(s_all.unique_requests, 3);
        assert_eq!(s_all.total_rows, 3);
    }

    // -----------------------------------------------------------------------
    // 3. by_model_groups_by_provider_and_model
    // -----------------------------------------------------------------------
    #[test]
    fn by_model_groups_by_provider_and_model() {
        let (conn, _p) = fresh_conn();
        // gpt-4o x2, gpt-4o-mini x1, claude x1
        let r1 = RequestId::new().to_string();
        let r2 = RequestId::new().to_string();
        let r3 = RequestId::new().to_string();
        let r4 = RequestId::new().to_string();
        insert(
            &conn,
            &r1,
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            200,
            100,
            50,
            0.5,
            Some(200),
            1200,
            false,
            None,
        );
        insert(
            &conn,
            &r2,
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            200,
            100,
            50,
            0.5,
            Some(200),
            1200,
            false,
            None,
        );
        insert(
            &conn,
            &r3,
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o-mini",
            Some(1),
            200,
            100,
            50,
            0.05,
            Some(200),
            600,
            false,
            None,
        );
        insert(
            &conn,
            &r4,
            &TraceId::new().to_string(),
            "anthropic",
            "claude-3.5-sonnet",
            Some(2),
            200,
            100,
            50,
            1.0,
            Some(200),
            1500,
            false,
            None,
        );

        let rows = by_model(&conn, &UsageFilter::default()).expect("by_model");
        assert_eq!(rows.len(), 3, "three (provider, model) buckets");

        // Order: total_cost_usd DESC. Costs are 1.0, 1.0, 0.05.
        assert_eq!(rows[0].upstream_model_id, "claude-3.5-sonnet");
        assert!((rows[0].total_cost_usd - 1.0).abs() < 1e-9);
        assert_eq!(rows[0].unique_requests, 1);
        assert_eq!(rows[0].winners, 1);

        // openai/gpt-4o cost 1.0 (0.5 + 0.5) — second by cost.
        let gpt = rows
            .iter()
            .find(|r| r.upstream_model_id == "openai/gpt-4o")
            .expect("gpt row");
        assert_eq!(gpt.unique_requests, 2);
        assert_eq!(gpt.total_rows, 2);
        assert!((gpt.total_cost_usd - 1.0).abs() < 1e-9);
        assert_eq!(gpt.provider_id, ProviderId::new("openrouter"));

        let mini = rows
            .iter()
            .find(|r| r.upstream_model_id == "openai/gpt-4o-mini")
            .expect("mini row");
        assert_eq!(mini.total_rows, 1);
        assert!((mini.total_cost_usd - 0.05).abs() < 1e-9);
    }

    // -----------------------------------------------------------------------
    // 4. by_account_groups_by_account
    // -----------------------------------------------------------------------
    #[test]
    fn by_account_groups_by_account() {
        let (conn, _p) = fresh_conn();
        // account 1: 2 successes
        // account 2: 1 success, 1 error
        let r1 = RequestId::new().to_string();
        let r2 = RequestId::new().to_string();
        let r3 = RequestId::new().to_string();
        let r4 = RequestId::new().to_string();
        insert(
            &conn,
            &r1,
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            200,
            10,
            5,
            0.10,
            Some(100),
            600,
            false,
            None,
        );
        insert(
            &conn,
            &r2,
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            200,
            10,
            5,
            0.10,
            Some(100),
            600,
            false,
            None,
        );
        insert(
            &conn,
            &r3,
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(2),
            200,
            10,
            5,
            0.05,
            Some(100),
            600,
            false,
            None,
        );
        insert(
            &conn,
            &r4,
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(2),
            500,
            10,
            5,
            0.0,
            Some(100),
            600,
            false,
            Some("upstream 500"),
        );

        let rows = by_account(&conn, &UsageFilter::default()).expect("by_account");
        assert_eq!(rows.len(), 2);

        // Ordered by total_cost_usd DESC: acc 1 (0.20) before acc 2 (0.05).
        assert_eq!(rows[0].account_id, AccountId::new(1));
        assert_eq!(rows[0].total_rows, 2);
        assert_eq!(rows[0].errors, 0);
        assert_eq!(rows[0].unique_requests, 2);
        assert!((rows[0].total_cost_usd - 0.20).abs() < 1e-9);
        assert_eq!(rows[0].provider_id, ProviderId::new("openrouter"));

        assert_eq!(rows[1].account_id, AccountId::new(2));
        assert_eq!(rows[1].total_rows, 2);
        assert_eq!(rows[1].errors, 1);
        assert!((rows[1].total_cost_usd - 0.05).abs() < 1e-9);
    }

    // -----------------------------------------------------------------------
    // 5. by_status_groups_by_code
    // -----------------------------------------------------------------------
    #[test]
    fn by_status_groups_by_code() {
        let (conn, _p) = fresh_conn();
        // 3x 200, 2x 429, 1x 500
        for _ in 0..3 {
            insert(
                &conn,
                &RequestId::new().to_string(),
                &TraceId::new().to_string(),
                "openrouter",
                "openai/gpt-4o",
                Some(1),
                200,
                10,
                5,
                0.01,
                Some(100),
                600,
                false,
                None,
            );
        }
        for _ in 0..2 {
            insert(
                &conn,
                &RequestId::new().to_string(),
                &TraceId::new().to_string(),
                "openrouter",
                "openai/gpt-4o",
                Some(1),
                429,
                10,
                5,
                0.0,
                Some(100),
                600,
                false,
                Some("rate limited"),
            );
        }
        insert(
            &conn,
            &RequestId::new().to_string(),
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            500,
            10,
            5,
            0.0,
            Some(100),
            600,
            false,
            Some("oops"),
        );

        let rows = by_status(&conn, &UsageFilter::default()).expect("by_status");
        assert_eq!(rows.len(), 3);
        // Ordered by count DESC: 200 (3) first, then 429 (2), then 500 (1).
        assert_eq!(rows[0].status_code, 200);
        assert_eq!(rows[0].count, 3);
        assert_eq!(rows[1].status_code, 429);
        assert_eq!(rows[1].count, 2);
        assert_eq!(rows[2].status_code, 500);
        assert_eq!(rows[2].count, 1);
    }

    // -----------------------------------------------------------------------
    // 6. errors_returns_only_4xx_5xx
    // -----------------------------------------------------------------------
    #[test]
    fn errors_returns_only_4xx_5xx() {
        let (conn, _p) = fresh_conn();
        // 1 success
        insert(
            &conn,
            &RequestId::new().to_string(),
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            200,
            10,
            5,
            0.01,
            Some(100),
            600,
            false,
            None,
        );
        // 1 400, 1 502
        insert(
            &conn,
            &RequestId::new().to_string(),
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            400,
            10,
            5,
            0.0,
            Some(100),
            600,
            false,
            Some("bad request"),
        );
        insert(
            &conn,
            &RequestId::new().to_string(),
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            502,
            10,
            5,
            0.0,
            Some(100),
            600,
            false,
            Some("upstream down"),
        );

        let rows = errors(&conn, &UsageFilter::default(), 50).expect("errors");
        assert_eq!(rows.len(), 2);
        // Both rows are >= 400, so we expect both.
        let codes: Vec<u16> = rows.iter().map(|r| r.status_code).collect();
        assert!(codes.contains(&400));
        assert!(codes.contains(&502));
        for r in &rows {
            assert!(r.status_code >= 400);
            assert!(r.error_msg_redacted.is_some());
        }
    }

    // -----------------------------------------------------------------------
    // 7. errors_respects_limit
    // -----------------------------------------------------------------------
    #[test]
    fn errors_respects_limit() {
        let (conn, _p) = fresh_conn();
        for i in 0..5 {
            let req = format!("req-{}", i);
            let trace = format!("trace-{}", i);
            insert(
                &conn,
                &req,
                &trace,
                "openrouter",
                "openai/gpt-4o",
                Some(1),
                500,
                10,
                5,
                0.0,
                Some(100),
                600,
                false,
                Some("err"),
            );
        }
        let rows = errors(&conn, &UsageFilter::default(), 2).expect("errors");
        assert_eq!(rows.len(), 2, "limit caps the result set");
    }

    // -----------------------------------------------------------------------
    // Bonus: filter composition + date bounds.
    // -----------------------------------------------------------------------
    #[test]
    fn summary_with_combo_and_model_filter() {
        let (conn, _p) = fresh_conn();
        // Two rows, one with combo 7 and one without.
        let r1 = RequestId::new().to_string();
        let r2 = RequestId::new().to_string();
        conn.execute(
            "INSERT INTO usage (\
                request_id, trace_id, attempt, provider_id, account_id, combo_id, \
                upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
                connect_ms, ttft_ms, total_ms, status_code, error_msg, \
                error_msg_redacted, race_total, race_lost, created_at\
             ) VALUES (\
                ?1, ?2, 1, 'openrouter', 1, 7, 'openai/gpt-4o', 10, 5, 0.01, 50, 100, 600, 200, NULL, NULL, 1, 0, datetime('now')\
             )",
            params![r1, TraceId::new().to_string()],
        ).expect("insert 1");
        conn.execute(
            "INSERT INTO usage (\
                request_id, trace_id, attempt, provider_id, account_id, combo_id, \
                upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
                connect_ms, ttft_ms, total_ms, status_code, error_msg, \
                error_msg_redacted, race_total, race_lost, created_at\
             ) VALUES (\
                ?1, ?2, 1, 'openrouter', 1, 8, 'openai/gpt-4o-mini', 10, 5, 0.02, 50, 100, 600, 200, NULL, NULL, 1, 0, datetime('now')\
             )",
            params![r2, TraceId::new().to_string()],
        ).expect("insert 2");

        let f = UsageFilter {
            combo_id: Some(ComboId(7)),
            model_id: Some("openai/gpt-4o".to_string()),
            ..Default::default()
        };
        let s = summary(&conn, &f).expect("summary");
        assert_eq!(s.total_rows, 1);
        assert_eq!(s.total_cost_usd, 0.01);
    }

    #[test]
    fn empty_db_returns_zero_summary() {
        let (conn, _p) = fresh_conn();
        let s = summary(&conn, &UsageFilter::default()).expect("summary");
        assert_eq!(s.unique_requests, 0);
        assert_eq!(s.total_rows, 0);
        assert_eq!(s.winners, 0);
        assert_eq!(s.losers, 0);
        // AVG over zero rows is NULL in SQLite → None.
        assert_eq!(s.avg_ttft_ms, None);
        // COALESCE keeps AVG(total_ms) at 0.0 even with no rows.
        assert_eq!(s.avg_total_ms, 0.0);
    }

    #[test]
    fn avg_ttft_skips_null_rows() {
        let (conn, _p) = fresh_conn();
        // Row 1: ttft=100; Row 2: ttft NULL (race_lost pre-body).
        insert(
            &conn,
            &RequestId::new().to_string(),
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            200,
            10,
            5,
            0.01,
            Some(100),
            600,
            false,
            None,
        );
        insert(
            &conn,
            &RequestId::new().to_string(),
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            200,
            10,
            5,
            0.01,
            None,
            600,
            true,
            Some("race lost"),
        );
        let s = summary(&conn, &UsageFilter::default()).expect("summary");
        assert_eq!(s.avg_ttft_ms, Some(100.0), "NULL ttft is excluded from AVG");
    }

    // -----------------------------------------------------------------------
    // 8. recent returns rows newer than since_id, in id ASC order.
    // -----------------------------------------------------------------------
    #[test]
    fn recent_returns_rows_after_since_id() {
        let (conn, _p) = fresh_conn();
        // Three rows, ids will be 1, 2, 3.
        insert(
            &conn,
            &RequestId::new().to_string(),
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            200,
            10,
            5,
            0.01,
            Some(100),
            600,
            false,
            None,
        );
        insert(
            &conn,
            &RequestId::new().to_string(),
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(2),
            200,
            20,
            10,
            0.02,
            Some(150),
            700,
            true,
            None,
        );
        insert(
            &conn,
            &RequestId::new().to_string(),
            &TraceId::new().to_string(),
            "anthropic",
            "claude-3.5-sonnet",
            Some(3),
            500,
            0,
            0,
            0.0,
            None,
            800,
            false,
            Some("oops"),
        );

        // since_id = 0 returns all three, in id ASC order.
        let all = recent(&conn, 0, 50).expect("recent");
        assert_eq!(all.len(), 3);
        assert!(all.iter().map(|r| r.id.0).eq(1..=3));
        // race_lost propagates (1 = true).
        assert!(!all[0].race_lost);
        assert!(all[1].race_lost);
        assert!(!all[2].race_lost);
        // cost_usd + tokens survive the round-trip.
        assert_eq!(all[0].prompt_tokens, Some(10));
        assert_eq!(all[0].completion_tokens, Some(5));
        assert!((all[0].cost_usd.unwrap() - 0.01).abs() < 1e-9);
        // The error row's cost is 0.0; the column is NOT NULL, so we get Some(0.0).
        assert_eq!(all[2].cost_usd, Some(0.0));
        assert_eq!(all[2].status_code, 500);

        // since_id = 1 → only ids > 1, i.e. 2 and 3.
        let after = recent(&conn, 1, 50).expect("recent");
        assert_eq!(after.len(), 2);
        assert!(after.iter().map(|r| r.id.0).eq(2..=3));

        // since_id past the last id → empty.
        let none = recent(&conn, 999, 50).expect("recent");
        assert!(none.is_empty());

        // limit caps the result.
        let capped = recent(&conn, 0, 2).expect("recent");
        assert_eq!(capped.len(), 2);
        assert!(capped.iter().map(|r| r.id.0).eq(1..=2));
    }

    #[test]
    fn recent_desc_returns_newest_first() {
        let (conn, _p) = fresh_conn();
        insert(
            &conn,
            &RequestId::new().to_string(),
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            200,
            10,
            5,
            0.01,
            Some(100),
            600,
            false,
            None,
        );
        insert(
            &conn,
            &RequestId::new().to_string(),
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(2),
            200,
            20,
            10,
            0.02,
            Some(150),
            700,
            true,
            None,
        );
        insert(
            &conn,
            &RequestId::new().to_string(),
            &TraceId::new().to_string(),
            "anthropic",
            "claude-3.5-sonnet",
            Some(3),
            500,
            0,
            0,
            0.0,
            None,
            800,
            false,
            Some("oops"),
        );

        let rows = recent_desc(&conn, 2).expect("recent_desc");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id.0, 3);
        assert_eq!(rows[1].id.0, 2);
    }

    /// H1 regression: three rows for the same request_id (the
    /// retry attempts) must be aggregated into a single
    /// `RecentUsageRow` whose `cost_usd` and token counts are
    /// the SUMs across all three attempts. Status_code picks
    /// the earliest 2xx (which here is the last attempt's
    /// `200`); `id` is the first attempt's id so the
    /// long-polling `since_id` cursor is still monotonically
    /// increasing.
    #[test]
    fn recent_aggregates_retry_attempts_by_request_id() {
        // UPDATED: `recent` no longer groups by request_id. Each attempt
        // is returned as its own row so the dashboard's live-logs view
        // shows each attempt individually (the operator can inspect a
        // specific 5xx/4xx/timeout attempt). The old grouping SUMmed
        // tokens across attempts, which produced misleading values
        // (e.g. "3.9M tokens" for a request with 10 race attempts of
        // 128k each) and used MAX(client_response) so the checkmark
        // appeared on rows where client_response was actually false.
        let (conn, _p) = fresh_conn();
        let shared_req = RequestId::new().to_string();
        // Attempt 1: 502 upstream failure.
        insert(
            &conn,
            &shared_req,
            "trace-a",
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            502,
            10,
            0,
            0.0,
            Some(100),
            600,
            false,
            Some("upstream 502"),
        );
        // Attempt 2: 429 rate limit.
        insert(
            &conn,
            &shared_req,
            "trace-b",
            "openrouter",
            "openai/gpt-4o",
            Some(2),
            429,
            10,
            0,
            0.0,
            Some(100),
            600,
            false,
            Some("rate limited"),
        );
        // Attempt 3: 200 success, full tokens.
        insert(
            &conn,
            &shared_req,
            "trace-c",
            "openrouter",
            "openai/gpt-4o",
            Some(3),
            200,
            100,
            50,
            0.03,
            Some(150),
            700,
            true,
            None,
        );

        let rows = recent(&conn, 0, 50).expect("recent");
        // Each attempt is its own row — NO grouping.
        assert_eq!(rows.len(), 3, "three attempts = three rows (no grouping)");
        // `recent` returns rows in id ASC order (long-poll semantics):
        // attempt 1, 2, 3.
        assert_eq!(rows[0].trace_id, "trace-a");
        assert_eq!(rows[0].status_code, 502);
        assert_eq!(rows[0].prompt_tokens, Some(10));

        assert_eq!(rows[1].trace_id, "trace-b");
        assert_eq!(rows[1].status_code, 429);
        assert_eq!(rows[1].prompt_tokens, Some(10));

        assert_eq!(rows[2].trace_id, "trace-c");
        assert_eq!(rows[2].status_code, 200);
        assert_eq!(rows[2].prompt_tokens, Some(100));
        assert_eq!(rows[2].completion_tokens, Some(50));
        let cost3 = rows[2].cost_usd.expect("cost_usd present");
        assert!((cost3 - 0.03).abs() < 1e-9, "cost_usd was {}", cost3);
    }

    /// H1 mirror: `recent_desc` (the head-of-table fetch the
    /// admin table uses) must apply the same per-request
    /// aggregation as `recent` (the long-poll feed). Without
    /// this, the admin table at the top of the dashboard
    /// would show the cost of the LAST attempt only, even
    /// though the same request's history in the log would
    /// show the summed cost.
    #[test]
    fn recent_desc_aggregates_retry_attempts_by_request_id() {
        // UPDATED: `recent_desc` no longer groups by request_id, mirroring
        // `recent`. Each attempt is its own row.
        let (conn, _p) = fresh_conn();
        let shared_req = RequestId::new().to_string();
        insert(
            &conn,
            &shared_req,
            "trace-a",
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            502,
            10,
            0,
            0.0,
            Some(100),
            600,
            false,
            Some("upstream 502"),
        );
        insert(
            &conn,
            &shared_req,
            "trace-b",
            "openrouter",
            "openai/gpt-4o",
            Some(2),
            200,
            100,
            50,
            0.03,
            Some(150),
            700,
            true,
            None,
        );
        let rows = recent_desc(&conn, 50).expect("recent_desc");
        // No grouping — two attempts = two rows.
        assert_eq!(rows.len(), 2);
        // Ordered by id DESC: attempt 2 (success) first, then attempt 1 (502).
        assert_eq!(rows[0].trace_id, "trace-b");
        assert_eq!(rows[0].status_code, 200);
        assert_eq!(rows[0].prompt_tokens, Some(100));
        assert_eq!(rows[0].completion_tokens, Some(50));
        assert_eq!(rows[1].trace_id, "trace-a");
        assert_eq!(rows[1].status_code, 502);
        assert_eq!(rows[1].prompt_tokens, Some(10));
    }

    /// Compression regression: `recent_desc` (used by the WS
    /// initial history batch) must include the compression stats
    /// columns `compression_savings_pct` and
    /// `compression_techniques`. Before the migration-00031
    /// backfill the row mapping hardcoded both to `None`, so the
    /// dashboard's compression column showed `—` for every
    /// historical row even when compression was applied. Mirrors
    /// the same coverage `recent` already had.
    #[test]
    fn recent_desc_includes_compression_columns() {
        let (conn, _p) = fresh_conn();
        let req = RequestId::new().to_string();
        let trace = TraceId::new().to_string();
        conn.execute(
            "INSERT INTO usage (\
                request_id, trace_id, attempt, provider_id, account_id, \
                upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
                connect_ms, ttft_ms, total_ms, status_code, error_msg, \
                error_msg_redacted, race_total, race_lost, created_at, \
                compression_savings_pct, compression_techniques\
             ) VALUES (\
                ?1, ?2, 1, 'openrouter', 1, 'openai/gpt-4o', 100, 50, 0.01, \
                50, 200, 1200, 200, NULL, NULL, 1, 0, datetime('now'), \
                42.0, 'lite::collapse_whitespace'\
             )",
            params![req, trace],
        )
        .expect("insert with compression stats");
        let row_id = conn.last_insert_rowid();

        let rows = recent_desc(&conn, 10).expect("recent_desc");
        let row = rows
            .iter()
            .find(|r| r.id.0 == row_id)
            .expect("inserted row should be in recent_desc results");
        assert_eq!(row.compression_savings_pct, Some(42.0));
        assert_eq!(
            row.compression_techniques.as_deref(),
            Some("lite::collapse_whitespace")
        );
    }

    #[test]
    fn detail_by_id_returns_full_usage_row() {
        let (conn, _p) = fresh_conn();
        let req = RequestId::new().to_string();
        let trace = TraceId::new().to_string();
        conn.execute(
            "INSERT INTO usage (\
                request_id, trace_id, attempt, provider_id, account_id, combo_id, \
                model_row_id, upstream_model_id, combo_target_id, prompt_tokens, \
                completion_tokens, connect_ms, ttft_ms, total_ms, tokens_per_sec, \
                status_code, error_msg, error_msg_redacted, race_total, race_lost, \
                api_key_id, created_at\
             ) VALUES (\
                ?1, ?2, 2, 'openrouter', 1, 7, 9, 'openai/gpt-4o', 11, 100, 50, \
                25, 200, 1200, 50.0, 500, 'raw secret', 'raw secret', 3, 1, NULL, \
                datetime('now')\
             )",
            params![req, trace],
        )
        .expect("insert detail");
        let id = conn.last_insert_rowid();

        let row = detail_by_id(&conn, id)
            .expect("detail_by_id")
            .expect("row exists");
        assert_eq!(row.id.0, id);
        assert_eq!(row.request_id, req);
        assert_eq!(row.trace_id, trace);
        assert_eq!(row.attempt, 2);
        assert_eq!(row.provider_id, ProviderId::new("openrouter"));
        assert_eq!(row.account_id, Some(AccountId::new(1)));
        assert_eq!(row.combo_id, Some(ComboId(7)));
        assert_eq!(row.model_row_id, Some(ModelRowId(9)));
        assert_eq!(row.upstream_model_id, "openai/gpt-4o");
        assert_eq!(row.combo_target_id, Some(ComboTargetId(11)));
        assert_eq!(row.prompt_tokens, Some(100));
        assert_eq!(row.completion_tokens, Some(50));
        assert_eq!(row.connect_ms, Some(25));
        assert_eq!(row.ttft_ms, Some(200));
        assert_eq!(row.total_ms, 1200);
        assert_eq!(row.tokens_per_sec, Some(50.0));
        assert_eq!(row.status_code, 500);
        assert_eq!(row.error_msg, Some("raw secret".to_string()));
        assert_eq!(row.error_msg_redacted, Some("raw secret".to_string()));
        assert_eq!(row.race_total, 3);
        assert!(row.race_lost);
        assert_eq!(row.api_key_id, None);
        assert!(!row.created_at.is_empty());

        assert!(
            detail_by_id(&conn, id + 1)
                .expect("detail_by_id missing")
                .is_none()
        );
    }

    #[test]
    fn prune_expired_recording_bodies_clears_old_rows() {
        use rusqlite::params;
        let (conn, _p) = fresh_conn();
        // Insert a row with bodies and a very recent created_at.
        conn.execute(
            "INSERT INTO usage (request_id, trace_id, attempt, provider_id, \
             upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
             connect_ms, ttft_ms, total_ms, status_code, race_total, race_lost, \
             created_at, request_body_json, response_body_json, request_headers, response_headers) \
             VALUES (?, ?, 1, 'openrouter', 'openai/gpt-4o', 100, 50, 0.01, 50, 200, 1200, 200, 1, 0, \
                     datetime('now'), '{\"q\":\"hello\"}', '{\"a\":\"world\"}', '{\"ct\":\"text/plain\"}', '{\"ct\":\"text/plain\"}')",
            params!["req1", "trace1"],
        )
        .expect("insert recent row");
        // Insert a row with bodies that is 10 minutes old.
        conn.execute(
            "INSERT INTO usage (request_id, trace_id, attempt, provider_id, \
             upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
             connect_ms, ttft_ms, total_ms, status_code, race_total, race_lost, \
             created_at, request_body_json, response_body_json, request_headers, response_headers) \
             VALUES (?, ?, 1, 'openrouter', 'openai/gpt-4o', 100, 50, 0.01, 50, 200, 1200, 200, 1, 0, \
                     datetime('now', '-600 seconds'), '{\"q\":\"old\"}', '{\"a\":\"old\"}', '{\"ct\":\"old\"}', '{\"ct\":\"old\"}')",
            params!["req2", "trace2"],
        )
        .expect("insert old row");
        // TTL of 5 minutes: only the old row should be pruned.
        let pruned = prune_expired_recording_bodies(&conn, 300).expect("prune");
        assert_eq!(pruned, 1, "only the old row should be pruned");
        // Verify: recent row still has bodies.
        let recent_body: Option<String> = conn
            .query_row(
                "SELECT request_body_json FROM usage WHERE request_id = 'req1'",
                [],
                |r| r.get(0),
            )
            .expect("query recent");
        assert!(recent_body.is_some(), "recent row should still have body");
        // Verify: old row bodies are now NULL.
        let old_body: Option<String> = conn
            .query_row(
                "SELECT request_body_json FROM usage WHERE request_id = 'req2'",
                [],
                |r| r.get(0),
            )
            .expect("query old");
        assert!(old_body.is_none(), "old row body should be NULL");
        // Verify: metadata (e.g. status_code) is preserved for both.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM usage WHERE status_code = 200",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(count, 2, "metadata should be preserved for both rows");
    }

    #[test]
    fn prune_expired_recording_bodies_zero_ttl_clears_all() {
        let (conn, _p) = fresh_conn();
        conn.execute(
            "INSERT INTO usage (request_id, trace_id, attempt, provider_id, \
             upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
             connect_ms, ttft_ms, total_ms, status_code, race_total, race_lost, \
             created_at, request_body_json, response_body_json) \
             VALUES (?, ?, 1, 'openrouter', 'openai/gpt-4o', 100, 50, 0.01, 50, 200, 1200, 200, 1, 0, \
                     datetime('now'), '{\"q\":\"hi\"}', '{\"a\":\"ok\"}')",
            params!["req1", "trace1"],
        )
        .expect("insert");
        let pruned = prune_expired_recording_bodies(&conn, 0).expect("prune");
        assert_eq!(pruned, 1, "zero TTL should clear all bodies");
        let body: Option<String> = conn
            .query_row(
                "SELECT request_body_json FROM usage WHERE request_id = 'req1'",
                [],
                |r| r.get(0),
            )
            .expect("query");
        assert!(body.is_none(), "body should be NULL after zero-TTL prune");
    }

    // -----------------------------------------------------------------------
    // by_provider: groups by provider_id, ordered by total_cost_usd DESC.
    // -----------------------------------------------------------------------
    #[test]
    fn by_provider_groups_by_provider_id() {
        let (conn, _p) = fresh_conn();
        // openrouter: 2 rows, total cost 0.50, both winners.
        // anthropic: 1 row, total cost 1.00, winner.
        // openai: 1 row, total cost 0.10, loser (race_lost=1).
        let r1 = RequestId::new().to_string();
        let r2 = RequestId::new().to_string();
        let r3 = RequestId::new().to_string();
        let r4 = RequestId::new().to_string();
        insert(
            &conn,
            &r1,
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            200,
            100,
            50,
            0.25,
            Some(200),
            1200,
            false,
            None,
        );
        insert(
            &conn,
            &r2,
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o-mini",
            Some(1),
            200,
            100,
            50,
            0.25,
            Some(200),
            600,
            false,
            None,
        );
        insert(
            &conn,
            &r3,
            &TraceId::new().to_string(),
            "anthropic",
            "claude-3.5-sonnet",
            Some(2),
            200,
            100,
            50,
            1.00,
            Some(200),
            1500,
            false,
            None,
        );
        insert(
            &conn,
            &r4,
            &TraceId::new().to_string(),
            "openai",
            "gpt-4o",
            Some(3),
            200,
            100,
            50,
            0.10,
            Some(200),
            800,
            true,
            None,
        );

        let rows = by_provider(&conn, &UsageFilter::default()).expect("by_provider");
        assert_eq!(rows.len(), 3, "three distinct providers");

        // Order: anthropic (1.00), openrouter (0.50), openai (0.10).
        assert_eq!(rows[0].provider_id, "anthropic");
        assert!((rows[0].total_cost_usd - 1.00).abs() < 1e-9);
        assert_eq!(rows[0].total_rows, 1);
        assert_eq!(rows[0].unique_requests, 1);
        assert_eq!(rows[0].winners, 1);
        assert_eq!(rows[0].total_prompt_tokens, 100);
        assert_eq!(rows[0].total_completion_tokens, 50);

        assert_eq!(rows[1].provider_id, "openrouter");
        assert!((rows[1].total_cost_usd - 0.50).abs() < 1e-9);
        assert_eq!(rows[1].total_rows, 2);
        assert_eq!(rows[1].unique_requests, 2);
        assert_eq!(rows[1].winners, 2);
        assert_eq!(rows[1].total_prompt_tokens, 200);

        assert_eq!(rows[2].provider_id, "openai");
        assert!((rows[2].total_cost_usd - 0.10).abs() < 1e-9);
        assert_eq!(rows[2].winners, 0, "race_lost=1 means 0 winners");

        // Filter by a single provider — only that row comes back.
        let f = UsageFilter {
            provider_id: Some(ProviderId::new("openrouter")),
            ..Default::default()
        };
        let filtered = by_provider(&conn, &f).expect("by_provider filtered");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].provider_id, "openrouter");
        assert_eq!(filtered[0].total_rows, 2);
    }

    // -----------------------------------------------------------------------
    // monthly_by_provider: groups by (provider_id, month).
    // -----------------------------------------------------------------------
    #[test]
    fn monthly_by_provider_groups_by_month() {
        let (conn, _p) = fresh_conn();
        // Insert rows with explicit created_at values across two months
        // and three providers. The `insert` helper uses datetime('now'),
        // so we hand-roll the INSERTs to pin the timestamp.
        let insert_at = |provider: &str, cost: f64, prompt: i64, created_at: &str| {
            conn.execute(
                "INSERT INTO usage (\
                    request_id, trace_id, attempt, provider_id, account_id, \
                    upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
                    connect_ms, ttft_ms, total_ms, status_code, error_msg, \
                    error_msg_redacted, race_total, race_lost, created_at\
                 ) VALUES (\
                    ?1, ?2, 1, ?3, 1, 'm', ?4, 0, ?5, 50, 200, 1200, 200, NULL, NULL, 1, 0, ?6\
                 )",
                params![
                    RequestId::new().to_string(),
                    TraceId::new().to_string(),
                    provider,
                    prompt,
                    cost,
                    created_at,
                ],
            )
            .expect("insert");
        };

        // June 2026: openrouter $1.00 + $0.50; anthropic $0.25.
        // RFC-3339 created_at so the lexical string comparison with
        // the filter `from`/`to` (also RFC-3339) is well-defined.
        insert_at("openrouter", 1.00, 100, "2026-06-01T12:00:00Z");
        insert_at("openrouter", 0.50, 100, "2026-06-15T12:00:00Z");
        insert_at("anthropic", 0.25, 50, "2026-06-20T12:00:00Z");
        // July 2026: openrouter $0.10; openai $0.75.
        insert_at("openrouter", 0.10, 10, "2026-07-01T12:00:00Z");
        insert_at("openai", 0.75, 20, "2026-07-31T12:00:00Z");

        let rows =
            monthly_by_provider(&conn, &UsageFilter::default()).expect("monthly_by_provider");
        assert_eq!(rows.len(), 4, "four (provider, month) buckets");

        // Ordered by month ASC, then cost DESC.
        // June (3 rows total):
        //   - openrouter $1.50
        //   - anthropic  $0.25
        // July (2 rows):
        //   - openai     $0.75
        //   - openrouter $0.10
        assert_eq!(rows[0].month, "2026-06");
        assert_eq!(rows[0].provider_id, "openrouter");
        assert!((rows[0].total_cost_usd - 1.50).abs() < 1e-9);
        assert_eq!(rows[0].total_rows, 2);
        assert_eq!(rows[0].unique_requests, 2);
        assert_eq!(rows[0].total_prompt_tokens, 200);

        assert_eq!(rows[1].month, "2026-06");
        assert_eq!(rows[1].provider_id, "anthropic");
        assert!((rows[1].total_cost_usd - 0.25).abs() < 1e-9);

        assert_eq!(rows[2].month, "2026-07");
        assert_eq!(rows[2].provider_id, "openai");
        assert!((rows[2].total_cost_usd - 0.75).abs() < 1e-9);

        assert_eq!(rows[3].month, "2026-07");
        assert_eq!(rows[3].provider_id, "openrouter");
        assert!((rows[3].total_cost_usd - 0.10).abs() < 1e-9);

        // Date filter: only June.
        let f = UsageFilter {
            from: Some("2026-06-01T00:00:00Z".to_string()),
            to: Some("2026-07-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        let june_only = monthly_by_provider(&conn, &f).expect("monthly_by_provider june");
        assert_eq!(june_only.len(), 2);
        assert!(june_only.iter().all(|r| r.month == "2026-06"));
    }

    // -----------------------------------------------------------------------
    // summary.rows_with_null_pricing counts rows where cost_usd = 0 AND
    // prompt_tokens > 0 (pricing was missing at record time).
    // -----------------------------------------------------------------------
    #[test]
    fn summary_counts_rows_with_null_pricing() {
        let (conn, _p) = fresh_conn();
        // Row 1: normal — has prompt_tokens and a non-zero cost.
        insert(
            &conn,
            &RequestId::new().to_string(),
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            200,
            100,
            50,
            0.01,
            Some(100),
            600,
            false,
            None,
        );
        // Row 2: NULL pricing — prompt_tokens > 0 but cost_usd = 0.
        insert(
            &conn,
            &RequestId::new().to_string(),
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            200,
            200,
            50,
            0.00,
            Some(100),
            600,
            false,
            None,
        );
        // Row 3: NULL pricing — same shape, different provider.
        insert(
            &conn,
            &RequestId::new().to_string(),
            &TraceId::new().to_string(),
            "anthropic",
            "claude-3.5-sonnet",
            Some(2),
            200,
            300,
            0,
            0.00,
            Some(100),
            600,
            false,
            None,
        );
        // Row 4: zero tokens AND zero cost — NOT null pricing (no tokens consumed).
        insert(
            &conn,
            &RequestId::new().to_string(),
            &TraceId::new().to_string(),
            "openrouter",
            "openai/gpt-4o",
            Some(1),
            500,
            0,
            0,
            0.00,
            Some(100),
            600,
            false,
            Some("err"),
        );

        let s = summary(&conn, &UsageFilter::default()).expect("summary");
        assert_eq!(s.total_rows, 4);
        assert_eq!(
            s.rows_with_null_pricing, 2,
            "rows 2 and 3 had prompt_tokens > 0 but cost_usd = 0"
        );

        // Filter to just anthropic — only row 3 matches, so count drops to 1.
        let f = UsageFilter {
            provider_id: Some(ProviderId::new("anthropic")),
            ..Default::default()
        };
        let s = summary(&conn, &f).expect("summary filtered");
        assert_eq!(s.total_rows, 1);
        assert_eq!(s.rows_with_null_pricing, 1);

        // Empty DB: counter is 0.
        let (conn2, _p2) = fresh_conn();
        let s_empty = summary(&conn2, &UsageFilter::default()).expect("summary empty");
        assert_eq!(s_empty.rows_with_null_pricing, 0);
    }

    // -----------------------------------------------------------------------
    // Regression: `created_at` is stored as `"YYYY-MM-DD HH:MM:SS"` (space
    // separator, via SQLite's `datetime('now')`), but `UsageFilter` bounds
    // arrive as RFC-3339 `"YYYY-MM-DDTHH:MM:SSZ"` (T separator, Z suffix).
    // A naive byte-wise `created_at >= ?` comparison would put `' '` (0x20)
    // before `'T'` (0x54) and reject every row, making the dashboard show
    // all zeros. The fix wraps both sides in `datetime(...)` so SQLite
    // parses them as real datetimes before comparing.
    // -----------------------------------------------------------------------
    #[test]
    fn date_filter_matches_space_separated_created_at() {
        let (conn, _p) = fresh_conn();
        // Pin a row to 2026-06-15 12:00 UTC. `datetime('now')` writes the
        // space-separated form, so we hand-roll the INSERT.
        conn.execute(
            "INSERT INTO usage (\
                request_id, trace_id, attempt, provider_id, account_id, \
                upstream_model_id, prompt_tokens, completion_tokens, cost_usd, \
                connect_ms, ttft_ms, total_ms, status_code, error_msg, \
                error_msg_redacted, race_total, race_lost, created_at\
             ) VALUES (\
                ?1, ?2, 1, 'openrouter', 1, 'm', 100, 50, 0.10, \
                50, 200, 1200, 200, NULL, NULL, 1, 0, '2026-06-15 12:00:00'\
             )",
            params![RequestId::new().to_string(), TraceId::new().to_string()],
        )
        .expect("insert");

        // Filter bounds in RFC-3339 (T separator + Z suffix), matching the
        // shape that `resolve_preset` produces for `preset=today`.
        let f = UsageFilter {
            from: Some("2026-06-15T00:00:00Z".to_string()),
            to: Some("2026-06-16T00:00:00Z".to_string()),
            ..Default::default()
        };
        let s = summary(&conn, &f).expect("summary");
        assert_eq!(
            s.total_rows, 1,
            "row at 2026-06-15 12:00:00 must match a filter spanning 2026-06-15T00:00:00Z..2026-06-16T00:00:00Z; \
             if this fails, the date-format mismatch bug is back"
        );
        assert_eq!(s.unique_requests, 1);
        assert!((s.total_cost_usd - 0.10).abs() < 1e-9);

        // Same row should NOT match a filter on a different day.
        let f_other = UsageFilter {
            from: Some("2026-06-16T00:00:00Z".to_string()),
            to: Some("2026-06-17T00:00:00Z".to_string()),
            ..Default::default()
        };
        let s_other = summary(&conn, &f_other).expect("summary other day");
        assert_eq!(s_other.total_rows, 0);
    }
}

// Globals for broadcast
static USAGE_SENDER: once_cell::sync::OnceCell<
    tokio::sync::broadcast::Sender<openproxy_types::RecentUsageRow>,
> = once_cell::sync::OnceCell::new();
static STAGE_SENDER: once_cell::sync::OnceCell<
    tokio::sync::broadcast::Sender<openproxy_types::usage::StageEvent>,
> = once_cell::sync::OnceCell::new();

pub fn init_usage_broadcast() -> tokio::sync::broadcast::Sender<openproxy_types::RecentUsageRow> {
    let (tx, _rx) = tokio::sync::broadcast::channel(1024);
    let _ = USAGE_SENDER.set(tx.clone());
    let _ = openproxy_types::usage::USAGE_ROW_PUBLISHER.set(publish_usage_global);
    tx
}

pub fn init_stage_broadcast() -> tokio::sync::broadcast::Sender<openproxy_types::usage::StageEvent>
{
    let (tx, _rx) = tokio::sync::broadcast::channel(200);
    let _ = STAGE_SENDER.set(tx.clone());
    let _ = openproxy_types::usage::STAGE_EVENT_PUBLISHER.set(publish_stage_global);
    tx
}

fn publish_usage_global(row: openproxy_types::RecentUsageRow) {
    if let Some(tx) = USAGE_SENDER.get() {
        let _ = tx.send(openproxy_types::usage::redact_for_broadcast(row));
    }
}

fn publish_stage_global(event: openproxy_types::usage::StageEvent) {
    if let Some(tx) = STAGE_SENDER.get() {
        let _ = tx.send(event);
    }
}
pub use analytics::prune_expired_recording_bodies;
pub use analytics::prune_expired_usage_rows;
