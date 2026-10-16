//! Integration check loading the captured profile through the production path.
use openproxy_adapters::adapters::claude_code::{cloaking, signing};
use openproxy_adapters::spoofer::claude_code_profile::{current_profile, parse_profile};

#[test]
fn captured_profile_matches_final_body_and_betas() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(path) = std::env::var("OPENPROXY_CLAUDE_CODE_PROFILE_PATH") else {
        return Ok(());
    };
    let fixture = parse_profile(&std::fs::read(path)?)?;
    let loaded = current_profile();
    assert_eq!(loaded.cli_version, fixture.cli_version);
    assert_eq!(loaded.package_version, fixture.package_version);
    assert_eq!(loaded.betas, fixture.betas);
    assert_eq!(loaded.request_defaults, fixture.request_defaults);
    let mut body = serde_json::json!({
        "model": "claude-opus-5-5", "max_tokens": 128000,
        "messages": [{"role":"user", "content":"fixture"}],
        "tools": [{"name":"ReadFile", "description":"Read", "input_schema":{"type":"object"}}]
    });
    cloaking::cloak_claude_request_body(
        &mut body,
        "claude-opus-5-5",
        "account",
        "session",
        "device",
    );
    for key in ["thinking", "output_config", "context_management"] {
        assert_eq!(body[key], fixture.request_defaults[key]);
    }
    assert_eq!(body["max_tokens"], 128000);
    assert_eq!(
        openproxy_adapters::anthropic_cache::count_cache_controls(&body),
        3
    );
    for cc in [
        &body["system"][1]["cache_control"],
        &body["tools"][0]["cache_control"],
        &body["messages"][0]["content"][0]["cache_control"],
    ] {
        assert_eq!(cc["ttl"], fixture.cache_ttl);
    }
    let betas = cloaking::build_claude_code_betas_for_body("claude-opus-5-5", &body, true);
    for beta in &fixture.betas {
        assert!(betas.split(',').any(|b| b == beta));
    }
    assert!(
        betas
            .split(',')
            .any(|b| b == cloaking::BETA_EXTENDED_CACHE_TTL)
    );
    assert!(
        !betas
            .split(',')
            .any(|b| b == cloaking::BETA_REDACT_THINKING)
    );
    let mut bytes = serde_json::to_vec(&body)?;
    signing::sign_anthropic_messages_body(&mut bytes)?;
    let signed = bytes.clone();
    signing::sign_anthropic_messages_body(&mut bytes)?;
    assert_eq!(bytes, signed);
    Ok(())
}
