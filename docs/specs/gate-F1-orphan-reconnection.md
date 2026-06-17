# Gate F1 — Reconexión automática de combo_targets huérfanos

## Goal

Cuando un modelo es re-insertado por `upsert_many` (porque volvió al upstream
después de una ausencia transitoria), los `combo_targets` que quedaron con
`model_row_id = NULL` deben re-vincularse al nuevo `row_id` automáticamente.

## Why

El REVIEWER del audit end-to-end (P4) levantó esto como el único WARNING
operacionalmente importante. Escenario real: nvidia-nim tiene un blip de 5 min,
un modelo en el combo `nerd` se borra por Gate B, FK pasa a NULL por Gate D,
5 min después el modelo vuelve, se inserta con un `row_id` autoincrement
distinto (SQLite nunca reusa ids) y la FK target del combo queda huérfana para
siempre. Sin reconexión, el operador tiene que editar el combo a mano y no
tiene idea de por qué el combo perdió el target.

## Module boundaries

**Toca:**
- `crates/openproxy-core/src/models.rs` — `upsert_many` (transactions y
  orden de operaciones)
- `crates/openproxy-core/src/combos.rs` — nueva función helper
  `reconnect_orphan_targets(conn, tx, model_id, new_id, provider_id)` (NO
  modifica ninguna API existente)

**NO toca:**
- Schema (migración nueva NO es necesaria, los datos están)
- Routing / pipeline / handler de admin
- Cualquier gate anterior

## Diseño

La reconexión es posible si y solo si el `combo_targets` huérfano todavía
recuerda qué modelo era. Hoy el schema de `combo_targets` tiene:
- `id` (PK)
- `combo_id` (FK al combo)
- `model_row_id` (FK a models, NULL tras la ausencia)
- `sub_combo_id` (FK a otro combo, NULL si era target directo)
- `target_format`, `weight`, `priority`, ...

**NO tiene `upstream_model_id` en el target.** Para re-vincular necesitamos
buscar el `model_id` original. Dos opciones:

### Opción A: join por (provider_id, model_id) vía el modelo viejo

Imposible: el modelo viejo fue borrado por Gate B.

### Opción B: agregar `upstream_model_id` a `combo_targets` (migración)

Es la única forma. Necesitamos:
1. Nueva columna `upstream_model_id TEXT` en `combo_targets` (nullable,
   backwards-compatible).
2. Llenar la columna cuando se crea un target con `model_row_id IS NOT NULL`
   (helper nuevo o backfill desde `models.model_id`).
3. En `upsert_many`, ANTES del bloque DELETE, capturar los
   `(id, combo_id, upstream_model_id)` que están a punto de quedar huérfanos.
4. En el bloque INSERT, hacer match `WHERE upstream_model_id = <new_model_id>
   AND provider_id = <provider_id>` para los targets huérfanos y
   `UPDATE combo_targets SET model_row_id = <new_row_id>`.

Esto preserva la atomicidad: todo dentro del mismo `tx`.

## Acceptance criteria

1. **AC1**: Test unitario `upsert_many_reconnects_orphan_combo_targets`:
   - Setup: crear modelo M en provider P, crear combo C con target a M.
   - Borrar M (refleja ausencia).
   - Llamar `upsert_many(P, [M])` (refleja reaparición).
   - Verificar que el target del combo sigue existiendo, con
     `model_row_id = <new_id>` y `combo_id = C`.

2. **AC2**: Test unitario `upsert_many_does_not_reconnect_wrong_model`:
   - Setup: target huérfano para M_a.
   - Re-insert M_b (distinto model_id) en el mismo provider.
   - Verificar que el target sigue huérfano (no se reconecta a M_b).

3. **AC3**: Test unitario `upsert_many_atomic_orphan_reconnection`:
   - Verificar que el UPDATE de re-vinculación ocurre DENTRO del mismo `tx`
     que el DELETE y el INSERT (no en una transacción separada).
   - Manera: hacer fallar el INSERT del re-aparecido (e.g. constraint
     violation inyectada), verificar que el target huérfano SIGUE huérfano.

4. **AC4**: Test E2E (extensión del existente en
   `crates/openproxy-server/tests/e2e_models_discovery.rs`):
   - Scenario "aparece → desaparece → reaparece" en combo.
   - El combo rutéa al modelo tras la reaparición.

5. **AC5**: Migración 000026 agrega `upstream_model_id` a `combo_targets` con
   backfill de filas existentes (LEFT JOIN con `models`).

6. **AC6**: El handler de creación de target (`POST /v1/admin/combos/:id/targets`)
   ahora escribe `upstream_model_id` cuando se crea con un `model_row_id`.

## Test requirements

- 3 tests unitarios nuevos en `models::tests` (AC1, AC2, AC3).
- 1 test E2E nuevo (AC4) — extender el file existente.
- Tests existentes en `combos` y `models` siguen pasando (no regression).

## Out of scope

- Backfill de targets creados ANTES de este gate (la migración los llena
  automáticamente con el LEFT JOIN, pero no aplica lógica especial).
- Reconexión de targets que apuntan a `sub_combo_id` (eso es recursivo y
  requiere un gate aparte si el usuario lo pide).
- Log/observabilidad específica para "target reconectado" (puede entrar en
  gate F3).

## Riesgos

- **Riesgo**: si dos modelos del mismo provider tienen el mismo
  `model_id` upstream (improbable pero teóricamente posible), la
  reconexión sería ambigua. Mitigación: la constraint UNIQUE sobre
  `(provider_id, model_id)` ya garantiza que no existan dos filas en
  `models` con esa key, así que no hay ambigüedad.
- **Riesgo**: el backfill en la migración puede ser lento en DBs con
  muchos combos. Mitigación: la columna es NULL-able y se llena con un
  UPDATE batch, no requiere lock exclusivo. Si la DB tiene <100k filas,
  el backfill tarda <1s.
