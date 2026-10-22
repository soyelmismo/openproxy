use super::{DynamicHeaderOverrides, merge_header_refs};

/// Macro to generate the standard static `DynamicHeaderOverrides`, env-backed extra headers lazy lock,
/// test lock, and dynamic getter/setter functions for a spoofer.
#[macro_export]
macro_rules! define_spoofer_state {
    // 1. Version-centric spoofer with custom UA formatting (e.g., Cline, Kilocode):
    (
        overrides: $overrides:ident,
        extra_headers: $extra_headers:ident = $extra_env:literal,
        $(test_lock: $test_lock:ident,)?
        version: {
            env: $ver_env:literal,
            default: $ver_default:expr,
            getter: $ver_getter:ident,
            setter: $ver_setter:ident,
            doc: $ver_doc:literal $(,)?
        },
        ua: {
            getter: $ua_getter:ident,
            format: |$ver_param:ident| $ua_expr:expr,
            doc: $ua_doc:literal $(,)?
        },
        extra_header_setter: $extra_setter:ident,
        reset: $reset_fn:ident $(,)?
    ) => {
        static $overrides: $crate::spoofer::DynamicHeaderOverrides =
            $crate::spoofer::DynamicHeaderOverrides::new();

        #[doc = $ver_doc]
        pub fn $ver_getter() -> String {
            $overrides.current_version($ver_env, $ver_default)
        }

        #[doc = $ua_doc]
        pub fn $ua_getter() -> String {
            let $ver_param = $ver_getter();
            $ua_expr
        }

        #[doc = concat!("Set dynamic version override in memory at runtime without recompiling.")]
        pub fn $ver_setter(ver: impl Into<String>) {
            $overrides.set_version(ver);
        }

        #[doc = concat!("Set dynamic extra header override in memory at runtime without recompiling.")]
        pub fn $extra_setter(key: impl Into<String>, val: impl Into<String>) {
            $overrides.set_extra_header(key, val);
        }

        #[doc = concat!("Reset dynamic in-memory overrides (useful for tests and cleanup).")]
        pub fn $reset_fn() {
            $overrides.reset();
        }

        $(
            #[cfg(any(test, feature = "test-utils"))]
            pub static $test_lock: std::sync::Mutex<()> = std::sync::Mutex::new(());
        )?

        static $extra_headers: std::sync::LazyLock<Vec<(String, String)>> =
            std::sync::LazyLock::new(|| $crate::spoofer::parse_env_extra_headers($extra_env));
    };

    // 2. UA-only spoofer (e.g. Kiro):
    (
        overrides: $overrides:ident,
        extra_headers: $extra_headers:ident = $extra_env:literal,
        $(test_lock: $test_lock:ident,)?
        ua: {
            env: $ua_env:literal,
            default: $ua_default:expr,
            getter: $ua_getter:ident,
            setter: $ua_setter:ident,
            doc: $ua_doc:literal $(,)?
        },
        extra_header_setter: $extra_setter:ident,
        reset: $reset_fn:ident $(,)?
    ) => {
        static $overrides: $crate::spoofer::DynamicHeaderOverrides =
            $crate::spoofer::DynamicHeaderOverrides::new();

        #[doc = $ua_doc]
        pub fn $ua_getter() -> String {
            $overrides.current_version($ua_env, $ua_default)
        }

        #[doc = concat!("Set dynamic User-Agent override in memory at runtime without recompiling.")]
        pub fn $ua_setter(ua: impl Into<String>) {
            $overrides.set_version(ua);
        }

        #[doc = concat!("Set dynamic extra header override in memory at runtime without recompiling.")]
        pub fn $extra_setter(key: impl Into<String>, val: impl Into<String>) {
            $overrides.set_extra_header(key, val);
        }

        #[doc = concat!("Reset dynamic in-memory overrides (useful for tests and cleanup).")]
        pub fn $reset_fn() {
            $overrides.reset();
        }

        $(
            #[cfg(any(test, feature = "test-utils"))]
            pub static $test_lock: std::sync::Mutex<()> = std::sync::Mutex::new(());
        )?

        static $extra_headers: std::sync::LazyLock<Vec<(String, String)>> =
            std::sync::LazyLock::new(|| $crate::spoofer::parse_env_extra_headers($extra_env));
    };

    // 3. Spoofer with version and separate UA with fallback to env or default (e.g. CommandCode):
    (
        overrides: $overrides:ident,
        extra_headers: $extra_headers:ident = $extra_env:literal,
        $(test_lock: $test_lock:ident,)?
        version: {
            env: $ver_env:literal,
            default: $ver_default:expr,
            getter: $ver_getter:ident,
            setter: $ver_setter:ident,
            doc: $ver_doc:literal $(,)?
        },
        ua: {
            env: $ua_env:literal,
            default: $ua_default:expr,
            header_key: $ua_header_key:literal,
            getter: $ua_getter:ident,
            setter: $ua_setter:ident,
            doc: $ua_doc:literal $(,)?
        },
        extra_header_setter: $extra_setter:ident,
        reset: $reset_fn:ident $(,)?
    ) => {
        static $overrides: $crate::spoofer::DynamicHeaderOverrides =
            $crate::spoofer::DynamicHeaderOverrides::new();

        #[doc = $ver_doc]
        pub fn $ver_getter() -> String {
            $overrides.current_version($ver_env, $ver_default)
        }

        #[doc = $ua_doc]
        pub fn $ua_getter() -> String {
            if let Some(ua) = $overrides.current_extra_header($ua_header_key) {
                return ua;
            }
            std::env::var($ua_env)
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| $ua_default.to_string())
        }

        #[doc = concat!("Set dynamic version override in memory at runtime without recompiling.")]
        pub fn $ver_setter(ver: impl Into<String>) {
            $overrides.set_version(ver);
        }

        #[doc = concat!("Set dynamic User-Agent override in memory at runtime without recompiling.")]
        pub fn $ua_setter(ua: impl Into<String>) {
            $overrides.set_extra_header($ua_header_key, ua);
        }

        #[doc = concat!("Set dynamic extra header override in memory at runtime without recompiling.")]
        pub fn $extra_setter(key: impl Into<String>, val: impl Into<String>) {
            $overrides.set_extra_header(key, val);
        }

        #[doc = concat!("Reset dynamic in-memory overrides (useful for tests and cleanup).")]
        pub fn $reset_fn() {
            $overrides.reset();
        }

        $(
            #[cfg(any(test, feature = "test-utils"))]
            pub static $test_lock: std::sync::Mutex<()> = std::sync::Mutex::new(());
        )?

        static $extra_headers: std::sync::LazyLock<Vec<(String, String)>> =
            std::sync::LazyLock::new(|| $crate::spoofer::parse_env_extra_headers($extra_env));
    };

    // 4. UA + secondary version header (e.g. MiniMax):
    (
        overrides: $overrides:ident,
        extra_headers: $extra_headers:ident = $extra_env:literal,
        $(test_lock: $test_lock:ident,)?
        ua: {
            env: $ua_env:literal,
            default: $ua_default:expr,
            getter: $ua_getter:ident,
            setter: $ua_setter:ident,
            doc: $ua_doc:literal $(,)?
        },
        secondary_version: {
            header_name: $sec_hdr:literal,
            env: $sec_env:literal,
            default: $sec_default:expr,
            getter: $sec_getter:ident,
            setter: $sec_setter:ident,
            doc: $sec_doc:literal $(,)?
        },
        extra_header_setter: $extra_setter:ident,
        reset: $reset_fn:ident $(,)?
    ) => {
        static $overrides: $crate::spoofer::DynamicHeaderOverrides =
            $crate::spoofer::DynamicHeaderOverrides::new();

        #[doc = $ua_doc]
        pub fn $ua_getter() -> String {
            $overrides.current_version($ua_env, $ua_default)
        }

        #[doc = $sec_doc]
        pub fn $sec_getter() -> String {
            if let Some(v) = $overrides.current_extra_header($sec_hdr) {
                return v;
            }
            std::env::var($sec_env)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| $sec_default.to_string())
        }

        #[doc = concat!("Set dynamic User-Agent override in memory at runtime without recompiling.")]
        pub fn $ua_setter(ua: impl Into<String>) {
            $overrides.set_version(ua);
        }

        #[doc = concat!("Set dynamic secondary version override in memory at runtime without recompiling.")]
        pub fn $sec_setter(ver: impl Into<String>) {
            $overrides.set_extra_header($sec_hdr, ver);
        }

        #[doc = concat!("Set dynamic extra header override in memory at runtime without recompiling.")]
        pub fn $extra_setter(key: impl Into<String>, val: impl Into<String>) {
            $overrides.set_extra_header(key, val);
        }

        #[doc = concat!("Reset dynamic in-memory overrides (useful for tests and cleanup).")]
        pub fn $reset_fn() {
            $overrides.reset();
        }

        $(
            #[cfg(any(test, feature = "test-utils"))]
            pub static $test_lock: std::sync::Mutex<()> = std::sync::Mutex::new(());
        )?

        static $extra_headers: std::sync::LazyLock<Vec<(String, String)>> =
            std::sync::LazyLock::new(|| $crate::spoofer::parse_env_extra_headers($extra_env));
    };
}

pub(crate) use define_spoofer_state;

/// Build spoofing headers list by transforming static base headers, appending extra headers from env and dynamic overrides.
pub fn build_spoofed_headers<F>(
    base_headers: &[(&str, &str)],
    extra_headers: &[(String, String)],
    overrides: &DynamicHeaderOverrides,
    mut transform: F,
) -> Vec<(String, String)>
where
    F: FnMut(&str, &str) -> (String, String),
{
    let mut list: Vec<(String, String)> =
        base_headers.iter().map(|(k, v)| transform(k, v)).collect();

    merge_header_refs(&mut list, extra_headers);
    overrides.apply_to_list(&mut list);
    list
}
