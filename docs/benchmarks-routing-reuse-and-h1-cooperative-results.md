# Routing request-scoped y transporte HTTP/1 cooperativo

Dos cambios medidos en la misma matriz A/B/C, con contraste de source aislado y binarios congelados.

Variantes:

- **A base**: routing anterior, transporte legacy de producción
- **B reuse**: `PreResolvedRoutingPlan` capturado en `auth_middleware` y reutilizado en `routing`, transporte legacy
- **C coop**: routing reuse más transporte HTTP/1 cooperativo (`cooperative_h1.rs`), opt-in y desactivado por defecto

Ninguno cambia la garantía de persistencia durable antes del cierre. La matriz describe el estado experimental de esas corridas, sin deploy

Estado de integración posterior: HTTP/1 plano y directo usa el transporte cooperativo por defecto. `OPENPROXY_UPSTREAM_TRANSPORT=legacy` restaura el transporte anterior. HTTPS, proxies y upgrades conservan el camino legacy. `io_uring` sigue siendo opt-in, apagado por defecto

## Arnés

Fuente: `/root/.hermes/cache/scratch/openproxy-perf/r9-ab/`

- 6 escenarios × 3 variantes × 3 repeticiones alternadas, procesos y SQLite nuevos por corrida
- 54 informes, 85.230.000 tokens validados, 24.309 peticiones, 900 cancelaciones planeadas, **0 fallos**
- Binarios congelados antes de medir, hashes en `r9-ab/binary-sha256.json`
- Contraste de source: A→B limitado a `middleware/auth.rs` y `middleware/routing.rs`, más el arnés de test. B→C añade `upstream/{cooperative_h1,cooperative_h1_tests,client,response,upload,mod,request_builder}.rs`
- Sin compilaciones ni cargas propias concurrentes: `run_o5_o8_ab.py` aborta si detecta `cargo`/`rustc` activos antes y después de cada corrida
- Afinidad cliente CPU 1, mock CPU 0, proxy CPUs 2,3, dos workers de Tokio, mimalloc, `benchmark-sql`, `synchronous=full`, recording apagado, background apagado
- Estadística: n=3 procesos independientes, Student-t con df=2, emparejado por ronda. Intervalos descriptivos, sin corrección de comparaciones múltiples

Escenarios: `single` 1×10.000, `short` 800×200, `fire` 800×10.000, `open` 800×1.000 a 100 req/s, `slow` 100×3.000 con pausa de 20 ms del cliente antes de cada frame, `cancel` 200×2.000 con 50 % de cancelación a los 500 tokens y 1 ms entre eventos del mock

## Cambio de framing: el hecho que condiciona todo

El transporte cooperativo entrega el mismo contenido en muchos menos frames HTTP. Tokens validados idénticos en las tres variantes, 0 fallos

| Escenario | base frames/stream | reuse frames/stream | coop frames/stream |
|---|---:|---:|---:|
| fire | 694,2 | 695,6 | 109,2 |
| short | 16,4 | 16,1 | 3,9 |
| slow | 216,6 | 252,7 | 33,7 |
| cancel | 191,5 | 196,2 | 92,2 |

Consecuencia directa en la latencia entre tokens de `fire`, media de p50 por frame:

- `token_gap_p50`: 4.096–8.192 µs en base y reuse, **32.768 µs** en coop
- `token_gap_p99`: 131.072 µs en base y reuse, **262.144 µs** en coop
- `max_gap_us` p50: unos 154–160 ms en base, 186–265 ms en coop

El intercambio es granularidad por throughput. El contenido llega antes en conjunto y con mucha menos CPU, pero cada token individual tarda más en aparecer. El mecanismo exacto que produce la agrupación no está establecido por medición: el código usa el mismo `Incoming` de hyper que el camino legacy, sin buffer de acumulación explícito

## o5: reutilización request-scoped de auth a routing

Lo que sí se demuestra, con evidencia SQL dura del propio trace (`routing_sql_evidence` en `analysis.json`): las dos consultas duplicadas de resolución pasan de 2 a 1 por request en todos los escenarios

```
SELECT active, rate_limit_scope FROM providers WHERE id = ?1        base 1600 → reuse 800
SELECT id, provider_id, model_id, ... FROM models WHERE model_id = ?1 ...  base 1600 → reuse 800
```

Con 800 peticiones por corrida. El ahorro total de sentencias baja entre un 6,3 % y un 8,1 % y es significativo en `short`, `fire`, `open`, `slow` y `cancel`. No lo es en `single`

Instrucciones retiradas por el proxy, IC95 del contraste emparejado:

- `single` −2,30 % [−3,53, −1,07] significativo
- `short` −7,23 % [−7,56, −6,89] significativo
- `open` −4,36 % [−7,02, −1,70] significativo
- `fire` −0,33 % [−1,17, 0,52] no significativo
- `slow` −1,20 % [−3,74, 1,34] no significativo
- `cancel` −0,86 % [−9,97, 8,25] no significativo

Throughput hasta cierre, IC95:

- `short` +9,06 % [−2,44, 20,57] no significativo
- `fire` +0,90 % [−6,68, 8,48] no significativo
- `open` +0,05 % [−0,10, 0,20] no significativo
- `single` +0,12 % [−157,11, 157,34] no significativo, con desviación de base de ±210.618 tokens/s
- `cancel` −7,32 % [−19,68, 5,04] no significativo
- `slow` −7,47 % [−14,27, −0,68] **regresión significativa**

Conclusión de o5: elimina trabajo real y medible, consultas SQL e instrucciones, sin traducción en throughput. La regresión de `slow` es pequeña, su límite inferior queda a 0,68 puntos del cero y convive con unas 200 pruebas sin corrección de comparaciones múltiples, así que puede ser azar. No se encontró mecanismo causal para ella. TTFT de `short` mejora un 13,2 % [−20,98, −5,41]

El aislamiento se mantiene: `PreResolvedRoutingPlan` viaja en `Request::extensions`, sin caché global, sin congelar salud, cooldowns, cuotas ni OAuth. Los overrides efectivos de `x-openproxy-combo`, `x-openproxy-provider` y `x-openproxy-account` producen mismatch y derivan a `resolve_routing_plan`. Un provider vacío se normaliza a `None` y un account no numérico no produce `AccountId`, igual que en el routing previo. Cuando la clave tiene restricciones que requieren filtrar targets y el plan es combo, la validación corre en `run_blocking`. Las comprobaciones puramente en memoria conservan el fast path de Tokio

## o8: transporte HTTP/1 cooperativo

Throughput hasta cierre, IC95 del contraste B→C:

- `fire` **+93,92 %** [74,12, 113,72] significativo
- `short` **+21,27 %** [14,63, 27,90] significativo
- `slow` **+492,45 %** [409,61, 575,30] significativo, ver advertencia
- `open` +0,24 % [−0,04, 0,51] no significativo, limitado por llegadas
- `cancel` +5,11 % [−10,15, 20,38] no significativo
- `single` +6,97 % [−1,73, 15,67] no significativo

CPU del proxy y trabajo retirado en `fire`:

- CPU proxy **−52,03 %** [−54,65, −49,40], de 18,98 s a 9,10 s
- instrucciones **−29,32 %** [−29,70, −28,95]
- `short` CPU −20,08 %, instrucciones −14,50 %
- `open` CPU −7,33 % no significativo, instrucciones −12,07 % [−14,63, −9,52] significativo
- `single` instrucciones −25,09 % [−30,75, −19,43] significativo

### Advertencia sobre `slow`

`slow` aplica una pausa de 20 ms del cliente antes de pedir cada frame. El tiempo total depende del número de frames, no de la capacidad del proxy. Con 6,4× menos frames el cliente duerme 6,4× menos: 216 frames × 20 ms ≈ 4,3 s frente a 33 frames × 20 ms ≈ 0,7 s, contra 5,1–5,4 s y 0,92–1,02 s medidos. **La mejora de `slow` es un artefacto del arnés y no debe leerse como ganancia de capacidad**

### Advertencia sobre `cancel`

300 cancelaciones planeadas por corrida, todas correctas, tokens parciales entre 50.410 y 53.360, 0 fallos, ninguna cancelación contada como validada. El transporte reduce los frames de 191 a 92 pero no mejora el throughput ni las instrucciones. Sin señal en ninguna dirección

### TTFT

- `short` empeora **+8,72 %** [4,40, 13,04]
- `open` mejora **−12,88 %** [−14,36, −11,39]
- `fire` +5,63 % no significativo
- `slow` +8,23 % no significativo

## Interpretación

El transporte cooperativo es una mejora real y grande de eficiencia con un coste de granularidad. `fire`, el escenario sin pacing artificial, gana 1,94× de throughput con la mitad de CPU del proxy y los mismos tokens validados. Ese es el número que respalda el cambio

No es una mejora universal de latencia percibida: en esta matriz la entrega llega en grupos más grandes, aunque la respuesta completa antes. Durante el experimento era opt-in. La integración posterior lo activa solo para HTTP/1 plano directo, con rollback mediante `OPENPROXY_UPSTREAM_TRANSPORT=legacy`. El camino legacy sigue siendo el de HTTPS, proxies y actualizaciones de protocolo

o5 es una limpieza de trabajo duplicado con efecto modesto y honesto: menos SQL, menos instrucciones, sin throughput. El valor está en quitar lecturas redundantes del camino caliente, no en capacidad

## Límites

- n=3 por escenario y variante. Intervalos anchos, sin corrección de comparaciones múltiples entre 6 escenarios × 2 contrastes × 17 métricas
- La afinidad no elimina el ruido del host. CPU incluye drenaje
- Tokens sintéticos, no tokenizer ni inferencia
- Los frames HTTP medidos son los que ve el cliente del proxy, no los eventos SSE, que el parser cuenta aparte
- El mecanismo que agrupa frames en el transporte cooperativo no está establecido por medición
- `open` impone 100 req/s: su throughput no puede mejorar por diseño
- No se midió el camino cooperativo con `recording` activado, así que estas cifras no dicen nada del `ResponseAccumulator`
