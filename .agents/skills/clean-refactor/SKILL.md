---
name: clean-refactor
description: Skill repo-agnóstica para detectar y corregir rápidamente malas prácticas de rendimiento, memoria y concurrencia, deduplicar lógica mediante macros/traits plug & play, y ejecutar refactors secuenciales con auditoría dual y commit automático.
---

# Clean Refactor & Bad Practices Remediation

Procedimiento repo-agnóstico de alta eficiencia para auditar código, erradicar malas prácticas de rendimiento, memoria y estabilidad, deduplicar lógica mediante abstracciones plug & play y ejecutar refactors secuenciales con doble verificación.

---

## 1. Catálogo de Malas Prácticas y Remediaciones Idiomáticas

### 1.1 `.unwrap()` indiscriminado (Riesgo de Panic)
- **Problema:** Lanza `panic!`, aborta el hilo o el proceso, oculta causas de error en producción.
- **Detección:** Búsqueda de `\.unwrap\(\)` y `\.expect\(` en rutas no-test.
- **Remediación:**
  - Propagar con operador `?` (`Result<T, E>` / `Option<T>`).
  - Proporcionar fallbacks: `.unwrap_or(default)`, `.unwrap_or_else(|| ...)`.
  - Transformar errores: `.ok_or_else(|| CustomError::...)`.
  - Control de flujo defensivo: `if let Some(...) = ...` o `match`.

### ~~1.2-1.3 `clone()` excesivo / `String` por valor~~ → Delegado
> Estas prácticas se auditan exclusivamente desde el skill **`rust-dedup-modernize`** (§0, prioridad P5) con filtro de ROI. No duplicar aquí.

### 1.4 `for i in 0..len` / `while i < len` (Bounds Checking Innecesario)
- **Problema:** Exige verificación de límites en cada acceso `arr[i]`, gasta CPU, propensa a bugs de off-by-one.
- **Detección:** `for\s+\w+\s+in\s+0\.\.`, `while\s+\w+\s*<\s*.*\.len\(\)`, indexación `[i]`, `[idx]`.
- **Remediación:**
  - Iteración directa: `.iter()`, `.iter_mut()`, `.into_iter()`.
  - Iteración con índice: `.iter().enumerate()`.
  - Pares o ventanas contiguas: `.windows(2)`.
  - Iteración en paralelo de colecciones: `.zip()`.
  - Localización de corte: `.position(|&x| ...)` o `.take_while(...)`.
  - Inserción masiva: `.extend(...)`.

### 1.5 `Rc<RefCell<T>>` / `Arc<Mutex<T>>` Ubicuo (Evasión del Borrow Checker)
- **Problema:** Evade el borrow checker estático, penaliza el runtime con locking / ref-counting dinámico y arriesga deadlocks o panics en runtime (`BorrowMutError`).
- **Detección:** Estructuras internas plagadas de `RefCell`, `Mutex` o `RwLock` para estados no compartidos.
- **Remediación:**
  - Repensar ownership: flujo unidireccional de datos.
  - Separar estado mutable de estado inmutable.
  - Uso de canales (MPSC / broadcast) para paso de mensajes en lugar de memoria compartida bloqueada.

### 1.6 `static mut` / Estado Global Mutable (Inseguridad y Concurrencia Rota)
- **Problema:** Requiere bloques `unsafe`, introduce condiciones de carrera (*data races*) y complica los tests concurrentes.
- **Detección:** `static\s+mut\s+`.
- **Remediación:**
  - Pasar estado explícito en structs o contextos de aplicación.
  - Tipos atómicos para enteros/flags simples (`AtomicBool`, `AtomicU64`).
  - Inicialización thread-safe inmutable con `std::sync::OnceLock<T>`.
  - Sincronización segura mediante `Arc<tokio::sync::RwLock<T>>`.

### 1.7 Ignorar o Suprimir Advertencias de Clippy / Linters
- **Problema:** Acumula deuda técnica, oculta anti-patrones y degrada la calidad del repositorio.
- **Detección:** Atributos `#![allow(clippy::...)]` o `#[allow(clippy::...)]` a nivel de crate o módulo.
- **Remediación:**
  - Eliminar supresiones artificiales.
  - Ejecutar verificación estricta: `cargo clippy --workspace --all-targets -- -D warnings`.
  - Corregir la causa raíz (función base o diseño de tipos).

### 1.8 Bloqueos Síncronos en Código Asíncrono
- **Problema:** `std::thread::sleep`, `std::fs`, o llamadas de red bloqueantes dentro de tareas async congelan el reactor/event loop de Tokio.
- **Detección:** Uso de `std::thread::sleep`, `std::sync::Mutex` en paths async continuos con `.await`.
- **Remediación:**
  - Usar alternativas asíncronas (`tokio::time::sleep`, `tokio::fs`).
  - Para trabajo intensivo de CPU o APIs síncronas legadas: `tokio::task::spawn_blocking`.

### 1.9 Aritmética Manual de Placeholders y Desbordamiento en Bases de Datos
- **Problema:** Concatenación manual de `(?1, ?2...)` o `?{base + i}` propensa a sobrepasar límites del motor (ej. `SQLITE_MAX_VARIABLE_NUMBER = 999`).
- **Detección:** Concatenaciones de strings SQL con índices dinámicos en bucles.
- **Remediación:**
  - Helpers genéricos de fragmentación por lotes (`query_in_chunks`).
  - Macros declarativas de inserción en batch (`batch_insert!`) que respetan el límite de parámetros.

### 1.10 Código Clonado y Duplicación de Lógica de Negocio
- **Problema:** Copias de parsers, normalizadores, tablas o helpers entre módulos generan bugs por desincronización.
- **Remediación:**
  - Centralizar en el crate/módulo base (`types`, `common` o `core`).
  - Reexportar limpiamente con `pub use`.

---

## 2. Ciclo de Ejecución "Encuentra > Arregla" (Find -> Fix Loop)

1. **Localizar:** Listar cada ocurrencia exacta con enlace navegable (`[archivo](file:///path/to/file#L10-L25)`).
2. **Proponer Reemplazo Idiomático:** Formular la solución de mayor jerarquía lazy (Stdlib > API nativa > Dependencia actual > Código mínimo).
3. **Parchear Directamente:** Aplicar cambios mínimos y precisos con anclas exactas en el codebase.
4. **Verificar:** Ejecutar linter con advertencias forzadas (ej. `cargo clippy -- -D <lint>`) y suite completa de pruebas unitarias/e2e.

---

## 3. Modularización y Arquitectura Plug & Play

Para habilitar la adición sin fricción de nuevos recursos (proveedores, endpoints, stages de streaming, filtros):
1. **Traits con Defaults:** Proporcionar implementaciones estándar para métodos comunes en el trait base (ej. `build_chat_url`, `models_url`, `fetch_models`).
2. **Macros Declarativas:** Generar boilerplate de configuración en una única invocación de 5 líneas (ej. `declare_openai_adapter!`, `sqlite_batch_insert!`).
3. **Pipelines Modulares:** Componer middleware mediante traits de etapas (`StreamingChunkStage` -> `process_chunk(&mut self, chunk) -> StreamAction`).
4. **Visitors Polimórficos:** Centralizar mutaciones de estructuras complejas (e.g. `mutate_message_text` para strings planos y arrays multipart) evitando código repetitivo.

---

## 4. Pipeline de Refactorización Secuencial (Dual-Agent)

Para refactorizaciones de gran escala:

```
[Punto N] -> [Subagente Refactor] -> [Subagente Reviewer/Auditor] -> [Tests & Lints] -> [Commit & Push] -> [Punto N+1]
```

1. **Subagente Refactor:**
   - Aplica los cambios estructurales acordados.
   - Elimina código redundante y conecta las nuevas macros/traits/helpers.
   - Verifica compilación básica.

2. **Subagente Reviewer / Auditor:**
   - Inspecciona el `git diff` completo.
   - Audita que **ninguna lógica de negocio**, comportamiento sutil, headers específicos, serializaciones o fallback se haya perdido o modificado.
   - Corrige discrepancias o añade pruebas de regresión.
   - Ejecuta la suite de pruebas del proyecto (`cargo test --workspace`, `npm test`, `pytest`, etc.).

3. **Commit y Sincronización Atómica:**
   - Realizar commit atómico con mensaje semántico (`refactor(...)`, `perf(...)`, `fix(...)`).
   - Sincronizar con el repositorio remoto (`git push`) antes de pasar al siguiente punto.
