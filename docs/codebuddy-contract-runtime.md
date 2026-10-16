# Contrato CodeBuddy: runtime, store y pin por petición

Este documento describe cómo el proveedor CodeBuddy (`@tencent-ai/codebuddy-code`)
participa del ciclo común de contratos de cliente: adquisición pinned/condicional,
validación con guards compilados, persistencia en el `ContractStore` compartido y
congelado del snapshot por petición.

## Piezas

| Pieza | Ruta | Rol |
| --- | --- | --- |
| Parser y tipos | `crates/openproxy-adapters/src/client_contract/providers/codebuddy.rs` | Parsea `product.json`, catálogo, vendor, endpoints; define `CodeBuddyContract::identity_activatable` |
| Runtime/adquisición | `crates/openproxy-adapters/src/client_contract/providers/codebuddy_runtime.rs` | Driver del pin: registry npm + unpkg, adquisición condicional, `CodeBuddyProfile`, `CodeBuddyReleaseDriver` |
| State/store global | `crates/openproxy-adapters/src/client_contract/providers/codebuddy_state.rs` | `CodeBuddyContractRuntime`, seed embebido del pin, validador, `runtime()`, `ensure_loaded()` |
| Adapter | `crates/openproxy-adapters/src/adapters/codebuddy/mod.rs` | `pin_request_contract` + `build_headers` leyendo el snapshot congelado |
| Refresher server-side | `crates/openproxy-server/src/background/codebuddy_contract.rs` | `CodeBuddyContractJob` + factory `codebuddy_refresh_service` bajo el ciclo compartido |

El ciclo de vida (backoff, single-flight, cancelación, drenado) es el común de
`crates/openproxy-server/src/background/client_contract.rs`; CodeBuddy no duplica
esa lógica.

## Pin y versiones

- El pin compilado es la revisión **2.164.0** (`CODEBUDDY_VERSION` en el parser).
- El seed embebido construye un snapshot válido con los fixtures reales
  (`fixtures/codebuddy-extracted/package.json` y `product.json`, vía `include_str!`),
  así que el proxy arranca siempre con una generación activa aunque el disco esté vacío.
- La adquisición es **pinned/conditional**: las URLs de unpkg se construyen sobre la
  versión fijada (`pinned_package_url`, `pinned_product_url`) y la segunda ronda
  reenvía los validadores (`etag`/`last-modified`) cacheados en la `TransportCache`
  compartida. La metadata mutable del registry solo se usa para *resolver* la
  revisión; nunca para elevar la identidad por sí sola.
- Ninguna ronda ejecuta el bundle del paquete: solo se leen `package.json` y
  `product.json`.

## Gate de identidad, fail-closed

El paquete npm de CodeBuddy no expone `gitHead`. Por eso:

- `PackageCommitCorrespondence` queda en `Unverified` y
  `CodeBuddyContract::identity_activatable` es `false`.
- El validador **recomputa** ese gate desde las fuentes del candidato
  (`correspondence_from_evidence`): un payload que declare identidad activable sin
  evidencia nueva se rechaza.
- Una versión distinta de la fijada se rechaza: no hay guards compilados para esa
  revisión, así que no se finge correspondencia entre la identidad de `2.157.0` y un
  catálogo de otra revisión.
- El catálogo verificado sí se publica como overlay (mismo paquete, mismos guards,
  digest verificado), pero eso no eleva la versión anunciada.

Consecuencia operativa: si no hay evidencia suficiente, el candidato se rechaza y el
último snapshot válido permanece activo. El refresh nunca deja una generación a
medias.

## Pin por petición

`CodeBuddyAdapter::pin_request_contract` congela el snapshot activo
(`codebuddy_state::ensure_loaded()`: LKG en disco o seed embebido). A partir de ahí,
`build_headers` solo adopta la versión de `client_identity` cuando el snapshot
marca `identity_activatable=true` y declara una versión. El catálogo 2.164.0 con
identidad no verificada no eleva los headers: conserva el fallback previo.
El pipeline aplica el pin por (petición, proveedor) vía
`ProviderAdapter::pinned_for_request`.

Discovery consume el catálogo del snapshot fijado o del store activo, sin
arrancar otra ronda. Mapea IDs, nombres, límites y capacidades declaradas.
El catálogo estático de 35 modelos queda como fallback si no hay catálogo
parseable, no como sustituto del overlay publicado.

Prioridad de headers: contrato congelado < `extra_headers` de configuración (el
override explícito del despliegue gana). Sin snapshot, el spoofer conserva su
comportamiento dinámico previo.

## Refresh y almacenamiento

- `codebuddy_refresh_service(upstream_client)` construye el job sobre
  `codebuddy_state::runtime()`: **el mismo** `ContractStore` que sirve el hot path,
  así que una generación activada por el refresher es visible de inmediato.
- Ruta del LKG: `$OPENPROXY_CODEBUDDY_STORE_DIR` o `~/.openproxy/contracts`.
- Commit atómico con anti-downgrade (`check_identity_transition`), límites
  (`ContractLimits`) y `publish` drenando el commit sobre `spawn_blocking`: una
  parada del supervisor no puede dejar una generación publicada a medias.
- El auto-refresh se controla con `OPENPROXY_CODEBUDDY_CONTRACT_AUTO_REFRESH`.
- Discovery (`fetch_models`) ya **no** dispara un spawn version-only contra la
  metadata de npm: la versión solo cambia por el contrato verificado.

## Tests

- `crates/openproxy-adapters/src/adapters/codebuddy/tests.rs`: pin por petición con
  snapshot embebido (determinista, sin red) y prioridad del override de configuración.
- `crates/openproxy-core/tests/codebuddy_upstream_contract_test.rs`: discovery no
  eleva la versión por sí sola y el pin anuncia su propia generación.
- `crates/openproxy-adapters/src/client_contract/providers/codebuddy_runtime.rs`
  (tests): adquisición condicional, rechazo de vendor desconocido, pin de versión.
- `crates/openproxy-adapters/src/client_contract/providers/codebuddy_state.rs`
  (tests): seed, overlay de catálogo, rechazo sin evidencia y persistencia/restart.

## Pruebas herméticas

El refresh se prueba sin red ni disco real. `CodeBuddyContractRuntime::refresh_validated`
acepta cualquier `ContractTransport`, así que los tests construyen un runtime sobre un
directorio temporal y le inyectan un transporte-mapa que sirve los fixtures compilados
(`fixtures/codebuddy*`). Los tests cubren:

- metadata que anuncia otra versión → candidato rechazado, generación activa intacta;
- ronda pinned completa → catálogo publicado como overlay, pero `refresh_validated`
  devuelve `None` porque la identidad no es activable (sin `gitHead`);
- 429 y cuerpos no-JSON → fail-closed, generación activa intacta.
