use super::{ClientSpoofer, build_spoofed_headers, define_spoofer_state};

pub const DEFAULT_KIRO_UA: &str = "aws-sdk-js/3.0.0 kiro/0.1";
pub const DEFAULT_KIRO_SDK_REQUEST: &str = "attempt=1; max=3";
pub const DEFAULT_KIRO_BEDROCK_CACHE_CONTROL: &str = "enable";
pub const DEFAULT_KIRO_ANTHROPIC_BETA: &str = "prompt-caching-2024-07-31";

pub const KIRO_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("Content-Type", "application/json"),
    ("x-amz-user-agent", DEFAULT_KIRO_UA),
    ("Amz-Sdk-Request", DEFAULT_KIRO_SDK_REQUEST),
    (
        "x-amzn-bedrock-cache-control",
        DEFAULT_KIRO_BEDROCK_CACHE_CONTROL,
    ),
    ("anthropic-beta", DEFAULT_KIRO_ANTHROPIC_BETA),
];

define_spoofer_state! {
    overrides: KIRO_OVERRIDES,
    extra_headers: KIRO_EXTRA_HEADERS = "OPENPROXY_KIRO_EXTRA_HEADERS",
    test_lock: KIRO_TEST_LOCK,
    ua: {
        env: "OPENPROXY_KIRO_USER_AGENT",
        default: DEFAULT_KIRO_UA,
        getter: current_kiro_ua,
        setter: set_dynamic_kiro_ua,
        doc: "Current dynamic User-Agent of Kiro / AWS CodeWhisperer.",
    },
    extra_header_setter: set_dynamic_kiro_extra_header,
    reset: reset_dynamic_kiro_overrides,
}

/// Preset for Kiro AI (AWS CodeWhisperer) client identity headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct KiroSpoofer;

impl ClientSpoofer for KiroSpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let cur_ua = current_kiro_ua();
        let mut list = build_spoofed_headers(
            KIRO_SPOOFING_HEADERS,
            &KIRO_EXTRA_HEADERS,
            &KIRO_OVERRIDES,
            |k, v| {
                if k == "x-amz-user-agent" {
                    (k.to_string(), cur_ua.clone())
                } else {
                    (k.to_string(), v.to_string())
                }
            },
        );

        // Dynamically inject unique invocation ID per request
        list.push((
            "Amz-Sdk-Invocation-Id".to_string(),
            uuid::Uuid::new_v4().to_string(),
        ));

        list
    }
}
