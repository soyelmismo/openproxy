# AGENTS.md — Guía Maestra de Ingeniería y Calidad de Código para openproxy

Este documento establece las directivas arquitectónicas, estándares de calidad y protocolos de trabajo obligatorios para cualquier agente IA o desarrollador que opere en el repositorio **openproxy**.

---

## 1. Filosofía de Trabajo y Comunicación

1. **Zero-Chat & Máximo SNR:** Emitir solo código limpio, parches exactos y respuestas técnicas de alta densidad. Cero texto de relleno o explicaciones redundantes.
2. **Jerarquía Lazy de Implementación:**
   $$\text{Stdlib robusta} > \text{Reusar código/traits locales} > \text{API nativa} > \text{Dependencia actual} > \text{Código mínimo}$$
3. **Causa Raíz:** Arreglar siempre la función base o el diseño de tipos. Prohibido parchar síntomas locales.
4. **Cero Dependencias Inútiles:** No introducir nuevas dependencias en `Cargo.toml` ni `package.json` sin justificación crítica.
5. **Formato de Salida:** Bloques `SEARCH/REPLACE` con anclas exactas (mínimo 2 líneas antes/después). Nunca generar archivos completos innecesariamente.

---

## 2. Mapa del Workspace y Responsabilidades

El repositorio es un monorepo modular en Rust + Frontend embebido:

```text
openproxy/
├── crates/
│   ├── openproxy-types/        # Structs de dominio, Enums, Error (CoreError), Ids (ProviderId, ModelId)
│   ├── openproxy-db/           # SQLite (rusqlite bundled), migraciones SQL, cifrado AES-256-GCM, repositorios
│   ├── openproxy-pipeline/     # Enrutamiento, dispatcher upstream, combo resolution, race execution, cooldowns
│   ├── openproxy-adapters/     # Adapters nativos de proveedores LLM (OpenAI, Anthropic, Gemini, SSE parsing)
│   ├── openproxy-core/         # Lógica de negocio headless, sincronización de modelos, OAuth, notificaciones
│   ├── openproxy-compression/ # Compresión de payloads (Lite y RTK command filtering)
│   ├── openproxy-server/       # Binario axum: rutas /v1/*, /admin/api/*, WebSocket /admin/ws, assets embebidos
│   │   └── web/                # Frontend SPA (TypeScript + Lit-HTML + esbuild + uPlot)
│   └── openproxy-api-client/   # Cliente SDK en Rust para la API REST administrativa
├── docs/                       # Documentación técnica, diagramas y especificaciones
└── config.example.toml         # Configuración TOML de arranque del servidor
```

### Reglas de frontera entre crates:
- **`openproxy-types`**: Cero dependencias pesadas; contiene exclusivamente tipos de datos e interfaces compartidas.
- **`openproxy-db`**: Toda consulta SQL debe estar encapsulada aquí. Cero queries SQL dispersas en los handlers del servidor.
- **`openproxy-adapters`**: Toda serialización y mapeo de protocolos upstream (OpenAI $\leftrightarrow$ Anthropic $\leftrightarrow$ Gemini) se aísla aquí.

---

## 3. Criterios de ROI, Deduplicación y Reglas de Parada (`rust-dedup-modernize`)

### 3.1 Clasificación de Prioridad por Retorno de Inversión (ROI)
| Prioridad | Categoría | Condición de Ejecución |
| :--- | :--- | :--- |
| **P1** | Eliminar dependencia externa (`once_cell` $\to$ `LazyLock`, etc.) | **Siempre ejecutar** |
| **P2** | Deduplicar lógica real ($>10$ líneas idénticas entre módulos) | **Siempre ejecutar** |
| **P3** | Extraer funciones monolíticas ($>100$ líneas) | Solo si se agrega al menos 1 test unitario para la función extraída |
| **P4** | Modernización de sintaxis (`let-else`, `is_some_and`, `split_once`) | Solo si reduce $\ge 3$ líneas netas por sitio de aplicación |
| **P5** | Reducción de `.clone()` innecesario | Solo con evidencia en hot paths medidos o bucles de alto tráfico |

### 3.2 Reglas de Parada y Anti-Bucle
- P1-P2 se ejecutan siempre. P3 exige tests unitarios de la función extraída.
- P4-P5 se agrupan como cleanup cosmético. Si el diff de P1-P3 supera 300 líneas, descartar P4-P5 de la sesión.
- **NUNCA mezclar prioridades distintas en el mismo commit.**
- Si un cambio cosmético de P4-P5 falla en compilar o testear al 2do intento, **descartarlo inmediatamente**.

### 3.3 Técnicas de Deduplicación Zero-Cost
1. **Extension Traits (Blanket Implementations):** Para transformaciones comunes sin envolver tipos en nuevos structs (`pub trait ResponseExt`).
2. **Macros Declarativas (`macro_rules!`):** Para deduplicar implementaciones de adapters o dispatchers.
3. **Traits con Métodos por Defecto:** Para endpoints base y URLs de upstream APIs.
4. **Copy-on-Write y Zero-Alloc:**
   - Usar `std::borrow::Cow<'a, T>` para transformaciones que raramente requieren mutar la entrada.
   - Usar `Arc::unwrap_or_clone(arc)` para evitar clonar el contenido si el `Arc` es el único dueño.
   - Usar `write!(buf, ...)` con buffer reutilizado en hot paths para evitar allocaciones de formato intermedias.

---

## 4. Directivas de Rust Moderno (Rust 1.80+ / Edition 2024)

### 4.1 Primitivos Contemporáneos de la Stdlib
- **`std::sync::LazyLock<T>` y `std::sync::OnceLock<T>`:** Primitivos nativos obligatorios para inicialización estática. Prohibido el uso de `lazy_static!` o `once_cell`.
- **`std::num::NonZero<T>`:** Usar el tipo genérico nativo de la stdlib (en lugar de `NonZeroU32`, etc.).
- **`std::io::IsTerminal`:** Para detección nativa de terminales (nunca `atty` ni `is-terminal`).
- **`std::path::absolute`:** Para normalización canónica de rutas.
- **`std::fs::exists(&path)`:** Detección segura que retorna `io::Result<bool>`.

### 4.2 Control de Flujo Idiomático
- **`let-else`:** Para validación temprana y desempaquetado sin anidamiento excesivo:
  ```rust
  let Some(value) = opt else {
      return Err(CoreError::NotFound);
  };
  ```
- **Predicados con `is_some_and` / `is_ok_and`:** Evitar `if opt.is_some() && opt.unwrap() ...`.
- **Inspección de Flujo con `inspect` / `inspect_err`:** Para logging/trazas intermedias sin alterar el `Result`/`Option`.
- **División con `str::split_once`:** Usar `if let Some((k, v)) = s.split_once(':')` en lugar de colecciones intermedias con `.split()`.
- **Rangos Exclusivos (Half-Open Ranges):** `match status { 200..300 => ..., 400..500 => ... }`.
- **Inline Const Expressions (`const { ... }`):** Para inicialización de arrays sin `Copy` o aserciones en tiempo de compilación.

### 4.3 Concurrencia, Async y Prevención de Deadlocks
- **Async Closures (`async || {}`) & `AsyncFn / AsyncFnMut`:** Para operaciones asíncronas reusables sin boxing de futures.
- **AFIT & RPITIT con Captura Precisa `use<..>`:** Async traits nativos sin dependencias externas.
- **Prevención Estricta de Deadlocks con Mutex:**
  - `parking_lot::Mutex` y `std::sync::Mutex` **NO son reentrantes**.
  - **REGLA DE ORO:** Jamás invocar métodos del repositorio (`repo.*`) ni funciones que soliciten `self.conn.lock()` mientras se mantiene un guard activo (`let conn = conn_clone.lock()`) en el mismo hilo.
  - Liberar siempre los guards de mutex antes de llamar a funciones externas o delegar tareas.
- **Async No Bloqueante:** Toda operación pesada o síncrona de SQLite debe aislarse con `tokio::task::spawn_blocking`.

---

## 5. Remediación de Malas Prácticas (Clean Refactor)

| Mala Práctica | Remediación Obligatoria |
| :--- | :--- |
| **`.unwrap()` / `.expect()` en producción** | Propagar con `?`, usar `.ok_or_else()`, `.unwrap_or()` o fallbacks defensivos. |
| **`for i in 0..len` / `arr[i]`** | Iteración directa (`.iter()`, `.into_iter()`, `.enumerate()`, `.windows()`, `.zip()`). |
| **`static mut`** | Prohibido. Usar `Atomic*`, `OnceLock` o structs de estado sincronizados. |
| **Supresión de advertencias (`#[allow(clippy::...)]`)** | Prohibido silenciar linters. Corregir siempre la causa raíz. |
| **Placeholders SQL dinámicos manuales** | Usar helpers de batch (`batch_insert!`, `query_in_chunks`). |
| **Lógica duplicada entre crates** | Centralizar en `openproxy-types`, `openproxy-db` o `openproxy-core` y reexportar con `pub use`. |

---

## 6. Base de Datos SQLite, Cifrado y Migraciones

1. **Migraciones Secuenciales:**
   - Todo cambio de esquema requiere un nuevo script numerado en `crates/openproxy-db/migrations/` (ej. `000060_feature_name.sql`).
   - Debe registrarse inmediatamente en el arreglo `MIGRATIONS` de `crates/openproxy-db/src/migrations.rs`.
2. **Cifrado en Reposo:**
   - Credenciales upstream, tokens OAuth y API keys privadas deben sellarse con **AES-256-GCM** mediante la master key configurada en `OPENPROXY_MASTER_KEY`.
3. **Cascadas de Borrado (`ON DELETE CASCADE`):**
   - Asegurar relaciones relacionales con claves foráneas para que la eliminación de entidades padre limpie sus tablas hijas (cooldowns, targets, etc.) automáticamente.

---

## 7. Directivas del Frontend Web (Dashboard SPA)

1. **Stack Tecnológico:**
   - TypeScript + Lit-HTML + Vanilla CSS (usando los Design Tokens de `tokens.css` y `themes.css`).
   - Gráficas en tiempo real construidas exclusivamente con **`uPlot`** para máxima eficiencia.
2. **Contraste y Temas:**
   - Verificar siempre legibilidad en tema oscuro (`:root[data-theme="dark"]`).
   - Evitar azules o colores oscuros con bajo contraste en gráficas sobre fondos oscuros (utilizar `CHART_COLORS.blue = "#38bdf8"`).
3. **Flujo de Compilación Web:**
   - Si se modifica el código en `crates/openproxy-server/web/src/`, es **obligatorio** ejecutar `pnpm --dir crates/openproxy-server/web run build` antes de compilar el binario Rust para que `rust-embed` incruste los assets actualizados.

---

## 8. Protocolo de Auditoría y Verificación Pre-Commit

### 8.1 Preguntas Críticas de Auditoría Lógica
- ¿Algún `let-else` cambió la rama de escape alterando el tipo o mensaje de error original?
- ¿Algún `split_once` asumió separadores inexistentes rompiendo casos borde?
- ¿Se agregaron tests unitarios para toda función extraída de $>20$ líneas?

### 8.2 Comandos de Verificación
Antes de emitir cualquier commit:
1. **Linter Estricto:**
   ```bash
   cargo clippy --workspace --all-targets -- -D warnings
   ```
2. **Suite Completa de Pruebas:**
   ```bash
   cargo test --workspace
   ```
3. **Typecheck Frontend (si aplica):**
   ```bash
   pnpm --dir crates/openproxy-server/web run typecheck
   pnpm --dir crates/openproxy-server/web run build
   ```
4. **Commits Semánticos Atómicos:** Formato Conventional Commits (`feat(...)`, `fix(...)`, `refactor(...)`, `docs(...)`, `perf(...)`).
