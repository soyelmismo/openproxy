# #14 — `db_pool` write-lock starvation

**Severity**: LOW
**Reported by**: REVIEWER pass 2026-06-18
**File (suspected)**: `crates/openproxy-core/src/db_pool.rs`

## Claim del REVIEWER

El `DbPool::writer()` toma un lock global (probablemente `Mutex<Connection>`
o similar). Si un query pesado (e.g. `usage_summary` con `from`/`to`
de 30 días) está corriendo, todos los demás writers se bloquean
incluyendo el path crítico de `record_attempt` que persiste cada request.

En producción, un dashboard abierto en `usage_summary` mientras hay
tráfico real causa latencia en todos los inserts de `usage`.

## Verification needed

1. Leer `db_pool.rs` y encontrar el tipo del writer (probablemente
   `Mutex<Connection>` o `RwLock<Connection>`).
2. Confirmar que `writer()` bloquea hasta que el lock se libere.
3. Buscar queries que toman el writer y ver cuánto pueden correr.
4. Confirmar si `record_attempt` (hot path) está protegido.

## Fix probable (pendiente de verificación)

- `writer()` debe ser **no-bloqueante** o tener un timeout corto
  (e.g. 100 ms). Si el lock no se adquiere, retornar error y el
  caller decide (drop el insert? queue para retry?).
- Alternativa: usar `rusqlite::Connection` directamente (SQLite tiene
  su propio locking interno via `SQLITE_OPEN_FULLMUTEX`).
- Mejor: separar el pool en read-replicas y write-master, o usar
  `tokio::sync::Semaphore` para cap el número de writers concurrentes.

## Tests (probable)

- 2 queries pesados en paralelo: el segundo no se cuelga indefinidamente.
- `record_attempt` retorna error (no panic) si el writer está saturado.
- Lock se libera aunque el query panicee (RAII / Drop guard).
