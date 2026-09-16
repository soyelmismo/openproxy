//! Database initialization, maintenance, schema migrations, and boot backfills.

use openproxy_db::conn::WriterGuard;

pub(crate) fn init_database(
    config: &openproxy_core::AppConfig,
) -> anyhow::Result<openproxy_db::DbPool> {
    let path = config.expanded_database_path();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(openproxy_db::DbPool::open(&path)?)
}

/// Migrations and persisted-config hydration run inline at boot because
/// the rest of `AppState::new` (load_adapters, supervisor wiring, etc.)
/// depends on the schema being current. The slow backfill (provider
/// re-pricing + `backfill_usage_pricing` full-table scan) is deferred
/// to [`crate::background::BackfillService`] so the listener socket can
/// bind immediately.
pub(crate) fn run_database_maintenance(
    w: &mut WriterGuard<'_>,
    config: &mut openproxy_core::AppConfig,
    recording_ttl_secs: &mut i64,
    idle_chunk_retryable: &mut bool,
    compression_mode: &mut openproxy_compression::CompressionMode,
    notifications_enabled: &mut bool,
) -> anyhow::Result<()> {
    openproxy_db::migrations::run(w)?;
    load_persisted_config_overrides(
        w,
        config,
        recording_ttl_secs,
        idle_chunk_retryable,
        compression_mode,
        notifications_enabled,
    )?;
    Ok(())
}

fn load_persisted_timeouts(
    w: &WriterGuard<'_>,
    config: &mut openproxy_core::AppConfig,
) -> anyhow::Result<()> {
    if let Some(override_cfg) = openproxy_db::app_config::load_timeouts_override_from_db(w)? {
        tracing::info!(
            connect_ms = override_cfg.connect_ms,
            request_send_ms = override_cfg.request_send_ms,
            ttft_ms = override_cfg.ttft_ms,
            idle_chunk_ms = override_cfg.idle_chunk_ms,
            total_ms = override_cfg.total_ms,
            "loaded persisted timeouts override from app_config"
        );
        config.timeouts = override_cfg;
    }
    Ok(())
}

fn load_persisted_runtime_flags(
    w: &WriterGuard<'_>,
    recording_ttl_secs: &mut i64,
    idle_chunk_retryable: &mut bool,
    notifications_enabled: &mut bool,
) -> anyhow::Result<()> {
    if let Some(ttl) = openproxy_db::app_config::load_recording_ttl_from_db(w)? {
        *recording_ttl_secs = ttl;
    }
    tracing::info!(
        recording_ttl_secs = *recording_ttl_secs,
        "loaded recording TTL from app_config (default 300s)"
    );

    if let Some(val) = openproxy_db::app_config::load_idle_chunk_retryable_from_db(w)? {
        *idle_chunk_retryable = val;
    }
    tracing::info!(
        idle_chunk_retryable = *idle_chunk_retryable,
        "loaded idle_chunk_retryable from app_config (default false)"
    );

    if let Some(val) = openproxy_db::app_config::load_notifications_enabled_from_db(w)? {
        *notifications_enabled = val;
    }
    tracing::info!(
        notifications_enabled = *notifications_enabled,
        "loaded notifications_enabled from app_config (default true)"
    );
    Ok(())
}

fn load_persisted_compression_and_quota(
    w: &WriterGuard<'_>,
    config: &mut openproxy_core::AppConfig,
    compression_mode: &mut openproxy_compression::CompressionMode,
) -> anyhow::Result<()> {
    if let Some(mode) = openproxy_db::app_config::load_compression_override_from_db(w)? {
        tracing::info!(
            ?mode,
            "loaded persisted compression override from app_config"
        );
        *compression_mode = mode;
    } else {
        tracing::info!(
            ?compression_mode,
            "no persisted compression override; using config default"
        );
    }

    if let Some(quota_cfg) = openproxy_db::app_config::load_quota_protection_override_from_db(w)? {
        tracing::info!(
            enabled = quota_cfg.enabled,
            threshold_percentage = quota_cfg.threshold_percentage,
            "loaded persisted quota_protection override from app_config"
        );
        config.quota_protection = quota_cfg;
    }

    if let Some(pii_cfg) = openproxy_db::app_config::load_pii_config_from_db(w)? {
        tracing::info!(
            pii_enabled = pii_cfg.pii_enabled,
            pii_reversible = pii_cfg.pii_reversible,
            "loaded persisted pii config override from app_config"
        );
        config.pii = pii_cfg;
    }
    Ok(())
}

fn load_persisted_config_overrides(
    w: &WriterGuard<'_>,
    config: &mut openproxy_core::AppConfig,
    recording_ttl_secs: &mut i64,
    idle_chunk_retryable: &mut bool,
    compression_mode: &mut openproxy_compression::CompressionMode,
    notifications_enabled: &mut bool,
) -> anyhow::Result<()> {
    load_persisted_timeouts(w, config)?;
    load_persisted_runtime_flags(
        w,
        recording_ttl_secs,
        idle_chunk_retryable,
        notifications_enabled,
    )?;
    load_persisted_compression_and_quota(w, config, compression_mode)?;
    Ok(())
}

fn run_provider_and_combo_seeding(w: &WriterGuard<'_>) -> anyhow::Result<()> {
    let seeded = openproxy_core::seed::seed_builtin_providers(w)?;
    if seeded > 0 {
        tracing::info!(seeded, "auto-seeded built-in providers on first start");
    }

    if openproxy_core::seed::seed_virtual_combo_provider(w)? {
        tracing::info!("auto-seeded virtual 'combo' provider for sub-combo targets");
    }
    Ok(())
}

fn run_model_and_usage_backfills(w: &WriterGuard<'_>) -> anyhow::Result<usize> {
    let mut total = 0usize;
    let backfilled: usize = openproxy_core::seed::backfill_model_metadata(w)? as usize;
    if backfilled > 0 {
        tracing::info!(
            backfilled,
            "backfilled model metadata from heuristics on first start"
        );
        total += backfilled;
    }

    let normalized = openproxy_core::models_dev_sync::backfill_model_id_normalized(w)?;
    if normalized > 0 {
        tracing::info!(
            normalized,
            "backfilled model_id_normalized for existing model rows on boot"
        );
        total += normalized;
    }

    let repriced = openproxy_core::models_dev_sync::recompute_costs(w)?;
    if repriced > 0 {
        tracing::info!(
            repriced,
            "re-priced historical usage rows with missing pricing on boot"
        );
        total += repriced;
    }

    // Also run the cost-rs backfill which targets rows the recompute
    // pass missed (e.g. when pricing was unknown at record time but
    // became available later via a models.dev sync). Wrapped in
    // with_busy_retry because this is the slowest writer op and the
    // most likely to race with concurrent vacuum/usage inserts.
    let cost_backfilled = openproxy_db::with_busy_retry("backfill::backfill_usage_pricing", || {
        openproxy_db::cost::backfill_usage_pricing(w)
    })
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "backfill_usage_pricing failed in background pass");
        0
    });
    if cost_backfilled > 0 {
        tracing::info!(
            cost_backfilled,
            "backfilled cost_usd on historical usage rows from updated pricing"
        );
        total += cost_backfilled;
    }

    Ok(total)
}

fn ensure_bootstrap_key_logged(w: &WriterGuard<'_>) -> anyhow::Result<()> {
    if let Some(b) = openproxy_core::bootstrap::ensure_bootstrap_key(w, "bootstrap")? {
        tracing::info!(
            id = b.id.0,
            prefix = ?b.key_prefix,
            "bootstrap key ready (see WARN log / stderr for plaintext)"
        );
    }
    Ok(())
}

/// Full boot-time backfill: provider seed, model-metadata backfill,
/// usage repricing, and bootstrap key creation. Intended to be called
/// from the background [`crate::background::BackfillService`] so the
/// listener socket can bind before the slow operations finish.
///
/// Returns the total number of rows touched across all steps; the
/// caller uses this to update the admin UI's "warming up" banner.
pub(crate) fn run_boot_backfill(w: &WriterGuard<'_>) -> anyhow::Result<usize> {
    run_provider_and_combo_seeding(w)?;
    let total = run_model_and_usage_backfills(w)?;
    ensure_bootstrap_key_logged(w)?;
    Ok(total)
}
