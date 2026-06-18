# Original audit — medium & low findings (filed for follow-up)

## Origen

El commit [`2f03e6d`](../https://github.com/.../commit/2f03e6d) cerró los 4 CRITICAL
y los 7 HIGH. El mensaje del commit dice textualmente:

> "medium and low findings remain filed for follow-up."

**No tenemos el reporte completo** — el reporte solo vive en los mensajes de
commit y en las PRs originales (de la auditoría externa, no en este repo).

## Qué hacer antes de tocar nada

1. **Recuperar el reporte** — pedirlo al usuario o buscarlo en el historial
   del PR que originó el commit `2f03e6d`.
2. Si no se consigue, **re-auditar** las áreas no tocadas por el round 1
   (seguridad del OAuth, race conditions en SSE streaming, validación de
   inputs en el admin dashboard, persistencia de secretos).
3. Documentar cada finding nuevo aquí abajo con la misma estructura
   (severity, file:line, root cause, fix, tests).

## Áreas de código no tocadas por los rounds 1-3

Estas áreas son candidatas a contener los medium/low originales. Verificar:

- `crates/openproxy-core/src/oauth.rs` — flujo OAuth completo (ver #3, #12).
- `crates/openproxy-core/src/streaming.rs` — chunked streaming, backpressure.
- `crates/openproxy-core/src/db_pool.rs` — locking, timeouts, contention
  (ver #14).
- `crates/openproxy-core/src/billing/cost.rs` — cálculo de costos, agregación
  (ver #13).
- `crates/openproxy-server/src/handlers/admin.rs` — handlers admin
  no tocados por #2/#4 (e.g. `oauth_callback`, `test_combo`).
- `crates/openproxy-core/src/crypto.rs` — encripción de secretos en DB.
- `crates/openproxy-core/src/circuit_breaker.rs` — circuit breaker state
  machine.

## Estado

**Sin desglose público**. Marcar como prioridad media hasta que se recupere
el reporte original o se complete una re-auditoría.
