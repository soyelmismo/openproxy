# Especificación de Arquitectura: Enrutamiento Jerárquico por Decisión (Hierarchical Decision Routing)

**Documento:** `docs/specs/hierarchical-decision-routing.md`  
**Estado:** Propuesta de Diseño / Especificación Técnica  
**Fecha:** 21 de Septiembre de 2026  
**Autores:** OpenProxy Core Team  

---

## 1. Resumen Ejecutivo y Motivación

OpenProxy actualmente soporta enrutamiento semántico en tiempo real mediante **System One** (modelos de decisión rápida como `jev-1.13` o `laya`) a través del modo `priority_mode = "decision"`. Sin embargo, la resolución actual de sub-combos (`sub_combo_id`) se ejecuta de manera **estática y aplanada** (`flatten_sub_combos` en `execution.rs`), expandiendo todos los sub-combos en modelos hoja antes de que el motor de enrutamiento evalúe cualquier prioridad.

Esta limitación impide arquitecturas de enrutamiento en cascada ("Árbol de Decisión Semántico"), como:
```text
Cliente → Pregunta: "¿Cómo calculo el VAN de una inversión?"
           ↓
    Combo "Topics" (Jev Router Nivel 1)
           ├─ combo:finance   ("Preguntas financieras, balances, inversiones, VAN/TIR")  <-- [GANADOR]
           ├─ combo:health    ("Consultas médicas y diagnósticos")
           ├─ combo:chat      ("Conversación casual, saludos")
           └─ combo:politics  ("Noticias de actualidad y política")
           ↓
    Combo "Finance" (Jev Router Nivel 2)
           ├─ claude-3-5-sonnet ("Cálculos financieros complejos y formulación matemática") <-- [GANADOR]
           └─ gpt-4o-mini       ("Conceptos básicos y definiciones simples")
           ↓
    Ejecución del upstream con Claude 3.5 Sonnet
```

El objetivo de esta especificación es definir la arquitectura necesaria para soportar **enrutamiento jerárquico recursivo**, donde cada nivel del árbol de combos puede actuar como un clasificador semántico independiente antes de descender al siguiente nivel.

---

## 2. Diagnóstico del Estado Actual

### 2.1 Aplanamiento Estático Prematuro
En `crates/openproxy-pipeline/src/stages/router.rs`:
```rust
async fn resolve_initial_targets(
    ctx: &PipelineContext,
    combo: &openproxy_types::combos::Combo,
) -> Result<Vec<ComboTarget>, CoreError> {
    let targets = ctx
        .pipeline
        .resolve_targets(combo, ctx.req.targets_override.as_deref())
        .await?;
    ctx.pipeline.flatten_targets(&combo.id, targets).await
}
```
* `flatten_targets` llama a `repo.resolve_combo_to_targets(sub_id, ...)`.
* Esto sustituye inmediatamente el `ComboTarget` del sub-combo (que contenía `description = "Preguntas financieras..."`) por la lista plana de modelos dentro de ese sub-combo.
* **Consecuencia:** La descripción del sub-combo se pierde y el combo padre nunca clasifica entre sub-combos.

### 2.2 Desacoplamiento de Políticas del Sub-Combo
Al aplanar la lista a modelos individuales:
* El combo hijo nunca se evalúa como una entidad con su propia estrategia (`priority`, `round_robin`, `decision`, etc.).
* La propiedad `priority_mode: Decision` del combo hijo nunca se ejecuta.

---

## 3. Arquitectura del Enrutamiento Jerárquico

### 3.1 Flujo de Ejecución por Etapas Recursivas

```mermaid
flowchart TD
    Req[Petición entrante: prompt] --> RStage[RouterStage: Cargar Combo Raíz]
    RStage --> CheckMode{Combo tiene priority_mode == Decision?}
    
    CheckMode -- Sí --> EvalDecision[Evaluar Jev entre Targets del Nivel Actual\n(Modelos directos o Sub-combos con description)]
    CheckMode -- No --> EvalStandard[Ordenar por prioridad / estrategia estándar]
    
    EvalDecision --> Winner[Identificar Target Ganador]
    EvalStandard --> Winner
    
    Winner --> IsSubCombo{¿El Target ganador es un Sub-Combo?}
    
    IsSubCombo -- Sí --> CheckDepth{depth < MAX_SUB_COMBO_DEPTH?}
    CheckDepth -- Sí --> Descend[Descender a Sub-Combo:\nCargar definición y targets del hijo]
    CheckDepth -- No --> ErrDepth[Error: Recursión excedida]
    
    Descend --> CheckMode
    
    IsSubCombo -- No --> Leaf[Target Hoja Resuelto: Modelo Upstream + Credenciales]
    Leaf --> UpstreamExec[UpstreamExecutorStage]
```

### 3.2 Reglas de Parada y Resiliencia
1. **Límite de Profundidad:** Máximo 5 niveles (`MAX_SUB_COMBO_DEPTH = 5`). La detección de ciclos previene bucles infinitos en tiempo de ejecución.
2. **Fallback por Timeout o Error de Jev:** Si la llamada a Jev en cualquier nivel supera `decision_timeout_ms` (por defecto 150ms) o falla, el enrutador de ese nivel utiliza el orden de prioridad estático predeterminado y continúa el flujo sin interrumpir la petición del usuario.
3. **Poda de Sub-Combos No Saludables:** Un sub-combo solo es elegible para la clasificación de Jev si tiene al menos un modelo saludable en el `CircuitBreaker`. Si un sub-combo está completamente degradado, se omite de las opciones pasadas a Jev.

### 3.3 Traza de Auditoría y Telemetría (`combo_walk_log`)
El contexto de la petición (`PipelineContext`) acumula el camino de decisión para observabilidad completa en los logs del dashboard:
```json
[
  "decision_router:combo=topics:winner=sub_combo:finance (elapsed=42ms)",
  "decision_router:combo=finance:winner=model:claude-3-5-sonnet (elapsed=38ms)"
]
```

---

## 4. Plan de Implementación por Fases

| Fase | Ámbito | Descripción |
| :--- | :--- | :--- |
| **Fase 1** | **Frontend SPA** | Refactorizar la visualización de combos en el Dashboard: agrupar sub-combos en bloques desplegables (accordion) con color distintivo, inspección de modelos internos y edición en el lugar. |
| **Fase 2** | **Core & Pipeline** | Modificar `RouterStage` para reemplazar el aplanamiento ciego inicial por resolución jerárquica con evaluación de `PriorityMode::Decision` a nivel de sub-combo. |
| **Fase 3** | **Observabilidad** | Exponer en el log en tiempo real y en la pestaña de Analytics el desglose de los saltos de decisión jerárquicos realizados por Jev. |

---

## 5. Invariantes de Calidad
* Cero regresiones en el modo plano actual (`flatten_targets`).
* Cumplimiento del límite de 800 LOC por archivo modular.
* Latencia añadida de Jev en sub-combos contenida en $\le 50\text{--}100\text{ ms}$ por salto mediante llamadas paralelas/optimizadas en formato binario System One.
