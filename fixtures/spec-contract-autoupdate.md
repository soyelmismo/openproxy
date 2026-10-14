# Autoactualización de contratos de clientes: especificación de implementación

> **Para Hermes:** implementar por tareas tras autorización, con delegación por proveedor una vez fijada la base común y revisión de conformidad y calidad

**Objetivo:** actualizar automáticamente datos soportados de Codex, CodeBuddy y OpenCode sin anunciar una identidad nueva con un contrato anterior

**Arquitectura:** compartir adquisición condicional, procedencia, validación estructural, persistencia y activación de snapshots. Mantener extractores, payloads y reglas de protocolo propios de cada proveedor. No ejecutar código upstream en el proceso del proxy

**Stack:** workspace Rust existente, serde, Arc y locks existentes, cliente HTTP y BackgroundService existentes. No se exige una nueva dependencia de runtime

**Estado:** especificación, no implementación. No se ejecutaron build, tests, inferencia ni deploy

## 1. Evidencia y alcance

Consolidación del lote `deleg_884aa7d2`, tres tareas completed según su manifest

Fuentes consultadas:
- Codex: `/root/.hermes/cache/scratch/codex-contract-audit.md`
- OpenCode: `/root/.hermes/cache/scratch/audit-opencode-zen-go-contracts.md`
- CodeBuddy: informe en `/root/.hermes/cache/delegation/live/deleg_884aa7d2/task-1.log:269-274` y artefactos en `/root/.hermes/cache/scratch/cb-audit/`. El log contiene resúmenes truncados, no un informe Markdown íntegro. Esta especificación se apoya además en la inspección directa del paquete descargado

Revisiones comprobadas en esta consolidación:
- openproxy: `bf8458d90766`
- openai/codex: `2f05c0a71f59`, checkout de main, NO probado equivalente a la release estable
- anomalyco/opencode: `48364310480d`, checkout local de dev
- CodeBuddy: paquete `@tencent-ai/codebuddy-code@2.164.0`

GET de metadata comprobados:
- `https://api.github.com/repos/openai/codex/releases/latest` devuelve `rust-v0.162.1`, `prerelease=false`. Corrige la afirmación previa de que este endpoint devolvía prerelease
- `https://registry.npmjs.org/@opencode%2Fcli/latest` devuelve `2.0.28` y declara `https://github.com/anomalyco/opencode.git` como repository. Confirma la relación del paquete con ese repo, no la equivalencia entre dev y el paquete publicado

El árbol openproxy tiene cambios locales previos de Claude Code y background. No se modificaron en esta fase. Las líneas citadas son del estado inspeccionado, no de un árbol limpio exclusivamente en ese commit

## 2. Decisiones que sustituyen propuestas contradictorias

1. No habrá un `ClientContract` universal con campos Codex obligatorios para todos
2. Habrá un envelope común con payload tipado por proveedor y catálogo separado
3. No se conserva `DynamicHeaderOverrides` como fuente independiente de versión, body y headers para estos contratos. Sus setters por campo no garantizan coherencia
4. Los refreshers actuales aportan la adquisición de metadata, no la activación inmediata de versión
5. Una sola operación de refresh devuelve `Unchanged` o un candidato completo. No hacer un GET de revalidate y otro de fetch
6. La activación será automática después de validación. No se requiere promoción humana en cada release
7. Cambios desconocidos de protocolo conservan el último snapshot válido y producen una alerta. Esto exige guards definidos explícitamente, no un detector omnisciente
8. Zen y Go tienen identidades separadas. Go no necesita adoptar la versión de OpenCode CLI para anunciar una identidad propia
9. Perfil pequeño y catálogo grande tienen límites independientes. Propuesta inicial configurable: 64 KiB por perfil, 4 MiB por catálogo y 1000 modelos. Son límites de diseño, no requisitos upstream
10. `max_context_window` se conserva como máximo declarado separado de `context_window`. No elevar la ventana efectiva automáticamente ni reinterpretar input/output limits
11. No borrar modelos, cambiar headers de autenticación, retirar tracing o eliminar tools solo por ausencia en un checkout. Cada cambio semántico necesita su prueba

## 3. Fuentes por proveedor: datos frente a reglas

### 3.1 Codex

Prefijo upstream para las líneas auditadas:
`https://github.com/openai/codex/blob/2f05c0a71f59/`

Fuentes:
- `codex-rs/login/src/auth/default_client.rs:42`: DEFAULT_ORIGINATOR
- mismo archivo `:150-182`: generación de User-Agent y versión de build
- mismo archivo `:456-469`: default_headers
- `codex-rs/model-provider-info/src/lib.rs:80`: base URL ChatGPT Codex
- mismo archivo `:425-455`: selección de provider y residency
- mismo archivo `:540`: header version derivado de CARGO_PKG_VERSION
- `codex-rs/codex-api/src/requests/headers.rs:5-33`: session/thread y subagent
- `codex-rs/core/src/client.rs:160-175`: nombres de headers y valor beta websocket
- mismo archivo `:1246-1263`: routing hint condicionado por modelo/tier/provider
- mismo archivo `:1409-1412`: beta websocket. No trasladarlo a HTTP
- `codex-rs/codex-api/src/common.rs:279-304`: esquema ResponsesApiRequest y reglas serde de omisión
- `codex-rs/core/src/client.rs:1033-1090`: construcción efectiva del body
- `codex-rs/models-manager/models.json`: catálogo bundled
- `codex-rs/models-manager/src/lib.rs:11-23`: inclusión del catálogo y client_version
- `codex-rs/codex-api/src/endpoint/models.rs:33-63`: catálogo y query client_version
- `codex-rs/core/tests/responses_headers.rs`: contrato de tests upstream

Datos extraíbles con parsers específicos:
- versión y revisión de release, originator, nombres de headers conocidos, literales beta y rutas conocidas
- catálogo: slug, display_name, description, context_window, max_context_window, visibility, supported_in_api, minimal_client_version, priority, input_modalities, supported_reasoning_levels, default_reasoning_level, support_verbosity, default_verbosity, service_tiers, supports_parallel_tool_calls, use_responses_lite y truncation_policy
- no inventar max_output_tokens si el catálogo no lo declara. Un fallback local debe identificarse como tal

Defaults literales comprobados en `core/src/client.rs:1078-1086`:
- `tool_choice="auto"`
- `store=false`
- `stream=true`

Son defaults de esa ruta del cliente, no una orden de sobrescribir cualquier petición de un cliente del proxy

Valores derivados que permanecen en Rust:
- parallel_tool_calls depende del prompt y de use_responses_lite
- include parte de reasoning.encrypted_content y añade output.encrypted_content según flags y provider
- stream_options, reasoning, verbosity/text, service_tier y prompt_cache_key
- identidad de sesión, cuenta, instalación y metadatos por turno
- routing hint, residency, FedRamp, transporte websocket/HTTP y Responses-Lite
- filtrado visible/API y relación minimal_client_version con la versión activa

Plan incremental de fuente:
- consultar release estable con ETag
- resolver tag a commit completo y comprobar versión del workspace/package en ESA revisión
- descargar paths conocidos desde el commit, no combinar release estable con main
- consultar árbol/blob IDs de los paths vigilados. Reutilizar blobs por hash si otra release no los cambió
- versionar parser y conjunto de guards. Si desaparece un símbolo, cambia su forma o cambian zonas semánticas no soportadas, rechazar candidato

Estado inicial bloqueado: falta comprobar el contrato de `rust-v0.162.1`. Las conclusiones de main no se aplican automáticamente a esa release

### 3.2 CodeBuddy

Fuentes versionadas de distribución:
- metadata: `https://registry.npmjs.org/@tencent-ai/codebuddy-code/latest`
- paquete fijado: `https://unpkg.com/@tencent-ai/codebuddy-code@2.164.0/package.json`
- producto: `https://unpkg.com/@tencent-ai/codebuddy-code@2.164.0/product.json`
- bundle: `https://unpkg.com/@tencent-ai/codebuddy-code@2.164.0/dist/codebuddy.js`
- listado y hashes: `https://unpkg.com/@tencent-ai/codebuddy-code@2.164.0/?meta`
- tarball oficial: `https://registry.npmjs.org/@tencent-ai/codebuddy-code/-/codebuddy-code-2.164.0.tgz`

Artefactos locales verificados:
- `cb-audit/unpkg-pkg.json:2-3`: nombre y versión
- mismo archivo `:69-83`: archivos distribuidos
- `cb-audit/product.json:10-18`: platform, endpoint y officialEndpoints
- mismo archivo `:129-148`: primer modelo y campos de capacidades/defaults
- `cb-audit/cli-head.js:281`, offset de caracteres 7732951: runIdentityHeaders
- mismo archivo `:507`, offset 9104928: asignación IDE_TYPE/IDE_NAME/IDE_VERSION de la petición de modelo
- mismo archivo `:507`, offset 9106682: eliminación inicial de user-agent y continuación de construcción de identidad
- mismo archivo `:300`, offset 7959638: x-codebuddy-request en llamada al daemon local

El bundle está minificado. Líneas y offsets son referencias solo de 2.164.0, nunca una API estable de extracción

Se compararon los bytes locales con los hashes de `?meta`: package.json, product.json y dist/codebuddy.js coincidieron. Esto valida consistencia con ese mirror. NO equivale a verificar archivos sueltos contra el dist.integrity oficial del tarball

Datos directamente extraíbles:
- versión/package name, productName, platform, endpoint y lista de endpoints oficiales
- 22 modelos del product.json inspeccionado, 370825 bytes
- campos presentes: id, name, vendor, maxInputTokens, maxOutputTokens, maxAllowedSize, contextWindow, supportsToolCall, supportsImages, supportsReasoning, onlyReasoning, canDisableThinking, disabledMultimodal, reasoning, relatedModels, isDefault, temperature, top_p, summary y tags
- campos opcionales se conservan como opcionales. No atribuir idéntica unidad a maxAllowedSize y contextWindow sin interpretar su consumidor

Reglas que necesitan código/captura:
- IDE headers con overrides de telemetryClientInfo y fallback de ClientInfoProvider
- IDs de conversación, request, mensaje, root y parent
- modo/intent/purpose, tracing, enterprise y run identity
- gateway oficial frente a custom model y filtros de headers por destino
- merge de providerData y campos adicionales del body
- User-Agent final. Que el bundle elimine un valor intermedio no prueba ausencia en el wire

Estrategia de bajo ancho de banda:
- metadata npm condicional. Misma versión/integrity: no bajar paquete ni bundle otra vez
- nueva versión: package.json y product.json versionados primero
- inspección del bundle SOLO una vez por nueva revisión si los guards de protocolo lo requieren. Guardarlo por hash
- no recomendar Range como extracción arbitraria de gzip. Rangos no descubren funciones movidas ni permiten validar el tarball entero
- jsDelivr rechazó este paquete por límite de 150 MB. No usar sus respuestas de error como fuentes

Bloqueo: product.json basta para capacidades/defaults de modelo, no para certificar nuevo contrato de gateway. Autoactivar una versión nueva requiere validar reglas conocidas mediante bundle vigilado y/o captura aislada. Sin esto, actualizar catálogo como overlay separado, no identidad

### 3.3 OpenCode Zen y Go

Repo relacionado con el paquete confirmado por metadata npm:
`https://github.com/anomalyco/opencode`

Prefijo de referencias auditadas:
`https://github.com/anomalyco/opencode/blob/48364310480d/`

Fuentes:
- `packages/script/src/index.ts:41`: fuente de versión @opencode/cli
- `packages/cli/script/build.ts:183`: User-Agent de build
- `packages/core/src/app.ts:16-26`: metadata y plantilla UA
- `packages/core/src/session/model-request.ts:278-301`: headers, affinity y promptCacheKey
- `packages/core/src/session/affinity.ts:7-8`: parent/fork/session
- `packages/console/app/src/lib/inference-proxy.ts:1-58`: shim de rutas y headers
- `services/www/src/docs/content/console/go.mdx:80-86`: identidad propia y sesión estable para Go
- `services/www/src/docs/content/console/models.mdx`: tabla de rutas por modelo

Fuentes runtime, NO contratos versionados de cliente:
- `https://opencode.ai/inference/v1/models`
- `https://opencode.ai/inference/go/v1/models`
- `https://models.dev/api.json`, origen de metadata de routing existente

Datos extraíbles:
- versión de @opencode/cli y plantilla UA conocida
- header names de la ruta conocida, modelos por flavor y routing cuando la fuente lo declare
- catálogo runtime conserva URL, hash, fetched_at y flavor. No afirmar que pertenece al mismo commit del CLI

Reglas imperativas:
- x-opencode-session-id lleva session.id, otros alias llevan affinity. No colapsar hijo/fork/root
- x-opencode-client depende de app.name. La plantilla no resuelve el entrypoint efectivo del paquete
- headers por parent, hooks, shaping de generación y providerOptions
- quartet de tools, aliases free-tier y autenticación/routing por formato

Openproxy actual:
- `crates/openproxy-adapters/src/spoofer/opencode.rs:14`: usa opencode-ai/latest
- `crates/openproxy-adapters/src/adapters/opencode_common.rs:98-121`: headers compartidos por flavors
- `crates/openproxy-core/tests/opencode_upstream_contract_test.rs:452-487`: rutas upstream obsoletas y success guard que permite pasar sin aserciones
- `crates/openproxy-pipeline/src/stages/target_headers/mod.rs:52-68`: propagación posterior que los tests de build_headers no cubren

Política:
- Zen puede actualizar contrato CLI solo con correspondencia paquete/commit comprobada
- Go usa UA propio y x-opencode-session estable conforme a documentación. No importar el spoof Zen por defecto
- listas de modelos Zen y Go separadas
- modelo nuevo sin routing conocido queda pendiente, no se adivina dialecto ni se publica como soportado
- GET 404 a una ruta POST no demuestra incompatibilidad de POST

Bloqueos: gate de inferencia/free-tier no público, variantes de rutas documentadas sin POST contrastado, cuarto segmento UA no capturado, dev no equiparado a 2.0.28

## 4. Interfaz mínima propuesta, no código implementado

Un único módulo genérico con estas operaciones:

```text
ContractDriver<T>
  id() -> ContractId
  refresh(previous, cache, http) -> Unchanged | Candidate<T> | Error
  validate(candidate, previous) -> Validated<T> | Error

ContractStore<T>
  load_validated_or_embedded() -> Arc<Snapshot<T>>
  commit(validated) -> Arc<Snapshot<T>>
  snapshot() -> Arc<Snapshot<T>>
```

Usar el estilo de traits async/enum que ya emplea el repo. No introducir async_trait por comodidad sin comprobar necesidad

`ContractId`: codex, codebuddy, opencode-zen, opencode-go. Son cuatro snapshots y tres implementaciones de adquisición por familia

`Snapshot<T>`:
- schema_version
- contract_id
- generation: digest del contenido semántico y de referencias de artefactos
- client_identity: versión opcional y origen de identidad, propio u oficial. Go puede tener versión propia
- rule_set_id y extractor_version: identifican lógica compilada, no código descargado
- sources[]: URL, revisión inmutable cuando exista, hash, tipo de evidencia
- payload: perfil tipado del proveedor
- catalog_ref opcional: digest y formato del catálogo tipado

Cache de transporte independiente:
- clave por URL completa, representación/Accept y ámbito de autenticación
- etag y last_modified opcionales, checked_at, hash/bytes del recurso previo
- ETag opaco, no identidad de versión ni prueba criptográfica
- 304 solo con bytes previos válidos. Si faltan, repetir GET no condicional

No hay strings de autorización, cookies ni IDs de usuario/sesión en archivos de contrato. Los nombres de headers de auth pueden formar parte de documentación, sus valores siempre vienen de credenciales runtime

## 5. Coherencia por petición y persistencia

Punto actual observado:
- `crates/openproxy-pipeline/src/stages/target.rs:229-235`: formatter, wrapper y strip
- mismo archivo `:471-483`: headers y propagación
- mismo archivo `:499-505`: finalize_request

Contrato requerido:
- capturar el snapshot una vez al resolver el proveedor para una petición
- body, headers, query client_version y finalización consumen ESE Arc
- mantenerlo para retries del mismo proveedor. Si una petición pasa a otro proveedor, pin propio en el mapa request-local
- prueba con barreras: refrescar entre wrap y headers, y entre headers y finalize. La petición anterior conserva generation antigua y la siguiente utiliza la nueva

Integración propuesta: añadir pin request-local explícito y forwarding en ProviderAdapter/ProviderAdapterEnum. El adapter concreto opera con su payload tipado. No usar globales por campo, TLS ambiental o lecturas independientes en cada hook. Revisar el API exacto del pin antes de implementar, no fingir que las firmas actuales ya lo permiten

Persistencia multiarchivo:
1. validar candidato completo en memoria, incluyendo compatibilidad de reglas y catálogo
2. escribir artefactos con nombres por digest en el mismo filesystem, permisos restrictivos
3. fsync de archivos, escribir manifest con hashes y fsync
4. rename atómico del manifest activo y fsync del directorio
5. publicar Arc completo en memoria

Un manifest nunca referencia archivos pendientes. Un fallo de disco no activa versión nueva en memoria. Tras caída entre rename y publicación, el reinicio recupera la misma generación persistida

Arranque: leer manifest, comprobar schema/hashes/límites/validador, cargar último válido. Si corrupto, fallback embebido y alerta. Guardar manifest previo para recuperación si se desea conservar LKG ante corrupción externa, no tres archivos mutables sobrescritos por separado

Reutilizar el patrón Claude, no copiarlo ciegamente:
- `spoofer/claude_code_profile.rs:70-130`: validación
- `:147-174`: carga y rechazo de downgrade en runtime
- `background/claude_profile.rs:22-26`: intervalo
- el anti-downgrade debe comprobarse ANTES del commit a disco. Validar solo después de que un capturador escriba el archivo deja un downgrade posible al reiniciar
- el patrón actual realiza lecturas independientes del perfil en hooks. No dar por demostrada coherencia por petición

## 6. Descarga incremental y guards de compatibilidad

- timer común parametrizado por provider, reutilizando supervisor BackgroundService
- default propuesto 6 h, mínimo 300 s, jitter y backoff acotado
- tick, arranque y discovery coordinados por single-flight por contrato. Discovery no debe bloquear inferencia para extraer un cliente
- política stable explícita. No convertir alpha a stable cortando el sufijo
- comparar SemVer dentro del mismo canal. SHA no se ordena como número ni lexicográficamente
- misma revisión y hashes: sin extracción, descarga de bundle ni reactivación
- misma versión con contenido cambiado: registrar conflicto y conservar anterior. No confiar en mutabilidad de latest
- fuentes versionadas se cachean por revision/path/blob digest. Mutable metadata se revalida por ETag si el servidor lo ofrece
- Last-Modified o GET acotado como fallback donde no exista ETag
- limitar bytes tras descompresión, tiempo, redirect hosts y número de modelos. URLs procedentes de metadata no autorizan nuevos hosts de inferencia

Guards:
- parser específico por símbolos/path y estructura esperada
- comprobar valores de enums soportados y formas de defaults
- fingerprints de regiones semánticas vigiladas o fixtures de request
- no activar cambios en regiones imperativas no interpretadas aunque la versión sea mayor
- guard de todo un archivo sería conservador pero bloquearía cambios irrelevantes. Empezar conservador y reducir regiones solo con pruebas
- un guard parcial no detecta cualquier cambio de comportamiento upstream. Documentar cobertura y alcance en el contrato

## 7. Tests y criterios de aceptación

Suite determinista sin red externa:
1. 200 inicial seguido de 304 con cache válida: sin reextraer y sin cambiar generation
2. 304 sin bytes previos: recuperación con GET completo o rechazo explícito
3. release nueva y uno de los archivos ausente/404: versión y snapshot anterior intactos
4. timeout, 429/Retry-After, HTML 200, JSON inválido o error CDN: no activar
5. límite de payload después de descompresión, duplicados case-insensitive y CR/LF de headers
6. versión inferior, prerelease fuera de canal, regla o enum semántico desconocido: rechazo
7. mismo version id con hash distinto: conflicto visible
8. error de disco y caída simulada antes/después de rename: sin manifest parcialmente válido
9. reinicio tras candidato rechazado: recuperar anterior, no candidato
10. refresh intercalado con hooks/retries: una generation por proveedor y petición
11. catalogue filter y nuevas/bajas con fixtures. No borrar configuraciones/combo_targets del usuario por ausencia en discovery
12. unknown routing en OpenCode: pendiente y alerta, no inferencia con dialecto adivinado
13. CodeBuddy official/custom separadas. Captura custom no certifica headers de gateway
14. Codex beta websocket nunca añadido a HTTP por una tabla global
15. defaults solo donde la política lo permita, respetando valores explícitos y coherencia de strip/finalize

CI:
- tests offline de contrato fijan bytes y provenance, no dependen de main/latest
- checks online de drift son job separado y fallan explícitamente si la fuente no se puede consultar. No transformar 404 en verde
- tests wire de cadena usan formatter, wrapper, strip, propagación y finalize reales. Comparar bytes enviados, no helpers que reimplementen el adapter
- captura CLI contra loopback prueba esa ruta custom y ese entrypoint. No demuestra paridad server-side, cuotas ni gate oficial

Comandos de verificación para la fase autorizada, NO ejecutados en esta especificación:
```bash
cargo fmt --all -- --check
cargo clippy --all-targets --workspace -- -D warnings
RUST_MIN_STACK=33554432 cargo test --workspace
cargo build --workspace --all-features
```

Tests nuevos sugeridos por paths:
- `crates/openproxy-adapters/tests/client_contract_refresh.rs`
- `crates/openproxy-adapters/tests/client_contract_store.rs`
- `crates/openproxy-adapters/tests/client_contract_snapshot.rs`

No se afirma que existan esos archivos ni que ningún comando pase

## 8. Secuencia y responsabilidades

### Tarea 1: fijar fuentes de release y fixtures
Objetivo: eliminar mezcla main/dev y release
- leer tags/manifests, resolver revisión completa y comprobar versión
- guardar fixture de release y pruebas de parsers fuera de producción
- cerrar blocked items de correspondencia antes de activar identidad

### Tarea 2: envelope, validadores y storage común
Propuestos:
- `crates/openproxy-adapters/src/client_contract/mod.rs`
- `crates/openproxy-adapters/src/client_contract/store.rs`
- `crates/openproxy-adapters/src/client_contract/refresh.rs`
- tests comunes de sección 7

Ciclo: test de 304/rechazo/persistencia en rojo, implementación mínima y mismo test en verde. No extraer aún los tres proveedores

### Tarea 3: pin de snapshot y ciclo completo con Codex
Archivos probables:
- `crates/openproxy-adapters/src/adapters/traits.rs`
- `crates/openproxy-adapters/src/adapters/macros.rs`
- `crates/openproxy-adapters/src/adapters/codex/{mod,models}.rs`
- `crates/openproxy-adapters/src/spoofer/codex.rs`
- `crates/openproxy-pipeline/src/stages/target.rs` y contexto request-local que corresponda
- `crates/openproxy-server/src/background/{mod,scheduler_services}.rs`
- `crates/openproxy-server/src/background/client_contract.rs` propuesto

Ciclo: test con barreras en rojo, pin explícito y transición atómica, test verde. El primer hito exige extracción real de una revisión soportada, persistencia, reload y generación coherente

### Tarea 4: CodeBuddy
Solo después de la base validada
- payload y extractor propios, product.json y capacidades
- guards del bundle/captura y región official/custom
- no usar solamente metadata npm como aprobación de contrato

### Tarea 5: OpenCode Zen/Go
Solo después de la base validada
- corregir fuente npm con correspondencia de release
- identidades y catálogos separados
- test de headers finales y routing pendiente
- cambios de gate/tools no comprobados quedan fuera

### Tarea 6: revisión integral y cierre
- verificar compilación, lint y suite desde el padre, sin cargo paralelo
- revisar logs de rechazo y comportamiento de fallback
- entregar commits/artifacts y resultado real de tests
- deploy es una autorización aparte. Esta especificación no lo autoriza

Delegación futura:
- padre posee storage, traits, macro, pipeline y background
- hijo Codex solo paths Codex y tests de su proveedor
- hijo CodeBuddy solo paths CodeBuddy y tests de su proveedor
- hijo OpenCode solo paths OpenCode y tests Zen/Go
- archivos globales sucios actuales son off-limits hasta acordar integración
- no lanzar implementaciones simultáneas hasta fijar signatures reales y haber ejercitado base+Codex

## 9. Límites de la entrega

Esta especificación consolida los tres análisis y corrige incompatibilidades entre sus interfaces

No promete extracción automática de código arbitrario, compatibilidad con una release desconocida, paridad oficial de tráfico ni autorización de modelos para una cuenta concreta

Una nueva versión puede activarse automáticamente cuando sus datos y guards conocidos pasan. Un cambio no soportado se bloquea con motivo visible y conserva inferencia usando el anterior

Trabajo realizado en esta fase: lectura de fuentes y metadata, verificación de hashes de tres archivos CodeBuddy y escritura de esta especificación. Ningún archivo de producto, servicio activo, base de datos o credencial se modificó
