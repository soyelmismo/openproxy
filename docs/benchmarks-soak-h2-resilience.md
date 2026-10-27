# Soak HTTPS/h2 bajo carga sostenida

Soak de resiliencia sobre el binario congelado del A/B TLS/h2 (mismo SHA256,
`OPENPROXY_IO_URING` sin activar salvo en las variantes `uring`). Busca el modo
de fallo detrás de la ráfaga de cortes del r13 sin atribuir causa no
reproducida.

## Método

24 corridas: 6 ciclos de 4 escenarios (`paced5` 400 streams, `fire` 800 streams
con 10.000 tokens, `cancel` 200 streams con 50% de desconexión planificada,
`light` 50 streams con ritmo de 20 ms). Las variantes `tokio` y `uring` se
alternan por ciclo. Cada corrida es un proceso nuevo con `--upstream-tls`,
`--perf-fsync` y afinidad por defecto (proxy en CPUs 2,3). La carga del host no
está aislada.

Binario: `f4e55a7489aebd219f84608599f6b8c4383447227b8af7e8942782f15187becf`.

## Resultado

23 de 24 corridas limpias: 0 fallos, 600 cancelaciones planeadas observadas
(100 por corrida de `cancel`), 0 peticiones con más de un intento, 0 errno
registrado en las operaciones de apagado.

Una corrida, `c5-fire-tokio` (ciclo 5, escenario firehose con backend Tokio),
falló con 723 de 800 peticiones:

```
request N failed: Some("inline SSE error")
Error: 723 requests failed validation/admission; report written
```

## Diagnóstico por operación

El `proxy.stderr` de esa corrida muestra la cadena completa:

```
upstream error: status=502 provider=mock-upstream
body=upstream connection error: client error (SendRequest):
http2 error: connection error received:
detected excessive load generating behavior (b"too_many_resets")
attempts=3
```

El `mock.stderr` de la misma corrida muestra el origen:

```
h2::proto::streams::recv: recv_reset;
remotely-reset pending-accept streams reached limit (20)
```

Los logs muestran resets de streams h2, el umbral de 20 resets pendientes de
aceptar en el mock y un GOAWAY `too_many_resets` recibido por el proxy. Las filas
fallidas registran 502 y hasta tres intentos. No se capturó el motivo inicial de
cada reset ni la identidad de conexión de cada intento: atribuirlos a timeouts,
contención o reutilización de una conexión concreta sigue sin verificar.

La búsqueda literal de `GOAWAY`, `too_many_resets` y `enhance your calm` en
`crates/` no encontró coincidencias. Esto solo limita lo encontrado en el código
propio, no demuestra cómo Hyper y h2 gestionan o renuevan esas conexiones.

## Por qué no se atribuye a la ráfaga del r13

Tres corridas aisladas del mismo escenario, mismo binario y host sin carga
concurrente:

| Reproducción | Validadas | Fallos | tokens/s |
|---|---|---|---|
| fire-tokio 1 | 800/800 | 0 | 1.463.340 |
| fire-tokio 2 | 800/800 | 0 | 1.423.839 |
| fire-tokio 3 | 800/800 | 0 | 1.420.960 |

El fallo no reproduce de forma aislada. Ocurrió una vez en seis corridas de
firehose durante el soak, con el host compartido y agentes delegados activos
hasta el minuto anterior. La cadena causal (GOAWAY por exceso de resets) está
medida en los logs de esa corrida, pero las condiciones que la disparan no.

## Conclusión

- El soak no reproduce la ráfaga de cortes del r13: 0 fallos en 23 de 24
  corridas y 600 cancelaciones todas planificadas.
- Existe un modo de fallo observado: GOAWAY h2 `too_many_resets`
  contra el upstream, con intentos que no recuperaron las peticiones de esa
  corrida. Se observó una vez bajo contención del host. No se verificó si cada
  intento abrió una conexión nueva.
- Ese modo de fallo es una candidata para la ráfaga original, no una causa
  demostrada. Confirmarlo exige una prueba dirigida que fuerce la acumulación
  de resets (muchas cancelaciones de cliente contra un upstream lento) con el
  host aislado.
