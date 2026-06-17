# Gate F2 — Auto-activation en el discovery scheduler

## Goal

El comportamiento de "auto-activación por keyword" (configurable por
proveedor, e.g. "auto-activar solo modelos que contengan 'gpt'") debe
aplicarse tanto cuando el operador pulsa "Refresh" en el dashboard como
cuando el background scheduler hace su tick.

## Why

El REVIEWER del audit end-to-end (P1) levantó una divergencia semántica:
`apply_auto_activation` en `models.rs:1008-1043` solo se llama desde el
handler admin `POST /v1/admin/models/refresh`. Si el operador define una
keyword de auto-activación para nvidia-nim (e.g. "solo activar modelos
con 'nemotron' en el nombre"), esta regla NO se aplica durante el ciclo
background. Consecuencia: modelos nuevos que matcheen la keyword se
insertan con `active=1` por default de schema (que es lo correcto
operacionalmente en muchos casos), pero modelos que NO matcheen la
keyword también quedan `active=1`. La regla se vuelve inútil para el
background refresh.

## Module boundaries

**Toca:**
- `crates/openproxy-core/src/admin.rs` — extraer `apply_auto_activation`
  para que sea accesible desde `discovery_scheduler.rs` (o moverlo a
  `models.rs` si hace más sentido por el data locality).
- `crates/openproxy-core/src/discovery_scheduler.rs` — invocar la
  función después de `refresh_models` exitoso en cada tick.

**NO toca:**
- Schema.
- Routing / pipeline.
- Ningún gate anterior.

## Diseño

Extraer `apply_auto_activation(conn, &provider_id, config)` de `admin.rs`
a `models.rs` (o dejarlo en `admin.rs` como `pub fn`). Que el
`DiscoveryScheduler` lo invoque al final de cada tick exitoso.

**Detalles del comportamiento:**

1. Por cada `provider_id` en `provider_auto_activation` (tabla de config,
   ya existe), después de un refresh exitoso, ejecutar:
   ```sql
   UPDATE models
   SET active = 0
   WHERE provider_id = ? AND custom = 0 AND <keyword_match>
   ```
   (el predicado varía si la config es "include" o "exclude" keyword).

2. Si la config del provider es **vacía** (sin keyword), `active` queda
   como lo dejó el `upsert_many` (default `1`). El comportamiento es
   idéntico al actual para providers sin config.

3. Si la config es **"include X"**: los modelos que matchean `X` quedan
   `active=1`, los que no quedan `active=0`.

4. Si la config es **"exclude X"**: los modelos que NO matchean `X`
   quedan `active=1`, los que matchean quedan `active=0`.

## Acceptance criteria

1. **AC1**: Test unitario `apply_auto_activation_include_keyword`:
   - Setup: provider P con config `auto_activation_include = "gpt"`.
   - Llamar la función con discovered = [gpt-4, claude-3, llama-3].
   - Verificar que gpt-4 queda `active=1`, claude-3 y llama-3 quedan
     `active=0`.

2. **AC2**: Test unitario `apply_auto_activation_exclude_keyword`:
   - Setup: provider P con config `auto_activation_exclude = "legacy"`.
   - Llamar la función con discovered = [gpt-4, gpt-legacy, claude-3].
   - Verificar que gpt-legacy queda `active=0`, los otros `active=1`.

3. **AC3**: Test unitario `apply_auto_activation_no_config_is_passthrough`:
   - Setup: provider P sin config de auto-activation.
   - Llamar la función con cualquier discovered.
   - Verificar que ningún modelo cambia su `active`.

4. **AC4**: Test unitario `discovery_scheduler_invokes_auto_activation`:
   - Verificar que `run_one_tick` llama a `apply_auto_activation` cuando
     el refresh es exitoso.
   - Manera: mockear el adapter para devolver modelos, espiar la
     conexión después.

5. **AC5**: Test unitario `discovery_scheduler_skips_auto_activation_on_failure`:
   - Si `refresh_models` falla (e.g. upstream 500), el scheduler NO debe
     llamar a `apply_auto_activation` (el catálogo no se tocó, no hay
     que reaplicar reglas).

## Test requirements

- 5 tests unitarios (AC1-AC5).
- Tests existentes siguen pasando.

## Out of scope

- Cambiar la UI del dashboard para reflejar la config (la keyword ya
  existe en algún lugar, verificar).
- Auto-populate de combos basado en auto-activation (eso sería F4 si el
  usuario lo pide).
- Cambiar el TTL o la semántica de expiración.

## Riesgos

- **Riesgo**: si `apply_auto_activation` corre en cada tick (cada 1h)
  y los modelos no cambiaron, se hace un UPDATE de 0 filas. No es
  problema operacional, pero es trabajo en vano. Mitigación: hacer
  el UPDATE sólo si la última invocación fue hace >1h (timestamp de
  última auto-activación por provider). Si esto agrega complejidad,
  aceptar el UPDATE de 0 filas como aceptable — SQLite lo maneja en
  microsegundos.
- **Riesgo**: si el operador cambia la config de keyword entre ticks,
  el próximo tick la aplica. Esperado.
