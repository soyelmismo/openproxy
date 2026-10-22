use super::{ClientSpoofer, build_spoofed_headers, define_spoofer_state};

pub const DEFAULT_KILOCODE_VERSION: &str = "4.108.0";

pub const KILOCODE_SPOOFING_HEADERS: &[(&str, &str)] = &[
    ("http-referer", "https://kilocode.ai"),
    ("x-title", "Kilo Code"),
    ("user-agent", "Kilo-Code/4.108.0"),
    ("x-kilocode-version", "4.108.0"),
    ("x-client-type", "VSCode Extension"),
    ("x-client-version", "4.108.0"),
];

define_spoofer_state! {
    overrides: KILOCODE_OVERRIDES,
    extra_headers: KILOCODE_EXTRA_HEADERS = "OPENPROXY_KILOCODE_EXTRA_HEADERS",
    test_lock: KILOCODE_TEST_LOCK,
    version: {
        env: "OPENPROXY_KILOCODE_VERSION",
        default: DEFAULT_KILOCODE_VERSION,
        getter: current_kilocode_version,
        setter: set_dynamic_kilocode_version,
        doc: "Current dynamic version of Kilocode.",
    },
    ua: {
        getter: current_kilocode_ua,
        format: |ver| format!("Kilo-Code/{ver}"),
        doc: "Dynamic Kilocode User-Agent.",
    },
    extra_header_setter: set_dynamic_kilocode_extra_header,
    reset: reset_dynamic_kilocode_overrides,
}

/// Preset for Kilocode client identity headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct KilocodeSpoofer;

impl ClientSpoofer for KilocodeSpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let cur_ver = current_kilocode_version();
        let cur_ua = current_kilocode_ua();
        build_spoofed_headers(
            KILOCODE_SPOOFING_HEADERS,
            &KILOCODE_EXTRA_HEADERS,
            &KILOCODE_OVERRIDES,
            |k, v| {
                if k == "user-agent" {
                    (k.to_string(), cur_ua.clone())
                } else if k == "x-kilocode-version" || k == "x-client-version" {
                    (k.to_string(), cur_ver.clone())
                } else {
                    (k.to_string(), v.to_string())
                }
            },
        )
    }
}
