//! Ciclo de vida compartido de los contratos de cliente auto-refrescados.
//!
//! Todo contrato de proveedor (Codex hoy, los demás después) necesita el mismo
//! bucle alrededor: una ronda al arrancar y luego cada `interval`; ante un
//! fallo, una espera con backoff exponencial acotado en vez de machacar el
//! upstream; nunca dos rondas del mismo contrato a la vez; y parada limpia sin
//! dejar una generación publicada a medias.
//!
//! Este módulo es dueño **solo** de ese bucle. No adquiere nada, no valida
//! nada y no persiste nada: un [`ContractRefreshJob`] aporta una ronda
//! (típicamente `CodexReleaseDriver::refresh` seguido de
//! [`ContractStore::commit`]) y el crate base conserva todos los guards. Aquí
//! no hay un refresher por proveedor a propósito: la cadencia, el backoff y el
//! single-flight son idénticos para todos, así que viven en un solo sitio.
//!
//! ## Single-flight real
//!
//! La exclusión entre rondas **no** es una bandera booleana escrita a mano:
//! es un `tokio::sync::Mutex<()>` compartido por todos los handles nacidos de
//! [`ContractRefreshService::spawn_under`]. Una ronda toma el lock al empezar y
//! lo suelta al terminar; mientras lo tiene, ninguna otra ronda —de este
//! handle o de cualquier otro del mismo grupo— puede empezar. La bandera
//! `in_flight` es solo la proyección observable de ese lock.
//!
//! El lock lo suelta un guard RAII ([`InFlightGuard`]) en su `Drop`, no una
//! escritura manual al final del cuerpo. Si el futuro de una ronda se cancela
//! o se descarta, el guard cae igual y la compuerta queda libre: el servicio
//! no puede quedarse *wedged* con `in_flight` en `true`.
//!
//! ## Cancelación
//!
//! La cancelación tiene tres puntos de comprobación, todos **antes** de tocar
//! el upstream:
//!
//! 1. al entrar en el bucle, de modo que un token ya cancelado no ejecuta ni
//!    la ronda de arranque;
//! 2. al empezar cada ronda, antes de adquirir la compuerta;
//! 3. mientras se espera la compuerta, para que una ronda encolada detrás de
//!    otra no retrase la parada.
//!
//! La espera entre rondas es asimismo cancelable.
//!
//! ## Frontera de commit
//!
//! Hay una frontera que la cancelación **no** cruza: la que separa la
//! adquisición del commit. El trabajo recibe el token
//! ([`ContractRefreshJob::refresh_round`]) y es responsable de observarlo
//! mientras adquiere y valida; en cuanto empieza a commitear, deja de
//! observarlo y drena hasta el final. Un commit ya iniciado publica la
//! generación completa o no publica nada: nunca una parcial, porque el store
//! commitea en un hilo bloqueante con rename atómico y abortar ese hilo no es
//! posible ni deseable.
//!
//! En términos de [`RoundOutcome`]: una ronda descartada por cancelación
//! devuelve [`RoundOutcome::Cancelled`] y no ha tocado el disco; cualquier
//! otro resultado significa que el commit ya está drenado.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use openproxy_adapters::client_contract::UpstreamHttpTransport;
use openproxy_adapters::client_contract::providers::codex_runtime::{
    CodexProfile, CodexReleaseDriver,
};
use openproxy_adapters::client_contract::providers::codex_state;
use openproxy_adapters::client_contract::{Candidate, ContractError, ContractStore};
use openproxy_adapters::upstream::{CancellationToken, UpstreamClient};
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::OwnedMutexGuard;

use super::BackgroundService;
use super::runner::{DriverContractJob, RefreshableDriver};

/// Cadencia de un contrato sano: una ronda cada seis horas.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// Primer escalón de backoff tras una ronda fallida.
pub const DEFAULT_BACKOFF_BASE: Duration = Duration::from_secs(30);
/// Techo del backoff: ningún fallo hace esperar más que esto.
pub const DEFAULT_BACKOFF_MAX: Duration = Duration::from_secs(15 * 60);

/// Lo que produjo una ronda, con independencia del proveedor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoundOutcome {
    /// La generación activa sigue en pie; no se persistió nada.
    Unchanged,
    /// Una generación nueva se validó, persistió y publicó.
    Activated { generation: String },
    /// La ronda se descartó por cancelación antes de tocar el upstream o el
    /// disco. No es un fallo: no cuenta para el backoff ni ensucia la foto de
    /// contadores con una ronda que nunca llegó a hacer trabajo.
    Cancelled,
}

/// El trabajo de refresh de un proveedor: adquirir, validar a través del store
/// del base y publicar. El ciclo de vida de [`ContractRefreshService`] llama a
/// esto y a nada más; la planificación, el backoff, el single-flight y la
/// cancelación viven ahí.
///
/// # Frontera de commit
///
/// El token se pasa para que el trabajo pueda **abandonar la adquisición**
/// cuando el supervisor pare el servicio: consultarlo mientras resuelve el pin
/// o descarga bytes evita que una ronda pendiente retrase la parada, y
/// devolviendo [`RoundOutcome::Cancelled`] garantiza que no se publicó nada.
///
/// En cuanto el trabajo empieza a commitear debe **dejar de mirar el token**:
/// el commit se drena hasta el final. Una implementación que abortase a medias
/// un commit podría dejar una generación publicada a medias, que es
/// exactamente lo que este bucle existe para impedir. En la práctica el
/// commit vive en [`publish`], sobre `spawn_blocking`, así que la frontera es
/// el propio `await` de `publish`.
pub trait ContractRefreshJob: Send + Sync + 'static {
    /// Nombre estable del servicio, para tracing y para el registro del
    /// supervisor.
    fn name(&self) -> &'static str;

    /// Ejecuta exactamente una ronda.
    ///
    /// El `+ Send` es la razón de escribirlo como `impl Future` en vez de
    /// `async fn`: el bucle se ejecuta dentro de una tarea `Send`, así que el
    /// trabajo debe garantizarlo. Las implementaciones siguen escribiéndose con
    /// `async fn` sin cambiar nada.
    ///
    /// `cancel` es el token del supervisor. Obsérvelo durante la adquisición;
    /// ignórelo una vez iniciado el commit.
    fn refresh_round(
        &self,
        cancel: &CancellationToken,
    ) -> impl std::future::Future<Output = Result<RoundOutcome, ContractError>> + Send;
}

/// Cadencia, backoff y tope de rondas de un refresher de contrato.
#[derive(Clone, Copy, Debug)]
pub struct RefreshPolicy {
    /// Cadencia en régimen entre rondas correctas.
    pub interval: Duration,
    /// Primera espera tras una ronda fallida; se duplica por fallo consecutivo.
    pub backoff_base: Duration,
    /// Techo del backoff y, por tanto, de cualquier espera por fallos.
    pub backoff_max: Duration,
    /// Ejecutar una ronda en cuanto arranca el servicio, antes de la primera
    /// espera.
    pub startup_refresh: bool,
    /// Tope de rondas: parar tras esta cantidad. `None` no para nunca.
    pub max_rounds: Option<u32>,
}

impl Default for RefreshPolicy {
    fn default() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
            backoff_base: DEFAULT_BACKOFF_BASE,
            backoff_max: DEFAULT_BACKOFF_MAX,
            startup_refresh: true,
            max_rounds: None,
        }
    }
}

impl RefreshPolicy {
    /// Cadencia en régimen.
    #[must_use]
    pub const fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Activar o desactivar la ronda de arranque.
    #[must_use]
    pub const fn with_startup_refresh(mut self, startup_refresh: bool) -> Self {
        self.startup_refresh = startup_refresh;
        self
    }

    /// Fijar el tope de rondas. `None` deja el bucle sin fin.
    #[must_use]
    pub const fn with_max_rounds(mut self, max_rounds: Option<u32>) -> Self {
        self.max_rounds = max_rounds;
        self
    }
}

/// Contadores observables de un refresher. Atómicos para poder leerse desde la
/// API de administración sin bloquear la ronda en curso.
#[derive(Debug, Default)]
struct Counters {
    rounds: AtomicU64,
    published: AtomicU64,
    unchanged: AtomicU64,
    failures: AtomicU64,
    cancelled: AtomicU64,
    triggered: AtomicU64,
    consecutive_failures: AtomicU32,
    /// `Retry-After` observado en la última ronda fallida, en segundos.
    retry_after_secs: AtomicU64,
}

/// Foto inmutable de los contadores, para la API de administración y los tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RefreshStats {
    /// Rondas ejecutadas, incluidas las descartadas por cancelación.
    pub rounds: u64,
    /// Rondas que activaron una generación nueva.
    pub published: u64,
    /// Rondas que dejaron la generación activa en pie.
    pub unchanged: u64,
    /// Rondas que fallaron.
    pub failures: u64,
    /// Rondas descartadas por cancelación antes de tocar el upstream.
    pub cancelled: u64,
    /// Fallos consecutivos actuales; `0` tras una ronda correcta.
    pub consecutive_failures: u32,
    /// Peticiones de ronda inmediata recibidas.
    pub triggered: u64,
    /// Si hay una ronda en curso ahora mismo.
    pub in_flight: bool,
}

/// Si el bucle debe continuar o ya ha agotado su tope de rondas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

/// Guard del single-flight compartido.
///
/// Mantiene el lock de la compuerta vivo mientras la ronda corre y proyecta su
/// estado sobre la bandera `in_flight`. Al caer el guard —también cuando el
/// futuro de la ronda se cancela o se descarta a mitad de camino— el lock se
/// libera y la bandera baja, de modo que el servicio nunca queda *wedged*.
struct InFlightGuard {
    /// Solo existe para mantener el lock; soltarlo suelta la compuerta.
    _lock: OwnedMutexGuard<()>,
    /// Bandera observable compartida por todos los handles del grupo.
    flag: Arc<AtomicBool>,
}

impl InFlightGuard {
    /// Toma la compuerta compartida respetando la cancelación.
    ///
    /// Devuelve `None` si el token ya está cancelado o si se cancela mientras
    /// se espera el lock, es decir si la ronda se descarta antes de empezar.
    /// En ningún caso se ejecuta trabajo del proveedor.
    async fn acquire(
        gate: Arc<Mutex<()>>,
        flag: Arc<AtomicBool>,
        cancel: &CancellationToken,
    ) -> Option<Self> {
        if cancel.is_cancelled() {
            return None;
        }
        let lock = tokio::select! {
            biased;
            () = cancel.cancelled() => return None,
            lock = gate.lock_owned() => lock,
        };
        flag.store(true, Ordering::Relaxed);
        Some(Self { _lock: lock, flag })
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Relaxed);
    }
}

/// El servicio de refresh de un contrato, genérico sobre el trabajo concreto.
///
/// Una sola ronda en vuelo por contrato: la compuerta compartida
/// (`Arc<tokio::sync::Mutex<()>>`) lo garantiza entre todos los handles
/// nacidos de [`Self::spawn_under`], no solo dentro de un bucle.
///
/// El alcance es exactamente ese grupo: la compuerta se comparte al admitir
/// handles con [`Self::spawn_under`]. Dos servicios construidos por separado
/// con [`Self::new`] —aunque hablen del mismo store— tienen compuertas
/// distintas y podrían solapar rondas del mismo contrato. El dueño de un
/// contrato debe ser un único servicio, y todo handle adicional debe nacer de
/// su [`Self::spawn_under`].
pub struct ContractRefreshService<J> {
    job: Arc<J>,
    policy: RefreshPolicy,
    /// Peticiones de ronda inmediata, coalescidas: una ráfaga de `trigger`
    /// cuesta como máximo una ronda extra.
    trigger: Arc<Notify>,
    counters: Arc<Counters>,
    /// Compuerta del single-flight. Compartida por todo el grupo de handles.
    gate: Arc<Mutex<()>>,
    /// Proyección observable de la compuerta. Compartida por el grupo.
    in_flight: Arc<AtomicBool>,
}

impl<J: ContractRefreshJob> ContractRefreshService<J> {
    /// Construye el servicio con la política indicada.
    #[must_use]
    pub fn new(job: J, policy: RefreshPolicy) -> Self {
        Self {
            job: Arc::new(job),
            policy,
            trigger: Arc::new(Notify::new()),
            counters: Arc::new(Counters::default()),
            gate: Arc::new(Mutex::new(())),
            in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    /// El trabajo que este servicio ejecuta, para inspección.
    #[must_use]
    pub fn job(&self) -> &Arc<J> {
        &self.job
    }

    /// La política en vigor.
    #[must_use]
    pub const fn policy(&self) -> &RefreshPolicy {
        &self.policy
    }

    /// Foto actual de los contadores.
    #[must_use]
    pub fn stats(&self) -> RefreshStats {
        let counters = &self.counters;
        RefreshStats {
            rounds: counters.rounds.load(Ordering::Relaxed),
            published: counters.published.load(Ordering::Relaxed),
            unchanged: counters.unchanged.load(Ordering::Relaxed),
            failures: counters.failures.load(Ordering::Relaxed),
            cancelled: counters.cancelled.load(Ordering::Relaxed),
            consecutive_failures: counters.consecutive_failures.load(Ordering::Relaxed),
            triggered: counters.triggered.load(Ordering::Relaxed),
            in_flight: self.in_flight.load(Ordering::Relaxed),
        }
    }

    /// Si hay una ronda en curso ahora mismo, en este handle o en cualquier
    /// otro del mismo grupo.
    #[must_use]
    pub fn in_flight(&self) -> bool {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Pide una ronda inmediata.
    ///
    /// Coalescida con cualquier ronda en curso o ya encolada: cien llamadas
    /// seguidas cuestan una sola ronda extra. El llamante debe limitar la
    /// frecuencia; aquí no hay limitación de tasa a propósito.
    pub fn trigger(&self) {
        self.counters.triggered.fetch_add(1, Ordering::Relaxed);
        self.trigger.notify_one();
    }

    /// Admite una instancia bajo `supervisor`, compartiendo el trabajo, la
    /// compuerta, la bandera de vuelo y los contadores de este handle, de modo
    /// que el llamante conserva uno para [`Self::trigger`] y [`Self::stats`].
    ///
    /// Precisamente porque la compuerta se comparte, dos handles admitidos así
    /// nunca solapan rondas: el segundo espera el lock del primero.
    pub fn spawn_under(&self, supervisor: &super::BackgroundSupervisor) -> bool {
        supervisor.spawn(Self {
            job: Arc::clone(&self.job),
            policy: self.policy,
            trigger: Arc::clone(&self.trigger),
            counters: Arc::clone(&self.counters),
            gate: Arc::clone(&self.gate),
            in_flight: Arc::clone(&self.in_flight),
        })
    }
}

impl<J: ContractRefreshJob> BackgroundService for ContractRefreshService<J> {
    fn name(&self) -> &'static str {
        self.job.name()
    }

    async fn run(&self, cancel: CancellationToken) {
        // Un token ya cancelado no ejecuta ni la ronda de arranque.
        if cancel.is_cancelled() {
            return;
        }
        if self.policy.startup_refresh && self.round(&cancel).await == Flow::Stop {
            return;
        }
        loop {
            if !self.wait_or_cancel(&cancel).await {
                return;
            }
            if self.round(&cancel).await == Flow::Stop {
                return;
            }
        }
    }
}

impl<J: ContractRefreshJob> ContractRefreshService<J> {
    /// Espera la próxima ronda: el temporizador, una petición de `trigger` o la
    /// cancelación, lo que ocurra antes. `false` significa cancelación.
    async fn wait_or_cancel(&self, cancel: &CancellationToken) -> bool {
        let wait = self.next_wait();
        // `enable` registra el interés antes de evaluar la `select`, así que un
        // `trigger` llegado durante el registro no se pierde.
        let notified = self.trigger.notified();
        tokio::pin!(notified);
        let _ = notified.as_mut().enable();
        tokio::select! {
            biased;
            () = cancel.cancelled() => false,
            () = notified => true,
            () = tokio::time::sleep(wait) => true,
        }
    }

    /// Si el tope de rondas ya está consumido.
    ///
    /// Se consulta dos veces por ronda: antes de la compuerta y otra vez con
    /// el lock en la mano, porque entre la primera comprobación y el
    /// incremento puede colarse otro handle del mismo grupo.
    fn rounds_ceiling_reached(&self) -> bool {
        let Some(cap) = self.policy.max_rounds else {
            return false;
        };
        self.counters.rounds.load(Ordering::Relaxed) >= u64::from(cap)
    }

    /// Ejecuta una ronda y contabiliza su resultado. Devuelve [`Flow::Stop`]
    /// cuando se ha agotado el tope de rondas o cuando la ronda se descarta por
    /// cancelación antes de empezar.
    ///
    /// La compuerta se toma aquí y se suelta al caer el guard, al final de esta
    /// función o antes si el futuro se cancela. Una ronda descartada mientras
    /// se esperaba la compuerta cuenta como `rounds` y como `cancelled`, para
    /// que la identidad `rounds = published + unchanged + failures + cancelled`
    /// se mantenga.
    ///
    /// El tope de rondas se comprueba dos veces —antes de la compuerta y de
    /// nuevo con el lock en la mano— porque dos handles del mismo grupo pueden
    /// pasar la primera comprobación antes de que ninguno haya contado su
    /// ronda; el segundo chequeo hace el tope exacto.
    async fn round(&self, cancel: &CancellationToken) -> Flow {
        if self.rounds_ceiling_reached() {
            return Flow::Stop;
        }
        let Some(guard) =
            InFlightGuard::acquire(Arc::clone(&self.gate), Arc::clone(&self.in_flight), cancel)
                .await
        else {
            // Descartada antes de tocar el upstream: no es un fallo.
            self.counters.rounds.fetch_add(1, Ordering::Relaxed);
            self.counters.cancelled.fetch_add(1, Ordering::Relaxed);
            return Flow::Stop;
        };
        // Re-chequeo bajo la compuerta: el chequeo previo puede pasarlo más de
        // un handle antes de que ninguno haya contado su ronda, y los dos
        // verían hueco. Con el lock en la mano el tope vuelve a ser exacto:
        // nadie más puede colarse entre la comprobación y el incremento.
        if self.rounds_ceiling_reached() {
            return Flow::Stop;
        }
        self.counters.rounds.fetch_add(1, Ordering::Relaxed);
        // Frontera de commit: el trabajo observa el token mientras adquiere y
        // lo ignora en cuanto empieza a commitear. El bucle espera el resultado
        // hasta el final, sin `select`, porque interrumpir aquí podría partir un
        // commit por la mitad.
        let outcome = self.job.refresh_round(cancel).await;
        self.account(&outcome);
        drop(guard);
        if self.rounds_ceiling_reached() {
            Flow::Stop
        } else {
            Flow::Continue
        }
    }

    /// Vuelve a la cadencia en régimen: sin fallos ni pista de `Retry-After`.
    fn reset_backoff(&self) {
        self.counters
            .consecutive_failures
            .store(0, Ordering::Relaxed);
        self.counters.retry_after_secs.store(0, Ordering::Relaxed);
    }

    /// Cuenta el resultado de la ronda y ajusta el backoff.
    fn account(&self, outcome: &Result<RoundOutcome, ContractError>) {
        match outcome {
            Ok(RoundOutcome::Activated { generation }) => {
                self.counters.published.fetch_add(1, Ordering::Relaxed);
                self.reset_backoff();
                tracing::info!(
                    service = self.job.name(),
                    generation = %generation,
                    "contract generation activated"
                );
            }
            Ok(RoundOutcome::Unchanged) => {
                self.counters.unchanged.fetch_add(1, Ordering::Relaxed);
                self.reset_backoff();
                tracing::debug!(service = self.job.name(), "contract unchanged");
            }
            Ok(RoundOutcome::Cancelled) => {
                self.counters.cancelled.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    service = self.job.name(),
                    "contract refresh round discarded due to cancellation"
                );
            }
            Err(error) => {
                self.counters.failures.fetch_add(1, Ordering::Relaxed);
                self.counters
                    .consecutive_failures
                    .fetch_add(1, Ordering::Relaxed);
                let retry_after = match error {
                    ContractError::RateLimited { retry_after, .. } => {
                        retry_after.unwrap_or_default()
                    }
                    _ => 0,
                };
                self.counters
                    .retry_after_secs
                    .store(retry_after, Ordering::Relaxed);
                tracing::warn!(
                    service = self.job.name(),
                    %error,
                    failures = self.counters.consecutive_failures.load(Ordering::Relaxed),
                    "contract refresh round failed; retaining last known good generation"
                );
            }
        }
    }

    /// Cuánto esperar antes de la próxima ronda.
    ///
    /// Sin fallos, la cadencia. Con fallos, `backoff_base` duplicado por fallo
    /// consecutivo, siempre acotado por `backoff_max` y por `interval`; un
    /// `Retry-After` observado solo puede alargar la espera dentro de ese mismo
    /// techo, nunca saltárselo.
    fn next_wait(&self) -> Duration {
        let failures = self.counters.consecutive_failures.load(Ordering::Relaxed);
        if failures == 0 {
            return self.policy.interval;
        }
        let ceiling = self.policy.backoff_max.min(self.policy.interval);
        let floor = self.policy.backoff_base.min(ceiling);
        let shift = failures.saturating_sub(1).min(31);
        let scaled = self.policy.backoff_base.saturating_mul(1u32 << shift);
        let mut wait = scaled.min(ceiling).max(floor);
        let retry_after = self.counters.retry_after_secs.load(Ordering::Relaxed);
        if retry_after > 0 {
            wait = wait.max(Duration::from_secs(retry_after)).min(ceiling);
        }
        wait
    }
}

/// Commitea un candidato a través del store del base en un hilo bloqueante.
///
/// El store es dueño de la validación, del anti-downgrade y de la escritura
/// atómica; esto solo saca esa IO sincrónica del runtime async y traduce el
/// resultado a un [`RoundOutcome`]. Un `Ok(None)` del store significa que el
/// candidato era byte-idéntico a la generación activa, o sea que no hay nada
/// nuevo que publicar.
///
/// Esta función **es la frontera de commit**: una vez dentro, la cancelación
/// del supervisor no puede ya interrumpir la escritura. Los dos `?` seguidos
/// son el `JoinError` de `spawn_blocking` y después el veredicto real del
/// store; aplanarlos aquí mantiene el resto del cuerpo legible y deja que un
/// candidato rechazado viaje como error.
pub async fn publish<T>(
    store: &Arc<ContractStore<T>>,
    candidate: Candidate<T>,
) -> Result<RoundOutcome, ContractError>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Clone + Send + Sync + 'static,
{
    let store = Arc::clone(store);
    let committed = tokio::task::spawn_blocking(move || store.commit(&candidate))
        .await
        .map_err(|error| {
            ContractError::Rejected(format!("contract commit task failed: {error}"))
        })??;
    Ok(committed.map_or(RoundOutcome::Unchanged, |snapshot| {
        RoundOutcome::Activated {
            generation: snapshot.generation.clone(),
        }
    }))
}

impl RefreshableDriver<CodexProfile> for CodexReleaseDriver {
    fn refresh(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<Candidate<CodexProfile>>, ContractError>> + Send
    {
        self.refresh()
    }
}

/// La ronda de Codex: resuelve el pin de release sobre el transporte compartido
/// y commitea el candidato en el store del base. El driver ya reutiliza
/// [`TransportCache`](openproxy_adapters::client_contract::TransportCache), los
/// límites del base y la semántica `304`; aquí solo se añade la persistencia.
///
/// La cancelación se observa **solo** mientras se resuelve el pin y se
/// construye el candidato. En cuanto hay candidato, [`publish`] drena el commit
/// hasta el final, así que una parada del supervisor no puede dejar una
/// generación publicada a medias.
pub struct CodexContractJob {
    inner: DriverContractJob<CodexReleaseDriver, CodexProfile>,
}

impl CodexContractJob {
    /// Construye el trabajo sobre un driver y un store que el llamante ya tiene.
    #[must_use]
    pub fn new(driver: Arc<CodexReleaseDriver>, store: Arc<ContractStore<CodexProfile>>) -> Self {
        Self {
            inner: DriverContractJob::new("codex_contract", driver, store),
        }
    }

    /// El driver, para inspección o precalentamiento de la cache.
    #[must_use]
    pub fn driver(&self) -> &Arc<CodexReleaseDriver> {
        self.inner.driver()
    }

    /// El store, para leer la generación publicada.
    #[must_use]
    pub fn store(&self) -> &Arc<ContractStore<CodexProfile>> {
        self.inner.store()
    }
}

impl ContractRefreshJob for CodexContractJob {
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

/// Envuelve el trabajo de Codex en el ciclo de vida compartido.
#[must_use]
pub fn codex_refresh_service(
    driver: Arc<CodexReleaseDriver>,
    store: Arc<ContractStore<CodexProfile>>,
    policy: RefreshPolicy,
) -> ContractRefreshService<CodexContractJob> {
    ContractRefreshService::new(CodexContractJob::new(driver, store), policy)
}

/// El servicio productivo de Codex: driver y store salen del runtime global
/// ([`codex_state::runtime`]), el MISMO que sirve el hot path de peticiones.
///
/// Un solo store y una sola cache por proceso: una generación activada por
/// este refresher es visible de inmediato para `pin_request_contract`, sin un
/// segundo store desconectado. El transporte reutiliza el [`UpstreamClient`]
/// compartido del server (mismo pool, TLS y postura SSRF que el resto de
/// llamadas de contrato).
#[must_use]
pub fn codex_runtime_refresh_service(
    upstream_client: Arc<UpstreamClient>,
) -> ContractRefreshService<CodexContractJob> {
    let runtime = codex_state::runtime();
    let transport = Arc::new(UpstreamHttpTransport::new(upstream_client));
    let driver = runtime.release_driver(transport);
    let store = runtime.store_handle();
    ContractRefreshService::new(
        CodexContractJob::new(driver, store),
        RefreshPolicy::default(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::pedantic, clippy::wildcard_imports)]

    use super::*;
    use crate::background::BackgroundSupervisor;
    use openproxy_adapters::client_contract::{
        ClientIdentity, ContractId, ContractLimits, ContractTransport, ContractValidator,
        EvidenceKind, FetchRequest, FetchResponse, IDENTITY_ORIGIN_OFFICIAL, Snapshot, SourceRef,
        StoreConfig, TransportCache,
    };
    use std::pin::Pin;
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
    fn policy() -> RefreshPolicy {
        RefreshPolicy::default().with_interval(Duration::from_secs(3600))
    }

    /// Cuenta rondas, avisa en cuanto empieza una y falla mientras `fail` valga.
    struct CountingJob {
        rounds: Arc<AtomicU64>,
        started: Arc<Notify>,
        fail: Arc<AtomicBool>,
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

    #[tokio::test]
    async fn a_startup_round_runs_before_the_first_wait_and_stops_at_the_ceiling() {
        let rounds = Arc::new(AtomicU64::new(0));
        let started = Arc::new(Notify::new());
        let supervisor = BackgroundSupervisor::new();
        let service = Arc::new(ContractRefreshService::new(
            CountingJob {
                rounds: Arc::clone(&rounds),
                started: Arc::clone(&started),
                fail: Arc::new(AtomicBool::new(false)),
            },
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
        let service = Arc::new(ContractRefreshService::new(
            CountingJob {
                rounds: Arc::clone(&rounds),
                started: Arc::clone(&started),
                fail: Arc::clone(&fail),
            },
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
        let service = ContractRefreshService::new(
            CountingJob {
                rounds: Arc::new(AtomicU64::new(0)),
                started: Arc::new(Notify::new()),
                fail: Arc::new(AtomicBool::new(false)),
            },
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
        let service = Arc::new(ContractRefreshService::new(
            CountingJob {
                rounds: Arc::clone(&rounds),
                started: Arc::new(Notify::new()),
                fail: Arc::new(AtomicBool::new(false)),
            },
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
        let service = ContractRefreshService::new(
            CountingJob {
                rounds: Arc::clone(&rounds),
                started: Arc::clone(&started),
                fail: Arc::new(AtomicBool::new(false)),
            },
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
            // Sin tope de rondas: los contadores son compartidos, así que un
            // tope cortaría el segundo handle antes de su ronda de arranque.
            policy(),
        ));
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
        ) -> Pin<
            Box<dyn std::future::Future<Output = Result<FetchResponse, ContractError>> + Send + '_>,
        > {
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
}
