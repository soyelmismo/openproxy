import re

with open("crates/openproxy-core/src/oauth/mod.rs", "r") as f:
    content = f.read()

# Search for the block starting with "for (i, account) in accounts.iter().enumerate() {"
# and ending at the end of the refresh loop.
start_str = "        for (i, account) in accounts.iter().enumerate() {"
end_str = "        // LEAK FIX: prune `failure_counts` / `last_refresh_attempts`"

start_idx = content.find(start_str)
end_idx = content.find(end_str)

if start_idx == -1 or end_idx == -1:
    print("Could not find blocks")
    exit(1)

new_loop = """        use governor::{Quota, RateLimiter};
        use std::num::NonZeroU32;
        let quota = Quota::with_period(std::time::Duration::from_secs(STAGGER_DELAY_SECS))
            .unwrap()
            .allow_burst(NonZeroU32::new(1).unwrap());
        let limiter = std::sync::Arc::new(RateLimiter::direct(quota));

        // Consume the initial token so we don't burst the first request.
        while limiter.check().is_ok() {}

        let mut join_set = tokio::task::JoinSet::new();

        for account in accounts {
            let provider = match registry.get(account.provider_id.as_str()) {
                Some(p) => p.clone(),
                None => {
                    tracing::debug!(
                        provider = %account.provider_id,
                        "oauth refresh: no provider impl found, skipping"
                    );
                    continue;
                }
            };

            let account_id = account.id.0;
            if let Some(last_attempt) = last_refresh_attempts.get(&account_id) {
                let failure_count = failure_counts.get(&account_id).copied().unwrap_or(0);
                let backoff = backoff_seconds(failure_count);
                let elapsed = chrono::Utc::now().signed_duration_since(*last_attempt);
                if elapsed.num_seconds() < backoff as i64 {
                    continue;
                }
            }

            let refresh_token = match refresh_tokens.get(&account.id) {
                Some(Ok(Some(t))) => Ok(Some(t.clone())),
                Some(Ok(None)) => Ok(None),
                Some(Err(e)) => Err(crate::error::CoreError::Internal(e.to_string())),
                None => {
                    Err(crate::error::CoreError::Internal(
                        "refresh token not found in batch".to_string(),
                    ))
                }
            };
            let refresh_token = match refresh_token {
                Ok(Some(rt)) => rt,
                Ok(None) => {
                    tracing::debug!(
                        account = account_id,
                        "oauth refresh: no refresh token stored, skipping"
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        account = account_id,
                        error = %e,
                        "oauth refresh: failed to decrypt refresh token"
                    );
                    continue;
                }
            };

            // We mark attempt locally first.
            last_refresh_attempts.insert(account_id, chrono::Utc::now());

            let lim = limiter.clone();
            let upstream_client = upstream_client.clone();
            let db_pool = db_pool.clone();
            let master_key = master_key.clone();

            join_set.spawn(async move {
                lim.until_ready().await;

                let res = TokenRefreshCoordinator::global()
                    .refresh_and_store(
                        account.provider_id.as_str(),
                        provider.clone(),
                        &refresh_token,
                        &upstream_client,
                        account.id,
                        DbRef::Pool(&db_pool),
                        &master_key,
                    )
                    .await;

                // 2-second settle gap after each refresh (Auth0 protection).
                tokio::time::sleep(std::time::Duration::from_secs(SETTLE_GAP_SECS)).await;

                (account, res)
            });
        }

        while let Some(res) = join_set.join_next().await {
            let (account, result) = match res {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "oauth refresh task panicked");
                    continue;
                }
            };

            let account_id = account.id.0;

            match result {
                Ok(token) => {
                    failure_counts.remove(&account_id);
                    last_refresh_attempts.remove(&account_id);

                    let db_pool = db_pool.clone();
                    let acc_id = account.id;
                    let _ = tokio::task::spawn_blocking(move || {
                        let conn = db_pool.writer();
                        if let Err(e) =
                            crate::accounts::set_health(&conn, acc_id, HealthStatus::Healthy)
                        {
                            tracing::warn!(
                                account = account_id,
                                error = %e,
                                "oauth refresh: failed to set health to healthy"
                            );
                        }
                    })
                    .await;

                    tracing::info!(
                        account = account_id,
                        provider = %account.provider_id,
                        token_type = %token.token_type,
                        "oauth refresh: tokens refreshed successfully"
                    );
                }
                Err(e) => {
                    let count = failure_counts.entry(account_id).or_insert(0);
                    *count += 1;

                    let new_health = if *count >= UNHEALTHY_THRESHOLD {
                        HealthStatus::Unhealthy
                    } else {
                        HealthStatus::Degraded
                    };

                    let db_pool = db_pool.clone();
                    let acc_id = account.id;
                    let count_val = *count;
                    let provider_id_str = account.provider_id.as_str().to_string();
                    let _ = tokio::task::spawn_blocking(move || {
                        let conn = db_pool.writer();
                        if let Err(update_err) =
                            crate::accounts::set_health(&conn, acc_id, new_health)
                        {
                            tracing::warn!(
                                account = account_id,
                                error = %update_err,
                                "oauth refresh: failed to update health status"
                            );
                        }

                        if count_val >= UNHEALTHY_THRESHOLD {
                            let dedup_key = format!(
                                "{}:{}",
                                crate::notifications::CODE_OAUTH_EXPIRED,
                                account_id
                            );
                            let payload = serde_json::json!({
                                "code": crate::notifications::CODE_OAUTH_EXPIRED,
                                "message": format!(
                                    "OAuth token for account {} on {} expired or could not be refreshed ({} consecutive failures)",
                                    account_id, provider_id_str, count_val,
                                ),
                                "provider_id": &provider_id_str,
                                "details": {
                                    "account_id": account_id,
                                    "provider_id": &provider_id_str,
                                    "reason": "refresh_failed",
                                    "consecutive_failures": count_val,
                                },
                            });
                            let _ = crate::notifications::insert_and_broadcast(
                                &conn,
                                crate::notifications::KIND_SYSTEM,
                                &payload,
                                Some(&dedup_key),
                                Some(&provider_id_str),
                            );
                        }
                    }).await;

                    tracing::warn!(
                        account = account_id,
                        provider = %account.provider_id,
                        error = %e,
                        consecutive_failures = *count,
                        health = new_health.as_str(),
                        "oauth refresh: token refresh failed"
                    );
                }
            }
        }

"""

new_content = content[:start_idx] + new_loop + content[end_idx:]

with open("crates/openproxy-core/src/oauth/mod.rs", "w") as f:
    f.write(new_content)

print("Rewritten")
