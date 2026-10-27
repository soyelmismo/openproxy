# UPDATE condicionado de rate limit: contraste aislado

## Resultado y contrato

El camino de éxito usa `UPDATE accounts SET rate_limited_until = NULL WHERE id = ?1 AND rate_limited_until IS NOT NULL`

Cuando el campo ya es NULL no modifica filas. El test `clearing_an_unset_rate_limit_writes_nothing` comprueba que `conn.total_changes()` permanece constante y que un rate limit real se limpia correctamente

Se mantiene SQLite WAL con synchronous FULL y persistencia durable de usage antes del cierre. No se desplegó ni se hicieron commits

## Procedencia

- Base de código `e0ca046ee` más cambios de arnés y group commit compartidos
- Binarios congelados en `/root/.hermes/cache/scratch/openproxy-perf/r7-noop/bin/`
- Old SHA256 `976ef7f60a4f3949695614f2761927e9d2fb57884b8a632c848688128ae3d65f`
- Conditional SHA256 `9422207d06de71fb015fabaebe653fc96cf269243c75111d808ecd85c06cb49a`
- Delta de source entre ambos exclusivo de `accounts/crud.rs`, `accounts/tests.rs` y `stages/target_breaker.rs`, conservado en los source patches de r7-noop
- Confirmación limpia en `r8-noop-confirm/{rows,summary}.json`, bajo la misma raíz de scratch
- 18 corridas seriales: tres escenarios × tres pares alternados, procesos y SQLite nuevos
- 53.760.000 tokens sintéticos íntegros y cero fallos
- Mimalloc, feature benchmark-sql, background apagado, recording apagado en ambos, cinco tokens/evento
- Proxy en CPUs 2,3, mock CPU 0, cliente CPU 1, dos workers de proxy
- Perf tracepoints fsync/fdatasync habilitados exclusivamente en la ventana medida
- Sin builds ni cargas propias concurrentes durante la matriz de confirmación

La primera matriz r7 coincidió al final con una compilación SSE. Sus cifras de timing no se usan para atribuir la mejora. Se conservaron sus binarios, source patches y trace por archivo, y se repitieron todas las mediciones sin compilaciones

Los dos binarios r5 anteriores, base y gc, no contenían este UPDATE condicionado. No atribuirle aquella comparación de group commit

## Medias de las tres repeticiones

Throughput de peticiones completas, incluido el cierre durable. SD muestral entre procesos independientes

| Escenario | Old tokens/s ± SD | Conditional tokens/s ± SD | Ratio | fsync/req old → conditional | Storage B/req old → conditional |
|---|---:|---:|---:|---:|---:|
| 800 × 200 tokens simultáneos | 15.826 ± 557 | 114.900 ± 2.481 | 7,26× | 4,221 → 0,128 | 374.484 → 15.374 |
| 800 × 10.000 tokens firehose | 427.924 ± 22.931 | 751.466 ± 38.900 | 1,76× | 4,221 → 0,380 | 374.811 → 38.011 |
| 800 × 1.000 tokens a 100 req/s | 83.563 ± 1.399 | 99.344 ± 71 | 1,19× | 4,228 → 3,366 | 383.222 → 292.673 |

IC95 bilateral Student-t, n=3, sobre throughput hasta cierre

- Short old: 14.442–17.210, conditional: 108.737–121.064 tokens/s
- Fire old: 370.961–484.887, conditional: 654.833–848.099 tokens/s
- Open-loop old: 80.089–87.038, conditional: 99.168–99.521 tokens/s

## Separación entre entrega y cierre

El UPDATE evita escrituras de cuenta redundantes y permite al journal agrupar más trabajo. La mejora de peticiones completas no equivale a generar ni entregar tokens 7× más rápido

Throughput hasta `[DONE]`, media ± SD

| Escenario | Old | Conditional |
|---|---:|---:|
| Short | 115.247 ± 5.141 | 126.086 ± 2.966 |
| Fire | 731.263 ± 57.170 | 753.787 ± 39.390 |
| Open-loop | 99.506 ± 20 | 99.428 ± 62 |

Los IC95 de entrega de short/fire se solapan. No hay evidencia de una gran mejora en la entrega de contenido. Open-loop mantiene prácticamente el mismo ritmo impuesto por llegadas

Media de los p50 DONE→close de cada corrida, no percentil de una mezcla

- Short: 6.672,6 → 135,2 ms
- Fire: 6.007,8 → 86,7 ms
- Open-loop: 17,67 → 13,86 ms

CPU proxy incluido drain

- Short: 4,563 → 2,603 s
- Fire: 22,057 → 20,200 s
- Open-loop: 5,640 → 5,313 s

## Atribución de storage por archivo

Trace intrusivo separado tras warmup y hasta drain, 800 peticiones × 200 tokens. No usar sus tiempos como benchmark

- Old WAL: 61.797.768 bytes, 30.057 pwrite64, 3.318 fsync
- Old DB: 6.856.704 bytes, 1.674 pwrite64, 59 fsync
- Conditional WAL: 8.668.736 bytes, 4.216 pwrite64, 293 fsync
- Conditional DB: 1.241.088 bytes, 303 pwrite64, 8 fsync
- Cero fallos y cero líneas unfinished pendientes de reconstrucción al detach

Fuente `r7-noop/trace-{old,conditional}/storage-summary.json` y `storage.strace`. El parser reconstruye unfinished/resumed por TID y atribuye los descriptores mediante strace -f -yy

Los bytes de pwrite al WAL/DB son bytes lógicos emitidos por esos syscalls. `/proc/PID/io` mide storage atribuido al proceso y puede incluir journal del filesystem y checkpoint. Ninguno mide directamente amplificación física del SSD

## Límites

Affinity no elimina ruido del host. Los tokens son unidades sintéticas, no inferencia ni tokenizer. Los ahorros simultáneos no se trasladan a poca simultaneidad: open-loop sigue pagando 3,37 fsync/req

El tamaño del archivo DB final no mide el tráfico acumulado al WAL, ni un porcentaje self de SQLite acota el coste causal de sus locks y esperas
