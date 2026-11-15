use crate::handlers::admin::{ApiError, AppState, CoreError};

use axum::{
    Json,
    extract::{Path, State},
};
use openproxy_core::oauth::{OAuthProvider, OAuthProviderEnum};

use super::get_oauth_redirect_uri;

fn validate_authorize_flow(
    provider: &str,
    provider_impl: &OAuthProviderEnum,
) -> Result<(), ApiError> {
    let flow = provider_impl.flow();
    if flow != openproxy_core::oauth::OAuthFlow::AuthorizationCodePkce
        && flow != openproxy_core::oauth::OAuthFlow::AuthorizationCode
    {
        return Err(ApiError(CoreError::Validation(format!(
            "provider '{provider}' does not support authorization code flow"
        ))));
    }
    Ok(())
}

pub async fn oauth_authorize(
    State(s): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let registry = s.oauth_provider_registry();
    let provider_impl = registry.get(&provider).ok_or_else(|| {
        ApiError(CoreError::Validation(format!(
            "provider '{provider}' does not support OAuth authorize"
        )))
    })?;

    validate_authorize_flow(&provider, &provider_impl)?;
    let redirect_uri = get_oauth_redirect_uri(&s);
    let (auth_url, code_verifier, effective_redirect_uri, state) =
        provider_impl.build_auth_url(&redirect_uri).await?;
    let redirect_uri = if effective_redirect_uri.is_empty() {
        redirect_uri
    } else {
        effective_redirect_uri
    };

    // Security (OP-19): persist the generated state with the verifier and
    // redirect URI actually used, so the exchange can validate `state` and
    // stop trusting client-supplied redirect_uri / code_verifier.
    s.oauth_states()
        .insert(&provider, &state, &code_verifier, &redirect_uri);

    // Security (audit H-1): the zai provider used to ALSO bind one of four
    // fixed loopback ports here (first-bind-wins) so a local process could
    // squat them and capture the browser redirect. The loopback listener now
    // lives where the callback URI is actually negotiated with the upstream
    // — `verify-captcha`, on an ephemeral port — see `zai_loopback`.

    Ok(Json(serde_json::json!({
        "authorization_url": auth_url,
        "code_verifier": code_verifier,
        "redirect_uri": redirect_uri,
        "state": state,
    })))
}
