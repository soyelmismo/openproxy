# #13 — `error_message` redact consistency

**Severity**: LOW
**Reported by**: REVIEWER pass 2026-06-18
**File (suspected)**: `crates/openproxy-core/src/cost.rs` y
`crates/openproxy-core/src/pipeline.rs`

## Claim del REVIEWER

Hay dos paths de redacción del `error_msg` que pueden divergir:
1. `cost.rs:75-89` redacta `x-api-key:` y `Authorization: Bearer` en
   el `error_msg` que se persiste a `usage.error_msg`.
2. `pipeline.rs` tiene otro redact para el `error_msg` que se publica
   en el WebSocket del dashboard.

Si los dos regex son distintos, un error puede pasar redactado en la
DB pero crudos en el WebSocket (o viceversa). El admin ve el secreto
live en el log feed aunque la DB diga `[REDACTED]`.

## Verification needed

1. Leer `cost.rs` y `pipeline.rs` lado a lado.
2. Comparar los regex/lógica de redact.
3. Si divergen, identificar quién tiene la versión "buena" (más keys
   redactadas) y propagar al otro.
4. Confirmar que hay un único `redact_sensitive_in_string` (o
   equivalente) usado por ambos paths.

## Fix probable (pendiente de verificación)

- Si los dos paths divergen, unificarlos en una función helper
  en `openproxy_core::redact` (ya existe, ver `redact_sensitive_headers`).
- El helper toma un `&str` y devuelve un `String` con todas las
  keys sensibles redactadas.
- Ambos call sites lo usan.

## Tests (probable)

- Un `error_msg` que contiene `x-api-key: sk-secret` se redacta igual
  en la DB row Y en el WebSocket payload.
- La lista de keys redactadas es exhaustiva (no se escapa ninguna).
