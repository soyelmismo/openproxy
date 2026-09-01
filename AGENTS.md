# AGENTS.md: Directivas de ingeniería para openproxy

Reglas de arquitectura, calidad y flujo de trabajo para agentes y desarrolladores en openproxy.

---

## 1. Filosofía de Trabajo y Comunicación

1. **Zero-Chat & Máximo SNR:** Emitir solo código limpio, parches exactos y respuestas técnicas de alta densidad. Cero texto de relleno o explicaciones redundantes.
2. **Jerarquía Lazy de Implementación:**
   $$\text{Stdlib robusta} > \text{Reusar código/traits locales} > \text{API nativa} > \text{Dependencia actual} > \text{Código mínimo}$$
3. **Causa Raíz:** Corregir la función base o el diseño de tipos. No parchar síntomas locales.
4. **Cero Dependencias Inútiles:** No añadir dependencias a `Cargo.toml` o `package.json` sin justificación crítica.
5. **Formato de Salida:** Bloques `SEARCH/REPLACE` con anclas exactas (mínimo 2 líneas antes y después). No generar archivos completos.

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

---

## 5. Corrección de Patrones No Permitidos

| Patrón No Permitido | Corrección |
| :--- | :--- |
| **`.unwrap()` / `.expect()` en producción** | Propagar con `?`, usar `.ok_or_else()`, `.unwrap_or()` o fallbacks defensivos. |
| **`for i in 0..len` / `arr[i]`** | Iteración directa (`.iter()`, `.into_iter()`, `.enumerate()`, `.windows()`, `.zip()`). |
| **Indexación de `&str[..n]` sin char boundary** | Validar con `s.is_char_boundary(n)` retrocediendo hasta el límite válido antes de cortar. |
| **`static mut`** | Prohibido. Usar `Atomic*`, `OnceLock` o structs de estado sincronizados. |
| **Supresión de advertencias (`#[allow(clippy::...)]`)** | Prohibido silenciar linters. Corregir la causa raíz. |
| **Placeholders SQL dinámicos manuales** | Usar helpers de batch (`batch_insert!`, `query_in_chunks`). |
| **Lógica duplicada entre crates** | Centralizar en `openproxy-types`, `openproxy-db` o `openproxy-core` y reexportar con `pub use`. |
| **Queries SQLite síncronas en hilo async Tokio** | Aislar con `tokio::task::spawn_blocking(move || { ... })`. |
| **Retener `MutexGuard` a través de `.await`** | Liberar o hacer `drop(guard)` antes de cualquier punto de suspensión `.await`. |
| **Llamar a `repo.*` con lock `conn` activo (Deadlock)** | Pasar `&conn` a funciones `openproxy_db::*` directamente en vez de llamar a métodos de `repo`. |
| **Publicar eventos o broadcasts con lock activo** | Llamar a `drop(conn)` antes de `publish_notification` o buses de eventos. |

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
2. **Contraste y Temas:**
   - Verificar legibilidad en tema oscuro (`:root[data-theme="dark"]`).
   - Evitar azules o colores oscuros con bajo contraste sobre fondos oscuros (usar `CHART_COLORS.blue = "#38bdf8"`).
3. **Compilación Web:**
   - Ejecutar `pnpm --dir crates/openproxy-server/web run build` tras modificar `crates/openproxy-server/web/src/` antes de compilar el binario Rust para incrustar los assets actualizados.

---

## 8. Verificación Pre-Commit

### 8.1 Auditoría Lógica
- ¿Algún `let-else` alteró el tipo o mensaje de error original?
- ¿Algún `split_once` asumió separadores inexistentes rompiendo casos borde?
- ¿Se agregaron tests unitarios para toda función extraída de $>20$ líneas?

### 8.2 Comandos de Verificación
1. **Linter:**
   ```bash
   cargo clippy --workspace --all-targets -- -D warnings
   ```
2. **Pruebas:**
   ```bash
   cargo test --workspace
   ```
3. **Frontend (si aplica):**
   ```bash
   pnpm --dir crates/openproxy-server/web run typecheck
   pnpm --dir crates/openproxy-server/web run build
   ```
4. **Commits:** Formato Conventional Commits (`feat(...)`, `fix(...)`, `refactor(...)`, `docs(...)`, `perf(...)`).
