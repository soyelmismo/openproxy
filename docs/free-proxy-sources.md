# Índice de fuentes gratuitas de proxies para OpenProxy

Solo fuentes públicas sin cuenta, tarjeta ni plan de pago. El índice prioriza SOCKS5 y mide tres cosas por separado: frescura del archivo, endpoints que aporta y coincidencia con proxies que OpenProxy ya había marcado `alive`. Ninguna garantiza IP fija, disponibilidad sostenida ni compatibilidad con Antseed.

## Fuentes recomendadas

| Fuente | Feed gratuito | Uso recomendado |
| --- | --- | --- |
| ProxyScraper | [SOCKS5 TXT](https://raw.githubusercontent.com/ProxyScraper/ProxyScraper/main/socks5.txt), [repositorio](https://github.com/ProxyScraper/ProxyScraper) | Fuente primaria candidata por cobertura. El workflow visible programa una ejecución por hora y delega la recolección a un script no público. El README anuncia 30 min. No tomar esa cadencia como garantía ni ejecutar el updater externo. |
| IPLocate | [SOCKS5 TXT](https://raw.githubusercontent.com/iplocate/free-proxy-list/main/protocols/socks5.txt) | Fuente primaria candidata. Ya registrada en `builtin_github`. El proyecto anuncia 30 min. |
| monosans | [SOCKS5 TXT](https://raw.githubusercontent.com/monosans/proxy-list/main/proxies/socks5.txt), [repositorio](https://github.com/monosans/proxy-list) | Lista pequeña para complementar cobertura. El proyecto anuncia revalidación cada hora y publica metadatos de comprobación en JSON. |
| ProxyScrape v4 | [API pública TXT](https://api.proxyscrape.com/v4/free-proxy-list/get?request=get_proxies&proxy_format=protocolipport&format=text&limit=15000) | Fuente directa, distinta de **ProxyScraper**. Formato `esquema://host:puerto`. Si se importa, conservar el esquema de cada línea y no asumir que `limit=15000` garantiza esa cantidad. |
| ProxyScrape mirror | [SOCKS5 TXT](https://raw.githubusercontent.com/ProxyScrape/free-proxy-list/main/proxies/protocols/socks5/data.txt), [mirror oficial](https://github.com/ProxyScrape/free-proxy-list) | Respaldo de la API o complemento si aporta endpoints útiles. El proyecto anuncia 5 min. OpenProxy ya tiene un built-in de ProxyScrape que usa el JSON del mirror vía jsDelivr. No contar API y mirror como publishers independientes. |
| VPSLab | [SOCKS5 TXT](https://raw.githubusercontent.com/VPSLabCloud/VPSLab-Free-Proxy-List/main/socks5_all.txt) | Fuente secundaria. El proyecto anuncia 15 min. En la muestra de las 06:35 compartía 327 endpoints con el mirror de ProxyScrape. |
| Proxifly | [SOCKS5 TXT](https://raw.githubusercontent.com/proxifly/free-proxy-list/main/proxies/protocols/socks5/data.txt) | Fuente de volumen. Muestrear antes de cederle todo el presupuesto del checker. El TXT no es el mismo endpoint que el built-in de Proxifly. |

## Evidencia: 19 de noviembre de 2026

Muestra de las 06:35 UTC: downloads directos de los TXT, sin conectar a los proxies de las listas. Solo se contaron IPs públicas unicast con puerto válido, deduplicadas por `(host, port)`. El inventario SQLite, consultado en modo solo lectura, tenía 4.395 SOCKS5, de los que 34 estaban `alive` con validación en los 40 minutos anteriores.

| Fuente | SOCKS5 únicos | No estaban en SQLite | Coincidencias con `alive` reciente | Último cambio del archivo observado (UTC) |
| --- | ---: | ---: | ---: | --- |
| ProxyScraper | 2.216 | 447 | 23 | 19 nov 06:04 |
| IPLocate | 748 | 441 | 21 | 19 nov 06:30 |
| monosans | 104 | 44 | 4 | 19 nov 06:17 |
| ProxyScrape mirror | 1.091 | 714 | 14 | 19 nov 06:30 |
| VPSLab | 1.107 | 701 | 20 | 19 nov 06:18 |
| Proxifly | 5.537 | 4.766 | 28 | 19 nov 02:00 |

**ProxyScrape v4, toma posterior:** HTTP 200 sin credenciales. Devolvió 1.563 endpoints únicos: 844 SOCKS5, 28 SOCKS4 y 691 HTTP, sin filas rechazadas. De los SOCKS5, 601 no estaban en el mirror descargado en esa misma toma y 555 no estaban en SQLite. Ocho coincidieron con `alive` reciente sobre un inventario posterior de 4.689 SOCKS5 y 33 `alive` recientes. No comparar esta toma como si usara el mismo inventario de las 06:35.

Una coincidencia con `alive` es un cruce con salud ya registrada hacia el destino de prueba de OpenProxy. No prueba que el publisher detectara esos endpoints ni mide la tasa de éxito de su lista. Un endpoint ausente de SQLite es un candidato, no un proxy comprobado. El último cambio es del **archivo** y no un `pushed_at` del repositorio ni una prueba de cadencia sostenida.

## No priorizar

- [hookzof](https://raw.githubusercontent.com/hookzof/socks5_list/master/proxy.txt): exactamente el mismo conjunto de 5.537 endpoints que Proxifly en esa muestra, aunque no el mismo archivo byte a byte.
- [jetkai](https://raw.githubusercontent.com/jetkai/proxy-list/main/online-proxies/txt/proxies-socks5.txt): último cambio del TXT en abril de 2023. Cero coincidencias con `alive` reciente en esta toma.
- [mmpx12](https://raw.githubusercontent.com/mmpx12/proxy-list/master/socks5.txt): último cambio del TXT el 20 de octubre, aunque el repositorio tenía actividad más reciente. Cero coincidencias con `alive` reciente y una línea rechazada. Revisar si el TXT vuelve a refrescarse.
- [roosterkid SOCKS5](https://raw.githubusercontent.com/roosterkid/openproxylist/main/SOCKS5_RAW.txt): 7 endpoints, todos ya inventariados y ninguno con `alive` reciente en esta toma.
- [Tor](https://spec.torproject.org/socks-extensions.html): ofrece SOCKS5 local, pero no es un feed de proxies ni una salida con IP garantizada. No sustituye una salida estable para Antseed.

## Cómo mantener el índice

1. Descargar cada feed sin credenciales. Registrar hora UTC, HTTP, tamaño, SHA-256 y último commit del **archivo**. Alertar por vacío, cambio de formato, caída fuerte de volumen o archivo desactualizado. Un HTTP 200 no basta.
2. Contar por protocolo y deduplicar por `(host, port)`. Rechazar endpoints no públicos. Comparar conjuntos, no hashes, para detectar listas equivalentes con distinta serialización.
3. Comparar contra la misma instantánea SQLite en modo solo lectura. Registrar nuevos candidatos, coincidencias con `alive` reciente y solapamiento entre feeds. No atribuir a cada fuente todo lo que comparten.
4. Antes de promover una fuente a primaria, medir en varias ventanas cuántos candidatos distintos pasan la validación y sobreviven a repeticiones, su latencia p95 y su contribución después de deduplicar. Para Antseed, medir aparte hacia un destino autorizado que represente sus conexiones largas. Mantener las listas públicas fuera de flujos sensibles hasta validarlas.

El registro del importador incluye tres built-ins independientes: `builtin_proxyscraper` y `builtin_vpslab` (solo SOCKS5) y `builtin_proxyscrape_v4` (conserva el esquema explícito). El mirror oficial conserva su built-in previo. El próximo sync tras desplegar el binario registra las entradas nuevas y deduplica candidatos en la base de datos, sin marcarlos `alive` hasta que pasen el checker.
