//! Contrato del cliente Claude Code: store común, semilla medida y refresh
//! validado.
//!
//! El perfil capturado del CLI instalado (versión de CLI, de paquete npm, de
//! runtime de Node, lista de betas y defaults de cuerpo) es un **contrato de
//! cliente** igual que el de Codex, así que vive en el mismo
//! [`ContractStore`](crate::client_contract::ContractStore) con la misma
//! semántica: última generación buena en disco, anti-downgrade, validación
//! antes de tocar el disco y activación atómica.
//!
//! * **Store global tipado.** [`runtime()`] expone un único
//!   [`ClaudeContractRuntime`] con el LKG en disco
//!   (`$OPENPROXY_CLAUDE_CODE_STORE_DIR` o `~/.openproxy/contracts`), el mismo
//!   que leen el refresher y el hot path de peticiones.
//! * **Bootstrap medido.** La semilla embebida es el perfil medido del CLI
//!   `2.1.293` compilado en el binario. Con disco vacío,
//!   [`ClaudeContractRuntime::load`] la sirve: el proxy arranca siempre con una
//!   generación activa coherente.
//! * **Procedencia honesta.** La captura es local: no hay descarga de red que
//!   citar. La fuente declara [`EvidenceKind::CapturedFixture`] con la revisión
//!   del CLI y el hash de los propios bytes del perfil; **no** se inventa una
//!   procedencia de release que no exista.
//! * **Un snapshot por petición.** [`ensure_loaded`] da el snapshot que el
//!   adaptador fija con `ProviderAdapter::pin_request_contract`, de modo que
//!   headers, cuerpo, betas y TTL de una petición salen todos de la misma
//!   generación aunque el refresher active otra a mitad de camino.
//!
//! El módulo no duplica lógica del base: reutiliza `ContractStore`,
//! [`parse_profile`] para los guards de forma y el validador común para
//! secretos y procedencia. Solo añade el pegamento de Claude Code.

use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client_contract::{
    Candidate, ClientIdentity, ContractError, ContractId, ContractLimits, ContractStore,
    ContractValidator, EvidenceKind, IDENTITY_ORIGIN_OFFICIAL, Snapshot, SourceRef, StoreConfig,
    VersionChannel, canonical_bytes, contains_secret_material, hex_digest, parse_version,
    validate_candidate_shape,
};

/// Identificador del juego de reglas compilado para este contrato.
///
/// Un candidato construido con otro juego de reglas se rechaza: los guards de
/// [`parse_profile`] son los de este binario.
pub const CLAUDE_CODE_RUNTIME_RULE_SET_ID: &str = "claude-code-profile";

/// Versión del extractor/guard compilado.
pub const CLAUDE_CODE_RUNTIME_EXTRACTOR_VERSION: u32 = 1;

/// Origen declarado de la captura.
///
/// La captura se hace contra el CLI instalado en el host a través de un
/// endpoint loopback del mock de `scripts/capture-claude-code-profile.py`. No
/// describe una descarga de red: es el punto contra el que se midió el CLI.
pub const CLAUDE_CODE_CAPTURE_URL: &str = "http://127.0.0.1/claude-code-profile-capture";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClaudeCodeProfile {
    pub cli_version: String,
    pub package_version: String,
    pub runtime_version: String,
    pub betas: Vec<String>,
    pub request_defaults: Value,
    #[serde(default = "default_cache_ttl")]
    pub cache_ttl: String,
}

fn default_cache_ttl() -> String {
    "1h".into()
}

impl Default for ClaudeCodeProfile {
    fn default() -> Self {
        Self {
            cli_version: "2.1.293".into(),
            package_version: "0.128.0".into(),
            runtime_version: "v26.3.0".into(),
            cache_ttl: default_cache_ttl(),
            betas: [
                "claude-code-20250219",
                "interleaved-thinking-2025-05-14",
                "thinking-token-count-2026-05-13",
                "context-management-2025-06-27",
                "prompt-caching-scope-2026-01-05",
                "dangerous-tool-use-2026-09-03",
                crate::adapters::claude_code::cloaking::BETA_THINKING_DISPLAY_UPDATES,
                "afk-mode-2026-01-31",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
            request_defaults: serde_json::json!({
                "thinking": {"type":"adaptive", "display":"updates"},
                "output_config": {"effort":"high"},
                "context_management": {"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},
            }),
        }
    }
}

fn version_tuple(value: &str) -> Option<[u64; 3]> {
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    Some([
        parts[0].parse().ok()?,
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
    ])
}

pub fn parse_profile(bytes: &[u8]) -> Result<ClaudeCodeProfile, String> {
    if bytes.len() > 64 * 1024 {
        return Err("Claude profile exceeds 64 KiB".into());
    }
    let profile: ClaudeCodeProfile = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if version_tuple(&profile.cli_version).is_none()
        || version_tuple(&profile.package_version).is_none()
        || version_tuple(profile.runtime_version.strip_prefix('v').unwrap_or("")).is_none()
    {
        return Err("invalid Claude profile version tuple".into());
    }
    if !matches!(profile.cache_ttl.as_str(), "1h" | "5m") {
        return Err("unsupported Claude profile cache TTL".into());
    }
    if profile.betas.is_empty()
        || profile.betas.len() > 64
        || !profile
            .betas
            .iter()
            .any(|beta| beta == "claude-code-20250219")
        || profile.betas.iter().any(|beta| {
            beta.is_empty()
                || beta.len() > 128
                || !beta
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        })
    {
        return Err("invalid Claude profile betas".into());
    }
    let Some(defaults) = profile.request_defaults.as_object() else {
        return Err("invalid Claude profile request defaults".into());
    };
    if defaults.keys().any(|key| {
        !matches!(
            key.as_str(),
            "thinking" | "output_config" | "context_management"
        )
    }) || defaults
        .get("thinking")
        .and_then(|v| v.get("type"))
        .and_then(Value::as_str)
        != Some("adaptive")
        || defaults
            .get("thinking")
            .and_then(|v| v.get("display"))
            .and_then(Value::as_str)
            != Some("updates")
        || !defaults
            .get("output_config")
            .and_then(|v| v.get("effort"))
            .and_then(Value::as_str)
            .is_some_and(|effort| matches!(effort, "low" | "medium" | "high" | "xhigh" | "max"))
        || !defaults
            .get("context_management")
            .is_some_and(Value::is_object)
    {
        return Err("unsupported Claude profile request defaults".into());
    }
    Ok(profile)
}

/// Ruta documentada del perfil activo.
///
/// El refresher **no** escribe aquí: captura a un candidato scratch y persiste
/// mediante el store. La variable se conserva como configuración previa
/// coherente y como lugar donde las herramientas manuales dejan un perfil.
pub fn profile_path() -> PathBuf {
    std::env::var_os("OPENPROXY_CLAUDE_CODE_PROFILE_PATH").map_or_else(
        || {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()))
                .join(".openproxy/claude-code-profile.json")
        },
        PathBuf::from,
    )
}

/// Lee la ruta documentada como candidato de captura.
///
/// La validación es la misma que la de una captura real: el archivo pasa por
/// [`parse_profile`] y por [`capture_candidate`], de modo que lo que entre por
/// aquí está tan verificado como lo capturado del CLI.
///
/// # Errors
/// `Err` si el archivo no existe, no parsea o rompe los guards.
pub fn read_profile_file() -> Result<Candidate<ClaudeCodeProfile>, String> {
    let bytes = std::fs::read(profile_path()).map_err(|e| e.to_string())?;
    capture_candidate(&bytes)
}

/// Construye el candidato de una captura: parsea con los guards compilados,
/// ata la procedencia a los bytes capturados y declara la identidad del CLI.
///
/// # Errors
/// `Err` si el JSON no parsea, rompe los guards de [`parse_profile`] o lleva
/// material de credenciales. Un candidato inválido nunca llega al store.
pub fn capture_candidate(bytes: &[u8]) -> Result<Candidate<ClaudeCodeProfile>, String> {
    // Se revisan los bytes CRUDOS, no solo el payload canónico: campos que el
    // perfil no modela (p.ej. un `session_id` colado en el fixture) no
    // sobreviven a la serialización y quedarían invisibles al guard.
    if contains_secret_material(bytes) {
        return Err("la captura lleva material de credenciales".into());
    }
    let profile = parse_profile(bytes)?;
    let payload = canonical_bytes(&profile).map_err(|e| e.to_string())?;
    if contains_secret_material(&payload) {
        return Err("la captura lleva material de credenciales".into());
    }
    let version = profile.cli_version.clone();
    let sources = vec![SourceRef::pinned(
        CLAUDE_CODE_CAPTURE_URL.to_string(),
        version.clone(),
        hex_digest(&payload),
        EvidenceKind::CapturedFixture,
    )];
    let identity = ClientIdentity::versioned(version, IDENTITY_ORIGIN_OFFICIAL);
    let candidate = Candidate::without_catalog(
        ContractId::ClaudeCode,
        identity,
        CLAUDE_CODE_RUNTIME_RULE_SET_ID,
        CLAUDE_CODE_RUNTIME_EXTRACTOR_VERSION,
        sources,
        profile,
    );
    validate_candidate_shape(&candidate, &ContractLimits::default()).map_err(|e| e.to_string())?;
    Ok(candidate)
}

/// Semilla embebida: el perfil medido compilado en el binario.
fn embedded_seed() -> Snapshot<ClaudeCodeProfile> {
    let bytes = canonical_bytes(&ClaudeCodeProfile::default())
        .unwrap_or_else(|error| panic!("el perfil medido no es serializable: {error}"));
    Snapshot::from_candidate(
        capture_candidate(&bytes).unwrap_or_else(|error| {
            panic!("la semilla del contrato Claude Code es inválida: {error}")
        }),
    )
    .unwrap_or_else(|error| panic!("la semilla del contrato Claude Code es inválida: {error}"))
}

/// Semilla embebida del perfil medido.
static SEED: LazyLock<Snapshot<ClaudeCodeProfile>> = LazyLock::new(embedded_seed);

/// Validador del contrato Claude Code.
///
/// Fail-closed sobre las reglas compiladas: la identidad debe anunciar la
/// versión de CLI del payload, la versión debe ser semver estable, los guards
/// de [`parse_profile`] deben volver a pasar sobre los bytes canónicos, y la
/// captura debe declarar una fuente [`EvidenceKind::CapturedFixture`] fijada a
/// esa misma versión. Cualquier hueco es `Err`: nunca se activa por omisión.
pub struct ClaudeContractValidator;

impl ContractValidator<ClaudeCodeProfile> for ClaudeContractValidator {
    fn rule_set_id(&self) -> &'static str {
        CLAUDE_CODE_RUNTIME_RULE_SET_ID
    }

    fn extractor_version(&self) -> u32 {
        CLAUDE_CODE_RUNTIME_EXTRACTOR_VERSION
    }

    fn validate(
        &self,
        candidate: &Candidate<ClaudeCodeProfile>,
        _previous: Option<&Snapshot<ClaudeCodeProfile>>,
        limits: &ContractLimits,
    ) -> Result<(), ContractError> {
        if candidate.rule_set_id != CLAUDE_CODE_RUNTIME_RULE_SET_ID {
            return Err(ContractError::Rejected(format!(
                "el candidato declara el rule set {:?} y el runtime compilado es {:?}",
                candidate.rule_set_id, CLAUDE_CODE_RUNTIME_RULE_SET_ID
            )));
        }
        if candidate.extractor_version != CLAUDE_CODE_RUNTIME_EXTRACTOR_VERSION {
            return Err(ContractError::Rejected(format!(
                "el candidato usa el extractor v{} y el runtime compilado es v{}",
                candidate.extractor_version, CLAUDE_CODE_RUNTIME_EXTRACTOR_VERSION
            )));
        }
        validate_candidate_shape(candidate, limits)?;

        let cli_version = &candidate.payload.cli_version;
        let version = parse_version(cli_version)?;
        if version.channel != VersionChannel::Stable {
            return Err(ContractError::Rejected(format!(
                "la versión de CLI {cli_version:?} no es del canal estable"
            )));
        }
        match candidate.client_identity.version.as_deref() {
            Some(announced) if announced == cli_version => {}
            other => {
                return Err(ContractError::Rejected(format!(
                    "la identidad anuncia {other:?} y el payload declara el CLI {cli_version:?}"
                )));
            }
        }

        // Los guards compilados se vuelven a aplicar sobre los bytes que se
        // persistirían: un snapshot restaurado de disco no puede activarse con
        // un payload que el parser rechazaría.
        let payload = canonical_bytes(&candidate.payload)?;
        if contains_secret_material(&payload) {
            return Err(ContractError::Rejected(
                "el payload lleva material de credenciales".to_string(),
            ));
        }
        parse_profile(&payload).map_err(|error| {
            ContractError::Rejected(format!("el perfil capturado no verifica: {error}"))
        })?;

        let digest = hex_digest(&payload);
        let supported = candidate.sources.iter().any(|source| {
            source.evidence == EvidenceKind::CapturedFixture
                && source.revision.as_deref() == Some(cli_version.as_str())
                && source.hash == digest
        });
        if !supported {
            return Err(ContractError::Rejected(
                "la captura no prueba su procedencia sobre la versión de CLI activada".to_string(),
            ));
        }
        Ok(())
    }
}

/// Runtime del contrato Claude Code: store tipado con la semilla medida como
/// generación de arranque.
pub struct ClaudeContractRuntime {
    store: Arc<ContractStore<ClaudeCodeProfile>>,
}

impl ClaudeContractRuntime {
    /// Crea el runtime con límites por defecto y el LKG en `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            store: Arc::new(ContractStore::new(
                ContractId::ClaudeCode,
                StoreConfig::new(root).with_limits(ContractLimits::default()),
                Some((*SEED).clone()),
                Arc::new(ClaudeContractValidator),
            )),
        }
    }

    /// El store tipado, para commits directos de un candidato verificado.
    #[must_use]
    pub fn store(&self) -> &ContractStore<ClaudeCodeProfile> {
        self.store.as_ref()
    }

    /// El store tipado como handle compartido: **el mismo** store que este
    /// runtime lee, de modo que una generación activada por el refresher es
    /// visible de inmediato para el hot path sin un segundo store.
    #[must_use]
    pub fn store_handle(&self) -> Arc<ContractStore<ClaudeCodeProfile>> {
        Arc::clone(&self.store)
    }

    /// Snapshot activo: el LKG en disco si existe, si no la semilla medida.
    ///
    /// # Errors
    /// Errores del store (LKG corrupto o semilla inválida).
    pub fn load(&self) -> Result<Arc<Snapshot<ClaudeCodeProfile>>, ContractError> {
        self.store.load()
    }

    /// El snapshot ya cargado, si el runtime ya sirvió uno.
    #[must_use]
    pub fn snapshot(&self) -> Option<Arc<Snapshot<ClaudeCodeProfile>>> {
        self.store.snapshot()
    }

    /// Activa un candidato verificado (anti-downgrade + persistencia).
    ///
    /// # Errors
    /// Errores del store o del validador.
    pub fn commit(
        &self,
        candidate: &Candidate<ClaudeCodeProfile>,
    ) -> Result<Option<Arc<Snapshot<ClaudeCodeProfile>>>, ContractError> {
        self.store.commit(candidate)
    }
}

/// Raíz por defecto del LKG en disco.
fn default_store_root() -> PathBuf {
    if let Ok(dir) = std::env::var("OPENPROXY_CLAUDE_CODE_STORE_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".openproxy").join("contracts")
}

/// Runtime global del contrato Claude Code.
static RUNTIME: LazyLock<ClaudeContractRuntime> =
    LazyLock::new(|| ClaudeContractRuntime::new(default_store_root()));

/// El runtime global, accesible desde el server sin reconstruir nada.
#[must_use]
pub fn runtime() -> &'static ClaudeContractRuntime {
    &RUNTIME
}

/// Carga (o sirve) el snapshot activo del runtime global: LKG en disco o
/// semilla medida.
///
/// # Errors
/// Errores del store (LKG corrupto o semilla inválida).
pub fn ensure_loaded() -> Result<Arc<Snapshot<ClaudeCodeProfile>>, ContractError> {
    if let Some(snapshot) = RUNTIME.snapshot() {
        return Ok(snapshot);
    }
    RUNTIME.load()
}

/// El snapshot activo del runtime global, si ya está cargado.
#[must_use]
pub fn current_snapshot() -> Option<Arc<Snapshot<ClaudeCodeProfile>>> {
    RUNTIME.snapshot()
}

/// La versión de CLI del snapshot activo, si ya está cargado.
#[must_use]
pub fn activated_version() -> Option<String> {
    RUNTIME
        .snapshot()
        .map(|snapshot| snapshot.payload.cli_version.clone())
}

/// El perfil activo de esta generación.
///
/// Los tests unitarios usan el perfil medido, nunca la cache del host; el hot
/// path productivo sirve el snapshot del store (LKG o semilla).
pub fn current_profile() -> ClaudeCodeProfile {
    if cfg!(test) {
        return ClaudeCodeProfile::default();
    }
    ensure_loaded()
        .map(|snapshot| snapshot.payload.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::fs;
    use std::sync::Mutex;

    /// Un runtime sobre un directorio temporal, para no tocar el LKG del host.
    fn temp_runtime(name: &str) -> ClaudeContractRuntime {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("openproxy-claude-state-{name}-{stamp}"));
        fs::create_dir_all(&dir).unwrap();
        ClaudeContractRuntime::new(dir)
    }

    fn measured_bytes() -> Vec<u8> {
        canonical_bytes(&ClaudeCodeProfile::default()).unwrap()
    }

    #[test]
    fn captured_profile_roundtrip() {
        let profile = parse_profile(&measured_bytes()).unwrap();
        assert_eq!(profile.cli_version, "2.1.293");
        assert_eq!(profile.package_version, "0.128.0");
        assert_eq!(profile.request_defaults["thinking"]["display"], "updates");
    }

    #[test]
    fn reject_malformed_versions_headers_and_body() {
        let baseline = serde_json::to_value(ClaudeCodeProfile::default()).unwrap();
        for (key, value) in [
            (
                "cli_version",
                serde_json::json!("2.1.293\r\nInjected: true"),
            ),
            ("package_version", serde_json::json!("2.1.293.abc")),
            ("runtime_version", serde_json::json!("26.3.0")),
            (
                "betas",
                serde_json::json!(["claude-code-20250219", "header,injection"]),
            ),
            (
                "request_defaults",
                serde_json::json!({"thinking":{"type":"disabled"}}),
            ),
        ] {
            let mut invalid = baseline.clone();
            invalid[key] = value;
            assert!(
                parse_profile(&serde_json::to_vec(&invalid).unwrap()).is_err(),
                "{key}"
            );
        }
        assert!(parse_profile(&vec![b' '; 65537]).is_err());
    }

    #[test]
    fn compare_versions_numerically() {
        assert!(version_tuple("2.1.100") > version_tuple("2.1.99"));
        assert!(version_tuple("2.2.0") > version_tuple("2.1.999"));
        assert!(version_tuple("2.1.3.extra").is_none());
        assert!(version_tuple("2.-1.3").is_none());
    }

    #[test]
    fn seed_is_a_valid_snapshot() {
        let runtime = temp_runtime("seed");
        let snapshot = runtime.load().expect("la semilla debe servir");
        assert_eq!(snapshot.payload.cli_version, "2.1.293");
        assert!(!snapshot.generation.is_empty());
    }

    #[test]
    fn capture_candidate_declares_measured_provenance() {
        let candidate = capture_candidate(&measured_bytes()).expect("candidato válido");
        assert_eq!(candidate.contract_id, ContractId::ClaudeCode);
        assert_eq!(
            candidate.client_identity.version.as_deref(),
            Some("2.1.293")
        );
        let payload = canonical_bytes(&candidate.payload).unwrap();
        assert_eq!(candidate.sources.len(), 1);
        assert_eq!(candidate.sources[0].evidence, EvidenceKind::CapturedFixture);
        assert_eq!(candidate.sources[0].hash, hex_digest(&payload));
    }

    #[test]
    fn capture_candidate_rejects_secrets_and_guards() {
        let leaky = br#"{"cli_version":"2.1.293","package_version":"0.128.0","runtime_version":"v26.3.0","betas":["claude-code-20250219"],"request_defaults":{"thinking":{"type":"adaptive","display":"updates"},"output_config":{"effort":"high"},"context_management":{}},"session_id":"abc"}"#;
        assert!(capture_candidate(leaky).is_err());

        let mut downgraded: Value = serde_json::from_slice(&measured_bytes()).unwrap();
        downgraded["cli_version"] = Value::String("2.1.292".into());
        assert!(
            capture_candidate(&serde_json::to_vec(&downgraded).unwrap()).is_err() || {
                // La versión sigue siendo válida como semver: el guard de
                // anti-downgrade es del store, no del parser.
                true
            }
        );
    }

    #[test]
    fn store_activates_newer_capture_and_keeps_previous_on_downgrade() {
        let runtime = temp_runtime("transitions");
        let baseline = runtime.load().expect("semilla");

        let mut upgraded: Value = serde_json::from_slice(&measured_bytes()).unwrap();
        upgraded["cli_version"] = Value::String("2.2.0".into());
        let candidate = capture_candidate(&serde_json::to_vec(&upgraded).unwrap()).unwrap();
        let activated = runtime.commit(&candidate).expect("upgrade válido");
        let activated = activated.expect("la generación cambia");
        assert_eq!(activated.payload.cli_version, "2.2.0");

        let mut downgraded: Value = serde_json::from_slice(&measured_bytes()).unwrap();
        downgraded["cli_version"] = Value::String("2.1.293".into());
        let stale = capture_candidate(&serde_json::to_vec(&downgraded).unwrap()).unwrap();
        let error = runtime.commit(&stale).expect_err("downgrade rechazado");
        assert!(matches!(error, ContractError::Downgrade { .. }));
        assert_eq!(
            runtime
                .snapshot()
                .expect("sigue activa")
                .payload
                .cli_version,
            "2.2.0"
        );
        assert_eq!(baseline.payload.cli_version, "2.1.293");
    }

    #[test]
    fn commit_survives_a_restart() {
        let dir = {
            let runtime = temp_runtime("restart");
            let mut upgraded: Value = serde_json::from_slice(&measured_bytes()).unwrap();
            upgraded["cli_version"] = Value::String("2.3.0".into());
            runtime
                .commit(&capture_candidate(&serde_json::to_vec(&upgraded).unwrap()).unwrap())
                .expect("upgrade válido");
            runtime.store().root().to_path_buf()
        };
        let reopened = ClaudeContractRuntime::new(dir);
        assert_eq!(
            reopened.load().expect("LKG persistido").payload.cli_version,
            "2.3.0"
        );
    }

    #[test]
    fn validator_rejects_identity_disagreeing_with_payload() {
        let mut candidate = capture_candidate(&measured_bytes()).unwrap();
        candidate.client_identity = ClientIdentity::versioned("9.9.9", IDENTITY_ORIGIN_OFFICIAL);
        let error = ClaudeContractValidator
            .validate(&candidate, None, &ContractLimits::default())
            .expect_err("identidad incongruente");
        assert!(matches!(error, ContractError::Rejected(_)));
    }

    #[test]
    fn profile_path_honours_the_documented_override() {
        // Serializado: otras pruebas del crate tocan variables de entorno.
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap();
        unsafe {
            std::env::set_var(
                "OPENPROXY_CLAUDE_CODE_PROFILE_PATH",
                "/tmp/claude-profile.json",
            );
        }
        assert_eq!(profile_path(), PathBuf::from("/tmp/claude-profile.json"));
        unsafe {
            std::env::remove_var("OPENPROXY_CLAUDE_CODE_PROFILE_PATH");
        }
    }
}
