# SSE: extracción conjunta de content y reasoning

## Alcance

Medición aislada del cambio en `sse_accumulator`: extracción de `content` y `reasoning_content` en una sola pasada por delta, con búsqueda de fin de string mediante `memchr2`

No cambia la garantía de persistencia antes del cierre. No se desplegó ni se hicieron commits

## Corrección y contrato

La suite release de `openproxy-pipeline` ejecutó 698 tests unitarios, siete regresiones Unicode/PII y cinco tests de telemetría concurrente, todos sin fallos. Dos tests quedaron ignored por su configuración existente

Las ocho regresiones nuevas cubren primer campo null/no-string, strings truncados, variaciones de orden, escapes y surrogates, primer delta de tools, anidamiento y prioridad content → tools → reasoning en el límite de bytes

El localizador de delta sigue siendo el existente `find_delta_object`, basado en búsqueda textual. Esta optimización no demuestra aislamiento semántico ante cualquier JSON arbitrario con un delta anterior fuera de `choices[0]`. La prueba de choices adicionales cubre el caso ordinario con delta válido en la primera choice

## Microbenchmark comparable

Fuente: `/root/.hermes/cache/scratch/openproxy-perf/r8-sse-micro/`

- Mismo archivo `benches/token_throughput.rs` en ambas variantes
- Misma baseline congelada en `wt-base`, cambiando exclusivamente los archivos de `sse_accumulator`
- Dos executables copiados antes de medir, hashes y fuentes conservados
- Tres rondas alternadas, cada proceso Criterion con 20 muestras, warmup de un segundo y medición de dos segundos por caso
- Affinity en CPU 2, sin compilaciones propias durante las mediciones
- Cada iteración procesa 1.000 eventos mediante `StreamingChunkStage::process_chunk`

Medias entre procesos independientes, desviación muestral y unidades de microsegundos por 1.000 eventos

| Caso | Base, media ± SD | Conjunto, media ± SD | Ratio de velocidad local |
|---|---:|---:|---:|
| Solo content | 136,389 ± 0,315 µs | 76,734 ± 0,103 µs | 1,78× |
| Solo reasoning | 256,464 ± 0,606 µs | 79,556 ± 0,142 µs | 3,22× |
| Ambos campos | 292,405 ± 0,435 µs | 115,493 ± 0,182 µs | 2,53× |

Estos ratios pertenecen al acumulador aislado. No son mejoras de throughput del proxy completo, del transporte ni de inferencia

La primera ejecución `r7-sse/bench-base.log` no contenía casos porque a la baseline le faltaba la función de benchmark. Se descartó ese contraste y se sustituyó por esta matriz con el bench idéntico. Un exit 0 de Criterion no prueba que haya medido casos

## HTTP end-to-end con recording

Fuente: `/root/.hermes/cache/scratch/openproxy-perf/r7-sse/`

- Tres pares alternados de procesos y DB nuevos
- 800 peticiones × 10.000 tokens sintéticos, cinco tokens por evento
- Feature `benchmark-sql`, mimalloc, SQLite FULL, background apagado, recording activado en ambos
- Proxy CPUs 2,3, mock CPU 0, cliente CPU 1
- 48 millones de tokens íntegros en el caso fire, cero fallos
- Verificación SQLite read-only en las primeras DB: ambas tienen 813 filas de usage, todas con request/response body; 32.762.274 bytes de response JSON acumulados en cada una. Incluye las 13 peticiones de warmup

Resultados fire, media ± desviación muestral entre tres procesos

- Base: 769.549 ± 24.918 tokens/s, CPU proxy 20,147 s
- Conjunto: 756.871 ± 2.484 tokens/s, CPU proxy 20,390 s
- No se observa mejora end-to-end separable de la dispersión

El segundo caso se guardó originalmente bajo el nombre `reasoning`, pero el mock emitía exclusivamente `content`. Es un caso de contenido de 200 peticiones × 2.000 tokens, no una medición HTTP de reasoning. Sus seis corridas validaron otros 2,4 millones de tokens, sin fallos. No se usa como evidencia end-to-end de reasoning

Los árboles HTTP también difieren en código de shutdown/crawling de proxies, desactivado por la configuración aislada. El microbenchmark anterior conserva un contraste de source estricto para atribuir el coste del acumulador

## Interpretación

El cambio reduce el coste local medido y elimina lecturas duplicadas sin quitar funcionalidades. El benchmark completo está dominado por otros trabajos y no muestra ganancia de capacidad atribuible a esta optimización

La affinity no elimina ruido del host. Tokens sintéticos no son unidades de tokenizer. Los límites de JSON y de propiedad de buffers conservan el contrato previo, no un parser general nuevo
