# #7 — `errors.rs` cap

**Severity**: LOW
**Reported by**: REVIEWER pass 2026-06-18
**File (suspected)**: `crates/openproxy-core/src/errors.rs` (o donde
viva el `ApiError` que se serializa a JSON)

## Claim del REVIEWER

El body de error que se devuelve al cliente puede incluir el `error_msg`
del upstream completo. Si el upstream devuelve un error body de 1 MiB
(stack trace de Python, HTML error page, etc.), el proxy lo reenvía
verbatim y el cliente lo recibe. DoS amplification.

## Verification needed

1. Leer `errors.rs` (o el equivalente) y encontrar el `Display`/`Serialize`
   impl de `ApiError`.
2. Confirmar si el `error_msg` (campo del upstream) pasa por algún cap.
3. Si no, confirmar si el upstream está rate-limited upstream del proxy
   (e.g. 4xx no DoS-amplifica). Si no, es el bug.

## Fix probable (pendiente de verificación)

- Cap `error_msg` a 4 KiB en el boundary (`cost.rs` ya hace esto para
  el `error_msg` que se persiste, ver `sanitized.truncate(2048)` en
  el commit `2f03e6d`).
- El cap debe aplicarse **antes** de serializar al response, no solo
  en la persistencia.

## Tests (probable)

- Upstream devuelve `error_msg` de 1 MiB → cliente recibe max 4 KiB
  + truncation marker.
- El `error_msg` persistido también está capeado (verificar
  `usage.error_msg` row size).
