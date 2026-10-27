# Patrones de clientes y métricas de cola

## Alcance y procedencia

Medición opcional ejecutada fuera de tests y CI, sobre procesos y SQLite nuevos por repetición

- Fuente: `r6/{fire,model,open,slow,cancel}.json` bajo `/root/.hermes/cache/scratch/openproxy-perf/`
- Resumen comprobado: `r6/metrics-summary.json`
- Código del arnés y optimizaciones: `r6/matrix-source.patch`, base `e0ca046ee`
- SHA256 del binario congelado: `b94a525141fd7e336911bc94ae85a011b6f21048b4d62c9199b36ae58bb3dfd3`
- Feature `benchmark-sql`, allocator mimalloc, proxy en CPU 2-3 con dos workers, mock CPU 0 y cliente CPU 1
- Listener backlog 4096, coalescer 1 ms, journal group commit y UPDATE condicionado de rate limit
- SQLite WAL en disco con `synchronous=FULL`, sin cambiar garantías de persistencia ni cierre
- Background discovery/supervisor desactivados en todos los escenarios, como configuración de aislamiento del arnés
- Tres repeticiones independientes por escenario, con tracepoints reales de `fsync`/`fdatasync`

La ejecución terminó con `MATRIX_VERIFIED`: 15 corridas, 32.700.000 tokens sintéticos íntegros, 300 cancelaciones deliberadas y cero fallos. El throughput excluye el contenido parcial de los cancelados

## Resultados

Los valores de throughput son media e IC95 bilateral Student-t sobre tres procesos/DB nuevos. Las demás cifras son medias de los valores de cada corrida, no percentiles calculados sobre una mezcla de muestras

| Escenario | Solicitudes por corrida | Tokens por stream | Tokens/s | IC95 tokens/s | fsync por éxito | Storage B por éxito | Media de p50 DONE→close |
|---|---:|---:|---:|---|---:|---:|---:|
| Firehose | 800 | 10.000 | 743.163 | 722.212–764.115 | 0,455 | 45.466 | 53,5 ms |
| Intervalo upstream 20 ms | 800 | 2.000 | 137.526 | 53.438–221.614 | 0,197 | 21.791 | 104,3 ms |
| Open-loop 100 req/s | 800 | 1.000 | 99.312 | 99.264–99.359 | 3,357 | 292.181 | 13,9 ms |
| Lectura cliente cada 20 ms | 100 | 3.000 | 52.352 | 45.677–59.026 | 0,343 | 33.423 | 21,1 ms |
| Mitad cancelada tras 500 tokens | 200 | 2.000 | 280.662 | 261.092–300.233 | 0,603 | 61.604 | 20,6 ms |

En el caso mixto, fsync, storage y CPU divididos por éxitos incluyen también trabajo de los cancelados. No son atribución causal por request exitosa

El escenario de intervalo 20 ms sufrió gran varianza: 98.632–160.332 tokens/s. El IC amplio es parte del resultado. Esta matriz no compara variantes ni prueba una mejora frente a otra compilación

El resultado open-loop está limitado por la llegada configurada de 100 req/s y 1.000 tokens/req. No significa que 99k tokens/s sea el máximo de capacidad del proxy

## Gaps, timeline y scheduling

- Firehose: límites de bucket p50 <8.192 µs y p99 <131.072 µs en las tres corridas
- Open-loop: p50 <2.048 µs y p99 <4.096 µs en las tres corridas
- Cliente lento: p50 y p99 <32.768 µs en las tres corridas, coherente con la pausa artificial de 20 ms
- La timeline usa ventanas de 100 ms. Se comprobó que la suma de ventanas coincide exactamente con los tokens validados en las 15 corridas
- Open-loop: scheduling lag p99 2.373–3.010 µs, latencia programada p99 80.406–103.555 µs
- Todas las corridas tuvieron cero ListenOverflows y ListenDrops. SYN retrans y retransmisiones siguen siendo contadores compartidos del namespace, no exclusivamente del proxy
- Se recogieron context switches de todos los hilos vivos, espera runnable de schedstat y contadores context-switches de perf

El histograma mide separación entre lecturas con contenido ya validado. Excluye comentarios keepalive y espera posterior a DONE. Su resolución es log2, no el tiempo de generación de cada token dentro de una lectura

## Persistencia y memoria

A poca simultaneidad, las admisiones no suelen coincidir y el group commit tiene menos oportunidades: open-loop sigue pagando ~3,36 fsync y ~285 KiB de storage por éxito. No se puede extrapolar el ahorro del lote simultáneo a todas las tasas de llegada

El contenido de recording no se desactivó para obtener resultados. El arnés usa la configuración de prueba que tenía antes. La inspección de `r5/prof-rec/bench.db` mostró 813 usage rows con cuerpos y headers NULL, un journal vacío y 664 KiB de DB final. No atribuir las escrituras de ese caso a cuerpos de recording que no están presentes. La atribución WAL/DB requiere tracing por archivo aparte

En firehose, RSS pico del proxy fue 537–594 MiB y RSS al cierre 399–429 MiB. Son medidas de residencia del proceso en ese instante, no allocations, no ownership por stream y no prueba de fuga. El proceso termina después de tomar el snapshot

## Límites

La afinidad no aísla el host. No se ejecutaron compilaciones propias mientras corría la carga. El proxy del arnés usa router real y AppState::for_test, no todo el startup de producción. No se desplegaron estos cambios ni se modificó el servicio activo

`/proc/PID/io` mide storage atribuido, no amplificación física del SSD. Los contadores por hilo son cotas inferiores si termina un hilo. BEGIN explícitos y fsync reales se reportan por separado, sin equipararlos

Los perfiles self de SQLite o Hyper no son un límite causal completo del coste de sus rutas: locks, atomics y scheduler pueden quedar atribuidos a otros símbolos. No usar esos porcentajes como prueba de que una optimización no puede aportar nada
