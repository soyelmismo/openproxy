use super::{ClientSpoofer, build_spoofed_headers, define_spoofer_state};

pub const DEFAULT_COMMANDCODE_CLI_VERSION: &str = "1.54.0";
pub const DEFAULT_COMMANDCODE_CLI_ENVIRONMENT: &str = "production";
pub const DEFAULT_COMMANDCODE_PROJECT_SLUG: &str = "project";
pub const DEFAULT_COMMANDCODE_TASTE_LEARNING: &str = "true";
pub const DEFAULT_COMMANDCODE_UA: &str = "cli";

pub const COMMANDCODE_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("Content-Type", "application/json"),
    ("user-agent", DEFAULT_COMMANDCODE_UA),
    ("x-command-code-version", DEFAULT_COMMANDCODE_CLI_VERSION),
    ("x-cli-environment", DEFAULT_COMMANDCODE_CLI_ENVIRONMENT),
    ("x-project-slug", DEFAULT_COMMANDCODE_PROJECT_SLUG),
    ("x-taste-learning", DEFAULT_COMMANDCODE_TASTE_LEARNING),
];

define_spoofer_state! {
    overrides: COMMANDCODE_OVERRIDES,
    extra_headers: COMMANDCODE_EXTRA_HEADERS = "OPENPROXY_COMMANDCODE_EXTRA_HEADERS",
    test_lock: COMMANDCODE_TEST_LOCK,
    version: {
        env: "OPENPROXY_COMMANDCODE_CLI_VERSION",
        default: DEFAULT_COMMANDCODE_CLI_VERSION,
        getter: current_commandcode_version,
        setter: set_dynamic_commandcode_version,
        doc: "Current dynamic version of Command Code CLI.",
    },
    ua: {
        env: "OPENPROXY_COMMANDCODE_USER_AGENT",
        default: DEFAULT_COMMANDCODE_UA,
        header_key: "user-agent",
        getter: current_commandcode_ua,
        setter: set_dynamic_commandcode_ua,
        doc: "Dynamic Command Code User-Agent string.",
    },
    extra_header_setter: set_dynamic_commandcode_extra_header,
    reset: reset_dynamic_commandcode_overrides,
}

/// Preset for Command Code client identity headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct CommandCodeSpoofer;

impl ClientSpoofer for CommandCodeSpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let cur_ver = current_commandcode_version();
        let cur_ua = current_commandcode_ua();
        let mut list = build_spoofed_headers(
            COMMANDCODE_SPOOFING_HEADERS,
            &COMMANDCODE_EXTRA_HEADERS,
            &COMMANDCODE_OVERRIDES,
            |k, v| {
                if k == "x-command-code-version" {
                    (k.to_string(), cur_ver.clone())
                } else if k == "user-agent" {
                    (k.to_string(), cur_ua.clone())
                } else {
                    (k.to_string(), v.to_string())
                }
            },
        );

        if !list
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("x-session-id"))
        {
            let session_id = uuid::Uuid::new_v4().to_string();
            list.push(("x-session-id".to_string(), session_id.clone()));
            list.push(("x-session-affinity".to_string(), session_id));
        }

        list
    }
}
