# Validación de cierre de streaming y persistencia

## Procedencia y alcance

Matriz de caracterización del candidato, no comparación A/B ni prueba de producción

- Artefactos: `/root/.hermes/cache/scratch/openproxy-perf/close-final-v2/`
- Resultado del analizador: `CLOSING_ANALYSIS_VERIFIED 16 5212 1148000 100 20`
- 16 corridas, 5.212 requests medidas, 1.148.000 tokens sintéticos íntegros
- 100 cancelaciones deliberadas y 20 fallos upstream inyectados, sin fallos adicionales
- Binario del harness: SHA256 `e5bcce116c6ef0fdc2fbdc358183754eabb59afb220456bb6680872b1604919e`
- Binario normal compilado antes de la matriz: SHA256 `9cc5363e7aef1f8e1d50be2567de5e90ad5d5bfbca5c26f2d5642878213c4872`
- Manifest completo de fuentes congeladas en `sources.json`, binarios en `bin/`, comandos y comprobaciones SQLite en `measurements.jsonl`
- Sin drift de fuentes ejecutables ni compilaciones concurrentes durante las mediciones
- HTTPS upstream con CA local verificada y ALPN h2 obligatorio, `io_uring` desactivado
- Procesos separados y CPUs disjuntas: mock 0, cliente 1, proxy 2,3 con dos workers
- SQLite nuevo por corrida, WAL y `synchronous=FULL`, background y recording apagados
- Puertos efímeros y DBs aisladas, sin credenciales ni cambios de producción

Los contadores de status consultados después del cierre incluyen warmups. Los totales medidos del cliente y la ventana de SQL/batching los excluyen

## Correcciones del harness antes de medir

El mock reconoce el warmup por un marcador en el mensaje sintético del body. Una cabecera privada no bastaba: el proxy puede no reenviarla. El warmup no consume posiciones del contador de cortes

La fórmula de cortes es `request_index % 100 < percent`, comprobada para 0, 1, 20, 50 y 100 %. Los offsets de rondas persistentes comparten un reloj global, sin superponer las ventanas de las rondas posteriores

`entries` cuenta jobs del journal, no requests. Un éxito escribe un attempt y un marcador de respuesta. Se verificaron INSERT y DELETE reales del journal, igualdad entre admisión y replay, suma de tamaños de lote y journal vacío al terminar las 16 corridas

El escenario con cortes conserva exit code no cero y su informe. No convierte un stream incompleto en éxito ni en cancelación del cliente

## Llegadas y group commit

400 requests de 200 tokens, 5 tokens por evento, tres procesos/DB independientes por tasa. `0` significa lote simultáneo, no cero llegadas

| Llegadas | Tokens/s, media ± desviación | IC95 Student-t | fsync+fdatasync/request | Jobs/lote de admisión |
|---|---:|---|---:|---:|
| 25 req/s | 4.996 ± 0,5 | 4.995–4.997 | 3,808 | 1,00 |
| 100 req/s | 19.759 ± 21 | 19.706–19.813 | 3,356 | 1,21 |
| 400 req/s | 75.367 ± 528 | 74.054–76.679 | 0,628 | 6,08 |
| Simultáneas | 151.366 ± 602 | 149.869–152.862 | 0,110 | 34,30 |

Los fsync son tracepoints reales del proceso proxy y sus threads, normalizados por requests solicitadas. No se infieren a partir de BEGIN. La media de jobs/lote no es requests/lote

### Colas de admisión y cierre

Cotas superiores de histogramas log2 por lote, en µs. Los rangos describen las tres corridas, no percentiles combinados

| Llegadas | Queue p95 | Queue p99 | Lock p99 | Transaction p99 | DONE→close p99 observado |
|---|---:|---:|---:|---:|---|
| 25 req/s | 1.023 | 1.023 | 8.191 | 8.191 | 12,6–14,4 ms |
| 100 req/s | 8.191 | 16.383 | 8.191 | 8.191 | 13,3–35,6 ms |
| 400 req/s | 16.383 | 16.383–32.767 | 16.383 | 8.191–16.383 | 34,2–49,9 ms |
| Simultáneas | 65.535 | 65.535 | 65.535 | 8.191–16.383 | 91,0–126,0 ms |

Queue mide enqueue→dequeue del job más antiguo del lote. Excluye lock y transacción. El replay no tiene muestras de esa cola y sus lotes medios fueron 1,21–1,24, 1,27–1,33, 7,69–8,89 y 50,0–61,5 jobs respectivamente

El group commit funciona con coincidencia de productores. No elimina el coste de fsync a baja simultaneidad: 25 y 100 req/s siguen pagando más de tres syscalls de sincronización por request. Añadir una espera artificial de batching cambiaría latencia y no está justificado por esta matriz

## SELECT y locks de entrada

La consulta de resolución `SELECT active, rate_limit_scope FROM providers WHERE id = ?1` se ejecutó 400 veces por cada lote de 400 requests, no 800. La resolución request-scoped conserva la eliminación de consultas duplicadas medida en el [A/B dedicado](benchmarks-routing-reuse-and-h1-cooperative-results.md)

Por corrida hay 3.200 adquisiciones de reader y 400 de writer instrumentadas en el pool. Los locks del journal obtenidos mediante `writer_arc()` se contabilizan por separado en batching

- Reader p99: cota ≤1.023 ns a 25, 100 y 400 req/s, y 4,19–8,39 ms en el lote simultáneo
- Writer p99: cota 4,19 ms a 25 req/s, 16,78–268,44 ms a 100 req/s y 33,55–67,11 ms en el lote simultáneo
- El writer muestra colas largas aunque el reader permanezca barato. Esta medición no atribuye causalmente toda la espera a una sentencia concreta

Entre los SELECT de mayor duración agregada aparecen lectura de modelos, datos del proveedor, selección de cuentas healthy y lectura del journal. En la primera corrida de cada tasa, la lectura de proveedor se ejecuta 800 veces y las consultas de pricing 1.200. Su repetición es un candidato de análisis, no prueba de que puedan cachearse entre requests

SQLite PROFILE expresa ns con resolución efectiva habitual de milisegundos. Una ejecución con duración cero no tiene coste cero. No sumar tiempos de statements anidados como si fueran tiempo exclusivo de CPU

Decisión de cierre: conservar reuse dentro de la misma request y group commit durable. No introducir caché entre requests de auth, credenciales, salud, cooldowns, cuotas o pricing sin un contrato de invalidación y un A/B propio. Los tests del router cubren restricciones de API key, allowlist/blacklist, combo, overrides y ausencia de reuse entre requests

## HTTPS/h2 y resiliencia

Todos los casos siguientes usaron el mismo binario congelado, CA verificada y ALPN h2

| Caso | Requests | Resultado | Retries | DONE→close p99 |
|---|---:|---|---:|---:|
| Persistentes, 16 × 5 rondas, relay 20 ms | 80 | 80 íntegras | 0 | 27,4 ms |
| Lectura lenta 5 ms, relay 10 ms | 32 | 32 íntegras | 0 | 34,7 ms |
| Cancelaciones, 40 × 5 rondas | 200 | 100 íntegras + 100 canceladas | 0 | 31,7 ms entre éxitos |
| Corte upstream tras 20 eventos | 100 | 80 íntegras + 20 fallos esperados | 0 | 49,1 ms entre éxitos |

Las cancelaciones se registraron exactamente como 100 filas 499, sin 502 espurios. La cuenta quedó healthy, `rate_limited_until=NULL` y ambas tablas de cooldown vacías

Los cortes upstream se registraron como 20 filas 502. No hubo nuevos intentos tras contenido parcial ni streams concatenados. El proceso terminó con exit code 1 y escribió el JSON como exige el contrato de fallo

Los tests dirigidos adicionales comprueban FIFO de contenido y error terminal, ausencia de DONE tras corte, ausencia de fallback después de bytes comprometidos y retry permitido antes de emitir contenido. Incluyen `/v1/chat/completions`, `/v1/messages`, `/v1/responses` y el árbitro común de ganador de stream y resultado

## Límites y estado de entrega

El relay duerme por cada bloque TCP de hasta 16 KiB. Es pacing sintético sobre loopback, no una WAN, ni una simulación de BDP, congestión o pérdida. El SSH router no tenía devices configurados: la prueba WAN remota quedó bloqueada, no aprobada por equivalencia con el relay

Las rondas persistentes reutilizan el cliente y su pool. No demuestran por sí solas que todos los requests usaran una única conexión upstream. La CA y ALPN prueban TLS/h2, no una topología WAN

El soak previo observó un GOAWAY h2 `too_many_resets` una vez, sin reproducirlo en tres corridas aisladas. Esta matriz limpia no elimina ese hallazgo ni establece la causa de los cortes históricos. Detalle en [soak h2](benchmarks-soak-h2-resilience.md)

Los tests locales del workspace tuvieron un bloqueo independiente de estos cambios: el test remoto `test_codex_remote_upstream_models_json_drift_detection` detectó un modelo nuevo `gpt-6.1-terra` en el catálogo externo. La pasada local posterior excluyó únicamente ese test. No presentar esa pasada como un workspace completo sin exclusiones ni como CI verde

Después de esta matriz se añadió `cfg(target_os = "linux")` al módulo, dependencia y variante de socket de io_uring para no romper los builds Windows/macOS. Es un delta de portabilidad posterior a la medición, no parte de los hashes anteriores. La resolución de dependencias confirmó io-uring presente en Linux y ausente en Windows/macOS. Los gates posteriores al delta lo cubren con hashes de binario propios:

- `cargo fmt --all -- --check` y `clippy -D warnings` limpios en el workspace y en `bench_token_stress` con `benchmark-sql`
- Workspace: 3117 tests pasados y 0 fallidos, excluyendo solo el test de drift remoto citado arriba
- `sql_profile::` 5/5 y `worker::profile::` 2/2 con `--test-threads=1`
- `cargo build --release` del servidor y del harness completados
- Smoke corto sobre los binarios reconstruidos, aparte de la matriz: 3 corridas, 172 requests, 66.400 tokens validados, 20 cancelaciones 499 y 20 cortes upstream 502 sin retries
- Binario normal `affeaa043b8b835ee3a6c4627594ac1184ac3811d488149fa8694078c68133d4` y harness `0cbc0e95ddc5f664edcdef4ff30008f74ba13ae2968ab18cc7a4d54c09b7663f`
- Manifiesto de 770 fuentes ejecutables sin cambios entre los gates y el commit

El smoke confirma que el delta no cambió el comportamiento en Linux. No sustituye a la matriz ni aporta cifras de rendimiento

No hubo deploy ni publicación de release como parte de estas mediciones
