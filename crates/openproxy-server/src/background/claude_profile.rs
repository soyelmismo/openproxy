//! Refresco del contrato Claude Code dentro del flujo común de contratos.
//!
//! El perfil se captura del CLI instalado contra el mock de loopback del
//! script (nunca contra auth real), se valida con los mismos guards que el hot
//! path y se persiste en el MISMO [`ContractStore`] que sirve `pin_request_contract`,
//! de modo que una generación activada aquí es visible de inmediato para las
//! peticiones. La captura nunca toca el fichero de perfil documentado: escribe
//! a un candidato scratch único y el store es el único que publica.
//!
//! El ciclo de vida (backoff, single-flight, cancelación) lo aporta
//! [`ContractRefreshService`]; aquí solo está la ronda.

use super::client_contract::{
    ContractRefreshJob, ContractRefreshService, DEFAULT_INTERVAL, RefreshPolicy, RoundOutcome,
    publish,
};
use openproxy_adapters::client_contract::{Candidate, ContractError, ContractStore};
use openproxy_adapters::spoofer::claude_code_profile::{
    ClaudeCodeProfile, capture_candidate, runtime,
};
use openproxy_adapters::upstream::CancellationToken;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Margen por encima del timeout del script: si el proceso no acaba solo, la
/// ronda se descarta y el store conserva la generación buena.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(45);

/// Suelo de la cadencia: ningún valor explícito refresca más a menudo que esto.
const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// La ronda de Claude Code: captura del CLI instalado a un candidato scratch y
/// commit en el store compartido.
///
/// La cancelación solo se observa mientras el proceso de captura vive. En
/// cuanto hay candidato, [`publish`] drena el commit hasta el final sobre
/// `spawn_blocking`, así que una parada del supervisor no puede dejar una
/// generación publicada a medias.
pub struct ClaudeContractJob {
    script: PathBuf,
    scratch: PathBuf,
    store: Arc<ContractStore<ClaudeCodeProfile>>,
}

impl ClaudeContractJob {
    /// Construye el trabajo con el store del runtime global: **el mismo** que
    /// lee el hot path, así que no hay un segundo store desconectado.
    #[must_use]
    pub fn new() -> Self {
        let store = runtime().store_handle();
        let script = std::env::var_os("OPENPROXY_CLAUDE_CODE_PROFILE_CAPTURE_SCRIPT").map_or_else(
            || {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../scripts/capture-claude-code-profile.py")
            },
            PathBuf::from,
        );
        let scratch = std::env::var_os("OPENPROXY_CLAUDE_CODE_CAPTURE_SCRATCH").map_or_else(
            || runtime().store().root().join("capture-scratch"),
            PathBuf::from,
        );
        Self {
            script,
            scratch,
            store,
        }
    }

    /// El store compartido con el runtime global, solo para los tests de
    /// wiring de la factory: en producción nadie lo necesita y el hot path
    /// lee el store a través de [`runtime`].
    #[cfg(test)]
    #[must_use]
    pub fn store(&self) -> &Arc<ContractStore<ClaudeCodeProfile>> {
        &self.store
    }

    /// Ejecuta el script sobre un candidato scratch único y devuelve el
    /// candidato validado.
    ///
    /// El ejecutable es fijo y los argumentos no llevan credenciales: el script
    /// solo observa una petición contra su propio mock de loopback. El hijo
    /// lleva `kill_on_drop`, así que una cancelación no deja un CLI huérfano.
    async fn capture(&self) -> Result<Candidate<ClaudeCodeProfile>, ContractError> {
        let output = self.scratch_output();
        tokio::fs::create_dir_all(&self.scratch)
            .await
            .map_err(|error| ContractError::Rejected(format!("scratch de captura: {error}")))?;

        let mut command = tokio::process::Command::new("python3");
        command
            .arg(&self.script)
            .arg("--output")
            .arg(&output)
            .arg("--timeout")
            .arg("30")
            .arg("--scratch")
            .arg(&self.scratch)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let status = tokio::time::timeout(CAPTURE_TIMEOUT, command.status())
            .await
            .map_err(|_| {
                ContractError::Rejected("la captura del perfil Claude Code caducó".to_string())
            })?
            .map_err(|error| {
                ContractError::Rejected(format!("no se pudo lanzar la captura: {error}"))
            })?;
        if !status.success() {
            return Err(ContractError::Rejected(
                "la captura del perfil Claude Code falló".to_string(),
            ));
        }

        // El candidato pasa por los mismos guards que el hot path. Un perfil
        // inválido se descarta aquí y la generación buena sigue en pie.
        let bytes = tokio::fs::read(&output).await.map_err(|error| {
            ContractError::Rejected(format!("candidato de captura ilegible: {error}"))
        })?;
        // El scratch no sobrevive a la ronda ni cuando el candidato no
        // verifica: solo el store publica.
        let _ = tokio::fs::remove_file(&output).await;
        let candidate = capture_candidate(&bytes)
            .map_err(|error| ContractError::Rejected(format!("candidato inválido: {error}")))?;
        Ok(candidate)
    }

    /// Candidato scratch por ronda: único por proceso y tiempo, y **nunca** la
    /// ruta del perfil documentado, que es configuración previa, no publicación.
    fn scratch_output(&self) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        self.scratch.join(format!(
            "claude-code-profile-{}-{stamp}.json",
            std::process::id()
        ))
    }
}

impl Default for ClaudeContractJob {
    fn default() -> Self {
        Self::new()
    }
}

impl ContractRefreshJob for ClaudeContractJob {
    fn name(&self) -> &'static str {
        "claude_code_contract"
    }

    async fn refresh_round(
        &self,
        cancel: &CancellationToken,
    ) -> Result<RoundOutcome, ContractError> {
        let candidate = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(RoundOutcome::Cancelled),
            acquired = self.capture() => acquired?,
        };
        // Frontera de commit: a partir de aquí la cancelación ya no se mira.
        publish(&self.store, candidate).await
    }
}

/// Cadencia del refresco de captura: por defecto seis horas, la misma que el
/// resto de contratos ([`DEFAULT_INTERVAL`]).
fn refresh_interval() -> Duration {
    refresh_interval_from(
        std::env::var("OPENPROXY_CLAUDE_CODE_PROFILE_REFRESH_SECS")
            .ok()
            .as_deref(),
    )
}

/// Cadencia efectiva a partir del valor crudo de la variable de entorno.
///
/// Función pura: los tests ejercen el parseo sin mutar el entorno global.
/// Ausente, vacía o inválida cae a la cadencia documentada de seis horas; un
/// valor explícito nunca baja del suelo de [`MIN_REFRESH_INTERVAL`].
fn refresh_interval_from(raw: Option<&str>) -> Duration {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .map_or(DEFAULT_INTERVAL, |secs| {
            Duration::from_secs(secs).max(MIN_REFRESH_INTERVAL)
        })
}

/// El servicio productivo de Claude Code: store y runtime salen del global
/// ([`runtime`]), el MISMO que sirve el hot path de peticiones.
#[must_use]
pub fn claude_refresh_service() -> ContractRefreshService<ClaudeContractJob> {
    ContractRefreshService::new(
        ClaudeContractJob::new(),
        RefreshPolicy::default().with_interval(refresh_interval()),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic, clippy::wildcard_imports)]

    use super::*;

    /// El default documentado es seis horas: ausente o inválido ya no cae al
    /// `Duration::ZERO` que el suelo recortaba a cinco minutos, y un valor
    /// explícito nunca baja del suelo.
    #[test]
    fn refresh_interval_defaults_to_six_hours_and_never_dips_below_the_floor() {
        assert_eq!(refresh_interval_from(None), DEFAULT_INTERVAL);
        assert_eq!(refresh_interval_from(Some("")), DEFAULT_INTERVAL);
        assert_eq!(refresh_interval_from(Some("no-numérico")), DEFAULT_INTERVAL);
        assert_eq!(refresh_interval_from(Some("120")), MIN_REFRESH_INTERVAL);
        assert_eq!(refresh_interval_from(Some("21600")), DEFAULT_INTERVAL);
        assert_eq!(
            refresh_interval_from(Some("7200")),
            Duration::from_secs(7200)
        );
    }

    /// La factory productiva usa el MISMO store que el runtime global que
    /// sirve las peticiones: nunca un segundo store desconectado.
    #[test]
    fn the_claude_factory_reuses_the_global_store() {
        let service = claude_refresh_service();
        assert_eq!(service.job().name(), "claude_code_contract");
        assert!(
            Arc::ptr_eq(service.job().store(), &runtime().store_handle()),
            "el job debe commitear al store del runtime global"
        );
    }
}
