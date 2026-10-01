//! Resolución de proxy: status + asignación por provider/cuenta. Único
//! submódulo que NO accede a `self.conn` directamente — toda la BD se
//! consulta mediante el repositorio asíncrono, fuera del reactor Tokio.

use super::UpstreamDispatcher;

impl UpstreamDispatcher {
    /// `spawn_blocking` porque `repo.get_proxy_status_by_url` toma el lock
    /// del `Mutex<Connection>` síncronamente.
    pub(super) async fn fetch_proxy_status(&self, proxy_url: Option<&str>) -> Option<String> {
        let url = proxy_url?.to_string();
        self.async_repo()
            .run(move |repo| Ok(repo.get_proxy_status_by_url(&url)))
            .await
            .unwrap_or(None)
    }

    /// Un `proxy_override` del request gana; si no, se consulta
    /// `repo.get_or_assign_provider_proxy`, que asigna uno nuevo cuando el
    /// provider no tiene.
    pub(super) async fn resolve_and_assign_proxy(
        &self,
        req: &crate::PipelineRequest,
        target: &openproxy_types::combos::ComboTarget,
    ) -> Result<(Option<String>, Option<String>), openproxy_types::error::CoreError> {
        let proxy_url = if let Some((_, ref purl)) = req.proxy_override {
            Some(purl.clone())
        } else {
            let provider_id = target.provider_id.clone();
            let account_id = target.account_id;
            self.async_repo()
                .run(move |repo| repo.get_or_assign_provider_proxy(&provider_id, account_id))
                .await?
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
