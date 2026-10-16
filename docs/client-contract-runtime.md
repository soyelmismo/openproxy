# Runtime del contrato de cliente

Este documento describe la API pública de
`crates/openproxy-server/src/background/client_contract.rs` y, sobre todo, las
**garantías reales** que ofrece. Si algo de lo escrito aquí no está sostenido
por el código, es un bug del código, no del documento.

> **Estado de integración:** Claude Code, Codex y CodeBuddy están conectados
> al ciclo compartido bajo el supervisor en `state/background.rs`.
> Validación del padre el 2026-10-14, sin stubs: **16 tests del ciclo común**,
> **3 tests de factories/cadencia** y **1 test de supervisor** aprobados.
> Workspace: fmt, check y clippy `-D warnings` correctos. Suite completa:
> **2904 aprobados, 1 fallo remoto OpenCode (HTTP 403), 12 ignorados**.
> Wiring persistente en código, **sin deploy ni reinicios**.

## Qué es y qué no es

El módulo es dueño **solo** del bucle: cadencia, backoff, single-flight,
cancelación y drenaje. No adquiere nada, no valida nada y no persiste nada.
Eso es trabajo de [`ContractRefreshJob`], que en la práctica es
`CodexReleaseDriver::refresh` seguido de `ContractStore::commit`. El crate
base conserva todos los guards (anti-downgrade, límites, escritura atómica).

## Las cuatro garantías

### 1. Single-flight real y compartido

La exclusión entre rondas es un `Arc<tokio::sync::Mutex<()>>` en
`ContractRefreshService::gate`, **no** un `AtomicBool` escrito a mano. Se
comparte por `Arc::clone` en `spawn_under`, así que cubre dos handles distintos
del mismo grupo, no un solo bucle.

* Una ronda toma el lock al empezar y lo suelta al terminar.
* Mientras lo tiene, ninguna otra ronda puede empezar: ni de este handle, ni
  de otro nacido del mismo `spawn_under`.
* `in_flight` es solo la proyección observable del lock, no la fuente de
  exclusión.

Lo que esto **no** garantiza: el alcance del single-flight es exactamente el
grupo, y el grupo se forma al admitir handles con `spawn_under`. Dos servicios
construidos con `new()` —aunque hablen del mismo store— tienen compuertas
distintas y sus rondas pueden solaparse. Para un contrato dado hay que usar un
único `ContractRefreshService` como dueño y colgar de él todos los handles.

### 2. Liberación segura de la compuerta

El lock lo suelta `InFlightGuard` en su `Drop`. No hay ninguna escritura
manual de `in_flight = false` al final del cuerpo de la ronda.

* Si el futuro de una ronda se cancela o se descarta a mitad de camino, el
  guard cae y el lock se libera.
* El servicio no puede quedarse *wedged* con `in_flight` en `true`.

### 3. Cancelación cooperativa en tres puntos

Todos **antes** de tocar el upstream o el disco:

| Punto | Dónde | Efecto |
|---|---|---|
| Entrada al bucle | `run` | un token ya cancelado no ejecuta ni la ronda de arranque |
| Antes de la compuerta | `round` + `InFlightGuard::acquire` | una ronda encolada detrás de otra no retrasa la parada |
| Espera entre rondas | `wait_or_cancel` | la espera de cadencia o de backoff es interrumpible |

Una ronda descartada así se contabiliza en `RefreshStats::cancelled` y **no**
cuenta como fallo: no ensucia el backoff.

**Invariante de contadores:** `rounds == published + unchanged + failures +
cancelled`. Una ronda descartada mientras esperaba la compuerta cuenta en
`rounds` y en `cancelled`; una descartada desde dentro del trabajo, igual. Los
tres caminos de descarte mantienen la identidad.

### 4. Frontera de commit

Hay una frontera que la cancelación no cruza. El trabajo recibe
`&CancellationToken` y es responsable de observarlo **mientras adquiere y
valida**; en cuanto empieza a commitear debe dejar de mirarlo y drenar hasta el
final.

* Antes de la frontera: cancelable. El contrato vigente queda intacto y no se
  ha tocado el disco.
* Después de la frontera: no cancelable. `publish` corre sobre
  `spawn_blocking`, y el store escribe payload y manifiesto con rename atómico.
  Una ronda que empieza a commitear publica la generación completa o no publica
  nada: nunca una parcial.
* El bucle espera el resultado de la ronda **sin** `select`. Interrumpir ahí
  sería justo partir un commit por la mitad.

En resumen: `Cancelled` ⇒ nada se publicó. `Unchanged` o `Activated` ⇒ el
commit ya está drenado.

## API pública

### Constantes

```rust
pub const DEFAULT_INTERVAL: Duration;      // 6 h
pub const DEFAULT_BACKOFF_BASE: Duration;  // 30 s
pub const DEFAULT_BACKOFF_MAX: Duration;   // 15 min
```

### `RoundOutcome`

```rust
pub enum RoundOutcome {
    Unchanged,
    Activated { generation: String },
    Cancelled,
}
```

`Cancelled` es nueva. Existe para que el llamador distinga «no se ejecutó
trabajo» de «se ejecutó y falló».

### `ContractRefreshJob`

```rust
pub trait ContractRefreshJob: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn refresh_round(
        &self,
        cancel: &CancellationToken,
    ) -> impl Future<Output = Result<RoundOutcome, ContractError>> + Send;
}
```

Cambio de firma: `refresh_round` **recibe ahora el token**. Es lo que permite
al trabajo descartar la adquisición sin publicar nada.

Contrato para los implementadores:

1. Observe `cancel` mientras adquiere y valida. Si se activa, devuelva
   `Ok(RoundOutcome::Cancelled)` sin tocar el disco.
2. En cuanto empiece a commitear, ignore el token y drene.
3. El futuro debe ser `Send` (el bucle corre en una tarea `Send`).

`CodexContractJob` es la implementación de referencia: envuelve
`driver.refresh()` en un `select!` contra `cancel.cancelled()` y, si sale
candidato, llama a `publish`, que es la frontera.

### `RefreshPolicy`

```rust
pub struct RefreshPolicy {
    pub interval: Duration,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    pub startup_refresh: bool,
    pub max_rounds: Option<u32>,
}
```

Backoff: `backoff_base` duplicado por fallo consecutivo, acotado por
`backoff_max` **y** por `interval`. Un `Retry-After` observado solo puede
alargar la espera dentro de ese techo, nunca saltárselo ni acortarla.

Constructores: `Default`, `with_interval`, `with_startup_refresh`,
`with_max_rounds`. Los campos son públicos, así que `backoff_base` y
`backoff_max` se ajustan por mutación directa del struct.

`max_rounds` es un tope **del grupo**, no de cada handle: `round` lo comprueba
antes de la compuerta y otra vez con el lock en la mano, porque dos handles
pueden pasar la primera comprobación antes de que ninguno haya contado su
ronda. Con el lock tomado el incremento es exacto y el segundo handle ve el
tope consumido.

### `RefreshStats`

```rust
pub struct RefreshStats {
    pub rounds: u64,               // incluye las descartadas por cancelación
    pub published: u64,
    pub unchanged: u64,
    pub failures: u64,
    pub cancelled: u64,            // nuevo
    pub consecutive_failures: u32,
    pub triggered: u64,
    pub in_flight: bool,
}
```

`in_flight` refleja ahora el lock compartido, no una bandera por handle.

### `ContractRefreshService<J>`

```rust
impl<J: ContractRefreshJob> ContractRefreshService<J> {
    pub fn new(job: J, policy: RefreshPolicy) -> Self;
    pub fn job(&self) -> &Arc<J>;
    pub fn policy(&self) -> &RefreshPolicy;
    pub fn stats(&self) -> RefreshStats;
    pub fn in_flight(&self) -> bool;
    pub fn trigger(&self);
    pub fn spawn_under(&self, supervisor: &BackgroundSupervisor) -> bool;
}

impl<J: ContractRefreshJob> BackgroundService for ContractRefreshService<J> {
    fn name(&self) -> &'static str;
    async fn run(&self, cancel: CancellationToken);
}
```

`spawn_under` comparte job, política, `trigger`, contadores **y la compuerta**.
Eso es lo que hace real el single-flight entre handles.

`trigger()` está coalescida: una ráfaga de cien llamadas cuesta como máximo una
ronda extra. No hay limitación de tasa a propósito.

### `publish`

```rust
pub async fn publish<T>(
    store: &Arc<ContractStore<T>>,
    candidate: Candidate<T>,
) -> Result<RoundOutcome, ContractError>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Clone + Send + Sync + 'static;
```

La frontera de commit. Delega en `ContractStore::commit` sobre `spawn_blocking`
y traduce el resultado: `Ok(None)` del store ⇒ `Unchanged`; snapshot nuevo ⇒
`Activated { generation }`. Los dos `?` seguidos son el `JoinError` de
`spawn_blocking` y después el veredicto del store.

### `CodexContractJob` y `codex_refresh_service`

```rust
pub struct CodexContractJob { /* driver, store */ }

impl CodexContractJob {
    pub fn new(driver: Arc<CodexReleaseDriver>, store: Arc<ContractStore<CodexProfile>>) -> Self;
    pub fn driver(&self) -> &Arc<CodexReleaseDriver>;
    pub fn store(&self) -> &Arc<ContractStore<CodexProfile>>;
}

pub fn codex_refresh_service(
    driver: Arc<CodexReleaseDriver>,
    store: Arc<ContractStore<CodexProfile>>,
    policy: RefreshPolicy,
) -> ContractRefreshService<CodexContractJob>;
```

`name()` devuelve `"codex_contract"`.

**Wiring integrado:** `codex_runtime_refresh_service(Arc<UpstreamClient>)`
construye el driver con el cache del runtime y usa su `store_handle()`.
Las factories Claude y CodeBuddy usan el mismo `ContractStore<T>` que sus
adapters, con payload y guards propios. Son tres servicios del mismo tipo
compartido, no un único store ni una compuerta global entre proveedores.
`state/background.rs` arranca uno por proveedor, con gates
`OPENPROXY_CLAUDE_CODE_PROFILE_AUTO_REFRESH`,
`OPENPROXY_CODEX_CONTRACT_AUTO_REFRESH` y
`OPENPROXY_CODEBUDDY_CONTRACT_AUTO_REFRESH` (default on, `0`/`false` off).
Claude captura el CLI local y mantiene cadencia de seis horas por defecto.
CodeBuddy rechaza revisiones fuera de sus guards y no eleva identidad sin
correspondencia verificable, aunque publique un catálogo válido.

## Tests

Quince tests en el archivo, todos bajo `#[cfg(test)]`, sin wiring productivo.

Los siete previos, con la firma nueva de `refresh_round`:

1. `a_startup_round_runs_before_the_first_wait_and_stops_at_the_ceiling`
2. `a_failed_round_waits_the_backoff_until_a_trigger_arrives`
3. `the_backoff_doubles_is_capped_and_honours_retry_after_within_the_ceiling`
4. `concurrent_triggers_never_overlap_two_rounds`
5. `cancellation_stops_the_loop_without_starting_a_round`
6. `a_published_candidate_reports_its_generation_and_a_repeat_reports_unchanged`
7. `the_codex_job_reports_a_transport_failure_and_publishes_nothing`

Los cinco nuevos que cubren las garantías que antes no existían:

| Test | Qué sostiene |
|---|---|
| `an_already_cancelled_token_before_startup_runs_no_round` | token cancelado antes del arranque ⇒ `rounds == 0`, compuerta intacta |
| `cancellation_during_a_pending_acquisition_publishes_nothing` | el trabajo observa el token; `cancelled == 1`, `failures == 0`, `published == 0` |
| `two_handles_spawned_under_one_supervisor_never_overlap` | dos `spawn_under` del mismo grupo: `peak == 1` |
| `cancelling_the_owner_never_wedges_the_gate` | tras cancelar al dueño, `in_flight` baja y otro supervisor puede admitir otra ronda |
| `the_codex_job_discards_an_acquisition_on_cancellation` | con token cancelado, el wrapper devuelve `Cancelled` y el store no publica |

Los tres añadidos en la ronda 2, **aprobados contra los crates reales**:

| Test | Qué sostiene |
|---|---|
| `two_rounds_queued_on_the_gate_share_one_max_rounds_ceiling` | con `max_rounds = Some(1)`, dos rondas encoladas en la compuerta ejecutan una sola ronda: el tope se re-chequea bajo el lock |
| `a_round_queued_on_the_gate_is_discarded_on_cancellation` | una ronda encolada tras la compuerta se descarta al cancelar sin ejecutar trabajo y sin esperar al dueño |
| `cancelling_during_commit_drains_before_releasing_the_gate` | cancelar con el commit en curso no lo aborta: drena, cuenta como publicación y la compuerta se suelta después |

## Verificación real del padre

El 2026-10-14 se ejecutó `validate-shared-real.py --offline` contra el repo,
sin stubs ni parches de adapters:

* Tests del módulo `background::client_contract` del paquete servidor, en modo offline: **15 aprobados, 0 fallos, 0 ignorados**.
* Clippy del paquete servidor (lib y tests), en modo offline con `-D warnings`: exit **0**.
* Declaración temporal del módulo retirada, con restauración byte a byte confirmada por el runner.
* Workspace: `cargo fmt --all -- --check`, `cargo check --offline --workspace --all-targets` y `cargo clippy --offline --workspace --all-targets -- -D warnings`: exit **0**.
* `RUST_MIN_STACK=33554432 cargo test --offline --workspace --no-fail-fast`: **2846 aprobados, 1 fallo, 12 ignorados**. El único fallo es el test remoto de OpenCode, que recibió HTTP 403 en lugar de 200. Los 15 tests del módulo no forman parte de ese conteo porque su declaración ya estaba restaurada.
* E2E: **90 aprobados** usando ese tamaño de stack. Sin él el harness debug abortó por stack overflow.

Logs del runner: `/root/.hermes/cache/scratch/contract-implementation/resume-20261014/validate-logs/20261014T135701Z/`.
Logs del workspace: `/root/.hermes/cache/scratch/contract-implementation/resume-20261014/final-validation/`.

## Verificación histórica previa, superada por la ejecución real

* `rustfmt --edition 2024` sobre el archivo: limpio (1312 líneas).
* **Harness aislado de compilación y test** en
  `scratch/contract-implementation/resume-20261014/harness/` — un mini-workspace
  fuera del repo con stubs fieles de los símbolos que el servicio consume
  (`ContractStore::commit`, `CodexReleaseDriver`, `CancellationToken` **real**
  copiado del repo, `BackgroundSupervisor`) y con el archivo de producción
  copiado **byte a byte** (verificado por sha256, `a94ee105070ca50f`):
  * `cargo test --offline`: **12 passed; 0 failed** en
    `background::client_contract::tests` (+15 del `cancel.rs` real, 0 fallos).
  * `cargo clippy --offline --all-targets` con el set de lints del workspace
    real replicado (`pedantic = deny`, `await_holding_lock = deny`,
    `unwrap_used`/`expect_used` con el mismo `cfg_attr(test)` de `lib.rs`):
    **0 errores, 0 warnings**.
  * Un fallo real encontrado y corregido durante el harness: el test del
    wrapper Codex esperaba `SourceMissing` y el stub no consultaba el
    transporte; se hizo fiel al driver real (404 ⇒ `SourceMissing`).
* Lo que el harness **no** demuestra: que el archivo compile dentro del árbol
  real con los tipos reales de `openproxy-adapters` (el stub reproduce firmas
  verificadas, no el crate entero). Esa es la verificación pendiente.
* No se ejecutó `cargo` **dentro del repo** en ningún momento: había ediciones
  concurrentes de compañeros.
* Ronda 2 (correcciones sin `cargo`, por coordinación): `rustfmt --edition
  2024` limpio sobre `client_contract.rs` (60266 B, sha256 `4596b5a11099cc37`);
  los 3 tests nuevos y el re-chequeo del tope aún sin ejecutar. Los valida la
  corrida del runner.
* La validación contra los crates reales queda en manos del runner scratch
  `validate-shared-real.py` (mismo directorio de scratch): instrumenta
  `background/mod.rs` con `pub mod client_contract;` de forma temporal, corre
  `cargo test` y `cargo clippy` en serie con los logs en scratch, y restaura
  los bytes originales solo si nadie más tocó el archivo durante la ejecución.

## Cierre de integración

Las declaraciones de módulos, factories y arranque de los tres servicios ya
están integradas. El test añadido
`the_codex_runtime_factory_reuses_the_global_store_and_cache` eleva el conteo
del ciclo común a 16. Pasaron también las factories Claude/CodeBuddy, la
cadencia Claude, el drenaje conjunto y los tests de snapshot por petición.

La corrida final usa stores separados en scratch y
`RUST_MIN_STACK=33554432`, sin escribir contratos en el store del servicio activo.
Log: `/root/.hermes/cache/scratch/contract-integration-20261014/parent-final-validation/workspace-tests-final.log`.

Pendiente operativo: deploy y comprobación del binario activo, fuera del alcance
de esta integración. El workspace no está completamente verde: el único fallo
es el test live de OpenCode que recibe HTTP 403, sin assertions modificadas.
