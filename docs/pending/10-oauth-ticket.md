# #12 — OAuth ticket persistence

**Severity**: LOW
**Reported by**: REVIEWER pass 2026-06-18
**File (suspected)**: `crates/openproxy-core/src/oauth.rs`

## Claim del REVIEWER

El "ticket" (state + code_verifier + redirect_uri) del flow OAuth
se guarda en memoria (e.g. `Arc<Mutex<HashMap<String, OAuthTicket>>>`)
y se pierde en el restart. Si el server se reinicia entre el
`/oauth/authorize` y el callback del provider, el callback falla
con un error opaco tipo "unknown ticket".

Adicionalmente, los tickets en memoria nunca expiran — un atacante
que consigue un ticket por algún side-channel puede usarlo
indefinidamente.

## Verification needed

1. Leer `oauth.rs` y encontrar la storage del ticket.
2. Confirmar si es in-memory (`HashMap`, `Vec`, `DashMap`) o
   persistente (DB).
3. Buscar el TTL — ¿hay un timestamp? ¿se purgan los viejos?
4. Confirmar el formato del `state` (debe ser `base64url(random 32)`).

## Fix probable (pendiente de verificación)

- Persistir tickets en la DB (`oauth_tickets` table: `state`,
  `code_verifier`, `redirect_uri`, `provider`, `created_at`,
  `expires_at`).
- TTL de 10 minutos (suficiente para el round-trip, no tanto como
  para reuso).
- Cleanup job: DELETE WHERE expires_at < now cada 5 minutos
  (o via `cleanup_old_tickets` migration).
- El `state` debe ser un random de 32 bytes mínimo, no derivable
  del timestamp o del usuario.

## Tests (probable)

- Server restart entre authorize y callback → callback sigue
  funcionando (el ticket se recupera de la DB).
- Ticket de 11 minutos de antigüedad → 401/400 `ticket expired`.
- `state` de 16 bytes → rechazado (no cumple entropy).
- Ticket usado dos veces → la segunda es 401/400.
