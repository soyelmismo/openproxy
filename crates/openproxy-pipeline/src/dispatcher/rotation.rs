//! Proxy rotation: detección de triggers y aplicación del cambio de proxy
//! (marcar muerto, cooldown, buscar candidatos). Mantiene el patrón
//! `spawn_blocking` con `conn.lock()` confinado al closure (AGENTS.md §4.3:
//! el guard nunca cruza `.await`).

use super::UpstreamDispatcher;

/// Disparadores de rotación. `RateLimited` se evalúa siempre;
/// `Status(code)` y `ConnectError` se contrastan contra la lista
/// `proxy_rotation_errors` del provider.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ProxyRotationTrigger {
    Status(u16),
    ConnectError,
    RateLimited,
}

/// Argumentos para `apply_proxy_rotation`. Privados al submódulo: el resto
/// del código pasa por `check_and_trigger_proxy_rotation` que ya
/// construye los args.
pub(super) struct ProxyRotationArgs<'a> {
    pub(super) conn: &'a rusqlite::Connection,
    pub(super) provider_id: &'a openproxy_types::ids::ProviderId,
    pub(super) bad_proxy: &'a str,
    pub(super) trigger: ProxyRotationTrigger,
    pub(super) is_per_account: bool,
    pub(super) account_id: Option<openproxy_types::ids::AccountId>,
    pub(super) cooldown_ms: Option<u64>,
}

/// Resuelve qué proxy ID se considera "malo" para esta rotación.
///
/// Prioridad:
/// 1. Override explícito del request (`override_proxy_id`).
/// 2. Proxy per-cuenta (cuando `is_per_account=true`).
/// 3. Proxy actual del provider.
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

/// Decide si el trigger amerita rotación comparándolo contra la lista CSV
/// `proxy_rotation_errors` del provider. `RateLimited` rota siempre.
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

/// Aplica la rotación en BD:
/// - `ConnectError` → marca el proxy como `dead`.
/// - Inserta cooldown para `(provider_id, bad_proxy)` con la duración dada
///   (default 15 minutos).
/// - Limpia `current_proxy_id` del provider o de la cuenta según el modo.
/// - Devuelve `true` si existe al menos un proxy candidato disponible
///   (sin cooldown, status='alive').
///
/// Todas las escrituras son fire-and-forget (`let _ = …`) salvo el cómputo
/// final de candidatos, que sí propaga resultado para informar al caller.
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

impl UpstreamDispatcher {
    /// Versión async: busca el provider, valida `use_proxies`, y delega a
    /// `apply_proxy_rotation`. Todo el acceso a BD ocurre dentro de un
    /// `spawn_blocking` para no bloquear el reactor Tokio.
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

            if should_rotate_proxy(&provider, trigger)
                && let Some(ref bad_proxy) = bad_proxy_id
            {
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
            }
            false
        })
        .await
        .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    //! Test P3 obligatorio (AGENTS.md §3.1 P3): `apply_proxy_rotation` extraída
    //! con más de 20 líneas requiere al menos 1 test unitario.

    use super::*;
    use openproxy_db::migrations;

    /// Test unitario (no async, no spawn_blocking). Patrón corregido:
    /// adquirimos el guard UNA vez y pasamos `&Connection` por deref
    /// (`&*guard`) sin re-lock (AGENTS.md §4.3). Seeds mínimos vía SQL
    /// directo (`free_proxies::insert` no existe en `openproxy-db`).
    #[test]
    fn apply_proxy_rotation_marks_proxy_dead_on_connect_error() {
        // 1) Pool en disco temporal con migrations.
        let dir = std::env::temp_dir().join(format!(
            "openproxy-rotation-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let pool = openproxy_db::DbPool::open(&dir.join("rot.db")).expect("open pool");
        {
            let mut w = pool.writer();
            migrations::run(&mut w).expect("migrations");
        }

        // 2) Conexión propia para los seeds + llamada directa a la función.
        let conn_arc = std::sync::Arc::new(parking_lot::Mutex::new(
            pool.open_connection().expect("open extra connection"),
        ));

        // 3) Seeds: provider (use_proxies activo, rotación global) + un
        //    proxy "bad-1" marcado vivo y un candidato "cand-1" vivo.
        let provider_id = openproxy_types::ids::ProviderId::new("rot-test");
        {
            let c = conn_arc.lock();
            // Seed de los proxies ANTES del provider para que la FK
            // `current_proxy_id` → `free_proxies.id` se satisfaga cuando
            // actualicemos el provider.
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
            // Habilita proxies y fija binding actual al proxy malo.
            c.execute(
                "UPDATE providers SET use_proxies = 1, \
                 proxy_rotation_errors = 'connect_error,timeout', \
                 proxy_rotation_mode = 'global', \
                 current_proxy_id = 'bad-1' WHERE id = ?1",
                rusqlite::params![provider_id.as_str()],
            )
            .expect("enable provider proxies");
        }

        // 4) Llamada directa con guard único (sin re-lock).
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

        // 5) Aserciones sobre los efectos en BD.
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

        // El candidato vivo 'cand-1' no está en cooldown → hay candidato.
        assert!(
            had_candidate,
            "with an alive seed, get_candidate_proxies_for_provider must return a candidate"
        );

        // Cleanup best-effort.
        drop(c);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
