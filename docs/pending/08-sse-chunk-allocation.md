# #9 — SSE chunk allocation reuses buffer across providers

**Severity**: MEDIUM
**Reported by**: REVIEWER pass 2026-06-18
**File (suspected)**: `crates/openproxy-core/src/pipeline.rs`
(cerca de la línea 1980-1990, el path de `dispatch_upstream`)

## Claim del REVIEWER

El `Body` o `Bytes` que se lee del upstream se reusa como buffer
para el siguiente request. Si dos requests a providers distintos
están corriendo en paralelo y comparten un buffer interno (e.g.
vía `BytesMut` o `Arc<[u8]>` con Cow), el provider A puede ver
bytes del provider B (data race lógica, no de memoria — los tipos
son `Send`).

Síntoma: el cliente recibe un evento SSE de un upstream que no
corresponde a su request. El cliente lo rendera como "completion"
de su propio prompt.

## Verification needed

1. Leer `pipeline.rs` cerca del path de streaming.
2. Buscar allocations de `Vec<u8>`, `BytesMut`, `String` reutilizadas
   en hot loops.
3. Confirmar que cada request tiene su propio buffer (no global).
4. Buscar `Cow<[u8]>` o `Arc<[u8]>` compartidos.

## Fix probable (pendiente de verificación)

- Cada request debe tener su propio buffer local (no global, no
  thread-local, no compartido via `Arc` mutable).
- Si hay un pool de buffers, los buffers deben volver al pool SOLO
  cuando la response del upstream está completamente drenada
  (después del `[DONE]` SSE marker).
- Un test con dos providers en paralelo: el response de A no debe
  contener bytes de B.

## Tests (probable)

- 2 race_targets en paralelo a providers distintos con bodies
  distinguibles. Verificar que cada response tiene solo el body
  de su provider.
- Stress test: 1000 requests en paralelo, 0 cross-contamination
  en las 1000 responses.
