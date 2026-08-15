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

## 3. Directivas de Rust Moderno (Rust 1.80+ / Edition 2024)

### 3.1 Primitivos Contemporáneos de la Stdlib
- **`std::sync::LazyLock<T>` y `std::sync::OnceLock<T>`:** Primitivos nativos obligatorios para inicialización estática. Prohibido el uso de `lazy_static!` o `once_cell`.
- **`std::io::IsTerminal`:** Para detección nativa de terminales (nunca `atty` ni `is-terminal`).
- **`std::path::absolute`:** Para normalización canónica de rutas del sistema de archivos.

### 3.2 Control de Flujo Idiomático
- **`let-else`:** Para validación temprana y desempaquetado sin anidamiento excesivo:
  ```rust
  let Some(value) = opt else {
      return Err(CoreError::NotFound);
  };
  ```
- **Predicados con `is_some_and` / `is_ok_and`:** Evitar `if opt.is_some() && opt.unwrap() ...`.
- **División con `str::split_once`:** Usar `if let Some((k, v)) = s.split_once(':')` en lugar de colecciones intermedias con `.split()`.
- **Rangos Exclusivos (Half-Open Ranges):** `match status { 200..300 => ..., 400..500 => ... }`.

### 3.3 Concurrencia, Memoria y Prevención de Deadlocks
- **Zero-Copy con `Cow<'a, T>`:** Para transformaciones donde comúnmente no se requiere mutar la entrada.
- **Prevención Estricta de Deadlocks con Mutex:**
  - `parking_lot::Mutex` y `std::sync::Mutex` **NO son reentrantes**.
  - **REGLA DE ORO:** Jamás invocar métodos del repositorio (`repo.*`) ni funciones que soliciten `self.conn.lock()` mientras se mantiene un guard activo (`let conn = conn_clone.lock()`) en el mismo hilo.
  - Liberar siempre los guards de mutex antes de llamar a funciones externas o delegar tareas.
- **Async No Bloqueante:** Toda operación pesada o síncrona de SQLite debe aislarse con `tokio::task::spawn_blocking`.

---

## 4. Remediación de Malas Prácticas (Clean Refactor)

| Mala Práctica | Remediación Obligatoria |
| :--- | :--- |
| **`.unwrap()` / `.expect()` en producción** | Propagar con `?`, usar `.ok_or_else()`, `.unwrap_or()` o fallbacks defensivos. |
| **`for i in 0..len` / `arr[i]`** | Iteración directa (`.iter()`, `.into_iter()`, `.enumerate()`, `.windows()`, `.zip()`). |
| **`static mut`** | Prohibido. Usar `Atomic*`, `OnceLock` o structs de estado sincronizados. |
| **Supresión de advertencias (`#[allow(clippy::...)]`)** | Prohibido silenciar linters. Corregir siempre la causa raíz. |
| **Placeholders SQL dinámicos manuales** | Usar helpers de batch (`batch_insert!`, `query_in_chunks`). |
| **Lógica duplicada entre crates** | Centralizar en `openproxy-types`, `openproxy-db` o `openproxy-core` y reexportar con `pub use`. |

---

## 5. Base de Datos SQLite, Cifrado y Migraciones

1. **Migraciones Secuenciales:**
   - Todo cambio de esquema requiere un nuevo script numerado en `crates/openproxy-db/migrations/` (ej. `000060_feature_name.sql`).
   - Debe registrarse inmediatamente en el arreglo `MIGRATIONS` de `crates/openproxy-db/src/migrations.rs`.
2. **Cifrado en Reposo:**
   - Credenciales upstream, tokens OAuth y API keys privadas deben sellarse con **AES-256-GCM** mediante la master key configurada en `OPENPROXY_MASTER_KEY`.
3. **Cascadas de Borrado (`ON DELETE CASCADE`):**
   - Asegurar relaciones relacionales con claves foráneas para que la eliminación de entidades padre limpie sus tablas hijas (cooldowns, targets, etc.) automáticamente.

---

## 6. Directivas del Frontend Web (Dashboard SPA)

1. **Stack Tecnológico:**
   - TypeScript + Lit-HTML + Vanilla CSS (usando los Design Tokens de `tokens.css` y `themes.css`).
   - Gráficas en tiempo real construidas exclusivamente con **`uPlot`** para máxima eficiencia.
2. **Contraste y Temas:**
   - Verificar siempre legibilidad en tema oscuro (`:root[data-theme="dark"]`).
   - Evitar azules o colores oscuros con bajo contraste en gráficas sobre fondos oscuros (utilizar `CHART_COLORS.blue = "#38bdf8"`).
3. **Flujo de Compilación Web:**
   - Si se modifica el código en `crates/openproxy-server/web/src/`, es **obligatorio** ejecutar `pnpm --dir crates/openproxy-server/web run build` antes de compilar el binario Rust para que `rust-embed` incruste los assets actualizados.

---

## 7. Protocolo Obligatorio de Verificación Pre-Commit

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
