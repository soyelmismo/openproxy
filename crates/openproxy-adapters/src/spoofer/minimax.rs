use super::{ClientSpoofer, build_spoofed_headers, define_spoofer_state};

pub const DEFAULT_MINIMAX_UA: &str = "MiniMaxAgent";
pub const DEFAULT_MINIMAX_ANTHROPIC_VERSION: &str = "2023-06-01";

pub const MINIMAX_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("Content-Type", "application/json"),
    ("User-Agent", "MiniMaxAgent"),
    ("Anthropic-Version", "2023-06-01"),
    ("X-Mavis-Agent-Id", "main"),
    ("X-Mavis-Timezone-Offset", "0"),
];

define_spoofer_state! {
    overrides: MINIMAX_OVERRIDES,
    extra_headers: MINIMAX_EXTRA_HEADERS = "OPENPROXY_MINIMAX_EXTRA_HEADERS",
    test_lock: MINIMAX_TEST_LOCK,
    ua: {
        env: "OPENPROXY_MINIMAX_USER_AGENT",
        default: DEFAULT_MINIMAX_UA,
        getter: current_minimax_ua,
        setter: set_dynamic_minimax_ua,
        doc: "Current dynamic User-Agent of MiniMax.",
    },
    secondary_version: {
        header_name: "Anthropic-Version",
        env: "OPENPROXY_MINIMAX_ANTHROPIC_VERSION",
        default: DEFAULT_MINIMAX_ANTHROPIC_VERSION,
        getter: current_minimax_anthropic_version,
        setter: set_dynamic_minimax_anthropic_version,
        doc: "Current dynamic Anthropic-Version of MiniMax.",
    },
    extra_header_setter: set_dynamic_minimax_extra_header,
    reset: reset_dynamic_minimax_overrides,
}

/// Preset for MiniMax Coding client identity headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct MiniMaxSpoofer;

impl ClientSpoofer for MiniMaxSpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let cur_ua = current_minimax_ua();
        let cur_ver = current_minimax_anthropic_version();
        let mut list = build_spoofed_headers(
            MINIMAX_SPOOFING_HEADERS,
            &MINIMAX_EXTRA_HEADERS,
            &MINIMAX_OVERRIDES,
            |k, v| {
                if k == "User-Agent" {
                    (k.to_string(), cur_ua.clone())
                } else if k == "Anthropic-Version" {
                    (k.to_string(), cur_ver.clone())
                } else {
                    (k.to_string(), v.to_string())
                }
            },
        );

        list.push((
            "X-Mavis-Session-Id".to_string(),
            format!("session_{}", uuid::Uuid::new_v4().simple()),
        ));

        list
    }
}
