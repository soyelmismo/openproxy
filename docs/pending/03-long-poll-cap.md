# #6 — Long-poll cursor cap

**Severity**: LOW
**Reported by**: REVIEWER pass 2026-06-18
**File (suspected)**: `crates/openproxy-server/src/handlers/admin.rs`

## Claim del REVIEWER

El `since_id` cursor del long-poll del dashboard no tiene cap. Un cliente
puede pasar `?since_id=<MAX>` y forzar al server a escanear toda la tabla
`usage` desde el principio en cada poll. O el cliente puede pasar
`?since_id=9223372036854775807` (i64::MAX) y triggerear un scan O(n) del
lado del server.

## Verification needed

1. Buscar el handler del long-poll (`/v1/admin/usage/recent` o similar).
2. Confirmar que el `since_id` es un `Option<i64>` o `Option<u64>`.
3. Buscar el loop que itera desde `since_id` hasta el presente. Si es un
   `WHERE id > ?` con índice, el index lo cubre. Si es un scan lineal,
   es el bug.
4. Verificar si el cap se aplica al `limit` de la query (e.g. `LIMIT 1000`).

## Fix probable (pendiente de verificación)

- Si el cursor no tiene cap, agregar `MAX(since_id) = 0` clamp
  (rechazar `since_id > MAX_REASONABLE_CURSOR` con 400).
- Si el loop es lineal, mover a `WHERE id > ?` con índice
  (probablemente ya lo es, pero verificar).
- Confirmar que el `LIMIT` del SELECT es razonable (e.g. 1000 max).

## Tests (probable)

- `since_id = i64::MAX` → 400, no scan O(n).
- `since_id` negativo → 400.
- `since_id` ausente → OK, devuelve las últimas N rows.
