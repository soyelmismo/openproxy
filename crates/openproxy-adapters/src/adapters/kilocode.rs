use super::{
    AdapterAuthType, AdapterFormat, ModelId, ProviderAdapter, ProviderAdapterConfig, ProviderId,
    TargetFormat, UpstreamRequest,
};
pub use crate::spoofer::KILOCODE_SPOOFING_HEADERS;
use crate::spoofer::{ClientSpoofer, KilocodeSpoofer};

pub fn apply_kilocode_spoofing_headers(req: &mut UpstreamRequest) {
    KilocodeSpoofer.apply_to_request(req);
}

/// Adapter for <https://api.kilo.ai/api/openrouter>.
///
/// Kilocode is an OpenRouter gateway with its own auth. Chat goes through
/// `/v1/chat/completions` but models are listed at `/models` (not
/// `/v1/models`), so [`models_url`] overrides the default.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct KilocodeAdapter {
    config: ProviderAdapterConfig,
}

impl KilocodeAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("kilocode"),
                name: "Kilocode".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: "https://api.kilo.ai/api/openrouter/v1".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: vec![],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(KilocodeAdapter);

impl ProviderAdapter for KilocodeAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn config_mut(&mut self) -> Option<&mut ProviderAdapterConfig> {
        Some(&mut self.config)
    }

    fn models_url(&self) -> Option<String> {
        Some("https://api.kilo.ai/api/openrouter/models".into())
    }

    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        &["kilocode"]
    }

    fn build_headers(
        &self,
        access_token: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        super::build_spoofer_headers(
            self.build_auth_header(access_token),
            &KilocodeSpoofer,
            &self.config.extra_headers,
        )
    }

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        let mut meta = openproxy_types::ProviderMetadata::custom_default();
        meta.built_in = true;
        meta.deletable = false;
        meta.supports_quota = false;
        meta.quota_refresh_supported = false;
        meta
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spoofer::{
        KILOCODE_TEST_LOCK, current_kilocode_ua, current_kilocode_version,
        reset_dynamic_kilocode_overrides, set_dynamic_kilocode_extra_header,
    };
    use http::HeaderValue;

    #[test]
    fn test_apply_kilocode_spoofing_headers() {
        let mut req = UpstreamRequest::post_json("http://dummy.com", bytes::Bytes::new());
        apply_kilocode_spoofing_headers(&mut req);

        for &(k, v) in KILOCODE_SPOOFING_HEADERS {
            let header_value = req.headers.get(k).expect("header missing");
            if k == "user-agent" {
                assert_eq!(header_value, current_kilocode_ua().as_str());
            } else if k == "x-kilocode-version" || k == "x-client-version" {
                assert_eq!(header_value, current_kilocode_version().as_str());
            } else {
                assert_eq!(header_value, HeaderValue::from_str(v).unwrap());
            }
        }
    }

    #[test]
    fn test_kilocode_build_headers_with_extra_and_dynamic_overrides() {
        let _guard = KILOCODE_TEST_LOCK.lock().unwrap();
        reset_dynamic_kilocode_overrides();

        let mut adapter = KilocodeAdapter::new();
        adapter.config_mut().unwrap().extra_headers = vec![
            ("x-custom-org".to_string(), "kilo-org".to_string()),
            ("user-agent".to_string(), "CustomKiloUA/1.0".to_string()),
        ];

        set_dynamic_kilocode_extra_header("x-dynamic-header", "dynamic-val");

        let headers = adapter.build_headers("my-key", TargetFormat::Openai, &ModelId::new("any"));

        let find = |k: &str| {
            headers
                .iter()
                .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("Authorization"), Some("Bearer my-key"));
        assert_eq!(find("Content-Type"), Some("application/json"));
        assert_eq!(find("x-custom-org"), Some("kilo-org"));
        assert_eq!(find("user-agent"), Some("CustomKiloUA/1.0"));
        assert_eq!(find("x-dynamic-header"), Some("dynamic-val"));
        assert_eq!(find("x-title"), Some("Kilo Code"));

        reset_dynamic_kilocode_overrides();
    }

    #[test]
    fn test_kilocode_default_headers_contract() {
        let _guard = KILOCODE_TEST_LOCK.lock().unwrap();
        reset_dynamic_kilocode_overrides();

        let adapter = KilocodeAdapter::new();
        let headers =
            adapter.build_headers("kl-key-123", TargetFormat::Openai, &ModelId::new("any"));
        let find = |k: &str| {
            headers
                .iter()
                .find(|(hk, _)| hk.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        // Strict bijective closed-world set equality contract
        let actual_keys: std::collections::BTreeSet<String> = headers
            .iter()
            .map(|(k, _)| k.to_ascii_lowercase())
            .collect();
        let expected_keys: std::collections::BTreeSet<String> = [
            "authorization",
            "content-type",
            "http-referer",
            "x-title",
            "user-agent",
            "x-kilocode-version",
            "x-client-type",
            "x-client-version",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        assert_eq!(
            actual_keys, expected_keys,
            "Kilocode contract breach: header added or removed"
        );

        assert_eq!(find("authorization"), Some("Bearer kl-key-123"));
        assert_eq!(find("content-type"), Some("application/json"));
        assert_eq!(find("http-referer"), Some("https://kilocode.ai"));
        assert_eq!(find("x-title"), Some("Kilo Code"));
        assert_eq!(find("user-agent"), Some(current_kilocode_ua().as_str()));
        assert_eq!(
            find("x-kilocode-version"),
            Some(current_kilocode_version().as_str())
        );
        assert_eq!(find("x-client-type"), Some("VSCode Extension"));
        assert_eq!(
            find("x-client-version"),
            Some(current_kilocode_version().as_str())
        );
    }
}
