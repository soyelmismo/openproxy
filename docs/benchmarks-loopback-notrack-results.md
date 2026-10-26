# A/B de OPENPROXY_LOOPBACK_NOTRACK

Fecha: 2026-10-26

## Corrección del arnés

`bench_token_stress` usaba el router real pero no el startup que instala notrack. Se conectó al mismo `LoopbackNotrack::install` del servicio y se conserva el guard durante la vida del listener. En el arnés, una instalación fallida aborta en lugar de continuar con tracking normal. El JSON conserva `persistence.network`, confirma el borrado de la tabla al finalizar y separa el tiempo de limpieza del cierre HTTP y del drain de usage

El anterior +6-8% comparaba entornos de firewall distintos. No mide el efecto de esta opción y queda retirado como atribución a notrack

## Método

- Mismo binario release en las 16 corridas, con `benchmark-sql` y mimalloc
- SHA-256: `a140379009d139255d62be692bea8b276ef96bde93687a64b3cf4a5bbd05bca7`
- Dos series independientes, cuatro pares por serie, orden OFF/ON y ON/OFF alternado
- 800 streams × 10.000 tokens sintéticos, 5 tokens por evento upstream
- Coalescer: 1 ms en todas las corridas
- Afinidad: proxy CPUs 2,3 con dos workers, mock CPU 0 y cliente CPU 1
- SQLite nuevo por corrida, `synchronous=full`, mismos límites de admisión y pipeline
- Namespace de red privado, `lo` activo, mismo firewall base en todas las corridas
- Firewall fixture con filtros INPUT/OUTPUT, aceptación explícita de loopback y reglas `ct state` para mantener conntrack activo
- No se clonó el firewall del host ni se modificó el firewall de producción
- Única diferencia funcional de red: las dos reglas notrack sobre el puerto efímero del proxy
- CPU del proxy: ticks user + system desde después del warmup hasta fin del cliente, sin incluir limpieza notrack

La afinidad no aísla de la carga del host. Los tokens son unidades sintéticas ` tok`, no inferencia ni throughput de un tokenizer

## Resultado medido

16 corridas completas, 128.000.000 de tokens validados byte por byte, cero peticiones fallidas

Medias y desviación estándar entre ocho corridas por variante:

- OFF: 462.222 ± 18.950 tokens/s
- ON: 455.875 ± 21.618 tokens/s
- Diferencia de medias: −1,37%, menor que la dispersión de las corridas
- CPU total del proxy OFF: 20,260 ± 0,216 s
- CPU total del proxy ON: 20,288 ± 0,331 s, +0,14%
- CPU system del proxy OFF: 5,805 ± 0,175 s
- CPU system del proxy ON: 5,633 ± 0,329 s, −2,97% observado, no una mejora total demostrada
- RSS pico medio: 620,0 MiB OFF frente a 616,3 MiB ON, dentro de la dispersión
- Media de los p99 por corrida: 17,216 s OFF frente a 17,481 s ON, sin mejora estable de latencia

La primera serie dio −5,39% de throughput medio con ON. La segunda dio +2,84%. El cambio de signo no sostiene una mejora ni una regresión causal de throughput

**Conclusión: no hay ganancia de rendimiento reproducible con este workload. Mantener apagado por defecto**

## Validación funcional

- Durante las 16 cargas: OFF genera 800 flujos cliente→proxy en conntrack, ON genera 0
- Los flujos proxy→upstream permanecen rastreados en ambas variantes
- En las ocho corridas ON la tabla owner se elimina al cerrar el proxy y el ruleset vuelve a contener solo la tabla base
- Integridad del cliente: texto exacto, usage, finish_reason y un único `[DONE]` validados por el arnés
- Control negativo sin `CAP_NET_ADMIN`: arnés termina con exit 1 y no deja tabla notrack, evitando un A/B falso por fallback silencioso

Estos resultados verifican el arnés y la opción en un fixture local. No prueban una ganancia en el firewall real del host, clientes remotos, upstreams de Internet ni el despliegue de producción

## Artefactos originales

Base: `/root/.hermes/cache/scratch/openproxy-perf/`

- `notrack-ab-20261026-v2/`: primera serie, ocho reportes, flows, reglas y resúmenes
- `notrack-ab-20261026-confirm/`: segunda serie, ocho reportes, flows, reglas y resúmenes
- `r3/notrack_ab.py`: script de reproducción, requiere `unshare -n`

Los artefactos scratch caducan. El informe y los cambios de código quedan en el árbol del proyecto, sin commit, push ni deploy en esta tarea
