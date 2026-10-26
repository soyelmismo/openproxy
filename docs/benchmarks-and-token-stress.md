# Benchmarks opcionales de OpenProxy

La carga vive en `examples/`, los microbenchmarks en `benches/`. Ninguna carga se ejecuta con `cargo test` ni durante CI. `--self-test` comprueba el propio arnés, también de forma explícita

Resultados medidos y límites de la comparación de framing: [benchmarks-sse-v2-results.md](benchmarks-sse-v2-results.md)

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

No se llama a headers «tiempo de routing» ni a duración/eventos «CPU del pipeline». Estas métricas incluyen sockets, scheduling, upstream y cliente. Los porcentajes de CPU usan una ventana que incluye el drain de persistencia

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

## Orden de optimización apoyado por mediciones

1. Corregir integridad/rechazos antes de interpretar throughput
2. Consultar `by_prepared_sql` para localizar SELECT repetidos y transacciones, sin asumir caché a partir de lecturas de código
3. Correlacionar `perf` y stacks de heaptrack con microbenchmarks. Preservar reasoning, tools, PII, usage y recording
4. Cambiar una causa por vez y repetir la misma matriz con el mismo arnés. No reducir durabilidad ni desactivar funciones para presentar una ganancia
