# Pending fixes

Lista de findings de auditoría todavía abiertos, organizados por origen.

## Estado de los rounds

| Round | Status | Encontrados | Cerrados | Pendientes |
|-------|--------|-------------|----------|------------|
| **Original audit (commit `2f03e6d`)** | Cerrado CRITICAL+HIGH | 4 critical + 7 high | 11 | medium + low (sin desglose público) |
| **9c5c6d1 follow-up** | Cerrado | 3 high adicionales | 3 | — |
| **Round 1 (`72a5d8c` → `5a98889`)** | Cerrado | 5 (REVIEWER pass 2026-06-18) | 5 | 0 |
| **Round 2** | Pendiente | 6 del REVIEWER | 0 | 6 |
| **Round 3** | Pendiente | 3 del REVIEWER | 0 | 3 |

## Estructura

- `01-original-audit-medium-low.md` — los medium + low que el commit `2f03e6d` dejó filed-for-follow-up. No tenemos desglose público; hay que recuperar el reporte original o re-auditar.
- `02-oauth-port.md` → `07-expires-at-compare.md` — los 6 fixes del Round 2 (REVIEWER pass 2026-06-18).
- `08-sse-chunk-allocation.md` → `10-oauth-ticket.md` — los 3 fixes del Round 3.

## Reglas para tocar un pendiente

1. **Verificar** que el finding es real antes de tocar nada (REVIEWER ya falló en 1 de 15, "translator stateless regression" — ver commit `5a98889` doc).
2. **Tests reales** — no vale `assert!(true)` o "verify the build is clean". El user tiene la regla "no-approximations".
3. **0 nuevos `#[allow(...)]`** — el user rechaza silenciar warnings. Se eliminan refactorizando.
4. **1 fix por commit** — multi-fix BUILDER timeouts son predecibles (mi memoria: 11-fix → 0% success, 1-fix → 95%).
5. **Build + tests verdes antes de commitear** — el linter del `patch` tool miente, `cargo build`/`cargo test` es la verdad.

## Índice rápido de fixes pendientes

### Round 2 — LOW / MEDIUM

| ID | Severity | Title | File |
|----|----------|-------|------|
| #3 | LOW | OAuth callback port | `crates/openproxy-core/src/oauth.rs` |
| #6 | LOW | Long-poll cursor cap | `crates/openproxy-server/src/handlers/admin.rs` |
| #7 | LOW | `errors.rs` cap | `crates/openproxy-core/src/errors.rs` |
| #13 | LOW | `error_message` redact consistency | `crates/openproxy-core/src/cost.rs` |
| #14 | LOW | `db_pool` write-lock starvation | `crates/openproxy-core/src/db_pool.rs` |
| #15 | LOW | `expires_at` compare semantics | `crates/openproxy-core/src/api_keys.rs` |

### Round 3 — design / cancellation

| ID | Severity | Title | File |
|----|----------|-------|------|
| #9 | MEDIUM | SSE chunk allocation reuses buffer across providers | `crates/openproxy-core/src/pipeline.rs` |
| #10 | MEDIUM | `test_combo` cancellation | `crates/openproxy-server/src/handlers/admin.rs` |
| #12 | LOW | OAuth ticket persistence | `crates/openproxy-core/src/oauth.rs` |
