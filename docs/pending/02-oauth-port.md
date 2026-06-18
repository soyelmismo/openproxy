# #3 — OAuth callback port

**Severity**: LOW
**Reported by**: REVIEWER pass 2026-06-18
**File (suspected)**: `crates/openproxy-core/src/oauth.rs`

## Claim del REVIEWER

El callback server de OAuth bindea en un puerto fijo (e.g. `8080` o
`127.0.0.1:PORT`) y choca con otros servicios del dev. O bindea en `0.0.0.0`
en vez de `127.0.0.1`.

## Verification needed

Antes de tocar:
1. Leer `oauth.rs` y encontrar el `bind()` o equivalente.
2. Confirmar si el puerto es hardcodeado o configurable.
3. Si es hardcodeado, verificar que `127.0.0.1:<puerto>` (no `0.0.0.0`).
4. Si es configurable, verificar el default.

**No confiar en el REVIEWER** — ya falló en 1 de 15 findings (commit
`5a98889` doc). Verificar con `grep -n` y lectura directa.

## Fix probable (pendiente de verificación)

- Bind a `127.0.0.1:<puerto>` (loopback only — el callback no debe
  escuchar en la red).
- Si el puerto es hardcodeado, hacerlo configurable via `OAuthConfig`
  con un default sensato (e.g. random free port + reportar al log).
- Si choca con otro puerto, el dev puede override via env var.

## Tests (probable)

- Bind falla si el puerto ya está ocupado → error claro.
- Bind en `127.0.0.1` no acepta conexiones desde `0.0.0.0` (regression).
- Si configurable, override vía env var funciona.
