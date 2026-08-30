#[derive(Debug, Clone, Copy)]
pub(crate) enum ProxyRotationTrigger {
    Status(u16),
    ConnectError,
    RateLimited,
}

pub(crate) fn find_bad_proxy_id(
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

pub(crate) fn should_rotate_proxy(
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

pub(crate) struct ProxyRotationArgs<'a> {
    pub(crate) conn: &'a rusqlite::Connection,
    pub(crate) repo: &'a dyn crate::repository::PipelineRepository,
    pub(crate) provider_id: &'a openproxy_types::ids::ProviderId,
    pub(crate) bad_proxy: &'a str,
    pub(crate) trigger: ProxyRotationTrigger,
    pub(crate) is_per_account: bool,
    pub(crate) account_id: Option<openproxy_types::ids::AccountId>,
    pub(crate) cooldown_ms: Option<u64>,
}

pub(crate) fn apply_proxy_rotation(args: ProxyRotationArgs<'_>) -> bool {
    let cooldown_duration = args.cooldown_ms.map_or_else(
        || std::time::Duration::from_mins(15),
        std::time::Duration::from_millis,
    );

    if matches!(args.trigger, ProxyRotationTrigger::ConnectError) {
        let _ = args.repo.update_proxy_status(args.bad_proxy, "dead", None);
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
