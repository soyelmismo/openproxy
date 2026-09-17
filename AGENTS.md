# AGENTS.md: Directivas de ingeniería para openproxy

Reglas de arquitectura, calidad y flujo de trabajo para agentes y desarrolladores en openproxy.

---

## 1. Filosofía de Trabajo y Comunicación

1. **Zero-Chat & Máximo SNR:** Emitir solo código limpio, parches exactos y respuestas técnicas de alta densidad. Cero texto de relleno o explicaciones redundantes en las respuestas. (Nota: Esto aplica a la comunicación del asistente, NUNCA a podar strings descriptivos de UI, tooltips, hints de usuario o documentación del código fuente).
2. **Jerarquía Lazy de Implementación:**
   $$\text{Stdlib robusta} > \text{Reusar código/traits locales} > \text{API nativa} > \text{Dependencia actual} > \text{Código mínimo}$$
3. **Causa Raíz:** Corregir la función base o el diseño de tipos. No parchar síntomas locales.
4. **Cero Dependencias Inútiles:** No añadir dependencias a `Cargo.toml` o `package.json` sin justificación crítica.
5. **Formato de Salida:** Bloques `SEARCH/REPLACE` con anclas exactas (mínimo 2 líneas antes y después). No generar archivos completos.
6. **Invariante de Paridad Funcional y Visual 1:1:** Todo refactor debe preservar el 100% de la funcionalidad, vistas, layouts, media queries, tooltips, hints y compatibilidad con modelos. Un refactor NUNCA es una excusa para podar código útil o estilos.
7. **Límite Absoluto de Tamaño de Archivo (<800 LOC):** Hard invariant de 0 archivos > 800 LOC en todo el monorepo (Rust, TypeScript, CSS). Cuando un archivo supera este límite, la ÚNICA solución permitida es su **descomposición modular** en submódulos cohesivos (<500-800 líneas), NUNCA el borrado o recorte de funcionalidad.

---

## 2. Mapa del Workspace y Responsabilidades

Monorepo modular en Rust con frontend embebido:

```text
openproxy/
├── crates/
│   ├── openproxy-types/        # Structs de dominio, Enums, CoreError, Ids (ProviderId, ModelId)
│   ├── openproxy-db/           # SQLite (rusqlite bundled), migraciones SQL, cifrado AES-256-GCM, repositorios
│   ├── openproxy-pipeline/     # Enrutamiento, dispatcher upstream, combo resolution, race execution, cooldowns
│   ├── openproxy-adapters/     # Adapters LLM (OpenAI, Anthropic, Gemini, SSE parsing)
│   ├── openproxy-core/         # Lógica de negocio headless, sincronización de modelos, OAuth, notificaciones
│   ├── openproxy-compression/ # Compresión de payloads (Lite y RTK command filtering)
│   ├── openproxy-server/       # Binario axum: rutas /v1/*, /admin/api/*, WebSocket /admin/ws, assets embebidos
│   │   └── web/                # Frontend SPA (TypeScript + Lit-HTML + esbuild + uPlot)
│   └── openproxy-api-client/   # Cliente SDK en Rust para la API REST administrativa
├── docs/                       # Documentación técnica, diagramas y especificaciones
└── config.example.toml         # Configuración TOML de arranque
```

### Reglas de frontera entre crates:
- **`openproxy-types`**: Tipos de datos e interfaces compartidas sin dependencias pesadas.
- **`openproxy-db`**: Consultas SQL encapsuladas. Cero SQL en handlers del servidor.
- **`openproxy-adapters`**: Serialización y mapeo de protocolos upstream (OpenAI $\leftrightarrow$ Anthropic $\leftrightarrow$ Gemini).

---

## 3. Criterios de ROI, Deduplicación y Reglas de Parada (`rust-dedup-modernize`)

### 3.1 Clasificación por ROI
| Prioridad | Categoría | Condición de Ejecución |
| :--- | :--- | :--- |
| **P1** | Eliminar dependencia externa (`once_cell` $\to$ `LazyLock`, etc.) | Siempre |
| **P2** | Deduplicar lógica real ($>10$ líneas idénticas entre módulos) | Siempre |
| **P3** | Extraer funciones monolíticas ($>100$ líneas) | Requiere al menos 1 test unitario para la función extraída |
| **P4** | Modernización de sintaxis (`let-else`, `is_some_and`, `split_once`) | Solo si reduce $\ge 3$ líneas netas por punto de uso |
| **P5** | Reducción de `.clone()` innecesario | Solo con evidencia en hot paths o bucles de alto tráfico |

### 3.2 Reglas de Parada y Anti-Bucle
- P1-P2 aplican siempre. P3 exige tests unitarios de la función extraída.
- P4-P5 constituyen limpieza cosmética. Si el diff de P1-P3 supera 300 líneas, descartar P4-P5.
- No mezclar prioridades distintas en el mismo commit.
- Si un cambio cosmético de P4-P5 falla en compilar o testear al segundo intento, descartarlo.

### 3.3 Técnicas de Deduplicación Zero-Cost
1. **Extension Traits (Blanket Implementations):** Transformaciones comunes sin nuevos structs contenedores (`pub trait ResponseExt`).
2. **Macros Declarativas & Jump-Maps ($O(1)$ en Compile-Time):** Implementación de adapters, enrutadores o dispatchers. Todo mapeo estático usa tablas de saltos generadas por macro con chequeo exhaustivo en compilación.
3. **Traits con Métodos por Defecto:** Endpoints base y URLs de upstream APIs.
4. **Copy-on-Write y Zero-Alloc:**
   - Usar `std::borrow::Cow<'a, T>` en transformaciones que rara vez mutan la entrada.
   - Usar `Arc::unwrap_or_clone(arc)` cuando `Arc` es el único dueño.
   - Usar `write!(buf, ...)` con buffer reutilizado en hot paths para evitar allocaciones de formato.
   - Usar `u64` (`DefaultHasher` / `Hasher`) en tablas hash de trackers o cooldowns en vez de formatear `String`s intermedios.
   - Acumular en `Vec<u8>` sobre chunks TCP crudos, extrayendo líneas en `b'\n'` y decodificando con `std::str::from_utf8` para respetar UTF-8 multibyte y deserializar con `#[serde(borrow)]`.
5. **Distributed Plugin / Trait Registry:** Auto-registro modular de proveedores (`register_provider!`) en sus propios archivos sin alterar enums centralizados en `mod.rs`.

---

## 4. Directivas de Rust Moderno (Rust 1.80+ / Edition 2024)

### 4.1 Primitivos de la Stdlib
- **`std::sync::LazyLock<T>` y `std::sync::OnceLock<T>`:** Primitivos nativos obligatorios para inicialización estática. Prohibido usar `lazy_static!` o `once_cell`.
- **`std::num::NonZero<T>`:** Tipo genérico nativo de la stdlib.
- **`std::io::IsTerminal`:** Detección nativa de terminales (no usar `atty` ni `is-terminal`).
- **`std::path::absolute`:** Normalización canónica de rutas.
- **`std::fs::exists(&path)`:** Detección de existencia en disco (`io::Result<bool>`).

### 4.2 Control de Flujo Idiomático
- **`let-else`:** Validación temprana y desempaquetado plano:
  ```rust
  let Some(value) = opt else {
      return Err(CoreError::NotFound);
  };
  ```
- **Predicados con `is_some_and` / `is_ok_and`:** Reemplazar `if opt.is_some() && opt.unwrap() ...`.
- **Inspección con `inspect` / `inspect_err`:** Logging o trazas sin mutar el `Result` u `Option`.
- **División con `str::split_once`:** Usar `if let Some((k, v)) = s.split_once(':')` en vez de `.split()`.
- **Rangos Exclusivos (Half-Open Ranges):** `match status { 200..300 => ..., 400..500 => ... }`.
- **Inline Const Expressions (`const { ... }`):** Inicialización de arrays sin `Copy` o aserciones en tiempo de compilación.

### 4.3 Concurrencia, Async y Prevención de Deadlocks
- **Async Closures (`async || {}`) & `AsyncFn / AsyncFnMut`:** Operaciones asíncronas reusables sin boxing de futures.
- **AFIT & RPITIT con `use<..>`:** Async traits nativos sin dependencias externas.
- **Aislamiento de SQLite en Async (`spawn_blocking`):**
  - Prohibido llamar a `conn.lock()` o ejecutar consultas rusqlite en el hilo de trabajo de Tokio.
  - Aislar operaciones síncronas de SQLite o cómputo pesado en `tokio::task::spawn_blocking(move || { let conn = conn_arc.lock(); ... })`.
- **Prohibición de Locks a través de `.await`:**
  - Prohibido retener `MutexGuard` o referencias a `RefCell` a través de puntos de suspensión `.await`.
  - Acotar el scope del guard en bloques `{ let conn = ...; ... }` o llamar a `drop(conn)` antes de cualquier `.await`.
- **Prevención de Mutex Reentrante (Doble Bloqueo):**
  - `parking_lot::Mutex` y `std::sync::Mutex` no son reentrantes.
  - No llamar a métodos de `repo.*` con un guard de `conn` activo en el mismo hilo. Usar `openproxy_db::<modulo>::<fn>(&conn, ...)` pasando la referencia `&conn`.
  - No re-adquirir `self.conn.lock()` en la misma función tras haber obtenido un guard previo.
- **Liberación de Locks Antes de Publicaciones:**
  - Llamar a `drop(conn)` antes de publicar en canales o buses de eventos (`publish_notification`, `broadcast::Sender`, websockets, callbacks).
- **Tablas Concurrentes y Atómicos:**
  - Usar `entry(key).or_insert_with(...)` y `Ordering::Relaxed` para contadores y timestamps concurrentes sin necesidad de `SeqCst`.
- **Canales con Backpressure:** No usar `unbounded_channel` para colas de proxies, bufferings SSE o ingestiones masivas. Usar `channel(N)` con límite explícito.
- **Ciclo de Vida de Tareas en Fondo:** Todo `tokio::spawn` de larga duración o bucle `loop {}` debe escuchar un canal de shutdown (`broadcast::Receiver<()>` / `CancellationToken`).

### 4.4 Manejo Seguro de Strings y Fronteras UTF-8
- **Prohibición de Slicing Crudo:** Queda terminantemente prohibido rebanar `&str` o indexar bytes (`&s[..n]` o `&s[start..end]`) sin validación explícita de frontera de caracteres. En cadenas multibyte (emojis, tildes, caracteres CJK), el slicing directo sobre un límite no alineado causará un `panic` irrecuperable.
- **Validación con `is_char_boundary`:** Validar siempre con `s.is_char_boundary(n)`. Si no es límite válido, retroceder de forma iterativa hasta el límite UTF-8 anterior o avanzar usando `.char_indices()`.

### 4.5 Protocolos Streaming (SSE) y Contabilidad de Tokens
- **Terminadores de Protocolo Obligatorios:** En flujos SSE (tanto `/v1/chat/completions` como `/v1/responses`), el stream DEBE concluir siempre con el evento terminal canónico (`data: [DONE]` para Chat Completions; eventos terminales como `response.completed` o `response.done` para Responses API). Omitir el terminador provoca desconexiones abruptas o errores de timeout en clientes y arneses (ej. DeepSeek Harness, SDKs oficiales).
- **Preservación Acumulativa de Uso:** Durante la agregación de fragmentos de uso de proveedores upstream (ej. deltas de Anthropic/OpenAI), preservar el conteo acumulativo de `prompt_tokens` y `completion_tokens` en vez de sobreescribir con ceros o deltas vacíos en chunks intermedios.

### 4.6 Normalización de Payloads y Resiliencia de Upstreams LLM (Serde Strictness)
- **Incompatibilidad de Esquemas en Serde Nativo:** Múltiples upstreams compatibles con OpenAI (como NVIDIA NIM, Fireworks, Kilocode, Bai, vLLM) están construidos en Rust con Serde estricto (`async-openai`). No toleran extensiones no estándar:
  - **Mensajes `assistant`, `system` y `tool`:** No admiten arrays de partes estructuradas (`output_text`, `annotations`, `thinking`, `reasoning`). Deben aplanarse a un string plano (`Value::String(m.extract_text())`) antes de serializar hacia el upstream.
  - **Mensajes `user`:** Arrays de texto puro deben aplanarse a string plano; arrays multimodales (imágenes/audio) deben normalizarse convirtiendo `output_text`/`input_text` a `"text"` y eliminando campos propietarios (`annotations`).
  - **Parámetros no Estándar de Responses:** Parámetros como `prompt_cache_key`, `prompt_cache_retention`, `instructions`, `input`, `previous_response_id`, `store`, `background`, `truncation`, `disabled` son rechazados con `400 Bad Request` por upstreams estrictos. Deben depurarse en `OpenaiFormatter` antes de la serialización hacia upstreams OpenAI.
  - **Frontera de Sanitización:** Esta normalización debe residir exclusivamente en la capa de serialización hacia el upstream (`TargetFormatter` / adapters), NUNCA recortando campos de los modelos internos del dominio o middleware que rompan contratos entre módulos o tests internos.

---

## 5. Corrección de Patrones No Permitidos

| Patrón No Permitido | Corrección |
| :--- | :--- |
| **`.unwrap()` / `.expect()` en producción** | Propagar con `?`, usar `.ok_or_else()`, `.unwrap_or()` o fallbacks defensivos. |
| **`for i in 0..len` / `arr[i]`** | Iteración directa (`.iter()`, `.into_iter()`, `.enumerate()`, `.windows()`, `.zip()`). |
| **Indexación de `&str[..n]` sin char boundary** | Validar con `s.is_char_boundary(n)` retrocediendo hasta el límite válido antes de cortar o usar `.char_indices()`. |
| **`static mut`** | Prohibido. Usar `Atomic*`, `OnceLock` o structs de estado sincronizados. |
| **Supresión de advertencias (`#[allow(clippy::...)]`)** | Prohibido silenciar linters. Corregir la causa raíz. |
| **Placeholders SQL dinámicos manuales** | Usar helpers de batch (`batch_insert!`, `query_in_chunks`). |
| **Lógica duplicada entre crates** | Centralizar en `openproxy-types`, `openproxy-db` o `openproxy-core` y reexportar con `pub use`. |
| **Queries SQLite síncronas en hilo async Tokio** | Aislar con `tokio::task::spawn_blocking(move || { ... })`. |
| **Retener `MutexGuard` a través de `.await`** | Liberar o hacer `drop(guard)` antes de cualquier punto de suspensión `.await`. |
| **Llamar a `repo.*` con lock `conn` activo (Deadlock)** | Pasar `&conn` a funciones `openproxy_db::*` directamente en vez de llamar a métodos de `repo`. |
| **Publicar eventos o broadcasts con lock activo** | Llamar a `drop(conn)` antes de `publish_notification` o buses de eventos. |
| **Podar CSS, tooltips o features para bajar LOC** | Prohibido. Descomponer el archivo en submódulos cohesivos (<500-800 LOC) preservando el 100% de la funcionalidad y fidelidad visual. |
| **Borrar selectores CSS asumiendo desuso vía grep** | Prohibido. Lit-HTML interpola clases en runtime. Preservar y particionar por vista (`views/<vista>.css`). |
| **Sobrescribir selectores globales en hojas de vista** | Acotar selectores al contenedor de la vista (`#main .view-specific`). Las reglas de componentes (`button.*`, `.actions`, `.responsive-card-table`, `.slider`, `.chip`) pertenecen a `components/`. |
| **Borrar selectores o reglas móviles (`.mobile-*-cell`)** | Prohibido. Preservar intactos los estilos de tarjetas móviles y `@media` blocks. |
| **Arrays estructurados en mensajes assistant/system/tool hacia upstreams OpenAI** | Aplanar a string canónico en `OpenaiFormatter`. |
| **Enviar parámetros de Responses (`prompt_cache_key`, etc.) a `/v1/chat/completions`** | Depurar en `OpenaiFormatter` antes de serializar hacia el upstream. |
| **Cerrar streaming SSE sin frame terminal** | Emitir siempre `[DONE]` (Chat) o evento terminal canónico (Responses). |
| **Sobrescribir `prompt_tokens` con deltas vacíos en streaming** | Acumular tokens preservando valores previos no nulos. |
| **Modificar firmas de arneses de test sin sincronizar E2E** | Actualizar llamadores y mocks en el mismo commit para no quebrar CI. |
| **Múltiples `cargo test` concurrentes sobre el mismo `target/`** | Ejecutar un único `cargo test --workspace` o aislar por crate disjunto. |
| **Volcar logs crudos de tests/build en el chat entre agentes** | Escribir a archivos de artefacto (`.agents/.../handoff.md`) y referenciar la ruta. |
| **Workers concurrentes modificando el mismo crate** | Particionar tareas por crates disjuntos con contratos de interfaz predefinidos. |
| **Generación continua sin checkpoints (>10 min)** | Time-box estricto: emitir diffs y checkpoints modulares cada <10 min. |
| **Orquestador re-generando contexto mientras espera workers** | Espera reactiva pasiva sin llamadas superfluas ni pre-fills de chat. |

---

## 6. Base de Datos SQLite, Cifrado y Migraciones

1. **Migraciones Secuenciales:**
   - Registrar cada nuevo esquema numerado en `crates/openproxy-db/migrations/` (ej. `000060_feature_name.sql`) y en el array `MIGRATIONS` de `crates/openproxy-db/src/migrations.rs`.
2. **Cifrado en Reposo:**
   - Sellar credenciales upstream, tokens OAuth y API keys privadas con **AES-256-GCM** mediante la master key en `OPENPROXY_MASTER_KEY`.
3. **Cascadas de Borrado (`ON DELETE CASCADE`):**
   - Asegurar claves foráneas para que la eliminación de padres limpie tablas hijas (cooldowns, targets, etc.).
4. **Mapeo Tipado de Filas (`map_row_fields!`):**
   - Usar macros para mapear `rusqlite::Row` con calificadores `@bool(idx)`, `@u16(idx)`, `@json(idx)` y `@enum(idx, Type)`.
5. **Nombres de Tablas Tipados:**
   - Usar enums de tablas en operaciones de purga o mantenimiento; no hardcodear strings.

---

## 7. Directivas del Frontend Web (Dashboard SPA)

1. **Stack:** TypeScript + Lit-HTML + Vanilla CSS (usando design tokens de `tokens.css` y `themes.css`). Gráficas en tiempo real con **`uPlot`**.
2. **Modularidad y Límite de Estilos (<800 LOC):**
   - Cada vista tiene su hoja de estilos modular en `styles/views/<vista>.css` (y `<vista>_mobile.css` si el bloque responsive es extenso). Todas se importan en `views.css`.
   - Ningún archivo CSS puede superar 800 LOC. Si crece, particionarlo por responsabilidades de UI, nunca borrar selectores existentes.
   - Prohibido inventar clases sintéticas en lugar de reutilizar o modularizar los estilos existentes del diseño base.
3. **Contraste y Temas:**
   - Verificar legibilidad en tema oscuro (`:root[data-theme="dark"]`).
   - Evitar azules o colores oscuros con bajo contraste sobre fondos oscuros (usar `CHART_COLORS.blue = "#38bdf8"`).
4. **Preservación de Tooltips e Interactividad:**
   - Mantener siempre tooltips (`abbr[title]`), badges de estado dinámicos, hints y feedback visual.
   - Entradas numéricas con sliders en el playground deben usar `@change` en el campo de texto para no bloquear la edición de decimales (`0.`).
5. **Compilación Web:**
   - Ejecutar `pnpm --dir crates/openproxy-server/web run build` tras modificar `crates/openproxy-server/web/src/` antes de compilar el binario Rust para incrustar los assets actualizados.
6. **Aislamiento de Cascada y Jerarquía de Reglas CSS:**
   - La propiedad de los componentes pertenece estrictamente a `styles/components/`:
     - `components/forms.css`: posee las variantes de botones (`button.primary`, `button.small`, `button.danger`, `button.secondary`, `button.success`) y `.actions` genéricas.
     - `components/tables.css`: posee `.responsive-card-table` y los bloques `@media` responsive compartidos.
     - `components/badges.css`: posee `.chip`, `.status-pill` y variantes de color.
   - Queda prohibido declarar reglas de componentes con alcance global en hojas de vista (`views/*.css`).
   - Todo selector en hojas de vista debe acotarse al contenedor de la vista (ej. `#main .view-specific` o `.view-proxies .actions:has(button)`).
7. **Preservación Móvil y Responsive:**
   - Queda prohibido eliminar selectores de elementos móviles (ej. `.mobile-proxy-source-card-cell { display: none }`), ya que causan filtración de columnas móviles al layout de escritorio y rompen la renderización en tarjetas móviles.

---

## 8. Verificación Pre-Commit

### 8.1 Auditoría Lógica y Visual
- ¿Algún `let-else` alteró el tipo o mensaje de error original?
- ¿Algún `split_once` asumió separadores inexistentes rompiendo casos borde?
- ¿Alguna indexación o rebanado de `&str` corta bytes sin validar `is_char_boundary` o usar `.char_indices()`?
- ¿Se verificó que `OpenaiFormatter` aplane mensajes estructurados y purgue parámetros de Responses (`prompt_cache_key`, etc.) para evitar errores 400 en upstreams Serde estrictos?
- ¿Se verificó que todo flujo SSE emita su terminador canónico de protocolo (`[DONE]` / `response.completed`)?
- ¿Se agregaron tests unitarios para toda función extraída de $>20$ líneas?
- ¿Se verificó que ningún selector CSS, tooltip o feature de interfaz fue eliminado en el refactor?
- ¿Se verificó que no existan cascade leaks globales en archivos CSS particionados?
- ¿Se verificó que 0 archivos superan 800 LOC en `crates/` y `web/`?

### 8.2 Comandos de Verificación
1. **Linter Rust:**
   ```bash
   cargo clippy --workspace --all-targets -- -D warnings
   ```
2. **Pruebas Rust:**
   ```bash
   cargo test --workspace
   ```
3. **Frontend:**
   ```bash
   pnpm --dir crates/openproxy-server/web run typecheck:all
   pnpm --dir crates/openproxy-server/web run test
   pnpm --dir crates/openproxy-server/web run test:e2e
   pnpm --dir crates/openproxy-server/web run build
   ```
4. **Auditoría de Tamaño (<800 LOC):**
   ```bash
   python3 -c "import os; [print(f'{len(open(os.path.join(r,f)).readlines()):4} {os.path.join(r,f)}') for r,_,fs in os.walk('crates') if 'node_modules' not in r and 'target' not in r and 'dist' not in r for f in fs if f.endswith(('.rs','.ts','.css')) and len(open(os.path.join(r,f)).readlines()) > 800]"
   ```
5. **Commits:** Formato Conventional Commits (`feat(...)`, `fix(...)`, `refactor(...)`, `docs(...)`, `perf(...)`).

---

## 9. Gobernanza de Orquestación Multi-Agente y Eficiencia de Tokens

Para prevenir cuellos de botella de compilación, sobreconsumo desmedido de tokens (KV-cache blowup) y contención en el sistema de archivos:

1. **Compilación y Test Centralizado (Un Solo `cargo test --workspace`):**
   - Prohibido lanzar múltiples ejecuciones concurrentes de `cargo test` (ej. un runner para "otros crates", otro para "workspace" y otro para "submódulo"). Al compartir `target/` o locks del sistema de archivos, Cargo serializa la compilación triplicando el tiempo de CPU y quemando tokens innecesarios.
   - Ejecutar **una sola pasada integral** (`cargo test --workspace`) por compuerta de validación o tras finalizar la integración.
2. **Particionamiento Disjunto por Crate (No por Herramienta):**
   - Si se requiere paralelismo real, asignar a cada subagente un conjunto de crates estrictamente disjunto (`openproxy-pipeline`, `openproxy-adapters`, `openproxy-db`, `openproxy-server`). Prohibido solapar workers concurrentes sobre el mismo crate sin contratos de interfaz congelados previamente.
3. **Comunicación Vía Artefactos en Disco (Cero Logs Masivos en Chat):**
   - Prohibido volcar salidas completas de compilación, trazas o logs masivos de tests en los mensajes entre agentes.
   - Todo worker debe escribir sus hallazgos, diffs y reportes en un archivo de artefacto en disco (`.agents/<subagent>/handoff.md`). El agente sucesor o revisor debe leer el archivo mediante herramientas de lectura (`view_file`), nunca arrastrar el transcript completo en su ventana de contexto.
4. **Time-Boxing y Checkpoints Obligatorios (<10 Minutos):**
   - Toda tarea de generación de un agente debe producir un checkpoint verificable en menos de 10 minutos. Tareas en estado "Generating" más allá de 10 minutos constituyen un fallo de diseño y deben ser canceladas o reestructuradas.
5. **Silencio del Orquestador durante Esperas (Sin Re-Prefills):**
   - El orquestador debe suspender su ejecución (`idle` / reactivo) mientras los subagentes trabajan. Queda prohibido que el orquestador genere reflexiones o encadenamientos de pensamientos mientras espera, evitando re-prefills masivos que agotan la cuota global.

