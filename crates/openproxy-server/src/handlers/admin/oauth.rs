mod authorize;
mod autoclaw;
mod callback;
mod device;
mod exchange;
mod state_store;
mod support;

pub use authorize::oauth_authorize;
pub use autoclaw::{VerifyCaptchaInput, autoclaw_captcha_config, autoclaw_verify_captcha};
pub use callback::oauth_callback;
pub(crate) use callback::refresh_oauth_if_needed;
pub use device::{oauth_device_code, oauth_device_poll};
pub use exchange::oauth_exchange;
pub use state_store::OAuthStateStore;
pub(crate) use support::{
    get_oauth_redirect_uri, resolve_or_create_oauth_account, save_oauth_token_and_notify,
};

// Sibling handler module, reachable as `super::providers` from submodules.
use super::{AppState, providers};

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/{provider}/authorize", axum::routing::get(oauth_authorize))
        .route("/{provider}/exchange", axum::routing::post(oauth_exchange))
        .route(
            "/{provider}/device-code",
            axum::routing::post(oauth_device_code),
        )
        .route(
            "/{provider}/device-poll",
            axum::routing::post(oauth_device_poll),
        )
        .route(
            "/autoclaw/captcha-config",
            axum::routing::get(autoclaw_captcha_config),
        )
        .route(
            "/autoclaw/verify-captcha",
            axum::routing::post(autoclaw_verify_captcha),
        )
}
