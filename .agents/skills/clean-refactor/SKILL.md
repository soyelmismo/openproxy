---
name: clean-refactor
description: Skill repo-agnóstica para detectar y corregir rápidamente malas prácticas, optimizar bucles y límites, deduplicar lógica mediante macros/traits plug & play, y ejecutar refactors secuenciales con auditoría dual y commit automático.
---

# Clean Refactor & Bad Practices Remediation

Procedimiento repo-agnóstico de alta eficiencia para auditar código, erradicar malas prácticas de rendimiento, deduplicar lógica mediante abstracciones plug & play y ejecutar refactors secuenciales con doble verificación.

---

## 1. Detección y Auditoría de Malas Prácticas

### 1.1 Anti-patrones de Iteración e Indexación
- **Problema:** Bucles con rangos e indexación manual (`for i in 0..len`, `for i in 0..n`, `while i < len { arr[i] }`) exigen chequeo de límites (*bounds checking*) en cada iteración, gastan CPU innecesariamente y reducen la legibilidad.
- **Detección:**
  - Buscar rangos indexados en el código: `for\s+\w+\s+in\s+0\.\.`, `while\s+\w+\s*<\s*.*\.len\(\)`, accesos `[i]`, `[idx]`.
  - Buscar lints suprimidos (ej. `#[allow(clippy::needless_range_loop)]`).
- **Remediación:**
  - Iteración directa: `.iter()`, `.iter_mut()`, `.into_iter()`.
  - Iteración con índice: `.iter().enumerate()`.
  - Pares o ventanas: `.windows(2)`, `.iter().zip(...)`.
  - Búsqueda / corte: `.position(|item| ...)` en vez de contadores manuales.
  - Inserción masiva: `.extend(...)` en vez de bucles manuales de inserción.

### 1.2 Código Clonado y Módulos Duplicados
- **Problema:** Copias de utilidades, parsers, normalizadores o tablas de configuración entre crates o módulos generan desincronización y deuda técnica.
- **Remediación:** Centralizar en el crate/módulo de nivel más bajo (e.g. `types` o `common`) y reexportar con `pub use`.

### 1.3 Aritmética Manual de Placeholders y Base de Datos
- **Problema:** Concatenación manual de `(?1, ?2...)` o `?{base + i}` propensa a sobrepasar límites del motor (ej. `SQLITE_MAX_VARIABLE_NUMBER = 999`).
- **Remediación:** Helpers genéricos de fragmentación (`query_in_chunks`) y macros/constructores de batch insert tipados.

---

## 2. Ciclo "Encuentra > Arregla" (Find -> Fix Loop)

Para cada categoría de error identificada:
1. **Localizar:** Listar ubicaciones exactas con enlaces de archivo y número de líneas (`file:///path/to/file#L...`).
2. **Proponer Reemplazo Idiomático:** Formular la solución más simple con stdlib / iteradores nativos.
3. **Parchear Directamente:** Aplicar cambios mínimos y precisos con anclas exactas en el codebase.
4. **Verificar:** Ejecutar linter con advertencias forzadas (ej. `cargo clippy -- -D <lint>`) y suite de tests unitarios.

---

## 3. Análisis de Deduplicación y Arquitectura Plug & Play

Cuando se audita un codebase para modularización:
1. **Identificar Boilerplate Repetitivo:**
   - Adaptadores de proveedores / clientes de API con 80% de métodos idénticos.
   - Handlers de endpoints con orquestación duplicada (timeouts, watchdogs, streaming channels, redacción de secretos).
   - Visitors o middlewares imperativos sin interfaz común.
2. **Diseñar Abstracciones Reutilizables:**
   - **Traits con Defaults:** Proporcionar implementaciones estándar para métodos comunes en el trait base.
   - **Macros Declarativas:** Generar boilerplate de configuración mediante macros de pocas líneas (ej. `declare_adapter!`).
   - **Traits de Pipeline:** Encapsular middleware como etapas de streaming (`StreamingStage: process_chunk -> StreamAction`).
   - **Visitors de Contenido:** Manejar estructuras complejas (ej. strings vs arrays multipart) en un único punto (`mutate_content`).
3. **Documentar Plan de Acción:** Generar informe estructurado en `.md` con roadmap, impacto en LOC y beneficios arquitectónicos.

---

## 4. Flujo de Ejecución Secuencial con Subagentes (Dual-Agent Pipeline)

Para ejecutar refactorizaciones de gran escala de forma segura:

```
[Punto N] -> [Subagente Refactor] -> [Subagente Reviewer] -> [Tests & Lints] -> [Commit & Push] -> [Punto N+1]
```

### 4.1 Subagente Refactor (Punto N)
- Aplica el diseño técnico acordado para el módulo específico.
- Elimina código redundante y conecta las nuevas macros/traits/helpers.
- Verifica compilación básica del paquete.

### 4.2 Subagente Reviewer / Auditor (Punto N)
- Inspecciona el `git diff` completo.
- Audita exhaustivamente que **ninguna lógica de negocio**, comportamiento sutil, headers específicos, serializaciones o fallback se haya perdido o modificado inadvertidamente.
- Corrige discrepancias o añade pruebas de regresión.
- Ejecuta la suite de pruebas del proyecto (`cargo test --workspace`, `npm test`, `pytest`, etc.).

### 4.3 Commit y Sincronización
- Realizar commit atómico con mensaje semántico (`refactor(...)`).
- Sincronizar con el repositorio remoto (`git push`) antes de avanzar al siguiente punto.
