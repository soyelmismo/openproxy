# Benchmarks opcionales de OpenProxy

La carga vive en `examples/`, los microbenchmarks en `benches/`. Ninguna carga se ejecuta con `cargo test` ni durante CI. `--self-test` comprueba el propio arnés, también de forma explícita

Resultados medidos y límites de la comparación de framing: [benchmarks-sse-v2-results.md](benchmarks-sse-v2-results.md)

Validación final de group commit, SELECT, locks y resiliencia HTTPS/h2: [benchmarks-closing-validation.md](benchmarks-closing-validation.md)

## Microbenchmarks del pipeline

```bash
cargo bench --bench token_throughput -- --noplot --sample-size 10 openai_token_throughput
cargo bench --bench token_throughput -- --noplot --sample-size 10 anthropic_token_throughput
cargo bench --bench token_throughput -- --noplot --sample-size 10 gemini_token_throughput
cargo bench --bench token_throughput -- --noplot --sample-size 10 accumulator_token_scale
```

Estos casos miden trabajo en memoria. No representan el throughput HTTP del servicio ni su consumo global de recursos

## Carga HTTP multiproceso

```bash
cargo build --release -p openproxy-server --example bench_token_stress
./target/release/examples/bench_token_stress --self-test
./target/release/examples/bench_token_stress 50 10000 5 --repetitions 5 --json report.json
```

Se conservan los tres argumentos originales: concurrencia, tokens por stream y tokens por evento. Hay tres procesos separados: proxy, upstream mock y cliente. El coordinador muestrea los recursos desde fuera del proxy

`--recording` activa la captura y persistencia de bodies y headers en la DB aislada del benchmark. Está apagado por defecto, igual que en las mediciones anteriores. Para medir cambios en `ResponseAccumulator`, ejecutar el A/B con recording activado en ambos binarios y comprobar que las filas de usage contienen el response body. Un A/B con recording apagado no ejercita ese accumulator

En un host con CPUs permitidas `0-3`, el reparto por defecto es mock `0`, cliente `1`, proxy `2,3`, dos workers de Tokio. Se requiere Linux, `taskset`, `getconf` y al menos tres CPUs permitidas. Es afinidad, no aislamiento del resto del host

```bash
./target/release/examples/bench_token_stress 200 10000 5 \
  --proxy-cpus 2,3 --workers 2 --mock-cpus 0 --client-cpus 1 \
  --repetitions 5 --json report-200.json
```

Cada repetición levanta procesos y SQLite nuevos, calienta cada key, espera a que drene el journal y luego mide un lote simultáneo. Al terminar se espera el cierre del servidor y la persistencia de usage antes de tomar el último snapshot. No se usan el puerto, las credenciales ni la base de datos de producción

El mock usa buffers deterministas preconstruidos, fuera del proceso medido, y emite un evento por chunk solicitado. No agrupa más tokens por evento para inflar throughput

### Admisión y errores

El límite real de **64 streams por API key** se conserva. Por defecto se crean `ceil(concurrencia/64)` keys reales y se reparte la carga entre ellas. El RPM de producción tampoco se desactiva

Para probar el rechazo intencional:

```bash
./target/release/examples/bench_token_stress 200 10000 5 --keys 1 --repetitions 1 --json rejected.json
```

Este caso puede aceptar 64 y rechazar 136. Cuenta 640.000 tokens validados, nunca los 2 millones solicitados. El exit code es distinto de cero si alguna petición falla. El JSON se escribe incluso en ese caso

## Qué se valida y qué se mide

- **Texto recibido**: validación byte por byte de la secuencia sintética ` tok`, longitud exacta y checksum FNV-1a de 64 bits
- **Tokens sintéticos recibidos**: se cuentan al consumir texto, no multiplicando concurrencia por tokens pedidos. No son tokens producidos por un tokenizer ni inferencia de un modelo
- **Integridad**: `finish_reason=stop`, usage exacto `10/N/(N+10)`, un único `[DONE]`, sin contenido posterior, JSON válido y sin errores SSE
- **Framing**: parser independiente de los frames HTTP, delimitación SSE por líneas vacías, LF/CRLF, comentarios y `data:` multilínea. El número de frames HTTP y el de eventos SSE se guardan por separado
- **Latencia por petición en µs**: headers, primer evento con texto (TTFT) y fin de body. Media, desviación, mínimo, p50/p95/p99 por nearest rank y máximo. Los éxitos y los intentos fallidos se distinguen
- **Throughput**: tokens de streams completamente validados / tiempo real del lote. Los tokens parciales de streams fallidos se reportan aparte
- **RSS**: baseline después del warmup, pico muestreado cada 5 ms, snapshot al finalizar el cliente y después de persistir. `VmHWM` es el pico de toda la vida del proceso, incluido setup. RSS no equivale a allocations y dividirlo por requests no atribuye memoria a un payload
- **CPU**: ticks user/system desde `/proc/PID/stat`, `CLK_TCK` consultado con `getconf`, segundos de CPU y porcentaje respecto a un core. Los ticks no se presentan como ciclos hardware
- **I/O por proceso**: bytes lógicos, syscalls de lectura/escritura, bytes de storage contabilizados por Linux y page faults. `write_bytes` no cuenta fsync, no es exclusivamente WAL y no mide amplificación física en el SSD
- **Repeticiones**: media, desviación y rango de throughput entre procesos nuevos, además de cada petición individual en JSON
- **Tokens/s hasta `[DONE]`**: tokens validados divididos por el tiempo hasta el último `[DONE]`. Separa el streaming de la cola posterior, donde el cliente espera a que el usage quede durable
- **`[DONE]`→cierre**: µs desde el `[DONE]` hasta el fin del body. Con `synchronous=full` mide la espera de durabilidad del usage. Un `max_gap` de ~5 s cerrado por un comentario keepalive pertenece a esa espera, no a un corte de tokens
- **Requests completadas/s** y `start_offset_us` por request desde la barrera de salida

No se llama a headers «tiempo de routing» ni a duración/eventos «CPU del pipeline». Estas métricas incluyen sockets, scheduling, upstream y cliente. Los porcentajes de CPU usan una ventana que incluye el drain de persistencia

## Escenarios adicionales y métricas de cola

Resultados de la matriz de cinco escenarios y sus límites: [benchmarks-client-patterns-results.md](benchmarks-client-patterns-results.md)

Los tres escenarios siguientes son opcionales y solo se ejecutan mediante el example, nunca desde `cargo test` ni CI

```bash
# Llegadas open-loop a 100 req/s, 800 peticiones totales
./target/release/examples/bench_token_stress 800 1000 5 \
  --arrival-rate 100 --repetitions 3 --perf-fsync --json open-loop.json

# Clientes que esperan 20 ms antes de leer cada frame HTTP
./target/release/examples/bench_token_stress 100 3000 5 \
  --read-delay-ms 20 --repetitions 3 --json slow.json

# La mitad de los clientes desconecta tras observar 500 tokens
./target/release/examples/bench_token_stress 200 2000 5 \
  --event-interval-us 1000 --cancel-after-tokens 500 --cancel-percent 50 \
  --repetitions 3 --json cancelled.json
```

`--arrival-rate` programa todas las llegadas con deadlines absolutos desde un reloj común. No espera al cierre de la petición anterior. `concurrency` sigue siendo el número total de peticiones del lote y el máximo posible de clientes simultáneos, no una medición de cuántas estaban activas a cada instante

Se guardan `planned_start_offset_us`, `scheduling_lag_us` y `scheduled_latency_us` (lag + duración real). Así se ve la carga que el cliente no pudo iniciar a tiempo, en vez de ocultarla mediante coordinated omission. Es un ensayo finito de llegadas deterministas, no una prueba estacionaria ilimitada

Las cancelaciones previstas se separan de los éxitos y los fallos. Sus tokens parciales no inflan throughput ni timeline. El coordinador exige observar tantas cancelaciones como se pidieron. El escenario de clientes lentos mide backpressure de lectura, no simula por sí solo una conexión con ancho de banda o RTT prefijados

Métricas adicionales:

- `tcp_host_delta`: ListenOverflows, ListenDrops, TCPSynRetrans, RetransSegs, aperturas y resets desde `/proc/net/{netstat,snmp}`. Son contadores del namespace de red, compartidos con otros procesos del host, no atribución exclusiva al proxy
- Context switches voluntarios e involuntarios de todos los hilos vivos, y espera runnable desde `task/*/schedstat`. Las salidas de hilos pueden reducir las sumas, así que los deltas son cotas inferiores. El sampler RSS de 5 ms solo lee RSS, no recorre todos los hilos
- Histograma log2 de gaps entre lecturas con contenido validado, excluyendo keepalives y la cola tras `[DONE]`. p50/p99/p99.9 son límites superiores de bucket, con resolución de factor dos. No son tiempos de generación de tokens individuales dentro de un frame
- `tokens_per_window`: tokens validados por ventana de 100 ms desde el inicio del lote. Se exige que la suma coincida con los tokens validados. Los conteos y `[DONE]` proceden del parser SSE, resistentes a fragmentación HTTP
- CPU, bytes de storage, syscalls, context switches, espera runnable y statements SQL normalizados por request completada. En casos mixtos incluyen el trabajo de los clientes cancelados, no son una atribución causal por request
- `explicit_transaction_begins` cuenta BEGIN explícitos, incluidos intentos fallidos. Excluye autocommit y no equivale a fsync. `--perf-fsync` añade tracepoints reales de entrada a fsync/fdatasync y requiere permisos de perf, sin modificar permisos ni sysctl
- `throughput_mean_ci95`: intervalo bilateral Student-t del 95% sobre repeticiones independientes. Es `null` con n=1. Para más de 30 grados de libertad usa el valor conservador de df=30. El intervalo no elimina ruido compartido del host

## SQL real, opcional

```bash
cargo build --release -p openproxy-server --example bench_token_stress --features benchmark-sql
./target/release/examples/bench_token_stress 50 10000 5 --repetitions 3 --json sql.json
```

Activa `rusqlite::trace_v2` en todas las conexiones del `DbPool`, incluidas las reconstruidas. Cuenta ejecuciones reales por SQL preparado, total de statements, segundos de ventana y statements/s. Incluye routing, transacciones, triggers, persistencia y tareas de background durante la ventana. **No atribuye consultas a fases o a requests concretos**

No usa SQL expandido ni recoge los valores de parámetros ligados. El catálogo está acotado. La feature está desactivada en builds normales y añade overhead cuando se usa. Comparar dos variantes con la misma instrumentación

Sin la feature, SQL se etiqueta `not instrumented`. El antiguo «0 queries en el hot path» era una constante y no una medición

## Ciclos hardware con perf

```bash
./target/release/examples/bench_token_stress 50 10000 5 --repetitions 3 --perf --json perf.json
```

Adjunta `perf stat` solo al proxy y sus threads. Mediante FIFO activa contadores antes de `go` y los desactiva después del drain: cycles, instructions, cache-misses, task-clock, context-switches y page-faults. El JSON conserva valores, unidades y scaling, y guarda el CSV original

Requiere `perf`, `mkfifo` y permisos de perf_event. Si no están disponibles se reporta el error, sin inventar ciclos. Los contadores no soportados se conservan como `null` y su motivo en `raw`. No cambia sysctl ni permisos del host

## Allocations reales por stack y tamaño

```bash
# Instalar heaptrack con el package manager local, si no existe
# Mantener símbolos del example para atribuir las llamadas
cargo rustc --release -p openproxy-server --example bench_token_stress \
  --features benchmark-system-allocator -- -C strip=none -C force-frame-pointers=yes
./target/release/examples/bench_token_stress 5 1000 5 --repetitions 1 --heaptrack --json heap.json
```

El preload de heaptrack se lanza solo en el proxy, después de `taskset`, sin adjuntarse al servicio activo. La interpretación verifica que `Debuggee command was:` contiene `--role proxy` para no aceptar un perfil del wrapper. Produce:

- `heaptrack-proxy.raw` e `heaptrack-proxy.interpreted`
- `heaptrack-summary.txt`: llamadas, temporales y consumo por stack
- `allocation-sizes.tsv`: histograma de tamaños
- `allocation-stacks.txt`: stacks para flamegraph

Rutas Linux por defecto: `/usr/lib/heaptrack/libheaptrack_preload.so` y `/usr/lib/heaptrack/libexec/heaptrack_interpret`. En otras distribuciones usar `HEAPTRACK_PRELOAD` y `HEAPTRACK_INTERPRET`. También requiere `heaptrack_print`

El perfil usa **system allocator**, no mimalloc, porque interponer `malloc` no detecta todas las allocations de mimalloc. Es intrusivo y abarca la vida completa del proxy, setup/warmup incluidos. No comparar su RSS o throughput con la corrida de mimalloc ni dividir sus allocations totales por requests para atribuir ownership. Los stacks permiten localizar qué estructuras/serializadores reservan memoria, no asignarla automáticamente a parámetros de dominio

## Durabilidad y disco

SQLite está en un directorio temporal bajo `TMPDIR`, **no en RAM salvo que ese filesystem sea tmpfs**. Se conserva WAL y `synchronous=full` por defecto, coherente con el servicio

`--synchronous normal` permite estudiar ese tradeoff de manera explícita. Puede perder transacciones recientes ante pérdida de alimentación, no es una optimización gratuita ni cambia la configuración del servicio. Nunca comparar FULL contra NORMAL como si fuera solo una mejora algorítmica

## Reproducibilidad y límites

- Ejecutar comparaciones **en serie**, no dos arneses a la vez
- Usar mismo chunk size, CPUs, workers, durabilidad, features, allocator y volumen de contenido
- Ensayar 1 stream y alta concurrencia. Si satura el cliente o el mock, el throughput está limitado por el generador, no prueba el máximo del proxy
- Guardar JSON y commits de ambos binarios. Los registros antiguos del arnés in-process no son comparables con este diseño multiproceso
- El proxy usa `AppState::for_test`, router real y `axum::serve`, no el startup ni el accept loop endurecido del binario de producción. Este arnés no reemplaza una prueba del despliegue
- No se garantiza −80% ni 100× por añadir instrumentación. Solo afirmar mejoras que sobrevivan varias repeticiones comparables
- `--output-dir` elige el padre de artefactos. Cada ejecución crea un directorio único con JSON y stderr por rol. Los errores de setup/control terminan la corrida y limpian procesos. Los artefactos se conservan para diagnóstico

## Rondas persistentes, cortes dirigidos y telemetría de cierre

`--rounds N` repite el lote sobre el mismo cliente y su pool de conexiones.
Los IDs y offsets de todas las rondas usan un reloj global. El número total de
requests es `concurrency × rounds`. Las rondas esperan el cierre del lote
anterior, no son un generador estacionario de tráfico

```bash
# HTTPS/h2 upstream con cinco rondas y transporte sintético degradado
./target/release/examples/bench_token_stress 16 200 5 \
  --rounds 5 --upstream-tls --network-delay-ms 20 --event-interval-us 1000 \
  --repetitions 1 --json persistent.json

# Cortar 20 de cada 100 streams tras 20 eventos, sin cortar el warmup
./target/release/examples/bench_token_stress 100 500 5 \
  --upstream-tls --upstream-cut-after-events 20 --upstream-cut-percent 20 \
  --event-interval-us 5000 --repetitions 1 --json cut.json
```

Los cortes upstream producen fallos de validación reales y exit code distinto
de cero. Se conserva el JSON para exigir el número y status esperados y revisar
`attempt > 1` en la DB. No se convierten en éxitos ni en cancelaciones de cliente.
El mock reconoce el warmup mediante un marcador en el mensaje sintético del
body, no mediante una cabecera privada que el proxy puede descartar. El warmup
no consume posiciones del contador de inyección

`--network-delay-ms` levanta un relay TCP sobre loopback con buffers acotados.
Duerme por cada bloque de hasta 16 KiB en ambos sentidos, debajo de TLS. Es
pacing/latencia sintética local, no una WAN real ni un modelo de BDP, congestión
o pérdida de paquetes. `--upstream-tls` exige una CA de benchmark válida y ALPN
h2, sin desactivar la validación de certificados

Con `benchmark-sql`, `persistence.sql.profile_by_sql` añade duración de ejecución
por template y `locks` recoge esperas de adquisición de reader/writer del pool.
SQLite PROFILE suele tener resolución de milisegundos aunque el campo se
exprese en ns: un SELECT con duración cero no implica coste cero. Los locks
obtenidos directamente mediante `writer_arc()` no pasan por el contador del
pool y se miden por separado en el journal

`persistence.batching` separa admisión y replay: entradas, commits exitosos,
tamaño de lote, espera de lock y duración de transacción. La cola de admisión
es enqueue→dequeue de la entrada más antigua por lote, excluye lock y commit.
No es una distribución por request y el replay no tiene muestras de esa cola.
Los histogramas log2 dan cotas de percentil, no p95/p99 exactos

Los timers de batching y de adquisición de locks desaparecen de los builds
normales. El snapshot no guarda prompts, bindings ni credenciales. Medir fsync
reales con `--perf-fsync`, nunca inferirlos del número de commits

## Orden de optimización apoyado por mediciones

1. Corregir integridad/rechazos antes de interpretar throughput
2. Consultar `by_prepared_sql` para localizar SELECT repetidos y transacciones, sin asumir caché a partir de lecturas de código
3. Correlacionar `perf` y stacks de heaptrack con microbenchmarks. Preservar reasoning, tools, PII, usage y recording
4. Cambiar una causa por vez y repetir la misma matriz con el mismo arnés. No reducir durabilidad ni desactivar funciones para presentar una ganancia

## Variables de rendimiento del servicio

- `OPENPROXY_SSE_COALESCE_MS` (default `1`): ventana máxima para juntar frames SSE ya listos en una sola escritura. El primer frame sale inmediato. `0` vuelve a una escritura por evento. Medido a 800 streams: 216k → 477-492k tokens/s y CPU del proxy 61 s → 19-22 s. Ventanas de 2-20 ms no mejoran más
- `OPENPROXY_LOOPBACK_NOTRACK` (`1`/`true`, apagado por defecto): instala una tabla nftables `inet openproxy_loopback_notrack_<pid>` con `flags owner` que marca `notrack` solo el tráfico `lo` hacia y desde los puertos de `bind` y `admin_bind`. El kernel la borra cuando muere el proceso, también con `kill -9`. Requiere `CAP_NET_ADMIN` y el binario `nft`. Si falla, el servicio arranca con tracking normal y avisa en el log. No confundir el +6-8% observado al vaciar todo el firewall en un netns con la ganancia de estas dos reglas: esa comparación no mide esta opción. Las reglas `ct state` del host dejan de ver ese tráfico, y no aplica a clientes remotos ni a upstreams

### Medir notrack sin cambiar el firewall del host

El arnés instala la misma implementación `LoopbackNotrack` que el servicio, limitada al puerto efímero del proxy. A diferencia del servicio, **aborta si se solicita la opción y no se puede instalar**: continuar con tracking normal invalidaría el A/B. El JSON registra `persistence.network` con el estado solicitado/activo, puerto, tabla y verificación de borrado al terminar. `notrack_shutdown_us` separa el tiempo de limpieza del cierre HTTP y del drain de usage

[A/B medido](benchmarks-loopback-notrack-results.md): 16 corridas alternadas, 800 streams, 128 millones de tokens validados y cero fallos. OFF: 462k ± 19k tokens/s, ON: 456k ± 22k. CPU total del proxy: 20,26 s frente a 20,29 s. **Sin mejora de rendimiento reproducible**, mantener apagado por defecto

Ejecutar con `unshare -n` y levantar `lo` dentro de ese namespace. Mantener las mismas reglas base entre ambas variantes, cargar conntrack explícitamente para la baseline y alternar el orden OFF/ON y ON/OFF. Un namespace vacío sin conntrack no es una baseline válida para atribuir ahorros a notrack. Verificar durante la carga que los flujos cliente→proxy desaparecen de conntrack solo en ON y que los flujos proxy→upstream siguen rastreados. Comparar el mismo binario, workload, coalescer, afinidad, durabilidad y features, y conservar los JSON de cada corrida

## Group commit del usage y cola de conexiones

El journal de usage admite a los productores concurrentes en una sola transacción (`append_batch`, líder con cola y un fsync por grupo). Cada productor recibe su acuse solo después del commit durable de su grupo: la garantía de `synchronous=full` no cambia. El worker aplica el journal en lotes con la misma transacción. Si una entrada falla, el lote se repite una a una y se detiene en la entrada que falla

El listener público y el admin usan una cola de aceptación de 4096 (`openproxy_server::listener`). Con 128, 800 conexiones simultáneas producían 671 overflows y 333 reintentos SYN de 1 s

A/B alternado, 3 pares por escenario, 800 streams, CPU 2-3 para el proxy, mismo arnés y backlog 4096 en ambos binarios (`e0ca046ee` como base):

| Escenario | Métrica | Base | Group commit | Cambio |
|---|---|---|---|---|
| firehose 10.000 tokens | tokens/s | 470.836 ± 16.511 | 783.235 ± 26.545 | ×1,66 |
| firehose | `[DONE]`→cierre p50 | 5.634 ms | 59 ms | −99,0% |
| firehose | `[DONE]`→cierre p99 | 7.466 ms | 172 ms | −97,7% |
| firehose | latencia p99 | 16.900 ms | 10.195 ms | −39,7% |
| firehose | storage write del proxy | 285 MiB | 27,9-36,1 MiB | −88% |
| ritmo 20 ms, 2.000 tokens | tokens/s | 89.994 ± 3.978 | 171.622 ± 4.647 | ×1,91 |
| ritmo 20 ms | `[DONE]`→cierre p50 | 6.210 ms | 85 ms | −98,6% |
| ritmo 20 ms | latencia p99 | 17.721 ms | 9.273 ms | −47,7% |

28,8 M de tokens validados por variante (57,6 M en total), 0 fallos. La ganancia sale de eliminar la cola de durabilidad. Los tokens/s hasta `[DONE]` bajan un 6% en firehose (837.763 ± 40.965 → 784.004 ± 26.082) (hipótesis no medida: los commits ahora ocurren durante el streaming y compiten por la misma CPU), y en ritmo 20 ms suben de 164k a 172k, dentro del ruido. El proxy pasa de ~115% a ~190% de 2 cores y queda limitado por CPU
