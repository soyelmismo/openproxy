use super::ClientSpoofer;

/// Preset for Google Antigravity (Cloud Code) client identity headers.
#[derive(Debug, Clone, Default)]
pub struct AntigravitySpoofer {
    pub project_id: Option<String>,
}

impl AntigravitySpoofer {
    pub fn new() -> Self {
        Self { project_id: None }
    }

    pub fn with_project(project_id: impl Into<String>) -> Self {
        Self {
            project_id: Some(project_id.into()),
        }
    }
}

impl ClientSpoofer for AntigravitySpoofer {
    fn headers(&self) -> Vec<(String, String)> {
        let mut hm = http::HeaderMap::new();
        self.apply_to_header_map(&mut hm);
        hm.into_iter()
            .filter_map(|(k, v)| {
                k.map(|name| {
                    (
                        name.as_str().to_string(),
                        v.to_str().unwrap_or("").to_string(),
                    )
                })
            })
            .collect()
    }

    fn apply_to_header_map(&self, headers: &mut http::HeaderMap) {
        crate::antigravity_headers::inject_antigravity_headers(headers, self.project_id.as_deref());
    }
}
