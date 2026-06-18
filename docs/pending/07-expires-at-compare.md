# #15 — `expires_at` compare semantics

**Severity**: LOW
**Reported by**: REVIEWER pass 2026-06-18
**File (suspected)**: `crates/openproxy-core/src/api_keys.rs`

## Claim del REVIEWER

El check de "key expired" compara `expires_at` (string SQLite con
timezone-naive) con `datetime('now')` (también naive, UTC implícito).
Si el server está configurado con un timezone distinto a UTC, o si
SQLite se compiló con otra tz, el compare puede fallar por un lado
o por el otro (key que debería estar expirada sigue activa, o
key válida es rechazada como expirada).

Adicionalmente, el formato del string puede no ser comparable
lexicográficamente (e.g. `2026-06-18 07:00:00` vs
`2026-06-18T07:00:00`).

## Verification needed

1. Leer `api_keys.rs` y encontrar el check de expiración
   (probablemente en `get_by_hash` o el `authenticate` del chat).
2. Confirmar qué formato usa `expires_at` (debe ser
   `YYYY-MM-DD HH:MM:SS` UTC consistente).
3. Confirmar cómo se compara (string compare? `julianday()`? epoch?).
4. Ver el commit de creación: ¿se guarda como UTC o local time?

## Fix probable (pendiente de verificación)

- Forzar UTC en el `expires_at` siempre. Usar `chrono::Utc::now()`
  y guardar como `YYYY-MM-DD HH:MM:SS` UTC.
- Comparar con `datetime('now')` solo si SQLite está en UTC. Si no,
  usar `strftime('%s', 'now')` (epoch) y comparar epochs.
- Mejor: comparar con `julianday()` (fraccional, UTC implícito) o
  convertir ambos lados a `i64` epoch en Rust antes del compare.

## Tests (probable)

- Key con `expires_at` en el pasado → 401 `api key expired`.
- Key con `expires_at` exacto al segundo actual → comportamiento
  determinístico (definir si es "expirada" o "válida" y assert).
- Timezone shift del server no afecta el resultado.
