use super::{ClientSpoofer, build_spoofed_headers, define_spoofer_state};

pub const CLINE_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("http-referer", "https://cline.bot"),
    ("x-title", "Cline"),
    ("user-agent", "Cline/4.1.3"),
    ("x-is-multiroot", "false"),
    ("x-client-type", "VSCode Extension"),
    ("x-client-version", "4.1.3"),
    ("x-platform", "Visual Studio Code"),
    ("x-platform-version", "1.96.0"),
    ("x-core-version", "4.1.3"),
];

pub const DEFAULT_CLINE_VERSION: &str = "4.1.3";

define_spoofer_state! {
    overrides: CLINE_OVERRIDES,
    extra_headers: CLINE_EXTRA_HEADERS = "OPENPROXY_CLINE_EXTRA_HEADERS",
    test_lock: CLINE_TEST_LOCK,
    version: {
        env: "OPENPROXY_CLINE_VERSION",
        default: DEFAULT_CLINE_VERSION,
        getter: current_cline_version,
        setter: set_dynamic_cline_version,
        doc: "Current dynamic version of Cline.",
    },
    ua: {
        getter: current_cline_ua,
        format: |ver| format!("Cline/{ver}"),
        doc: "Dynamic Cline User-Agent.",
    },
    extra_header_setter: set_dynamic_cline_extra_header,
    reset: reset_dynamic_cline_overrides,
}

/// Preset for Cline client identity headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClineSpoofer;

impl ClientSpoofer for ClineSpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let cur_ver = current_cline_version();
        let cur_ua = current_cline_ua();
        build_spoofed_headers(
            CLINE_SPOOFING_HEADERS,
            &CLINE_EXTRA_HEADERS,
            &CLINE_OVERRIDES,
            |k, v| {
                if k == "user-agent" {
                    (k.to_string(), cur_ua.clone())
                } else if k == "x-client-version" || k == "x-core-version" {
                    (k.to_string(), cur_ver.clone())
                } else {
                    (k.to_string(), v.to_string())
                }
            },
        )
    }
}
