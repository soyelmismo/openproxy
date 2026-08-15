# AGENTS.md — Directivas de Ingeniería y Calidad de Código para openproxy

Este documento establece las directivas obligatorias para cualquier agente IA o desarrollador que trabaje en el codebase de **openproxy**.

---

## 1. Filosofía y Estilo de Trabajo

1. **Zero-Chat & Máximo SNR:** Emitir solo código limpio, parches exactos y respuestas técnicas de alta densidad. Cero relleno o explicaciones redundantes.
2. **Jerarquía Lazy de Implementación:**
   $$\text{Stdlib robusta} > \text{Reusar código/traits locales} > \text{API nativa} > \text{Dependencia actual} > \text{Código mínimo}$$
3. **Causa Raíz:** Arreglar siempre la función base o el diseño de tipos. Prohibido parchar síntomas locales.
4. **Cero Dependencias Inútiles:** No introducir nuevas dependencias externas (`Cargo.toml`) sin justificación crítica.

---

## 2. Directivas de Rust Moderno (Rust 1.80+ / Edition 2024)

### 2.1 Stdlib Contemporánea (Prioridad P1)
- **`std::sync::LazyLock<T>` y `std::sync::OnceLock<T>`:** Usar siempre los primitivos nativos de la stdlib en `static`. Prohibido el uso de `lazy_static!` o `once_cell`.
- **`std::io::IsTerminal`:** Para detección de TTY (nunca crates externos como `atty` o `is-terminal`).
- **`std::path::absolute`:** Para normalización canónica de rutas.

### 2.2 Control de Flujo Idiomático
- **`let-else`:** Para validación y salida temprana sin anidamientos innecesarios:
  ```rust
  let Some(value) = opt else {
      return Err(CoreError::NotFound);
  };
  ```
- **Predicados con `is_some_and` / `is_ok_and`:** Evitar `if opt.is_some() && opt.unwrap() ...`.
- **División con `str::split_once`:** Usar `if let Some((k, v)) = s.split_once(':')` en lugar de colecciones intermedias con `.split()`.
- **Rangos Exclusivos (Half-Open Ranges):** `200..300 => ...`, `400..500 => ...`.

### 2.3 Memoria, Concurrencia y Async
- **Zero-Copy con `Cow<'a, T>`:** Para transformaciones donde la entrada comúnmente no requiere mutación.
- **Prevención de Deadlocks:**
  - `parking_lot::Mutex` y `std::sync::Mutex` **NO son reentrantes**.
  - Jamás invocar métodos del repositorio o helpers que bloqueen `self.conn` mientras se mantiene un `conn.lock()` en el mismo hilo.
  - Liberar guards antes de llamar a funciones externas o delegar llamadas.
- **Async No Bloqueante:** Toda operación pesada de I/O de base de datos síncrona debe aislarse en `tokio::task::spawn_blocking`.

---

## 3. Remediación de Malas Prácticas (Clean Code)

| Mala Práctica | Remediación Obligatoria |
| :--- | :--- |
| **`.unwrap()` / `.expect()` en producción** | Propagar con `?`, usar `.ok_or_else()`, `.unwrap_or()` o fallbacks seguros. |
| **`for i in 0..len` / `arr[i]`** | Iteración directa (`.iter()`, `.into_iter()`, `.enumerate()`, `.windows()`, `.zip()`). |
| **`static mut`** | Prohibido. Usar `Atomic*`, `OnceLock`, o structs de estado encapsulados. |
| **Supresión de advertencias (`#[allow(clippy::...)]`)** | Prohibido silenciar linters. Corregir siempre la causa raíz. |
| **Placeholders SQL manuales (`(?1, ?2...)`)** | Usar helpers de batch (`batch_insert!`, `query_in_chunks`). |
| **Lógica duplicada entre crates** | Centralizar en `openproxy-types`, `openproxy-db` o `openproxy-core` y reexportar con `pub use`. |

---

## 4. Deduplicación y Arquitectura Plug & Play

1. **Extension Traits (Blanket Implementations):** Para enriquecer tipos externos o modelos de datos sin envoltorios redundantes.
2. **Traits con Métodos por Defecto:** Proveer URLs y serializaciones base en traits para adapters de proveedores LLM.
3. **Macros Declarativas (`macro_rules!`):** Deduplicar boilerplate de dispatch o mapeos de errores repetitivos.

---

## 5. Protocolo de Verificación y Commits

Antes de cada commit, verificar obligatoriamente:
1. **Linter Estricto:**
   ```bash
   cargo clippy --workspace --all-targets -- -D warnings
   ```
2. **Suite Completa de Pruebas:**
   ```bash
   cargo test --workspace
   ```
3. **Commits Semánticos Atómicos:** Formato Conventional Commits (`feat(...)`, `fix(...)`, `refactor(...)`, `docs(...)`).
