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
mod tests_basic;
#[cfg(test)]
mod tests_codex;
