use super::tests_basic::{CountingJob, policy};
use super::*;
use crate::background::BackgroundSupervisor;
use openproxy_adapters::client_contract::{
    Candidate, ClientIdentity, ContractError, ContractId, ContractLimits, ContractStore,
    ContractTransport, ContractValidator, EvidenceKind, FetchRequest, FetchResponse,
    IDENTITY_ORIGIN_OFFICIAL, Snapshot, SourceRef, StoreConfig, TransportCache,
};
use std::pin::Pin;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct DemoPayload {
    version: String,
}

/// Validador que lo difiere todo al base, como en los tests del store.
struct BaseOnly;

impl ContractValidator<DemoPayload> for BaseOnly {
    fn rule_set_id(&self) -> &'static str {
        "demo-rules"
    }

    fn extractor_version(&self) -> u32 {
        1
    }

    fn validate(
        &self,
        _candidate: &Candidate<DemoPayload>,
        _previous: Option<&Snapshot<DemoPayload>>,
        _limits: &ContractLimits,
    ) -> Result<(), ContractError> {
        Ok(())
    }
}

fn demo_candidate(version: &str) -> Candidate<DemoPayload> {
    Candidate::without_catalog(
        ContractId::Codex,
        ClientIdentity::versioned(version, IDENTITY_ORIGIN_OFFICIAL),
        "demo-rules",
        1,
        vec![SourceRef::pinned(
            "https://example.test/models.json",
            "rev-1",
            "d".repeat(64),
            EvidenceKind::PackageManifest,
        )],
        DemoPayload {
            version: version.to_string(),
        },
    )
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("cc-refresh-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[tokio::test]
async fn a_published_candidate_reports_its_generation_and_a_repeat_reports_unchanged() {
    let dir = temp_dir("publish");
    let store = Arc::new(ContractStore::new(
        ContractId::Codex,
        StoreConfig::new(&dir).with_limits(ContractLimits::tight()),
        None,
        Arc::new(BaseOnly),
    ));

    let activated = publish(&store, demo_candidate("1.0.0"))
        .await
        .expect("publish");
    match activated {
        RoundOutcome::Activated { generation } => {
            assert_eq!(
                generation,
                store.snapshot().expect("activa").generation,
                "la generación reportada es la publicada"
            );
        }
        _ => panic!("se esperaba una activación"),
    }

    let again = publish(&store, demo_candidate("1.0.0"))
        .await
        .expect("republish");
    assert_eq!(
        again,
        RoundOutcome::Unchanged,
        "un candidato idéntico no se republica"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Transporte que responde `404` a todo, para probar el cableado de Codex
/// sin red.
struct MissingTransport;

impl ContractTransport for MissingTransport {
    fn fetch(
        &self,
        _request: &FetchRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<FetchResponse, ContractError>> + Send + '_>>
    {
        Box::pin(async {
            Ok(FetchResponse {
                status: 404,
                headers: Vec::new(),
                body: Vec::new(),
            })
        })
    }
}

struct CodexBaseOnly;

impl ContractValidator<CodexProfile> for CodexBaseOnly {
    fn rule_set_id(&self) -> &'static str {
        "codex-rules"
    }

    fn extractor_version(&self) -> u32 {
        1
    }

    fn validate(
        &self,
        _candidate: &Candidate<CodexProfile>,
        _previous: Option<&Snapshot<CodexProfile>>,
        _limits: &ContractLimits,
    ) -> Result<(), ContractError> {
        Ok(())
    }
}

#[tokio::test]
async fn the_codex_job_reports_a_transport_failure_and_publishes_nothing() {
    let driver = Arc::new(CodexReleaseDriver::new(
        Arc::new(MissingTransport),
        Arc::new(TransportCache::new()),
        ContractLimits::default(),
    ));
    let dir = temp_dir("codex-job");
    let store: Arc<ContractStore<CodexProfile>> = Arc::new(ContractStore::new(
        ContractId::Codex,
        StoreConfig::new(&dir).with_limits(ContractLimits::tight()),
        None,
        Arc::new(CodexBaseOnly),
    ));

    let job = CodexContractJob::new(Arc::clone(&driver), Arc::clone(&store));
    assert_eq!(job.name(), "codex_contract");
    let cancel = CancellationToken::new();
    let error = job
        .refresh_round(&cancel)
        .await
        .expect_err("la fuente falta");
    assert!(
        matches!(error, ContractError::SourceMissing { .. }),
        "se obtuvo {error:?}"
    );
    assert!(
        store.snapshot().is_none(),
        "una ronda fallida no publica nada"
    );

    // El envoltorio del ciclo de vida cablea el trabajo de Codex.
    let service = codex_refresh_service(driver, store, RefreshPolicy::default());
    assert_eq!(service.name(), "codex_contract");
    assert_eq!(service.policy().interval, DEFAULT_INTERVAL);
    let _ = std::fs::remove_dir_all(&dir);
}

/// El envoltorio de Codex descarta la adquisición y no publica nada cuando
/// el supervisor para con la ronda pendiente.
#[tokio::test]
async fn the_codex_job_discards_an_acquisition_on_cancellation() {
    let driver = Arc::new(CodexReleaseDriver::new(
        Arc::new(MissingTransport),
        Arc::new(TransportCache::new()),
        ContractLimits::default(),
    ));
    let dir = temp_dir("codex-cancel");
    let store: Arc<ContractStore<CodexProfile>> = Arc::new(ContractStore::new(
        ContractId::Codex,
        StoreConfig::new(&dir).with_limits(ContractLimits::tight()),
        None,
        Arc::new(CodexBaseOnly),
    ));
    let job = CodexContractJob::new(Arc::clone(&driver), Arc::clone(&store));

    // Token ya cancelado: la adquisición ni empieza.
    let cancel = CancellationToken::new();
    cancel.cancel();
    let outcome = job
        .refresh_round(&cancel)
        .await
        .expect("la ronda descartada no es un error");
    assert_eq!(
        outcome,
        RoundOutcome::Cancelled,
        "una ronda descartada no toca el upstream ni publica"
    );
    assert!(
        store.snapshot().is_none(),
        "descartar la ronda no publica nada"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Trabajo que mantiene la primera ronda dentro hasta que el test la
/// suelta, sin mirar el token: modela una ronda ya en la frontera de
/// commit, que debe drenar aunque llegue la cancelación.
struct BlockingJob {
    rounds: Arc<AtomicU64>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl ContractRefreshJob for BlockingJob {
    fn name(&self) -> &'static str {
        "blocking"
    }

    async fn refresh_round(
        &self,
        _cancel: &CancellationToken,
    ) -> Result<RoundOutcome, ContractError> {
        self.rounds.fetch_add(1, Ordering::Relaxed);
        self.entered.notify_one();
        self.release.notified().await;
        Ok(RoundOutcome::Unchanged)
    }
}

/// Trabajo que cruza una frontera de commit simulada: observa el token
/// durante la adquisición, lo ignora después y drena hasta el final.
struct DrainingJob {
    acquired: Arc<Notify>,
    crossed: Arc<Notify>,
    in_commit: Arc<Notify>,
    release: Arc<Notify>,
    drained: Arc<AtomicBool>,
}

impl ContractRefreshJob for DrainingJob {
    fn name(&self) -> &'static str {
        "draining"
    }

    async fn refresh_round(
        &self,
        cancel: &CancellationToken,
    ) -> Result<RoundOutcome, ContractError> {
        self.acquired.notify_one();
        tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(RoundOutcome::Cancelled),
            () = self.crossed.notified() => {}
        }
        // Frontera de commit: desde aquí el token se ignora y el trabajo
        // drena hasta el final, pase lo que pase.
        self.in_commit.notify_one();
        self.release.notified().await;
        self.drained.store(true, Ordering::Relaxed);
        Ok(RoundOutcome::Activated {
            generation: "gen-drenada".to_string(),
        })
    }
}

/// El tope de rondas es del grupo: dos rondas compiten por la compuerta y
/// solo una consume la ronda permitida. El test retiene la compuerta para
/// que ambas pasen el chequeo previo (aún no hay ninguna contada) y se
/// encolen; sin el re-chequeo bajo el lock, la segunda contaría una ronda
/// de más y el test falla.
#[tokio::test]
async fn two_rounds_queued_on_the_gate_share_one_max_rounds_ceiling() {
    let job_rounds = Arc::new(AtomicU64::new(0));
    let service = Arc::new(ContractRefreshService::new(
        CountingJob {
            rounds: Arc::clone(&job_rounds),
            started: Arc::new(Notify::new()),
            fail: Arc::new(AtomicBool::new(false)),
        },
        policy().with_max_rounds(Some(1)),
    ));

    // El test retiene la compuerta: las dos rondas se encolan tras ella.
    let hold = service.gate.lock().await;
    let cancel = CancellationToken::new();
    let first = tokio::spawn({
        let service = Arc::clone(&service);
        let cancel = cancel.clone();
        async move { service.round(&cancel).await }
    });
    let second = tokio::spawn({
        let service = Arc::clone(&service);
        async move { service.round(&cancel).await }
    });
    // Ambas quedan esperando la compuerta antes de soltarla. La espera
    // acotada da margen al planificador y `is_finished` confirma que
    // ninguna ha retornado: están encoladas tras el chequeo previo.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !first.is_finished() && !second.is_finished(),
        "ambas rondas deben estar esperando la compuerta"
    );
    drop(hold);

    let _ = first
        .await
        .expect("la primera ronda no debe entrar en pánico");
    let _ = second
        .await
        .expect("la segunda ronda no debe entrar en pánico");

    assert_eq!(
        job_rounds.load(Ordering::Relaxed),
        1,
        "solo una ronda llegó al trabajo: la segunda ve el tope consumido"
    );
    assert_eq!(
        service.stats().rounds,
        1,
        "el tope es del grupo: la ronda encolada no lo supera"
    );
    assert_eq!(service.stats().unchanged, 1);
    assert!(!service.in_flight(), "la compuerta queda libre");
}

/// Una ronda encolada esperando la compuerta se descarta al cancelar: no
/// ejecuta trabajo del proveedor, cuenta como cancelada y no espera a que
/// el dueño de la compuerta termine.
#[tokio::test]
async fn a_round_queued_on_the_gate_is_discarded_on_cancellation() {
    let job_rounds = Arc::new(AtomicU64::new(0));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let supervisor = BackgroundSupervisor::new();
    let service = Arc::new(ContractRefreshService::new(
        BlockingJob {
            rounds: Arc::clone(&job_rounds),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        },
        policy(),
    ));
    assert!(service.spawn_under(&supervisor));

    // La primera ronda está en vuelo dentro del trabajo: tiene la compuerta.
    entered.notified().await;
    assert!(service.in_flight());

    // Una segunda ronda del mismo grupo se encola tras la compuerta. Se
    // llama a `round` directamente para fijar el encolado sin depender del
    // orden del planificador.
    let cancel = supervisor.token();
    let queued = tokio::spawn({
        let service = Arc::clone(&service);
        async move { service.round(&cancel).await }
    });
    tokio::task::yield_now().await;

    // Cancelar descarta la encolada aunque el dueño siga trabajando.
    supervisor.shutdown();
    let flow = tokio::time::timeout(Duration::from_secs(5), queued)
        .await
        .expect("la ronda encolada no debe esperar a que el dueño suelte la compuerta")
        .expect("la tarea encolada no debe entrar en pánico");
    assert_eq!(flow, Flow::Stop);

    // El dueño drena su ronda y el grupo termina.
    release.notify_one();
    supervisor.shutdown_and_wait().await;

    assert_eq!(
        job_rounds.load(Ordering::Relaxed),
        1,
        "la ronda descartada no llegó a ejecutar trabajo"
    );
    assert_eq!(
        service.stats().rounds,
        2,
        "una ronda ejecutada y una descartada"
    );
    assert_eq!(service.stats().unchanged, 1);
    assert_eq!(service.stats().cancelled, 1);
    assert_eq!(service.stats().failures, 0);
    assert!(!service.in_flight());
}

/// Cancelar con el commit ya iniciado no lo interrumpe: la ronda drena, se
/// contabiliza como activación —no como cancelación— y la compuerta se
/// suelta solo después del drenaje.
#[tokio::test]
async fn cancelling_during_commit_drains_before_releasing_the_gate() {
    let acquired = Arc::new(Notify::new());
    let crossed = Arc::new(Notify::new());
    let in_commit = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let drained = Arc::new(AtomicBool::new(false));
    let supervisor = BackgroundSupervisor::new();
    let service = Arc::new(ContractRefreshService::new(
        DrainingJob {
            acquired: Arc::clone(&acquired),
            crossed: Arc::clone(&crossed),
            in_commit: Arc::clone(&in_commit),
            release: Arc::clone(&release),
            drained: Arc::clone(&drained),
        },
        policy(),
    ));
    assert!(service.spawn_under(&supervisor));

    // Fase de adquisición: el trabajo observa el token y aún no está
    // cancelado. El test lo deja cruzar la frontera.
    acquired.notified().await;
    crossed.notify_one();
    in_commit.notified().await;

    // El commit está en curso: cancelar no lo interrumpe ni suelta la
    // compuerta.
    supervisor.shutdown();
    assert!(
        !drained.load(Ordering::Relaxed),
        "la cancelación no debe abortar un commit ya iniciado"
    );
    assert!(
        service.in_flight(),
        "la compuerta se suelta al drenar el commit, no antes"
    );

    // El commit drena: publica y solo entonces se libera la compuerta.
    release.notify_one();
    supervisor.shutdown_and_wait().await;

    assert!(drained.load(Ordering::Relaxed), "el commit llegó al final");
    assert_eq!(
        service.stats().published,
        1,
        "una ronda que drena tras la cancelación publica, no se cancela"
    );
    assert_eq!(service.stats().cancelled, 0);
    assert_eq!(service.stats().failures, 0);
    assert_eq!(service.stats().rounds, 1);
    assert!(
        !service.in_flight(),
        "la compuerta quedó libre tras el drenaje"
    );
}

/// El job productivo lee el MISMO store y la MISMA cache que el runtime
/// global que sirve las peticiones: nunca un segundo store desconectado.
#[tokio::test]
async fn the_codex_runtime_factory_reuses_the_global_store_and_cache() {
    let service = codex_runtime_refresh_service(UpstreamClient::new());
    assert_eq!(service.name(), "codex_contract");
    let runtime = codex_state::runtime();
    assert!(
        Arc::ptr_eq(service.job().store(), &runtime.store_handle()),
        "el job debe commitear al store del runtime global"
    );
    assert!(
        Arc::ptr_eq(service.job().driver().cache(), runtime.cache()),
        "el driver debe compartir la cache del runtime global"
    );
}
