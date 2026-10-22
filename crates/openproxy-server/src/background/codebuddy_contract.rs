//! Refresco del contrato CodeBuddy dentro del flujo común de contratos.
//!
//! El pin se resuelve contra el registry de npm y los mirrors de unpkg con
//! adquisición condicional sobre la MISMA `TransportCache` que ya usa el hot
//! path; el candidato pasa por los guards compilados de la revisión fijada y
//! se persiste en el MISMO `ContractStore` que sirve `pin_request_contract`,
//! de modo que una generación activada aquí es visible de inmediato para las
//! peticiones. Ninguna ronda ejecuta el bundle: solo se leen metadata y
//! catálogo.
//!
//! El ciclo de vida (backoff, single-flight, cancelación) lo aporta
//! `ContractRefreshService`; aquí solo está la ronda.
//!
//! ## Identidad fail-closed
//!
//! El paquete npm de CodeBuddy no expone `gitHead`, así que la correspondencia
//! paquete→commit no es verificable y `identity_activatable` es `false`. El
//! candidato se valida igual (catálogo, límites, anti-downgrade): el catálogo
//! verificado se publica como overlay, pero ninguna ronda eleva la identidad
//! anunciada a partir de metadata sola, y una versión distinta de la fijada
//! se rechaza porque no hay guards compilados para esa revisión. Si el
//! candidato no verifica, el último snapshot válido sigue en pie.

use super::client_contract::{
    ContractRefreshJob, ContractRefreshService, RefreshPolicy, RoundOutcome,
};
use super::runner::{DriverContractJob, RefreshableDriver};
use openproxy_adapters::client_contract::providers::codebuddy_runtime::{
    CodeBuddyProfile, CodeBuddyReleaseDriver,
};
use openproxy_adapters::client_contract::providers::codebuddy_state;
use openproxy_adapters::client_contract::{
    Candidate, ContractError, ContractStore, UpstreamHttpTransport,
};
use openproxy_adapters::upstream::{CancellationToken, UpstreamClient};
use std::sync::Arc;

impl RefreshableDriver<CodeBuddyProfile> for CodeBuddyReleaseDriver {
    fn refresh(
        &self,
    ) -> impl std::future::Future<
        Output = Result<Option<Candidate<CodeBuddyProfile>>, ContractError>,
    > + Send {
        self.refresh()
    }
}

/// La ronda de CodeBuddy: resuelve el pin de release sobre el transporte
/// compartido y commitea el candidato en el store del runtime global.
///
/// La cancelación se observa **solo** mientras se resuelve el pin y se
/// construye el candidato. En cuanto hay candidato, `publish` drena el commit
/// hasta el final sobre `spawn_blocking`, así que una parada del supervisor
/// no puede dejar una generación publicada a medias.
pub(crate) struct CodeBuddyContractJob {
    inner: DriverContractJob<CodeBuddyReleaseDriver, CodeBuddyProfile>,
}

impl CodeBuddyContractJob {
    /// Construye el trabajo sobre un driver y un store que el llamante ya tiene.
    #[must_use]
    pub(crate) fn new(
        driver: Arc<CodeBuddyReleaseDriver>,
        store: Arc<ContractStore<CodeBuddyProfile>>,
    ) -> Self {
        Self {
            inner: DriverContractJob::new("codebuddy_contract", driver, store),
        }
    }

    /// El driver del job, solo para los tests de wiring de la factory: en
    /// producción nadie más lo necesita fuera de la ronda.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn driver(&self) -> &Arc<CodeBuddyReleaseDriver> {
        self.inner.driver()
    }

    /// El store del job, solo para los tests de wiring de la factory: en
    /// producción la ronda lo usa directamente.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn store(&self) -> &Arc<ContractStore<CodeBuddyProfile>> {
        self.inner.store()
    }
}

impl ContractRefreshJob for CodeBuddyContractJob {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    async fn refresh_round(
        &self,
        cancel: &CancellationToken,
    ) -> Result<RoundOutcome, ContractError> {
        self.inner.refresh_round(cancel).await
    }
}

/// El servicio productivo de CodeBuddy: driver y store salen del runtime
/// global (`codebuddy_state::runtime`), el MISMO que sirve el hot path de
/// peticiones, así que una generación activada por el refresher es visible de
/// inmediato para `pin_request_contract`. Nunca hay un segundo store
/// desconectado del que lee la petición.
#[must_use]
pub(crate) fn codebuddy_refresh_service(
    upstream_client: Arc<UpstreamClient>,
) -> ContractRefreshService<CodeBuddyContractJob> {
    let runtime = codebuddy_state::runtime();
    let transport = Arc::new(UpstreamHttpTransport::new(upstream_client));
    let driver = runtime.release_driver(transport);
    let store = runtime.store_handle();
    ContractRefreshService::new(
        CodeBuddyContractJob::new(driver, store),
        RefreshPolicy::default(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic, clippy::wildcard_imports)]

    use super::*;

    /// La factory productiva lee el MISMO store y la MISMA cache que el
    /// runtime global que sirve las peticiones: nunca un segundo store
    /// desconectado.
    #[tokio::test]
    async fn the_codebuddy_factory_reuses_the_global_store_and_cache() {
        let service = codebuddy_refresh_service(UpstreamClient::new());
        assert_eq!(service.job().name(), "codebuddy_contract");
        let runtime = codebuddy_state::runtime();
        assert!(
            Arc::ptr_eq(service.job().store(), &runtime.store_handle()),
            "el job debe commitear al store del runtime global"
        );
        assert!(
            Arc::ptr_eq(service.job().driver().cache(), runtime.cache()),
            "el driver debe compartir la cache del runtime global"
        );
    }
}
