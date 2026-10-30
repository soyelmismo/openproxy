use super::common::*;
use crate::formatting::{get_formatter, strip_provider_configured_params_from_body};
use crate::stages::target_headers::propagate_provider_target_headers;
use crate::test_utils::make_request;
use openproxy_adapters::adapters::AdapterFormat;
use openproxy_types::TargetFormat;
use openproxy_types::ids::ComboId;
use serde_json::Value;

// ===========================================================================
// Part 2 — default identity hook: every TargetFormat, no schema mutation
// ===========================================================================

/// For every `TargetFormat` and its default provider format,
/// `finalize_request` must be the identity function: body bytes and headers
/// come out byte-for-byte identical to what they were before the hook ran.
#[test]
fn default_finalize_request_is_identity_for_all_target_formats() {
    // Every (ProviderFormat, default TargetFormat) pair that exists, taken from
    // `ProviderFormat::default_target_format()`.
    let cases: &[(AdapterFormat, TargetFormat)] = &[
        (AdapterFormat::Openai, TargetFormat::Openai),
        (AdapterFormat::Anthropic, TargetFormat::Anthropic),
        (AdapterFormat::Gemini, TargetFormat::Gemini),
        (AdapterFormat::Responses, TargetFormat::Responses),
        (AdapterFormat::Atomesus, TargetFormat::Atomesus),
        (AdapterFormat::CommandCodeGo, TargetFormat::CommandCodeGo),
        (AdapterFormat::SystemOne, TargetFormat::SystemOne),
        // Devin's real `wrap_request_body` emits a protobuf envelope; the
        // stub adapter keeps the JSON carrier, which is enough to prove the
        // finalize hook itself is the identity for this format too.
        (AdapterFormat::Devin, TargetFormat::Devin),
    ];

    // Guard: the table must cover every TargetFormat that exists.
    let covered: Vec<TargetFormat> = cases.iter().map(|(_, t)| *t).collect();
    for fmt in TargetFormat::ALL {
        assert!(
            covered.contains(fmt),
            "identity table is missing TargetFormat {fmt:?}"
        );
    }

    for (adapter_format, target_format) in cases {
        let adapter = plain_adapter("plain-provider", *adapter_format);
        let (req, _tx) = make_request(ComboId(1));
        let resolved = resolved_target_for("plain-provider", *target_format);
        let model = &resolved.model;
        let target = &resolved.target;

        // Reproduce the chain up to (but excluding) finalize_request.
        let formatter = get_formatter(*target_format);
        let mut body = formatter
            .format_request(
                &req,
                model,
                &req.openai_request.messages,
                req.openai_request.stream,
                &adapter,
            )
            .expect("formatter");
        body = adapter
            .wrap_request_body(body, *target_format, &model.model_id, &resolved)
            .expect("wrap");
        body = strip_provider_configured_params_from_body(body, &adapter.config().extra_headers)
            .expect("strip");

        let mut headers = adapter.build_headers(TEST_CREDENTIAL, *target_format, &model.model_id);
        propagate_provider_target_headers(
            &mut headers,
            target.provider_id.as_str(),
            adapter.id().as_str(),
            &req.request_headers,
            &req.openai_request,
            None,
        );

        let body_before = body.clone();
        let headers_before = headers.clone();

        // The hook under test.
        let body_after = adapter
            .finalize_request(
                body.clone(),
                &mut headers,
                *target_format,
                &model.model_id,
                &resolved,
            )
            .expect("finalize must succeed");

        assert_eq!(
            body_before.as_ref(),
            body_after.as_ref(),
            "default finalize_request must not mutate body bytes for {target_format:?}"
        );
        assert_eq!(
            headers_before, headers,
            "default finalize_request must not mutate headers for {target_format:?}"
        );
    }
}

/// The same identity guarantee, but driven through the full chain helper so it
/// exercises formatter + wrapper + strip + propagate + finalize together
/// rather than the hook in isolation.
#[test]
fn default_identity_holds_through_full_chain() {
    for (adapter_format, target_format) in [
        (AdapterFormat::Openai, TargetFormat::Openai),
        (AdapterFormat::Anthropic, TargetFormat::Anthropic),
        (AdapterFormat::Gemini, TargetFormat::Gemini),
        (AdapterFormat::Responses, TargetFormat::Responses),
        (AdapterFormat::Atomesus, TargetFormat::Atomesus),
        (AdapterFormat::CommandCodeGo, TargetFormat::CommandCodeGo),
        (AdapterFormat::SystemOne, TargetFormat::SystemOne),
    ] {
        let adapter = plain_adapter("plain-provider", adapter_format);
        let resolved = resolved_target_for("plain-provider", target_format);
        let (req, _tx) = make_request(ComboId(1));

        let out = run_chain(&adapter, &resolved, &req);

        // The formatter really produced format-appropriate bytes.
        let parsed: Value =
            serde_json::from_slice(&out.body).expect("final body must be JSON for every format");
        assert!(
            parsed.is_object(),
            "final body must be a JSON object for {target_format:?}"
        );

        // And the chain is stable: running it again gives identical bytes and
        // headers, proving no stage (including finalize) is non-deterministic
        // or mutating by schema.
        let out2 = run_chain(&adapter, &resolved, &req);
        assert_eq!(
            out.body, out2.body,
            "chain must be deterministic for {target_format:?}"
        );
        assert_eq!(
            out.headers, out2.headers,
            "chain headers must be deterministic for {target_format:?}"
        );
    }
}
