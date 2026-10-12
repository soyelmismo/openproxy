# AGENTS.md: Directivas de ingeniería para openproxy

Reglas de arquitectura, calidad y flujo de trabajo para agentes y desarrolladores en openproxy.

---

## 1. Filosofía de Trabajo y Comunicación

1. **Zero-Chat & Máximo SNR:** Emitir solo código limpio, parches exactos y respuestas técnicas de alta densidad. Cero texto de relleno o explicaciones redundantes. (Nota: Esto aplica a la comunicación del asistente, NUNCA a podar strings descriptivos de UI, tooltips, hints de usuario o documentación del código fuente).
2. **Jerarquía Lazy de Implementación:** `stdlib robusta > reusar código/traits locales > API nativa > dependencia actual > código mínimo`.
3. **Causa Raíz:** Corregir la función base o el diseño de tipos. No parchar síntomas locales.
4. **Cero Dependencias Inútiles:** No añadir dependencias a `Cargo.toml` o `package.json` sin justificación crítica.
5. **Formato de Salida:** Aplicar ediciones con las herramientas de edición (`patch` para reemplazos anclados, `write_file` para reescrituras completas). Anclar cada reemplazo con al menos 2 líneas de contexto antes y después, y verificar el diff devuelto. No volcar archivos completos en el chat.
6. **Invariante de Paridad Funcional y Visual 1:1:** Todo refactor debe preservar el 100% de la funcionalidad, vistas, layouts, media queries, tooltips, hints y compatibilidad con modelos. Un refactor NUNCA es una excusa para podar código útil o estilos.
7. **Límite Absoluto de Tamaño de Archivo (<800 LOC):** Hard invariant de 0 archivos > 800 LOC en todo el monorepo (Rust, TypeScript, CSS). Cuando un archivo supera este límite, la ÚNICA solución permitida es su **descomposición modular** en submódulos cohesivos (<500-800 líneas), NUNCA el borrado o recorte de funcionalidad.

---

## 2. Mapa del Workspace y Responsabilidades

Monorepo de 14 crates en Rust con frontend embebido. La fuente autoritativa de responsabilidades, DAG de dependencias, superficies HTTP y modos de despliegue es `docs/architecture.md` (no duplicar aquí: consultar antes de mover fronteras entre crates).

```text
crates/
├── openproxy-types/       # Structs de dominio, Enums, CoreError, Ids (ProviderId, ModelId). Sin deps internas
├── openproxy-db/          # SQLite (rusqlite bundled), migraciones numeradas, cifrado AES-256-GCM, repositorios
├── openproxy-adapters/    # Adapters LLM, registro de providers, transporte upstream compartido, SSRF
├── openproxy-compression/ # Compresión de payloads (Lite y RTK command filtering)
├── openproxy-pipeline/    # Enrutamiento, dispatcher upstream, combos, races, retries, cuotas, SSE, PII
├── openproxy-notifications/ # Notificaciones persistidas y su publicación
├── openproxy-analytics/   # Feeds de uso, percentiles de latencia, estadísticas de race, telemetría inflight
├── openproxy-pricing/     # Precios de modelos, cálculo de coste, interpretación de cuotas
├── openproxy-oauth/       # Flujos OAuth, coordinación de refresh, persistencia cifrada de tokens
├── openproxy-discovery/   # Descubrimiento de modelos por provider, enriquecimiento models.dev, scheduling
├── openproxy-proxy-pool/  # Gestión opt-in de fuentes de proxy, scraping, validación, sincronización
├── openproxy-core/        # Orquestación headless: config, bootstrap/seeding, DI facades, endpoints no-chat
├── openproxy-server/      # Binario axum: rutas /v1/*, /admin/api/*, WebSocket /admin/ws, assets embebidos
│   └── web/               # Frontend SPA (TypeScript + Lit-HTML + esbuild + uPlot)
└── openproxy-api-client/  # Cliente SDK en Rust para la API REST administrativa
```

### Reglas de frontera entre crates:
- **`openproxy-types`**: Tipos de datos e interfaces compartidas sin dependencias internas.
- **`openproxy-db`**: Consultas SQL encapsuladas. Cero SQL en handlers del servidor.
- **`openproxy-adapters`**: Serialización y mapeo de protocolos upstream (OpenAI ↔ Anthropic ↔ Gemini).
- **`openproxy-core`**: Reexports que preservan las rutas de dominio de los consumidores.

---

## 3. Criterios de ROI, Deduplicación y Reglas de Parada

### 3.1 Clasificación por ROI
| Prioridad | Categoría | Condición de Ejecución |
| :--- | :--- | :--- |
| **P1** | Eliminar dependencia externa (`once_cell` → `LazyLock`, etc.) | Siempre |
| **P2** | Deduplicar lógica real (>10 líneas idénticas entre módulos) | Siempre |
| **P3** | Extraer funciones monolíticas (>100 líneas) | Requiere al menos 1 test unitario para la función extraída |
| **P4** | Modernización de sintaxis (`let-else`, `is_some_and`, `split_once`) | Solo si reduce ≥3 líneas netas por punto de uso |
| **P5** | Reducción de `.clone()` innecesario | Solo con evidencia en hot paths o bucles de alto tráfico |

### 3.2 Reglas de Parada y Anti-Bucle
- P1-P2 aplican siempre. P3 exige tests unitarios de la función extraída. P4-P5 son cosmética.
- Si el diff de P1-P3 supera 300 líneas, descartar P4-P5.
- No mezclar prioridades distintas en el mismo commit.
- Si un cambio cosmético de P4-P5 falla al compilar o testear por segunda vez, descartarlo.

### 3.3 Técnicas de Deduplicación Zero-Cost
1. **Extension Traits (Blanket Implementations):** Transformaciones comunes sin nuevos structs contenedores (`pub trait ResponseExt`).
2. **Macros Declarativas & Jump-Maps (O(1) en compile-time):** Mapeos estáticos por tablas de saltos generadas por macro con chequeo exhaustivo. El registro de adapters ya sigue este patrón en `crates/openproxy-adapters/src/adapters/macros.rs`: `define_provider_adapter!` (implementación completa) y `declare_openai_adapter!` (adapter OpenAI-compatible), consumidas desde `ProviderAdapterEnum`.
3. **Traits con Métodos por Defecto:** Endpoints base y URLs de upstream APIs.
4. **Copy-on-Write y Zero-Alloc:** `Cow<'a, T>` en transformaciones que rara vez mutan; `Arc::unwrap_or_clone(arc)` cuando el `Arc` es el único dueño; `write!(buf, ...)` con buffer reutilizado en hot paths; hashes `u64`/`DefaultHasher` en tablas de trackers y cooldowns en vez de `String` intermedios; acumular en `Vec<u8>` sobre chunks TCP crudos, extrayendo líneas en `b'\n'` y decodificando con `std::str::from_utf8` para respetar UTF-8 multibyte.
5. **Providers en archivos propios:** cada adapter vive en su archivo y se registra por macro; no añadir enums centralizados nuevos en `crates/openproxy-adapters/src/adapters/mod.rs`.

---

## 4. Directivas de Rust Moderno (Rust 1.96 / Edition 2024)

Rust moderno obligatorio. El repo cumple estas directivas (verificado por grep): usarlas al escribir código nuevo y no revertirlas.

### 4.1 Primitivos de la Stdlib
- **`std::sync::LazyLock<T>` y `std::sync::OnceLock<T>`:** inicialización estática. Prohibido `lazy_static!` y `once_cell`.
- **`static mut`:** prohibido. Usar `Atomic*`, `OnceLock` o structs de estado sincronizados.
- **`std::num::NonZero<T>`:** tipo genérico nativo de la stdlib.
- **`std::io::IsTerminal`:** detección nativa de terminales (no usar `atty` ni `is-terminal`).
- **`std::path::absolute`:** normalización canónica de rutas.
- **`std::fs::exists(&path)`:** detección de existencia en disco (`io::Result<bool>`).

### 4.2 Control de Flujo Idiomático
- **`let-else`:** validación temprana y desempaquetado plano:
  ```rust
  let Some(value) = opt else {
      return Err(CoreError::NotFound);
  };
  ```
- **Predicados con `is_some_and` / `is_ok_and`:** reemplazar `if opt.is_some() && opt.unwrap() { ... }`.
- **Inspección con `inspect` / `inspect_err`:** logging o trazas sin mutar el `Result` u `Option`.
- **División con `str::split_once`:** `if let Some((k, v)) = s.split_once(':')` en vez de `.split()`.
- **Rangos Exclusivos:** `match status { 200..300 => ..., 400..500 => ... }`.
- **Inline Const Expressions (`const { ... }`):** inicialización de arrays sin `Copy` o aserciones en tiempo de compilación.

### 4.3 Concurrencia, Async y Prevención de Deadlocks
- **Aislamiento de SQLite en Async (`spawn_blocking`):** prohibido llamar a `conn.lock()` o ejecutar consultas rusqlite en el hilo de trabajo de Tokio. Aislar con `tokio::task::spawn_blocking(move || { let conn = conn_arc.lock(); ... })`.
- **Prohibición de locks a través de `.await`:** no retener `MutexGuard` ni referencias a `RefCell` a través de puntos de suspensión. Acotar el scope del guard en un bloque `{ ... }` o hacer `drop(conn)` antes de cualquier `.await`.
- **Prevención de Mutex Reentrante:** `parking_lot::Mutex` y `std::sync::Mutex` no son reentrantes. No llamar a `repo.*` con un guard de `conn` activo en el mismo hilo: pasar `&conn` a las funciones `openproxy_db::<modulo>::<fn>(&conn, ...)`. No re-adquirir `self.conn.lock()` tras haber obtenido un guard.
- **Liberación de locks antes de publicar:** `drop(conn)` antes de `publish_notification`, `broadcast::Sender`, websockets o callbacks.
- **Tablas concurrentes y atómicos:** `entry(key).or_insert_with(...)` y `Ordering::Relaxed` para contadores y timestamps, sin `SeqCst` innecesario.
- **Canales con Backpressure:** no usar `unbounded_channel` para colas de proxies, bufferings SSE o ingestiones masivas. Usar `channel(N)` con límite explícito.
- **Ciclo de vida de tareas en fondo:** todo `tokio::spawn` de larga duración o bucle `loop {}` escucha un canal de shutdown (`broadcast::Receiver<()>` / `CancellationToken`).
- **Async nativo:** preferir async closures y `AsyncFn`/`AsyncFnMut` para operaciones asíncronas reusables sin boxing de futures. AFIT y RPITIT con `use<..>` están disponibles para traits async nuevos, sin dependencias externas (como `async-trait`).

### 4.4 Manejo Seguro de Strings y Fronteras UTF-8
- **Prohibición de Slicing Crudo:** prohibido rebanar `&str` o indexar bytes (`&s[..n]`, `&s[start..end]`) sin validación explícita de frontera. En cadenas multibyte (emojis, tildes, CJK) el slice sobre un límite no alineado causa `panic` irrecuperable.
- **Validación con `is_char_boundary`:** validar siempre con `s.is_char_boundary(n)`. Si no es límite válido, retroceder de forma iterativa hasta el límite UTF-8 anterior o avanzar con `.char_indices()`.

### 4.5 Protocolos Streaming (SSE) y Contabilidad de Tokens
- **Terminadores de Protocolo Obligatorios:** en flujos SSE (tanto `/v1/chat/completions` como `/v1/responses`), el stream DEBE concluir siempre con el evento terminal canónico (`data: [DONE]` para Chat Completions; `response.completed` / `response.done` para Responses API). Omitirlo provoca desconexiones abruptas o timeouts en clientes y arneses (DeepSeek Harness, SDKs oficiales).
- **Preservación Acumulativa de Uso:** al agregar fragmentos de uso de upstreams (deltas de Anthropic/OpenAI), preservar el conteo acumulativo de `prompt_tokens` y `completion_tokens` en vez de sobreescribir con ceros o deltas vacíos en chunks intermedios.

### 4.6 Normalización de Payloads y Resiliencia de Upstreams LLM (Serde Strictness)
- **Incompatibilidad de Esquemas:** múltiples upstreams compatibles con OpenAI (NVIDIA NIM, Fireworks, Kilocode, Bai, vLLM) están construidos en Rust con Serde estricto (`async-openai`) y no toleran extensiones no estándar:
  - **Mensajes `assistant`, `system` y `tool`:** no admiten arrays de partes estructuradas (`output_text`, `annotations`, `thinking`, `reasoning`). Aplanar a string plano (`Value::String(m.extract_text())`) antes de serializar hacia el upstream.
  - **Mensajes `user`:** arrays de texto puro se aplanan a string; arrays multimodales (imágenes/audio) se normalizan convirtiendo `output_text`/`input_text` a `"text"` y eliminando campos propietarios (`annotations`).
  - **Parámetros no estándar de Responses:** `prompt_cache_key`, `prompt_cache_retention`, `instructions`, `input`, `previous_response_id`, `store`, `background`, `truncation`, `disabled` son rechazados con `400 Bad Request`. Depurarlos en `OpenaiFormatter` antes de serializar hacia upstreams OpenAI.
  - **Frontera de Sanitización:** esta normalización reside exclusivamente en la capa de serialización hacia el upstream (`TargetFormatter` / adapters), NUNCA recortando campos de los modelos internos del dominio o del middleware que rompan contratos entre módulos o tests internos.

---

## 5. Patrones Prohibidos No Cubiertos en Otras Secciones

Las reglas de concurrencia, SSE, UTF-8, Serde, CSS y orquestación están enunciadas en sus secciones (§4.3, §4.4, §4.5, §4.6, §7, §9); no se repiten aquí.

| Patrón No Permitido | Corrección |
| :--- | :--- |
| **`.unwrap()` / `.expect()` en producción** | Propagar con `?`, usar `.ok_or_else()`, `.unwrap_or()` o fallbacks defensivos. |
| **`for i in 0..len` / `arr[i]`** | Iteración directa (`.iter()`, `.into_iter()`, `.enumerate()`, `.windows()`, `.zip()`). |
| **Supresión de advertencias (`#[allow(clippy::...)]`)** | Prohibido silenciar linters. Corregir la causa raíz. Hay 3 allows vivos en `crates/`: reducirlos, no añadir más. |
| **Placeholders SQL dinámicos manuales** | Usar los helpers de batch de `openproxy-db` (`batch_insert`, `query_in_chunks`, `in_placeholders`). |
| **Lógica duplicada entre crates** | Centralizar en `openproxy-types`, `openproxy-db` o `openproxy-core` y reexportar con `pub use`. |
| **Modificar firmas de arneses de test sin sincronizar E2E** | Actualizar llamadores y mocks en el mismo commit para no quebrar CI. |

---

## 6. Base de Datos SQLite, Cifrado y Migraciones

1. **Migraciones Secuenciales:**
   - Añadir el archivo numerado a `crates/openproxy-db/migrations/` (siguiente número libre; a fecha de hoy el último es `000090_purge_stale_proxy_sources.sql`, así que un ejemplo de nombre válido sería `000091_feature_name.sql`). El array `MIGRATIONS` NO se edita a mano: `crates/openproxy-db/build.rs` lo genera en `OUT_DIR/migrations_generated.rs` desde el prefijo numérico de seis dígitos de cada archivo, y el runner es idempotente.
   - Verificar el número libre antes de elegir prefijo. Si el build falla con una migración ausente en `OUT_DIR`, es una migración obsoleta en caché: tocar el directorio y recompilar.
2. **Cifrado en Reposo:**
   - Sellar credenciales upstream, tokens OAuth y API keys privadas con **AES-256-GCM** mediante la master key en `OPENPROXY_MASTER_KEY` (y `OPENPROXY_MASTER_KEY_PREVIOUS` durante rotación). Ver `crates/openproxy-db/src/secrets.rs`.
3. **Cascadas de Borrado (`ON DELETE CASCADE`):**
   - Asegurar claves foráneas para que la eliminación de padres limpie tablas hijas (cooldowns, targets, etc.).
4. **Mapeo Tipado de Filas (`map_row_fields!`):**
   - Usar la macro de `crates/openproxy-db/src/crud/mod.rs` para mapear `rusqlite::Row` con calificadores (`@bool(idx)`, `@u16(idx)`, `@json(idx)`, `@enum(idx, Type)`, `@opt_id(idx, Type)`, ...) en vez de `row.get()` a mano.
5. **Nombres de Tablas Tipados:**
   - Usar el enum `DbTable` (`crates/openproxy-db/src/maintenance.rs`) en operaciones de purga o mantenimiento; no hardcodear strings.

---

## 7. Directivas del Frontend Web (Dashboard SPA)

Base de rutas de esta sección: `crates/openproxy-server/web/src/`.

1. **Stack:** TypeScript + Lit-HTML + Vanilla CSS (design tokens en `static/styles/tokens.css` y `static/styles/themes.css`). Gráficas en tiempo real con **`uPlot`**.
2. **Modularidad y Límite de Estilos (<800 LOC):**
   - Cada vista tiene su hoja modular en `static/styles/views/<vista>.css` (y `<vista>_mobile.css` si el bloque responsive es extenso). Todas se importan en `static/styles/views.css`.
   - Ningún archivo CSS puede superar 800 LOC. Si crece, particionarlo por responsabilidades de UI, nunca borrar selectores existentes.
   - Prohibido inventar clases sintéticas en lugar de reutilizar o modularizar los estilos existentes del diseño base.
3. **Contraste y Temas:**
   - Verificar legibilidad en tema oscuro (`:root[data-theme="dark"]`).
   - Evitar azules o colores oscuros con bajo contraste sobre fondos oscuros. La paleta canónica es `CHART_COLORS` en `static/src/components/uplot-chart/colors.ts` (`blue: "#38bdf8"`); usarla en vez de literales sueltos.
4. **Preservación de Tooltips e Interactividad:**
   - Mantener siempre tooltips (`abbr[title]`), badges de estado dinámicos, hints y feedback visual.
   - Entradas numéricas con sliders en el playground deben usar `@change` en el campo de texto para no bloquear la edición de decimales (`0.`).
5. **Compilación Web:**
   - Ejecutar `pnpm --dir crates/openproxy-server/web run build` tras modificar `static/src/` antes de compilar el binario Rust, para incrustar los assets actualizados.
6. **Aislamiento de Cascada y Jerarquía de Reglas CSS:**
   - La propiedad de los componentes pertenece estrictamente a `static/styles/components/`:
     - `components/forms.css`: variantes de botones (`button.primary`, `button.small`, `button.danger`, `button.secondary`, `button.success`) y `.actions` genéricas.
     - `components/tables.css`: `.responsive-card-table` y los bloques `@media` responsive compartidos.
     - `components/badges.css`: `.chip`, `.status-pill` y variantes de color.
   - Queda prohibido declarar reglas de componentes con alcance global en hojas de vista (`views/*.css`).
   - Todo selector en hojas de vista debe acotarse al contenedor de la vista, que es `#main` (p. ej. `.view-config .backup-warning-banner`, `.view-proxies .actions:has(button)`).
7. **Preservación Móvil y Responsive:**
   - Prohibido eliminar selectores de elementos móviles (ej. `.mobile-proxy-source-card-cell { display: none }`): causan filtración de columnas móviles al layout de escritorio y rompen la renderización en tarjetas móviles.

---

## 8. Verificación Pre-Commit

Verificar contra las reglas de §4 y §7 antes de commitear. Cada función extraída de >20 líneas necesita su test unitario.

### 8.1 Comandos de Verificación
```bash
# Linter Rust
cargo clippy --workspace --all-targets -- -D warnings
# Pruebas Rust (una sola pasada, ver §9)
cargo test --workspace
# Frontend
pnpm --dir crates/openproxy-server/web run typecheck:all
pnpm --dir crates/openproxy-server/web run test
pnpm --dir crates/openproxy-server/web run test:e2e
pnpm --dir crates/openproxy-server/web run build
```

**Auditoría de Tamaño (<800 LOC):**
```bash
python3 -c "import os; [print(f'{len(open(os.path.join(r,f)).readlines()):4} {os.path.join(r,f)}') for r,_,fs in os.walk('crates') if 'node_modules' not in r and 'target' not in r and 'dist' not in r for f in fs if f.endswith(('.rs','.ts','.css')) and len(open(os.path.join(r,f)).readlines()) > 800]"
```

**Commits:** formato Conventional Commits (`feat(...)`, `fix(...)`, `refactor(...)`, `docs(...)`, `perf(...)`).

---

## 9. Gobernanza de Orquestación Multi-Agente y Eficiencia de Tokens

Para prevenir cuellos de botella de compilación, sobreconsumo de tokens (KV-cache blowup) y contención del sistema de archivos:

1. **Compilación y Test Centralizado:** ejecutar **una sola pasada integral** (`cargo test --workspace`) por compuerta de validación o tras finalizar la integración.
   - Prohibido lanzar ejecuciones concurrentes de `cargo test` (un runner para "otros crates", otro para "workspace", otro para "submódulo"). Al compartir `target/` y locks del sistema de archivos, Cargo serializa la compilación triplicando CPU y quemando tokens.
2. **Particionamiento Disjunto por Crate:** si se requiere paralelismo real, asignar a cada subagente un conjunto de crates estrictamente disjunto (`openproxy-pipeline`, `openproxy-adapters`, `openproxy-db`, `openproxy-server`). Prohibido solapar workers concurrentes sobre el mismo crate sin contratos de interfaz congelados previamente.
3. **Comunicación Vía Artefactos en Disco:** prohibido volcar salidas completas de compilación, trazas o logs masivos en el chat entre agentes.
   - Todo worker escribe hallazgos, diffs y reportes en un archivo de artefacto en disco (`.agents/<subagent>/handoff.md`) y el agente sucesor o revisor lo lee con la herramienta de lectura, sin arrastrar el transcript completo en su ventana de contexto.
4. **Silencio del Orquestador durante Esperas:** el orquestador suspende su ejecución (`idle` / reactivo) mientras los subagentes trabajan. Prohibido generar reflexiones o encadenamientos de pensamiento mientras espera, evitando re-prefills masivos que agotan la cuota global.
