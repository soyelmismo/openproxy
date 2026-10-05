//! Proxy rotation: detección de triggers y aplicación del cambio de proxy
//! (marcar muerto, cooldown, buscar candidatos). Todo acceso a BD va dentro
//! de `spawn_blocking` con el `conn.lock()` confinado al closure: el guard
//! nunca cruza un `.await` (AGENTS.md §4.3).

use super::UpstreamDispatcher;

/// Triggers de rotación. `RateLimited` se evalúa siempre; `Status(code)` y
/// `ConnectError` se contrastan contra la lista CSV `proxy_rotation_errors` del
/// provider.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ProxyRotationTrigger {
    Status(u16),
    ConnectError,
    RateLimited,
}

/// Construido por `check_and_trigger_proxy_rotation`.
pub(super) struct ProxyRotationArgs<'a> {
    pub(super) conn: &'a rusqlite::Connection,
    pub(super) provider_id: &'a openproxy_types::ids::ProviderId,
    pub(super) bad_proxy: &'a str,
    pub(super) trigger: ProxyRotationTrigger,
    pub(super) is_per_account: bool,
    pub(super) account_id: Option<openproxy_types::ids::AccountId>,
    pub(super) cooldown_ms: Option<u64>,
}

/// Proxy ID considerado "malo" para esta rotación: override explícito del
/// request, proxy per-cuenta, o proxy actual del provider, en ese orden.
pub(super) fn find_bad_proxy_id(
    provider: &openproxy_types::providers::Provider,
    conn: &rusqlite::Connection,
    override_proxy_id: Option<&str>,
    is_per_account: bool,
    account_id: Option<openproxy_types::ids::AccountId>,
) -> Option<String> {
    if let Some(pid) = override_proxy_id {
        Some(pid.to_string())
    } else if is_per_account {
        account_id.and_then(|acc_id| {
            openproxy_db::accounts::get_current_proxy_id(conn, acc_id).unwrap_or(None)
        })
    } else {
        provider
            .current_proxy_id
            .as_deref()
            .map(ToString::to_string)
    }
}

pub(super) fn should_rotate_proxy(
    provider: &openproxy_types::providers::Provider,
    trigger: ProxyRotationTrigger,
) -> bool {
    match trigger {
        ProxyRotationTrigger::RateLimited => true,
        ProxyRotationTrigger::Status(sc) => {
            let sc_str = sc.to_string();
            provider
                .proxy_rotation_errors
                .split(',')
                .map(str::trim)
                .any(|e| e == sc_str)
        }
        ProxyRotationTrigger::ConnectError => provider
            .proxy_rotation_errors
            .split(',')
            .map(str::trim)
            .any(|e| e == "connect_error" || e == "timeout"),
    }
}

/// Aplica la rotación en BD. `ConnectError` marca el proxy como `dead`; en
/// ambos casos inserta el cooldown de `(provider_id, bad_proxy)` (15 minutos
/// por defecto) y limpia `current_proxy_id` del provider o de la cuenta según
/// el modo.
///
/// Las escrituras son fire-and-forget. Solo el cómputo final de candidatos
/// propaga resultado, para que el caller sepa si queda alguno disponible
/// (alive y sin cooldown).
pub(super) fn apply_proxy_rotation(args: ProxyRotationArgs<'_>) -> bool {
    let cooldown_duration = args.cooldown_ms.map_or_else(
        || std::time::Duration::from_mins(15),
        std::time::Duration::from_millis,
    );

    if matches!(args.trigger, ProxyRotationTrigger::ConnectError) {
        let _ = openproxy_db::free_proxies::update_proxy_status(
            args.conn,
            args.bad_proxy,
            "dead",
            None,
        );
    }

    let _ = openproxy_db::cooldowns::add_provider_proxy_cooldown(
        args.conn,
        args.provider_id.as_str(),
        args.bad_proxy,
        cooldown_duration,
    );
    let provider = openproxy_db::providers::get(args.conn, args.provider_id).unwrap_or(None);
    let direct_first = provider.as_ref().is_some_and(|p| p.direct_first);

    if direct_first {
        let assigned = openproxy_db::free_proxies::assign_new_proxy(
            args.conn,
            args.provider_id,
            args.account_id.as_ref(),
            args.is_per_account,
        );
        match assigned {
            Ok(Some(_)) => true,
            _ => {
                if args.is_per_account {
                    if let Some(acc_id) = args.account_id {
                        let _ = openproxy_db::accounts::clear_current_proxy_id(args.conn, acc_id);
                    }
                } else {
                    let _ = openproxy_db::providers::update_current_proxy(args.conn, args.provider_id, None);
                }
                false
            }
        }
    } else {
        if args.is_per_account {
            if let Some(acc_id) = args.account_id {
                let _ = openproxy_db::accounts::clear_current_proxy_id(args.conn, acc_id);
            }
        } else {
            let _ = openproxy_db::providers::update_current_proxy(args.conn, args.provider_id, None);
        }

        openproxy_db::free_proxies::get_candidate_proxies_for_provider(args.conn, args.provider_id, 1)
            .is_ok_and(|c| !c.is_empty())
    }
}

impl UpstreamDispatcher {
    /// Todo el acceso a BD ocurre dentro de un `spawn_blocking` para no bloquear
    /// el reactor Tokio.
    pub(super) async fn check_and_trigger_proxy_rotation(
        &self,
        provider_id: &openproxy_types::ids::ProviderId,
        account_id: Option<openproxy_types::ids::AccountId>,
        override_proxy_id: Option<&str>,
        trigger: ProxyRotationTrigger,
        cooldown_ms: Option<u64>,
    ) -> bool {
        let conn_clone = std::sync::Arc::clone(&self.conn);
        let provider_id = provider_id.to_owned();
        let override_proxy_id = override_proxy_id.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            let conn = conn_clone.lock();
            let Some(provider) = openproxy_db::providers::get(&conn, &provider_id).unwrap_or(None)
            else {
                return false;
            };
            if !provider.use_proxies {
                return false;
            }
            let is_per_account = provider.proxy_rotation_mode.as_ref() == "account";
            let bad_proxy_id = find_bad_proxy_id(
                &provider,
                &conn,
                override_proxy_id.as_deref(),
                is_per_account,
                account_id,
            );

            if should_rotate_proxy(&provider, trigger) {
                if let Some(ref bad_proxy) = bad_proxy_id {
                    tracing::warn!(
                        provider = %provider_id,
                        account_id = ?account_id,
                        proxy_id = %bad_proxy,
                        trigger = ?trigger,
                        "proxy rotation triggered: clearing binding and adding cooldown for provider"
                    );
                    return apply_proxy_rotation(ProxyRotationArgs {
                        conn: &conn,
                        provider_id: &provider_id,
                        bad_proxy,
                        trigger,
                        is_per_account,
                        account_id,
                        cooldown_ms,
                    });
                } else if provider.direct_first {
                    tracing::warn!(
                        provider = %provider_id,
                        account_id = ?account_id,
                        trigger = ?trigger,
                        "direct connection error with direct_first enabled: rotating to proxy pool"
                    );
                    let assigned = openproxy_db::free_proxies::assign_new_proxy(
                        &conn,
                        &provider_id,
                        account_id.as_ref(),
                        is_per_account,
                    );
                    return assigned.is_ok_and(|opt| opt.is_some());
                }
            }
            false
        })
        .await
        .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Guard adquirido una vez y pasado por deref (`&*guard`), nunca re-locked
    // (AGENTS.md §4.3). Los seeds van por SQL directo porque `openproxy-db` no
    // expone `free_proxies::insert`.
    #[test]
    fn apply_proxy_rotation_marks_proxy_dead_on_connect_error() {
        let pool = openproxy_db::DbPool::test_pool_with_prefix("openproxy-rotation-test")
            .expect("open pool");

        let conn_arc = std::sync::Arc::new(parking_lot::Mutex::new(
            pool.open_connection().expect("open extra connection"),
        ));

        let provider_id = openproxy_types::ids::ProviderId::new("rot-test");
        {
            let c = conn_arc.lock();
            // Proxies sembrados antes que el provider para que la FK
            // `current_proxy_id` → `free_proxies.id` se satisfaga al actualizar.
            c.execute(
                "INSERT INTO free_proxies (id, source, host, port, type, status) \
                 VALUES ('bad-1', 'custom', 'bad-host', 9999, 'http', 'alive')",
                [],
            )
            .expect("seed bad proxy");
            c.execute(
                "INSERT INTO free_proxies (id, source, host, port, type, status) \
                 VALUES ('cand-1', 'custom', 'cand-host', 8080, 'http', 'alive')",
                [],
            )
            .expect("seed alive candidate");

            openproxy_db::providers::create(
                &c,
                openproxy_db::providers::NewProvider {
                    id: &provider_id,
                    name: "rot-test",
                    base_url: "https://example.com",
                    auth_type: openproxy_types::providers::AuthType::Bearer,
                    format: openproxy_types::providers::ProviderFormat::Openai,
                    extra_headers_json: None,
                    auto_activate_keyword: None,
                    rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
                },
            )
            .expect("seed provider");
            c.execute(
                "UPDATE providers SET use_proxies = 1, \
                 proxy_rotation_errors = 'connect_error,timeout', \
                 proxy_rotation_mode = 'global', \
                 current_proxy_id = 'bad-1' WHERE id = ?1",
                rusqlite::params![provider_id.as_str()],
            )
            .expect("enable provider proxies");
        }

        let had_candidate = {
            let c = conn_arc.lock();
            apply_proxy_rotation(ProxyRotationArgs {
                conn: &c,
                provider_id: &provider_id,
                bad_proxy: "bad-1",
                trigger: ProxyRotationTrigger::ConnectError,
                is_per_account: false,
                account_id: None,
                cooldown_ms: Some(60_000),
            })
        };

        let c = conn_arc.lock();
        let bad_status: String = c
            .query_row(
                "SELECT status FROM free_proxies WHERE id = 'bad-1'",
                [],
                |r| r.get(0),
            )
            .expect("proxy 'bad-1' must exist");
        assert_eq!(
            bad_status, "dead",
            "ConnectError trigger must mark proxy as dead"
        );

        let cooldown_count: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM provider_proxy_cooldowns \
                 WHERE provider_id = ?1 AND proxy_id = 'bad-1'",
                rusqlite::params![provider_id.as_str()],
                |r| r.get(0),
            )
            .expect("count cooldowns");
        assert_eq!(
            cooldown_count, 1,
            "cooldown row must be inserted for (provider, bad_proxy)"
        );

        let cleared_provider: Option<String> = c
            .query_row(
                "SELECT current_proxy_id FROM providers WHERE id = ?1",
                rusqlite::params![provider_id.as_str()],
                |r| r.get(0),
            )
            .expect("provider current_proxy_id");
        assert!(
            cleared_provider.is_none(),
            "current_proxy_id must be cleared after non-per-account rotation"
        );

        assert!(
            had_candidate,
            "with an alive seed, get_candidate_proxies_for_provider must return a candidate"
        );

        drop(c);
    }

    #[test]
    fn test_direct_first_rotation_eagerly_assigns_candidate() {
        let pool = openproxy_db::DbPool::test_pool_with_prefix("openproxy-direct-first-test")
            .expect("open pool");

        let conn_arc = std::sync::Arc::new(parking_lot::Mutex::new(
            pool.open_connection().expect("open connection"),
        ));

        let provider_id = openproxy_types::ids::ProviderId::new("df-test");
        {
            let c = conn_arc.lock();
            c.execute(
                "INSERT INTO free_proxies (id, source, host, port, type, status) \
                 VALUES ('cand-1', 'custom', 'host-1', 8080, 'http', 'alive'), \
                        ('cand-2', 'custom', 'host-2', 8080, 'http', 'alive')",
                [],
            )
            .expect("seed proxies");

            openproxy_db::providers::create(
                &c,
                openproxy_db::providers::NewProvider {
                    id: &provider_id,
                    name: "df-test",
                    base_url: "https://example.com",
                    auth_type: openproxy_types::providers::AuthType::Bearer,
                    format: openproxy_types::providers::ProviderFormat::Openai,
                    extra_headers_json: None,
                    auto_activate_keyword: None,
                    rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
                },
            )
            .expect("seed provider");

            c.execute(
                "UPDATE providers SET use_proxies = 1, direct_first = 1, \
                 proxy_rotation_errors = '429,connect_error,timeout', \
                 proxy_rotation_mode = 'global' WHERE id = ?1",
                rusqlite::params![provider_id.as_str()],
            )
            .expect("enable direct_first");
        }

        // Initially with direct_first, get_or_assign_provider_proxy returns None (direct host connection)
        {
            let c = conn_arc.lock();
            let initial_proxy = openproxy_db::free_proxies::get_or_assign_provider_proxy(&c, &provider_id, None)
                .expect("get_or_assign");
            assert_eq!(initial_proxy, None, "direct_first must return None initially (using direct IP)");
        }

        // Direct IP encounters error: assign proxy from pool
        {
            let c = conn_arc.lock();
            let assigned = openproxy_db::free_proxies::assign_new_proxy(&c, &provider_id, None, false)
                .expect("assign proxy");
            assert!(assigned.is_some(), "must assign candidate from pool");

            let current: Option<String> = c
                .query_row(
                    "SELECT current_proxy_id FROM providers WHERE id = ?1",
                    rusqlite::params![provider_id.as_str()],
                    |r| r.get(0),
                )
                .expect("current_proxy_id");
            assert!(current.is_some(), "current_proxy_id must be bound");
            let bound_id = current.unwrap();

            // When bound proxy fails, apply_proxy_rotation eagerly assigns next candidate
            let rotated = apply_proxy_rotation(ProxyRotationArgs {
                conn: &c,
                provider_id: &provider_id,
                bad_proxy: &bound_id,
                trigger: ProxyRotationTrigger::ConnectError,
                is_per_account: false,
                account_id: None,
                cooldown_ms: Some(60_000),
            });
            assert!(rotated, "rotation must succeed and assign next candidate");

            let new_current: Option<String> = c
                .query_row(
                    "SELECT current_proxy_id FROM providers WHERE id = ?1",
                    rusqlite::params![provider_id.as_str()],
                    |r| r.get(0),
                )
                .expect("new current_proxy_id");
            assert!(new_current.is_some());
            assert_ne!(new_current.as_deref(), Some(bound_id.as_str()), "must rotate to different candidate");
        }
    }
}
