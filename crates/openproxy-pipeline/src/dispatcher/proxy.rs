//! Resolución de proxy: status + asignación por provider/cuenta. Único
//! submódulo que NO accede a `self.conn` directamente — toda la BD se
//! consulta vía `tracker.repo` (que es un `Arc<dyn PipelineRepository>` y
//! ya gestiona el `conn.lock()` internamente).

use super::UpstreamDispatcher;

impl UpstreamDispatcher {
    /// Devuelve el status (`"alive"`, `"dead"`, etc.) del proxy de la URL
    /// dada, o `None` si no hay match. La consulta se hace en
    /// `spawn_blocking` porque `repo.get_proxy_status_by_url` toma el lock
    /// del `Mutex<Connection>` síncronamente.
    pub(super) async fn fetch_proxy_status(&self, proxy_url: Option<&str>) -> Option<String> {
        let url = proxy_url?.to_string();
        let repo = std::sync::Arc::clone(&self.tracker.repo);
        tokio::task::spawn_blocking(move || repo.get_proxy_status_by_url(&url))
            .await
            .unwrap_or(None)
    }

    /// Resuelve la URL del proxy a usar:
    /// - Si el request trae `proxy_override`, se respeta.
    /// - Si no, se consulta `repo.get_or_assign_provider_proxy` (asigna
    ///   uno nuevo si el provider no tiene).
    ///
    /// Devuelve `(proxy_url, proxy_status)`.
    pub(super) async fn resolve_and_assign_proxy(
        &self,
        req: &crate::PipelineRequest,
        target: &openproxy_types::combos::ComboTarget,
    ) -> Result<(Option<String>, Option<String>), openproxy_types::error::CoreError> {
        let proxy_url = if let Some((_, ref purl)) = req.proxy_override {
            Some(purl.clone())
        } else {
            let repo = std::sync::Arc::clone(&self.tracker.repo);
            let provider_id = target.provider_id.clone();
            let account_id = target.account_id;
            tokio::task::spawn_blocking(move || {
                repo.get_or_assign_provider_proxy(&provider_id, account_id)
            })
            .await??
        };

        let proxy_status = self.fetch_proxy_status(proxy_url.as_deref()).await;

        tracing::info!(
            proxy_used = ?proxy_url,
            proxy_status = %proxy_status.as_ref().unwrap_or(&"none".to_string()),
            "assigned proxy for upstream request"
        );

        Ok((proxy_url, proxy_status))
    }
}
