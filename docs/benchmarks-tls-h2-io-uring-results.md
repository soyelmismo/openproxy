# A/B TLS/h2 con io_uring y corrección de retry mid-stream

## Veredicto

**No hay ganancia de throughput concluyente que justifique activar io_uring por defecto**

El backend queda opt-in mediante `OPENPROXY_IO_URING=1`. Tokio sigue siendo el default

Se conservan las correcciones de integridad del streaming y los contratos de I/O, independientemente del resultado de rendimiento

Esta evaluación es local. No hubo deploy ni commits de estos cambios

## Alcance y reproducibilidad

- 48 corridas completas: 8 escenarios × 2 variantes × 3 repeticiones
- 69.720.000 tokens sintéticos íntegramente validados
- 23.106 peticiones, 600 cancelaciones deliberadas, **0 fallos de validación de cliente**
- Mismo binario, alternando Tokio/io_uring por variable de entorno
- HTTPS con certificado validado y ALPN h2 obligatorio en la fixture. Una CA no confiable es rechazada
- Procesos y SQLite nuevos, durabilidad FULL, mismo allocator, afinidad, logging y arnés en ambas variantes
- Sin builds concurrentes. Guard ejecutable antes y después de cada corrida
- Manifest de fuentes y hash del binario verificados, `SOURCE_DRIFT=0` al cerrar la medición
- TLS y HTTP/2 conservan rustls y Hyper. io_uring se sitúa debajo de TLS en sockets directos, no sustituye el protocolo

Binario SHA256: `f4e55a7489aebd219f84608599f6b8c4383447227b8af7e8942782f15187becf`

Runners y evidencia bruta: `/root/.hermes/cache/scratch/openproxy-perf/`

- `run_r14_uring_ab.py`, `analyze_r14.py`
- `r14-run.log`, `r14-analysis.log`
- `r14/summary.json`, `r14/analysis.json`, `r14/analysis.md`
- `r14/sources.json`, `r14/binary-sha256.json`
- JSON de cada corrida y `*.proxy.stderr`
- Cada JSON conserva la ruta de su SQLite temporal y stderr de roles

Estos directorios son scratch, no archivo permanente. Este documento y la wiki conservan los resultados y las decisiones

## Resultados

Medias de throughput hasta cierre de respuesta, incluida la espera de persistencia. ± indica desviación estándar muestral entre tres corridas, no intervalo de confianza

Los IC95 son descriptivos de los cambios porcentuales emparejados por repetición, Student-t con df=2. No se corrigieron comparaciones múltiples. Todos incluyen cero

- **Single**, 1×10.000 tokens: Tokio 287.092 ±245.267 → io_uring 198.397 ±9.183 tok/s, **−30,9%**. IC95 [−169,4%, +185,2%]. La enorme dispersión de Tokio impide atribuir una regresión
- **Light**, 50×1.000 tokens, 20 ms/evento: 12.263 ±32 → 12.293 ±54 tok/s, **+0,25%**. IC95 [−1,0%, +1,4%]. Limitado por el ritmo del upstream
- **Short**, 800×200 tokens: 143.952 ±9.467 → 159.136 ±2.368 tok/s, **+10,55%**. IC95 [−8,4%, +30,1%]. Sin ganancia concluyente
- **Open-loop**, 800×1.000 tokens a 100 req/s: 99.458 ±124 → 99.431 ±19 tok/s, **−0,03%**. IC95 [−0,3%, +0,3%]. Limitado por las llegadas
- **Firehose**, 800×10.000 tokens: 1.368.288 ±49.699 → 1.341.615 ±26.883 tok/s, **−1,95%**. IC95 [−13,3%, +9,6%]
- **Paced5**, 400×2.000 tokens, 5 ms/evento: 177.660 ±3.416 → 161.082 ±16.230 tok/s, **−9,33%**. IC95 [−33,5%, +15,0%]
- **Recording**, 800×2.000 tokens: 696.904 ±74.879 → 687.026 ±66.138 tok/s, **−1,42%**. IC95 [−18,4%, +16,0%]
- **Cancel**, 200×2.000 tokens, 50% de cancelación tras 500 tokens y 1 ms/evento: 258.344 ±16.651 → 290.871 ±30.000 tok/s, **+12,59%**. IC95 [−26,5%, +52,7%]. Tokens parciales cancelados excluidos

La diferencia de medias no equivale a la media de los cambios porcentuales emparejados. El porcentaje y su IC describen estimadores distintos y no deben combinarse como si fueran el mismo número

## CPU y cadencia

CPU de proxy en segundos incluyendo drenaje de persistencia, Tokio → io_uring:

- Single: 0,020 → 0,013 s. Resolución insuficiente para inferir ahorro
- Light: 0,887 → 1,343 s, **+51,5%** sin mejora de throughput
- Short: 1,827 → 1,817 s, −0,5%
- Open-loop: 4,380 → 4,623 s, +5,6%
- Firehose: 10,387 → 10,960 s, **+5,5%**
- Paced5: 4,530 → 5,013 s, +10,7%
- Recording: 3,883 → 3,910 s, +0,7%
- Cancel: 0,917 → 0,950 s, +3,6%

En firehose, la cota superior del gap p50 baja de 16,384 a 8,192 ms. El p99 queda en 65,536 ms. El TTFT p50 medio pasa de 2023,6 a 2087,5 ms

En paced5, gap p50 permanece en 8,192 ms, p99 medio pasa de 32,768 a 54,613 ms y TTFT p50 de 287,7 a 387,1 ms

Son cotas del histograma log2 entre lecturas con tokens observadas por el cliente, no latencia exacta de generación. Una menor cota p50 no demuestra más capacidad

Hasta el último `[DONE]`, firehose entrega 1.369.961 → 1.343.174 tok/s y short 157.849 → 186.841 tok/s. Se conserva esta métrica separada del cierre durable

No se atribuye el resultado a un cuello de botella causal sin perfil adicional

## Retry y errores capturados

Solo una fila `attempt > 1`, en **short-tokio-3**:

- Intento 1: 502, `upload interrupted`, sin completion_tokens contabilizados
- Intento 2: 200 con exactamente 200 tokens
- El cliente validó íntegramente esa respuesta

Los 600 cortes deliberados produjeron 461 filas 499 y 139 filas 502 con texto `client disconnected`. Es la clasificación no uniforme de desconexiones planificadas, no 600 fallos de validación. El 502 adicional es el retry anterior

Se capturaron 11 `errno=107` (`ENOTCONN`), todos en **shutdown**. `poll_shutdown` contempla el socket ya desconectado como cierre final

No se capturaron errores de read, write, submit o eventfd. Las ráfagas de errores previas no se reprodujeron. No hay evidencia para atribuirlas a saturación

`RUST_LOG` por sí solo no bastaba: el example ahora instala un subscriber por worker y escribe en stderr para no corromper el JSON de control de stdout

Se verifica positivamente `persistence.io_uring_active` en cada corrida. La mera ausencia de un warning de fallback no prueba el backend usado

## Correcciones verificadas

### Retry mid-stream

El A/B anterior r13 quedó invalidado en la corrida 44/48. El pipeline reintentaba tras emitir un prefijo parcial y concatenaba una segunda generación. Se observaron 163 respuestas corruptas, de 280 o 360 tokens frente a 200 esperados

- Commit de bytes no vacíos compartido por `DirectSink`, después de reservar capacidad
- Gates en retry, target loop y races, sin rotación ni fallback después del commit
- `Discard` conserva retries seguros, porque no emite hacia el cliente
- Resultado final y primer chunk usan el mismo árbitro `claim_winner`
- Error terminal por el mismo FIFO que los datos, sin terminador de éxito posterior a un corte
- Regresiones con upstream TCP real cortado, bridge y routers OpenAI/Anthropic/Responses

### Contratos io_uring

- `poll_write` retorna `Ready(Ok(n))` al aceptar bytes en el mismo poll. No atribuye un buffer viejo a otro slice tras cancelación
- Buffer bounded de 32 KiB y drenaje de escrituras parciales en flush
- Flush/shutdown propagan EPIPE y shutdown espera el flush antes de SHUT_WR
- Lectura vacía inmediata y remanente conservado con buffers pequeños
- Slot retiene fd y buffers hasta CQE final
- Eventfd con PollAdd priorizado ante SQ llena
- Retry explícito EINTR/EBUSY/EAGAIN/ENOMEM y fallo fatal visible
- Sync-cancel antes de despertar callers con error fatal. Cuarentena defensiva si el kernel no permite cancelar

13 pruebas io_uring pasan, incluyendo SQ=2 con 256 operaciones y errores submit inyectados

## Gates y revisión

- `cargo fmt --all --check` pasa
- Clippy workspace/all-targets pasa con excepción del `ref-option` preexistente en horde. También se verificó con feature benchmark-sql
- Workspace: **3114 passed, 0 failed, 14 ignored**, excluyendo el test remoto `test_codex_remote_upstream_models_json_drift_detection`
- El gate sin exclusión detectó el nuevo modelo remoto `gpt-6.1-terra`. No se alteró el catálogo para ocultarlo
- Revisión independiente final del driver: `passed=true`, sin security_concerns ni logic_errors
- Cierre independiente del árbitro de races: `passed=true`, sin security_concerns ni logic_errors
- Draft delegado de tests SSE corregido y ejecutado por el proceso padre antes de darlo por verificado

## Límites

- n=3 por variante/escenario y host compartido. Afinidad no elimina interferencias
- Fixture loopback con TLS/h2 real, no red WAN ni APIs remotas de inferencia
- Tokens sintéticos, no tokenizer ni tokens generados por un modelo
- Contadores TCP del host, no exclusivos del benchmark
- Throughput limitado por pacing o llegadas en light/open-loop
- No se demostró mejora de throughput, ni una regresión causal universal
- Opt-in experimental, sin deploy. La integridad local verificada no declara el estado de producción
