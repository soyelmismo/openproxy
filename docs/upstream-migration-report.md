# Reporte de investigación: migración del call-site del chat a `UpstreamClient::call()`

**Tarea:** READ-ONLY investigation. ¿La migración requiere re-escribir el código downstream o se puede hacer con un adapter delgado?

**Resumen ejecutivo:** La migración es viable con un **adapter delgado** sólo para la **construcción de requests** (de `RequestBuilder` a `UpstreamRequest`) y para la **lectura de body** no-streaming (`bytes()` → `collect()`). El path streaming y la semántica del bucle de cancelación son **estructuralmente diferentes** y exigen re-escribir, no adaptar.

---

## 1. Líneas a cambiar en el call-site non-streaming (1371-1670)

**Rango total:** 300 líneas (`pipeline.rs:1371` → `pipeline.rs:1670`).

**Construcción del request** (8 líneas, 1371-1379): trivialmente traducible.

```rust
// 1371-1379
let mut request_builder = self
    .config
    .http_client
    .post(url)
    .timeout(resolved_timeouts.total);
for (k, v) in headers {
    request_builder = request_builder.header(k.as_str(), v.as_str());
}
request_builder = request_builder.json(body_value_param);
```

Equivalente directo con `UpstreamRequest`:
- `UpstreamRequest { method: POST, url, headers, body: Some(serde_json::to_vec(body)?) }`
- El `timeout(total)` se reemplaza por `TimeoutProfile::Custom(ResolvedTimeouts { total_ms: ..., ..SYSTEM_DEFAULTS })` o `TimeoutProfile::Chat.resolve()`.
- El `.json()` setter se reemplaza serializando el `serde_json::Value` a `Bytes` por mano (3-4 líneas extras).

**Send + race + cancel** (52 líneas, 1412-1498): el `tokio::select!` con `request_builder.send()` dentro de `tokio::time::timeout(connect, …)` se reemplaza por `client.call(req, profile, cancel).await` (1 línea). El `SendAbortReason` enum y los arms de match (1497-1548) **sobran** porque `UpstreamError` ya distingue `Timeout(UpstreamPhase::Headers)`, `Timeout(UpstreamPhase::Body)`, `Cancel`, `Connection`, etc. — hay que mapear cada variant a `CoreError::UpstreamTimeout { phase, ms }` / `UpstreamConnection` / `ClientDisconnected` (no es trivial: 1 variant de `reqwest::Error::is_timeout()` se reemplaza por 4 variants de `UpstreamError`).

**`response.status()` y `response.headers()`** (13 líneas, 1550-1562): traducible 1-a-1 (`response.status.as_u16()`, `response.headers.iter()`).

**`reqwest::Response::bytes(response).await`** (línea 1581): traducible a `response.collect().await` (1 línea). El error type cambia: `reqwest::Error` → `UpstreamError::Decode(_) | UpstreamError::Http(_) | UpstreamError::Cancel`.

**`response.text()` y `serde_json::from_slice` (1596-1670)**: `response.text()` **no existe** en `UpstreamResponse` (ver §3). Hay que agregar un adapter o usar `response.collect().await` + `String::from_utf8_lossy`. El resto (parse a `OpenAIResponse` / `AnthropicResponse` / `GeminiResponse`) es independiente del cliente HTTP — no se toca.

**Total call-sites de `reqwest::Response` en 1371-1670:** 6 ocurrencias
| Línea | Método | Traducción |
|---|---|---|
| 1424 | `reqwest::Response` (tipo) | `UpstreamResponse` |
| 1550 | `.status()` | `.status` (field) |
| 1555 | `.headers()` | `.headers` (field) |
| 1581 | `reqwest::Response::bytes` | `UpstreamResponse::collect()` |
| 1597 (implícito) | bytes → `String::from_utf8_lossy` | `collect()` + `from_utf8_lossy` |
| (no más) | — | — |

**Líneas a re-escribir:** ~70 (el `tokio::select!` de send + el `SendAbortReason` se eliminan; se inserta el `match` sobre `UpstreamError`). El resto se traduce mecánicamente.

---

## 2. Líneas a cambiar en el path streaming (1709-1869)

**Rango total:** 161 líneas (`pipeline.rs:1709` → `pipeline.rs:1869`).

**Función signature** (línea 1722): el parámetro `request_builder: reqwest::RequestBuilder` debe cambiar a `upstream_request: UpstreamRequest` (o eliminarse y construirse adentro).

**Send + race + cancel** (28 líneas, 1736-1753): misma traducción que en non-streaming. El `tokio::select!` con `request_builder.send()` se reduce a `client.call(req, profile, cancel).await` (1 línea) y el `SendAbortReason` enum desaparece.

**Status + text** (líneas 1835-1837): `response.status().as_u16()` → `response.status.as_u16()`; `response.text().await` → **no existe** en `UpstreamResponse` — hay que hacer `response.collect().await` y luego `String::from_utf8_lossy`.

**`response.bytes_stream()`** (línea 1869): **no existe** en `UpstreamResponse`. El equivalente es `response.body` (un `UpstreamBodyStream`) cuyo método para iterar es `next_chunk().await` (no implementa `futures::Stream`).

**El bucle `while let Some(chunk_result) = { tokio::select! { … stream.next() … } }` (1876-2002)**: aquí está el problema real. El `tokio::select!` está construido sobre `Stream::next()` (un método de `futures::Stream`), no sobre un future explícito. Como `UpstreamBodyStream` **no implementa `Stream`**, hay que re-escribir este `select!`. La forma equivalente es:

```rust
// no es válido usar stream.next() si no implementa Stream
// hay que hacer el select! con response.body.next_chunk() explícitamente
```

Eso significa re-escribir el `while let` completo (líneas 1876-2002, ~126 líneas) reemplazando `stream.next()` por un select! explícito entre `body.next_chunk()` y `cancel_rx_chunk.changed()`. La forma del bucle es la misma — el cuerpo del match arms es idéntico — pero **el constructo de control de flujo cambia**.

**Total call-sites de `reqwest::Response` en 1709-1869:** 4 ocurrencias
| Línea | Método | Traducción |
|---|---|---|
| 1722 | `reqwest::RequestBuilder` (tipo) | `UpstreamRequest` |
| 1736 | `reqwest::Response` (tipo) | `UpstreamResponse` |
| 1835 | `.status()` | `.status` (field) |
| 1837 | `.text()` | `collect()` + `from_utf8_lossy` |
| 1869 | `.bytes_stream()` | `body.next_chunk()` (sin `Stream`) |

**Líneas a re-escribir:** ~150 (todo el streaming path). El bucle SSE con cancelación es lo único que cambia estructuralmente.

---

## 3. Métodos de `reqwest::Response` que NO se traducen trivialmente a `UpstreamResponse`

Lo que expone `UpstreamResponse` (`crates/openproxy-core/src/upstream/response.rs:20-36`):

```rust
pub struct UpstreamResponse {
    pub status: StatusCode,        // field, no método
    pub headers: HeaderMap,        // field, no método
    pub body: UpstreamBodyStream,  // field, no método
}
impl UpstreamResponse {
    pub async fn collect(self) -> UpstreamResult<Bytes>  // sólo este método público
}
```

Y `UpstreamBodyStream` (`response.rs:59-171`) expone:
- `from_hyper(...)` (constructor interno)
- `empty(...)` (constructor interno)
- `collect_all()` (consume todo → `Bytes`)
- `next_chunk()` (un chunk a la vez)
- `next_chunk_boxed()` (idéntico, boxed)
- `Debug`

**Lo que falta:**

| Método reqwest | ¿Existe en UpstreamResponse? | Notas |
|---|---|---|
| `.status()` | **No** — es `response.status` (field público) | Trivial: quitar paréntesis |
| `.headers()` | **No** — es `response.headers` (field público) | Trivial: quitar paréntesis |
| `.text().await` | **No existe** | Hay que hacer `collect()` + `String::from_utf8_lossy` |
| `.bytes().await` | **No existe** como método en `UpstreamResponse` | Hay que hacer `response.collect().await` (existe, pero el nombre es diferente) |
| `.json::<T>().await` | **No existe** | Hay que hacer `collect()` + `serde_json::from_slice` |
| `.bytes_stream()` | **No existe** | `UpstreamBodyStream` no implementa `Stream` — sólo `next_chunk()` (futures no son un `Stream` trait) |
| `.error_for_status()` | **No existe** (y ver §6) | El cliente decide no hacer mapping; el caller chequea `status` |
| `.url()` | **No existe** | El caller no lo necesita (tienen el URL original) |
| `.remote_addr()` | **No existe** | N/A |
| `.cookies()` | **No existe** | N/A |

**Conclusión:** ningún método de `UpstreamResponse` rompe el contrato — son **traducibles** — pero **ninguno es 1-a-1 con `reqwest::Response`**. `text()`, `bytes()`, `json()` y `bytes_stream()` requieren adaptación. Ver §10.

---

## 4. ¿`executor_kiro` y `executor_antigravity` usan `self.config.http_client`?

**Sí.** Confirmado:

- `executor_kiro.rs:378` — `pub async fn execute_kiro(http_client: &reqwest::Client, …)`. Cuerpo: `http_client.post(&url).bearer_auth(token).header(...).body(body_json).send().await` (líneas 391-399), luego `resp.status()`, `resp.bytes().await` (líneas 401-405).
- `executor_antigravity.rs:301` — `pub async fn execute_antigravity(http_client: &reqwest::Client, …)`. Cuerpo: `http_client.post(url).header(...).body(envelope_json).send().await` (líneas 315-323), luego `response.status().as_u16()`, `response.text().await` (líneas 325-340).

Ambos son invocado desde `pipeline.rs:940-943` y `pipeline.rs:951-954`, recibiendo `&self.config.http_client`.

**Hay que migrarlos también** — usan los mismos métodos `reqwest` problemáticos. Pero **no son parte del call-site del chat**; son rutas alternativas (Kiro/Antigravity van por su propio path, no por el `dispatch_upstream_request` del chat). Estrictamente, la pregunta es sobre el call-site del chat, así que estos están **fuera del scope**, pero **bloquean** una migración total del crate (ver §5).

---

## 5. Otros call-sites de `http_client` o `reqwest::Client` en el crate

Búsqueda exhaustiva en `crates/openproxy-core/src/` y `crates/openproxy-server/src/`. El crate `core` tiene **7 archivos** con `reqwest::Client` o `http_client` y **3 archivos** en `server`:

### `openproxy-core/src/`
| Archivo | Líneas | Patrón |
|---|---|---|
| `pipeline.rs` | 41, 77, 940, 941, 951, 952, 1350, 1372, 1373, 1390, 1424, 1581, 1722, 1736, 2524, 4174, 4214 | Chat dispatch (1371-1869), executor_kiro call site (940-943), executor_antigravity call site (951-954), tests (2524, 4174, 4214) |
| `executor_kiro.rs` | 378, 391-405 | Función completa — usa `post`/`send`/`bytes` |
| `executor_antigravity.rs` | 301, 315-340 | Función completa — usa `post`/`send`/`status`/`text` |
| `oauth_kiro.rs` | 162, 172, 203, 253, 260, 293, 300, 419 | `register_client` y `device_auth_resp` |
| `oauth_antigravity.rs` | 115, 184, 233, 246, 338, 397 | Token refresh, JSON parsing |
| `quota.rs` | 74, 109, 357, 368, 392, 455, 578 | Quota check — usa `.json::<T>()` |
| `adapters.rs` | 123, 205, 640, 814, 967, 1134, 1216, 1783, 1798, 2083, 2098, 1810, 2110 | Provider adapter trait (`fetch_models`) y dos implementaciones que usan `.json::<serde_json::Value>()` |
| `admin.rs` | 340, 772, 799, 1206, 1214, 1236 | Admin endpoints |

### `openproxy-server/src/`
| Archivo | Patrón |
|---|---|
| `state.rs` | `http_client: Arc<RwLock<reqwest::Client>>` (líneas 44, 169, 278, 324, 379-380, 456) — la fuente |
| `handlers/admin.rs` | 18+ usos de `s.http_client()` para OAuth flows (refresh_token, device code, poll, etc.) |
| `handlers/chat.rs` | Línea 209 — pasa el client al `PipelineConfig` |

**Conclusión:** **el reqwest client vive en ~12 archivos** y tiene ~50+ call-sites. Una migración que toque sólo el call-site del chat es **el primer paso de una serie de ~10 migraciones adicionales**. Los demás call-sites están fuera del scope de la pregunta pero son el techo de la migración total.

---

## 6. ¿`reqwest::Response::error_for_status()` se usa en el pipeline?

**No.** Búsqueda exhaustiva: 0 ocurrencias en `pipeline.rs`. Verificado en líneas 1371-1670 y 1709-1869.

`error_for_status()` **sí** se usa implícitamente vía `response.status()` + match manual (líneas 1550, 1596, 1835-1837) — el pipeline **no** se basa en `error_for_status()`. La lógica de "es 2xx o no" está duplicada en cada call site. Esto es bueno para la migración: no hay que replicar el comportamiento de `error_for_status()`.

---

## 7. ¿`reqwest::Response::json::<T>()` se usa?

**No en el pipeline.** Búsqueda exhaustiva: 0 ocurrencias en `pipeline.rs`.

El body se parsea **manualmente** con `serde_json::from_slice` (línea 1613) o `serde_json::from_value` (línea 1632/1645/1660) después de haberlo leído como bytes con `reqwest::Response::bytes(response).await` (línea 1581). Esto es **mejor para la migración** — no hay que replicar `json::<T>()`.

`json::<T>()` **sí** se usa en otros archivos:
- `adapters.rs:1810` y `adapters.rs:2110` — `fetch_models` para Antigravity
- `quota.rs:392` — Quota check
- `oauth_kiro.rs:319`, `oauth_antigravity.rs:157`, `:211`, `:246` — OAuth token parsing

**Fuera del scope del call-site del chat**, pero es la "molestia principal" en esos otros call-sites.

---

## 8. ¿`response.bytes_stream()` se usa?

**Sí, una vez.** `pipeline.rs:1869` — `let mut stream = response.bytes_stream();`.

El `stream` se consume en el `while let Some(chunk_result) = { tokio::select! { … stream.next() … } }` (líneas 1876-1892). La firma del método:

```rust
// reqwest
fn bytes_stream(self) -> impl Stream<Item = Result<Bytes, Error>>

// upstream (no existe)
```

`UpstreamBodyStream` **no implementa `futures::Stream`**. Expone `next_chunk() -> UpstreamResult<Option<Bytes>>` (un future explícito). Esto significa que **el `tokio::select!` con `stream.next()` no se compila** — hay que re-escribirlo con un select! explícito sobre el future.

---

## 9. ¿Grosor del path streaming? ¿`bytes_stream()` itera `Bytes` igual que `UpstreamBodyStream`?

**Grosor:** 161 líneas (1709-1869) para el wrapper completo; **~127 líneas** (1876-2002) son el bucle SSE + parseo de líneas. Es el doble del path non-streaming y el más denso del archivo.

**Shape de los items:**

| Aspecto | `reqwest::bytes_stream()` | `UpstreamBodyStream::next_chunk()` |
|---|---|---|
| Item type | `Result<Bytes, reqwest::Error>` | `UpstreamResult<Option<Bytes>>` |
| Cierre | `None` cuando termina | `Ok(None)` cuando termina |
| Cancelación | Drop del stream cierra la conexión | Internamente consulta `CancellationToken` |
| Body limit | Sin cap por defecto | 32 MiB hard cap (en `client.rs:375`) |
| Trait | `impl futures::Stream<Item=…>` | **No implementa `Stream`** — sólo `async fn next_chunk()` |

**Diferencia crítica:** `UpstreamBodyStream` **no implementa `Stream`**. Eso significa que `stream.next()` (el método del trait) no compila. El caller debe consumirlo como `body.next_chunk().await` y armar su propio `select!` para cancelación. Esto **rompe la simetría** entre el código streaming y el non-streaming — el non-streaming se reemplaza por `collect()` (que sí existe), el streaming requiere re-escribir el bucle.

**¿Iteración equivalente?** Sí, los items son `Bytes` en ambos casos, pero el **shell** (Stream trait vs. método explícito) es diferente.

---

## 10. ¿Adapter delgado que envuelva `UpstreamResponse` con la misma firma de `reqwest::Response`?

**Sí se puede, pero no compensa.** Razonamiento:

### Lo que un adapter tendría que hacer

Para exponer `status() -> StatusCode`, `headers() -> &HeaderMap`, `text() -> …`, `bytes_stream() -> impl Stream<Item=…>`:

```rust
// Pseudo-shape (NO se escribe)
struct UpstreamResponseAdapter {
    inner: UpstreamResponse,
}
impl UpstreamResponseAdapter {
    fn status(&self) -> StatusCode { self.inner.status.clone() }  // ⚠ StatusCode: Copy
    fn headers(&self) -> &HeaderMap { &self.inner.headers }
    async fn text(self) -> Result<String, …> {
        let bytes = self.inner.collect().await?;
        Ok(String::from_utf8_lossy(&bytes).to_string())
    }
    fn bytes_stream(&mut self) -> impl Stream<Item = Result<Bytes, …>> + '_ {
        // requiere: implementar Stream sobre &mut UpstreamBodyStream
        // re-exportar next_chunk como poll_next
        // envolver UpstreamError en reqwest::Error o en un error local
    }
}
```

### Por qué no compensa

1. **`text()` y `bytes()` consumen `self` en reqwest, también en `UpstreamResponse::collect()`**. Eso es 1-a-1, fácil.

2. **`bytes_stream()` requiere implementar `futures::Stream` para `&mut UpstreamBodyStream`** — esto es ~30 líneas de boilerplate (poll_next que llama a `next_chunk`, wakeup, etc.). **El método es polimórfico** — no se puede llamar desde un `tokio::select!` con un future directo, hay que envolverlo.

3. **El `tokio::select!` con `stream.next()` igual cambia.** Aunque el adapter exponga un `Stream<Item=Result<Bytes, _>>`, **el `UpstreamBodyStream` original sigue ahí** y se está perdiendo su `CancellationToken` interno (el adapter tiene que pasar el cancel token a través de `Stream::poll_next`, lo que requiere un wrapper o Arc).

4. **El error type cambia.** `reqwest::Error` no se puede construir desde un `UpstreamError` (los enums son privados). El adapter tendría que definir un error propio `UpstreamAdapterError`, o usar `UpstreamError` directamente. En cualquier caso, todos los `match` sobre `e.is_timeout()` (líneas 1525, 1814) **se rompen** — `UpstreamError` no tiene `is_timeout()`. Hay que mapear `Timeout(UpstreamPhase::Headers)` → "es timeout", `Timeout(UpstreamPhase::Body)` → "es timeout", etc.

5. **El `SendAbortReason` enum (líneas 40-48)** sigue siendo inservible: sus variants `Reqwest(reqwest::Error) | ClientCancelled | Timeout` se traducen a `UpstreamError::Http | Connection | Tls | Cancel | Timeout(phase)`. Son **5 variants** contra 3. El `tokio::select!` con `client_disconnected.changed()` se mantiene, pero el arm interno se reescribe.

6. **El shape de `request_builder`** (línea 1722) también cambia. El adapter no puede evitar migrar la construcción del request (8 líneas, 1371-1379).

### Estimación de esfuerzo

| Approach | Líneas a tocar | Líneas a re-escribir | Líneas a borrar |
|---|---|---|---|
| **Migración directa** (sin adapter) | ~220 | ~150 (streaming bucle + select!) | ~80 (`SendAbortReason`, comments) |
| **Adapter delgado** | ~280 (adapter + cambios) | ~150 (mismo bucle) | ~80 |

El adapter **suma ~60 líneas** de boilerplate y **no elimina ninguna re-escritura** — el `tokio::select!` con `stream.next()` igual hay que tocarlo porque el `Stream` del adapter tiene que respetar el `CancellationToken` original, lo que requiere polimorfismo sobre el cancel mechanism.

### Veredicto

**No, no se puede hacer con un adapter delgado** que evite re-escribir el código downstream. Las razones son:

1. El `bytes_stream()` retorna `Result<Bytes, reqwest::Error>`, pero `UpstreamBodyStream::next_chunk()` retorna `UpstreamResult<Option<Bytes>>`. Un adapter que envuelva esto en un `Stream<Item=Result<Bytes, _>>` requiere **implementar `futures::Stream` a mano** sobre un future explícito — no es una traducción 1-línea.
2. El **`SendAbortReason` enum** es inservible porque `UpstreamError` tiene 5 variants contra las 3 de `reqwest::Error::is_timeout()` / `is_connect()`. El mapeo a `CoreError` cambia.
3. La construcción del request (`RequestBuilder` chain) **es reqwest-specific** y no se puede replicar — hay que migrar a `UpstreamRequest { method, url, headers, body }`.
4. El `tokio::select! { biased; _ = cancel.changed() => …, res = client.call(req, profile, cancel).await }` **se mantiene** como arm del select, pero el `client.call` es una llamada 1-línea contra el chain de 4 setters de `RequestBuilder` — es **menos código, no más**.

### Recomendación

**Migración directa**, sin adapter. Beneficio extra: el código resultante es **más corto** que el actual (se eliminan `SendAbortReason` y el chain de `RequestBuilder`). La parte streaming **requiere re-escribir el bucle SSE** porque `UpstreamBodyStream` no implementa `Stream` — eso es un costo fijo de ~30 líneas en el bucle (líneas 1876-1892).

**Alcance total estimado:** ~220 líneas tocadas, ~150 re-escritas, ~80 borradas, en `pipeline.rs` solamente. Si se quiere migrar el crate entero, hay ~10 archivos más con ~50 call-sites adicionales, **no triviales** (cada uno tiene su propio shape de response y manejo de errores).

---

## Apéndice: superficie de `UpstreamResponse` y `UpstreamBodyStream`

(Para referencia. Fuente: `crates/openproxy-core/src/upstream/response.rs:20-233`.)

```rust
// response.rs:22-27
pub struct UpstreamResponse {
    pub status: StatusCode,        // field
    pub headers: HeaderMap,        // field
    pub body: UpstreamBodyStream,  // field
}

// response.rs:33-35
impl UpstreamResponse {
    pub async fn collect(self) -> UpstreamResult<Bytes>  // método público único
}

// response.rs:51-57
pub struct UpstreamBodyStream {
    inner: Option<BodyStream<Limited<hyper::body::Incoming>>>,  // private
    cancel: CancellationToken,
    body_chunk_deadline: Instant,
    total_deadline: Instant,
}

// response.rs:100-107: pub async fn collect_all(mut self) -> UpstreamResult<Bytes>
// response.rs:112-170: pub async fn next_chunk(&mut self) -> UpstreamResult<Option<Bytes>>
// response.rs:227-232: pub fn next_chunk_boxed(&mut self) -> Pin<Box<dyn Future + Send>>
```

**No implementa `futures::Stream` intencionalmente** (comentario en `response.rs:220-223` — para mantener la dependencia de `futures-core::Stream` fuera de la API pública).
