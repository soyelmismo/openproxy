//! Gateway-side supervisor. No tokenizer or native session is created here.
use super::{CoreError, SystemOneRequest, SystemOneResponse, assets, worker};

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct Settings {
    pub model: Option<String>,
    pub tokenizer: Option<String>,
    pub config: Option<String>,
    pub threads: Option<usize>,
}

crate::adapters::native_engine_common::define_engine_lifecycle!(
    label: "Laya",
    thread_name: "laya-supervisor",
    idle_env: "OPENPROXY_LAYA_IDLE_TIMEOUT_MINUTES",
    recv_timeout_secs: 2400,
    call_timeout_secs: 30,
    unavail_msg: "Laya worker unavailable",
    req_ty: SystemOneRequest,
    resp_ty: SystemOneResponse,
    settings_ty: Settings,
    ensure_fn: assets::ensure,
    client_ty: worker::Client
);

pub fn execute_decision(req: &SystemOneRequest) -> Result<SystemOneResponse, CoreError> {
    worker::validate_request(req)?;
    submit(Some(req.clone()), Settings::default())?
        .ok_or_else(|| CoreError::Internal("Laya worker returned no response".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::adapters::native_engine_common::define_supervisor_tests!(
        SystemOneRequest,
        SystemOneResponse,
        Settings,
        "requires local Laya weights, server binary and Linux Landlock ABI 3"
    );
}
