# #10 — `test_combo` cancellation

**Severity**: MEDIUM
**Reported by**: REVIEWER pass 2026-06-18
**File (suspected)**: `crates/openproxy-server/src/handlers/admin.rs`
(handler `test_combo` o `test_target`)

## Claim del REVIEWER

El endpoint `POST /v1/admin/combos/:id/test` (o equivalente) hace
fan-out a N targets, espera el `first` que responda, y devuelve
esa respuesta. Pero NO cancela los otros N-1 requests que ya
están en vuelo (o ya están esperando al upstream). El upstream
sigue facturando los tokens aunque el cliente ya recibió la respuesta
del winner.

Adicionalmente, si el cliente desconecta (cierra el HTTP request)
a mitad del fan-out, los targets en vuelo siguen hasta su timeout
(segundos o minutos). El server no propaga la cancellation.

## Verification needed

1. Leer el handler `test_combo` (o `test_target`).
2. Ver el `tokio::select!` o el `try_join`/`join_all` que hace el
   fan-out.
3. Confirmar que cuando `first` completa, los `rest` se cancelan
   (vía `abort_handle` o `drop` del future).
4. Ver si hay un `CancellationToken` propagado al client.

## Fix probable (pendiente de verificación)

- Wrap cada target future en un `tokio::spawn` y guardar el
  `JoinHandle` (o `AbortHandle`).
- Cuando `first` completa, llamar `.abort()` en los otros.
- Usar `tokio_util::sync::CancellationToken` para propagar la
  cancellation del cliente (request scope) a todos los targets.
- El upstream recibe un `Connection: close` o el TCP se cierra
  cuando el local se va.

## Tests (probable)

- Fan-out a 3 targets con latencias 100/500/1000 ms. El winner es
  el de 100 ms. Después de 200 ms, los otros 2 deben estar
  abortados (no esperaron sus 500/1000 ms completos).
- Cliente desconecta a los 50 ms. Los targets en vuelo se cancelan
  en < 100 ms.
- El log del upstream muestra que los 2 perdedores nunca escribieron
  el body completo (cancelados al connection-close).
