# Comparación del framing SSE con el arnés v2

## Alcance

Comparación medida del pipeline de `06e95be31` frente al framing con buffers compartidos y bypass del decoder identity

El binario candidato de esta matriz es anterior al arreglo posterior de normalización de tools y reasoning. Ese arreglo cuenta con tests de regresión separados. No atribuirle las cifras de esta matriz

El resultado **no demuestra una mejora general de throughput, RSS o I/O**. No se alcanzaron los objetivos de −80% en tiempos ni 100× en tokens/s

## Condiciones

- Mismo código de arnés multiproceso y contador SQL en ambos árboles
- Feature `benchmark-sql`, mimalloc y SQLite `synchronous=FULL`
- Proxy en CPUs `2,3` con dos workers, mock en `0` y cliente en `1`
- 10.000 tokens sintéticos por petición, cinco tokens por evento
- Cinco repeticiones por variante y concurrencia, ejecutadas en serie y alternando el orden
- Procesos y base de datos nuevos por repetición
- 30 corridas y 25.100.000 tokens recibidos y validados, sin peticiones fallidas
- Afinidad de CPU, no aislamiento del ruido del host

La desviación indicada es poblacional entre las cinco corridas. Para TTFT y latencia se promedian los percentiles de cada corrida, no se calcula un percentil global

## Resultados

### Una petición

- Throughput: **128.080 ± 15.843 → 115.245 ± 867 tokens/s**, cambio −10,0%
- Duración media del lote: 79,19 → 86,78 ms
- TTFT p95 medio: 5,90 → 6,86 ms
- RSS pico muestreado medio: 39.386 → 38.374 KiB
- CPU del proxy, incluido drain: 0,036 → 0,032 s
- SQL: 40 → 40 statements

### 50 peticiones simultáneas

- Throughput: **243.229 ± 6.216 → 242.557 ± 6.331 tokens/s**, cambio −0,3%
- Duración media del lote: 2.057,03 → 2.062,78 ms
- TTFT p95 medio: 83,63 → 71,24 ms
- Latencia p95 media: 2.049,49 → 2.054,81 ms
- RSS pico muestreado medio: 119.718 → 109.363 KiB
- CPU del proxy, incluido drain: 2,578 → 2,564 s
- Storage write bytes medios: 17.878.221 → 17.878.221
- SQL: 1.859,2 → 1.859,6 statements medios

### 200 peticiones simultáneas

- Throughput: **355.271 ± 28.251 → 361.708 ± 24.875 tokens/s**, cambio +1,8%
- Duración media del lote: 5.663,53 → 5.558,06 ms
- TTFT p95 medio: 264,30 → 280,40 ms
- Latencia p95 media: 5.628,24 → 5.518,48 ms
- RSS pico muestreado medio: 245.158 → 249.856 KiB
- CPU del proxy, incluido drain: 9,110 → 8,668 s
- Storage write bytes medios: 111.607.808 → 121.307.136
- SQL: 7.426,2 → 7.425,0 statements medios

## Interpretación

En 50 y 200 streams la variación de throughput es menor que la dispersión observada. Con un stream la media fue peor

TTFT y RSS mejoraron en 50 streams y no en 200. Las diferencias de I/O tampoco justifican una reducción general de escrituras

Esta matriz no prueba el techo del servicio desplegado. El arnés usa router real, `AppState::for_test` y `axum::serve`, no el startup ni el accept loop endurecido de producción. Los recursos del cliente y del mock se reportan por separado

Los tokens son unidades sintéticas ` tok`, no inferencia ni throughput de un tokenizer. Las cifras antiguas del arnés in-process no son comparables con estas mediciones

## Controles de integridad y profiling

- Prueba con una key y 200 peticiones: 64 aceptadas, 136 rechazadas, 640.000 tokens validados y cero tokens parciales en fallos. El arnés sale con error y conserva JSON
- `perf` se ejercitó durante dos repeticiones y devolvió contadores positivos de ciclos e instrucciones del proxy
- `heaptrack` se ejercitó con system allocator, comprobando que el executable perfilado es `--role proxy`, no el wrapper de afinidad
- El perfil de allocations abarca toda la vida del proxy, setup y warmup incluidos. No es allocations por petición y no se compara directamente con mimalloc
- Tests específicos cubren framing, UTF-8, CRLF, ownership de buffers, recording, PII y reasoning

## Artefactos y reproducción

Los artefactos originales de esta sesión quedaron en `/root/.hermes/cache/scratch/openproxy-perf/paired-v2/`

- `manifest.json`: commit baseline, hashes de los binarios y condiciones
- `runs.json`: métricas resumidas de las 30 corridas
- `summary.json`: medias, desviaciones, rangos y cambios relativos
- JSON por corrida: resultados de cada petición, recursos por rol y statements SQL

La ruta de scratch es temporal. Este documento conserva los resultados agregados, no sustituye los registros crudos

Los comandos y contratos del arnés están en [benchmarks-and-token-stress.md](benchmarks-and-token-stress.md)
