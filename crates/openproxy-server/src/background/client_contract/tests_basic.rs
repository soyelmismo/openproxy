#![allow(clippy::pedantic, clippy::wildcard_imports)]

use super::*;
use crate::background::BackgroundSupervisor;
use std::sync::atomic::AtomicUsize;

/// Espera acotada a que `ready` se cumpla.
///
/// Este crate no tiene `tokio/test-util`, así que no hay reloj en pausa:
/// se espera con retardos reales y cortos, y el test falla en vez de
/// colgarse si la condición nunca se cumple. Hace falta porque los
/// contadores de resultado se incrementan **después** de que la ronda
/// devuelve, así que esperar por `rounds` no garantiza ver todavía
/// `unchanged` o `failures`.
async fn wait_until(ready: impl Fn() -> bool) {
    for _ in 0..500 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    panic!("la condición nunca se cumplió");
}

async fn wait_for(counter: &AtomicU64, target: u64) {
    wait_until(|| counter.load(Ordering::Relaxed) >= target).await;
}

/// Política de test: cadencia de una hora, de modo que ninguna espera
/// perdida en el reloj real arruine la determinación.
pub(super) fn policy() -> RefreshPolicy {
    RefreshPolicy::default().with_interval(Duration::from_secs(3600))
}

/// Servicio de fixture con un [`CountingJob`]: las sondas que el test
/// conserva se pasan ya clonadas para observar las rondas desde fuera.
pub(super) fn counting_service(
    rounds: Arc<AtomicU64>,
    started: Arc<Notify>,
    fail: Arc<AtomicBool>,
    policy: RefreshPolicy,
) -> ContractRefreshService<CountingJob> {
    ContractRefreshService::new(
        CountingJob {
            rounds,
            started,
            fail,
        },
        policy,
    )
}

/// Cuenta rondas, avisa en cuanto empieza una y falla mientras `fail` valga.
pub(super) struct CountingJob {
    pub rounds: Arc<AtomicU64>,
    pub started: Arc<Notify>,
    pub fail: Arc<AtomicBool>,
}

impl ContractRefreshJob for CountingJob {
    fn name(&self) -> &'static str {
        "counting"
    }

    async fn refresh_round(
        &self,
        _cancel: &CancellationToken,
    ) -> Result<RoundOutcome, ContractError> {
        self.rounds.fetch_add(1, Ordering::Relaxed);
        self.started.notify_one();
        if self.fail.load(Ordering::Relaxed) {
            return Err(ContractError::SourceMissing {
                url: "https://example.test/contract.json".to_string(),
            });
        }
        Ok(RoundOutcome::Unchanged)
    }
}

/// Mantiene una ronda abierta hasta que se suelta la compuerta, para
/// observar el solapamiento.
struct GatedJob {
    rounds: Arc<AtomicU64>,
    live: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    gate: Arc<Notify>,
    block: Arc<AtomicBool>,
}

impl ContractRefreshJob for GatedJob {
    fn name(&self) -> &'static str {
        "gated"
    }

    async fn refresh_round(
        &self,
        cancel: &CancellationToken,
    ) -> Result<RoundOutcome, ContractError> {
        let live = self.live.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak.fetch_max(live, Ordering::Relaxed);
        self.rounds.fetch_add(1, Ordering::Relaxed);
        if self.block.swap(false, Ordering::Relaxed) {
            // La espera de la compuerta también es cancelable: la ronda se
            // descarta sin haber publicado nada.
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Ok(RoundOutcome::Cancelled),
                () = self.gate.notified() => {}
            }
        }
        self.live.fetch_sub(1, Ordering::Relaxed);
        Ok(RoundOutcome::Unchanged)
    }
}

/// Trabajo cuya adquisición se queda pendiente hasta que el supervisor
/// pare, para observar la frontera de commit desde el lado del job.
struct PendingJob {
    entered: Arc<Notify>,
    observed_cancel: Arc<AtomicBool>,
}

impl ContractRefreshJob for PendingJob {
    fn name(&self) -> &'static str {
        "pending"
    }

    async fn refresh_round(
        &self,
        cancel: &CancellationToken,
    ) -> Result<RoundOutcome, ContractError> {
        self.entered.notify_one();
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                self.observed_cancel.store(true, Ordering::Relaxed);
                Ok(RoundOutcome::Cancelled)
            }
            () = tokio::time::sleep(Duration::from_secs(3600)) => Ok(RoundOutcome::Unchanged),
        }
    }
}

/// Trabajo lento pero determinista: mide cuántas rondas conviven dentro de
/// la compuerta compartida.
struct SlowJob {
    rounds: Arc<AtomicU64>,
    live: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl ContractRefreshJob for SlowJob {
    fn name(&self) -> &'static str {
        "slow"
    }

    async fn refresh_round(
        &self,
        cancel: &CancellationToken,
    ) -> Result<RoundOutcome, ContractError> {
        let live = self.live.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak.fetch_max(live, Ordering::Relaxed);
        self.rounds.fetch_add(1, Ordering::Relaxed);
        tokio::select! {
            biased;
            () = cancel.cancelled() => {}
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        self.live.fetch_sub(1, Ordering::Relaxed);
        Ok(RoundOutcome::Unchanged)
    }
}

/// Servicio de fixture con un [`SlowJob`]: devuelve el supervisor, el
/// servicio envuelto en `Arc` y las sondas de rondas del trabajo, para que
/// cada test decida cuántos handles admite y conserve su `spawn_under`.
///
/// Sin tope de rondas: los contadores son compartidos, así que un tope
/// cortaría el segundo handle antes de su ronda de arranque.
fn slow_service() -> (
    BackgroundSupervisor,
    Arc<ContractRefreshService<SlowJob>>,
    Arc<AtomicU64>,
    Arc<AtomicUsize>,
) {
    let rounds = Arc::new(AtomicU64::new(0));
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let supervisor = BackgroundSupervisor::new();
    let service = Arc::new(ContractRefreshService::new(
        SlowJob {
            rounds: Arc::clone(&rounds),
            live: Arc::clone(&live),
            peak: Arc::clone(&peak),
        },
        policy(),
    ));
    (supervisor, service, rounds, peak)
}

#[tokio::test]
async fn a_startup_round_runs_before_the_first_wait_and_stops_at_the_ceiling() {
    let rounds = Arc::new(AtomicU64::new(0));
    let started = Arc::new(Notify::new());
    let supervisor = BackgroundSupervisor::new();
    let service = Arc::new(counting_service(
        Arc::clone(&rounds),
        Arc::clone(&started),
        Arc::new(AtomicBool::new(false)),
        policy().with_max_rounds(Some(1)),
    ));
    assert!(service.spawn_under(&supervisor));

    // La ronda de arranque ocurre sin esperar la hora de cadencia.
    started.notified().await;
    supervisor.shutdown_and_wait().await;
    assert_eq!(
        service.stats().rounds,
        1,
        "el tope de rondas debe parar el bucle"
    );
    assert_eq!(service.stats().unchanged, 1);
    assert!(!service.in_flight());
}

#[tokio::test]
async fn a_failed_round_waits_the_backoff_until_a_trigger_arrives() {
    let rounds = Arc::new(AtomicU64::new(0));
    let started = Arc::new(Notify::new());
    let fail = Arc::new(AtomicBool::new(true));
    let supervisor = BackgroundSupervisor::new();
    let service = Arc::new(counting_service(
        Arc::clone(&rounds),
        Arc::clone(&started),
        Arc::clone(&fail),
        policy(),
    ));
    assert!(service.spawn_under(&supervisor));

    // Primera ronda fallida: contabilizada, con backoff pendiente.
    started.notified().await;
    wait_until(|| service.stats().failures >= 1).await;
    assert_eq!(service.stats().consecutive_failures, 1);

    // La siguiente ronda solo puede venir de un `trigger`: el temporizador
    // de la cadencia es de una hora.
    fail.store(false, Ordering::Relaxed);
    service.trigger();
    wait_until(|| service.stats().unchanged >= 1).await;
    assert_eq!(service.stats().rounds, 2);
    assert_eq!(service.stats().triggered, 1);
    assert_eq!(
        service.stats().consecutive_failures,
        0,
        "una ronda correcta reinicia el backoff"
    );
    supervisor.shutdown_and_wait().await;
}

#[test]
fn the_backoff_doubles_is_capped_and_honours_retry_after_within_the_ceiling() {
    let service = counting_service(
        Arc::new(AtomicU64::new(0)),
        Arc::new(Notify::new()),
        Arc::new(AtomicBool::new(false)),
        policy(),
    );
    let failures = &service.counters.consecutive_failures;

    // Sin fallos: cadencia en régimen.
    assert_eq!(service.next_wait(), Duration::from_secs(3600));

    // Un fallo: primer escalón. Dos: 60s. Cuatro: 240s.
    failures.store(1, Ordering::Relaxed);
    assert_eq!(service.next_wait(), Duration::from_secs(30));
    failures.store(2, Ordering::Relaxed);
    assert_eq!(service.next_wait(), Duration::from_secs(60));
    failures.store(4, Ordering::Relaxed);
    assert_eq!(service.next_wait(), Duration::from_secs(240));

    // Muchísimos fallos: techo de 15 minutos, nunca más.
    failures.store(64, Ordering::Relaxed);
    assert_eq!(service.next_wait(), Duration::from_secs(900));

    // `Retry-After` de 10 minutos alarga la espera desde un escalón bajo...
    failures.store(1, Ordering::Relaxed);
    service
        .counters
        .retry_after_secs
        .store(600, Ordering::Relaxed);
    assert_eq!(service.next_wait(), Duration::from_secs(600));
    // ...pero un `Retry-After` absurdo tampoco rompe el techo.
    service
        .counters
        .retry_after_secs
        .store(86_400, Ordering::Relaxed);
    assert_eq!(service.next_wait(), Duration::from_secs(900));
    // Y cuando el backoff ya está en el techo, un `Retry-After` menor no lo
    // acorta: la espera nunca baja de lo que ya se había decidido.
    failures.store(64, Ordering::Relaxed);
    assert_eq!(service.next_wait(), Duration::from_secs(900));
}

#[tokio::test]
async fn concurrent_triggers_never_overlap_two_rounds() {
    let rounds = Arc::new(AtomicU64::new(0));
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(Notify::new());
    let block = Arc::new(AtomicBool::new(true));
    let supervisor = BackgroundSupervisor::new();
    let service = Arc::new(ContractRefreshService::new(
        GatedJob {
            rounds: Arc::clone(&rounds),
            live: Arc::clone(&live),
            peak: Arc::clone(&peak),
            gate: Arc::clone(&gate),
            block: Arc::clone(&block),
        },
        policy(),
    ));
    assert!(service.spawn_under(&supervisor));

    // La ronda de arranque está abierta y bloqueada: los disparos llegan
    // mientras hay trabajo en vuelo.
    wait_for(&rounds, 1).await;
    assert!(service.in_flight(), "la primera ronda debe estar en vuelo");
    service.trigger();
    service.trigger();
    service.trigger();
    // `notify_one` guarda un permiso, así que no hay carrera entre el
    // momento en que el test despierta la compuerta y el momento en que la
    // ronda se registra en ella.
    gate.notify_one();

    // La ráfaga se coalesce en una única ronda extra.
    wait_for(&rounds, 2).await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(
        peak.load(Ordering::Relaxed),
        1,
        "dos rondas del mismo contrato nunca se solapan"
    );
    assert_eq!(
        rounds.load(Ordering::Relaxed),
        2,
        "una ráfaga de disparos cuesta una sola ronda extra"
    );
    assert_eq!(service.stats().triggered, 3);
    supervisor.shutdown_and_wait().await;
}

#[tokio::test]
async fn cancellation_stops_the_loop_without_starting_a_round() {
    let rounds = Arc::new(AtomicU64::new(0));
    let supervisor = BackgroundSupervisor::new();
    let service = Arc::new(counting_service(
        Arc::clone(&rounds),
        Arc::new(Notify::new()),
        Arc::new(AtomicBool::new(false)),
        policy().with_startup_refresh(false),
    ));
    assert!(service.spawn_under(&supervisor));

    supervisor.shutdown_and_wait().await;
    assert_eq!(
        rounds.load(Ordering::Relaxed),
        0,
        "la cancelación durante la espera no debe lanzar una ronda"
    );
}

/// Un token ya cancelado no ejecuta ni la ronda de arranque.
#[tokio::test]
async fn an_already_cancelled_token_before_startup_runs_no_round() {
    let rounds = Arc::new(AtomicU64::new(0));
    let started = Arc::new(Notify::new());
    let service = counting_service(
        Arc::clone(&rounds),
        Arc::clone(&started),
        Arc::new(AtomicBool::new(false)),
        policy(),
    );
    let cancel = CancellationToken::new();
    cancel.cancel();

    service.run(cancel).await;

    assert_eq!(
        service.stats().rounds,
        0,
        "un token cancelado antes del arranque no debe ejecutar la ronda inicial"
    );
    assert_eq!(rounds.load(Ordering::Relaxed), 0);
    assert!(!service.in_flight(), "la compuerta queda intacta");
}

/// La cancelación durante una adquisición pendiente descarta la ronda sin
/// publicar nada, y el trabajo llega a observar el token.
#[tokio::test]
async fn cancellation_during_a_pending_acquisition_publishes_nothing() {
    let entered = Arc::new(Notify::new());
    let observed = Arc::new(AtomicBool::new(false));
    let supervisor = BackgroundSupervisor::new();
    let service = Arc::new(ContractRefreshService::new(
        PendingJob {
            entered: Arc::clone(&entered),
            observed_cancel: Arc::clone(&observed),
        },
        policy(),
    ));
    assert!(service.spawn_under(&supervisor));

    // La ronda de arranque está en vuelo, pendiente en la adquisición.
    entered.notified().await;
    assert!(service.in_flight(), "la ronda está en vuelo");

    supervisor.shutdown_and_wait().await;

    assert!(
        observed.load(Ordering::Relaxed),
        "el trabajo debe recibir el token para poder descartar la adquisición"
    );
    assert_eq!(
        service.stats().cancelled,
        1,
        "la ronda se contabiliza como descartada, no como fallida"
    );
    assert_eq!(service.stats().failures, 0, "cancelar no es fallar");
    assert_eq!(service.stats().published, 0, "no se publicó nada");
    assert!(!service.in_flight(), "la compuerta se suelta al descartar");
}

/// Dos handles del mismo grupo, admitidos bajo el mismo supervisor, nunca
/// solapan rondas: la compuerta es compartida, no por handle.
#[tokio::test]
async fn two_handles_spawned_under_one_supervisor_never_overlap() {
    let (supervisor, service, rounds, peak) = slow_service();
    assert!(service.spawn_under(&supervisor));
    assert!(service.spawn_under(&supervisor));

    // Ambas rondas de arranque compiten por la misma compuerta.
    wait_for(&rounds, 2).await;
    supervisor.shutdown_and_wait().await;

    assert_eq!(
        service.stats().rounds,
        2,
        "cada handle ejecuta su propia ronda"
    );
    assert_eq!(
        peak.load(Ordering::Relaxed),
        1,
        "la compuerta compartida impide el solapamiento entre handles"
    );
}

/// Cancelar al dueño con una ronda en vuelo no deja la compuerta
/// *wedged*: el guard la suelta y una ronda posterior puede tomarla.
#[tokio::test]
async fn cancelling_the_owner_never_wedges_the_gate() {
    let (supervisor, service, rounds, peak) = slow_service();
    assert!(service.spawn_under(&supervisor));

    // Ronda en vuelo cuando llega la parada.
    wait_for(&rounds, 1).await;
    assert!(service.in_flight(), "la ronda está en vuelo");
    supervisor.shutdown_and_wait().await;
    assert!(
        !service.in_flight(),
        "la compuerta no debe quedarse wedged tras cancelar al dueño"
    );

    // La compuerta quedó libre: otro supervisor puede admitir otra ronda.
    let other = BackgroundSupervisor::new();
    assert!(
        service.spawn_under(&other),
        "una compuerta wedged rechazaría o colgaría la ronda siguiente"
    );
    wait_for(&rounds, 2).await;
    other.shutdown_and_wait().await;
    assert_eq!(peak.load(Ordering::Relaxed), 1);
}
