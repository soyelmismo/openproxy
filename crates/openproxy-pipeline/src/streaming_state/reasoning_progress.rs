//! Instrumentación metadata-only del "hidden thinking" de Anthropic.
//!
//! Anthropic reporta razonamiento que retiene del cliente mediante
//! `content_block_delta` con `delta.type == "thinking_delta"`, `thinking`
//! vacío y un `estimated_tokens` positivo. El traductor ya convierte ese
//! evento en liveness (ver [`crate::sse::UpstreamSseChunk::is_liveness`]),
//! pero esa conversión es invisible: no queda rastro de que el modelo estuvo
//! razonando ni de cuánto estimó.
//!
//! # Forma de línea real
//!
//! Este módulo clasifica la línea SSE **cruda**, tal y como la entregan
//! `SseParser::next_line` + `decode_and_record_line`, es decir **una línea por
//! llamada**:
//!
//! ```text
//! event: content_block_delta
//! data: {"index":0,"delta":{...}}
//! ```
//!
//! La forma combinada `"content_block_delta\ndata: {...}"` la fabrica
//! `parse_anthropic_sse_stream_line` (sse/anthropic/parser.rs) **después** de
//! que este probe haya corrido: el probe se invoca antes del traductor a
//! propósito, para ver el evento que el traductor descarta. Por eso el evento
//! se trackea aquí con un flag propio en lugar de leer
//! `StreamingState::current_event_type`, que en ese punto todavía contiene el
//! evento anterior: depender de ese orden entre módulos sería frágil y
//! silenciosamente incorrecto. El flag es un `bool` y no el nombre del
//! evento: ese texto lo controla el upstream y no se conserva.
//!
//! `decode_and_record_line` filtra las líneas vacías y los comentarios antes
//! de que este probe las vea, así que el único `data:` que debe clasificarse
//! es el que sigue a su `event:`. Por eso el flag es de **un solo uso**: la
//! primera línea `data:` lo consume, igual que el traductor resuelve como
//! `unknown` (y descarta) un `data:` que llega después del delimitador en
//! blanco. Límite conocido: un evento con varias líneas `data:` (permitido
//! por SSE, que Anthropic no emite) se cuenta una sola vez; el probe nunca
//! sobrecuenta respecto al traductor.
//!
//! # Qué se registra
//!
//! * tipo de delta (`thinking_delta` / `text_delta`) y block index.
//! * `estimated_tokens` solo cuando es un entero positivo válido — mismo
//!   criterio exacto que el traductor (`Option<i64>` con `> 0` en
//!   `sse/anthropic/events.rs`), para que el contador no diverja de la
//!   clasificación real.
//! * longitudes en bytes de `thinking` / `text` — **nunca su contenido**.
//!
//! # Qué no se registra
//!
//! SSE crudo, JSON completo, texto, nombre de evento controlado por el
//! upstream, headers, auth, `stop_reason`, usage facturado ni request body.
//! `estimated_tokens` es una estimación del vendor, **no** uso facturado:
//! nada de aquí alimenta `StreamingState::usage`, `ttft_ms` ni el stop
//! reason, y ningún contador altera un deadline del stream.
//!
//! Tampoco se emite ningún `StageEvent`. `publish_stage_global`
//! (openproxy-analytics) aplica `stage_rank`, que devuelve 0 para un stage
//! desconocido: un `"reasoning_hidden"` **retrocedería** el stage del intento
//! de `streaming` (rank 3) a rank 0 en el `INFLIGHT_REGISTRY` compartido, o
//! insertaría un intento fantasma si la clave aún no existiera. La
//! observabilidad queda en `tracing`, que es metadata-only y no tiene
//! efectos sobre estado compartido.

use serde_json::Value;

/// Tope de línea inspeccionada. Se alinea con el límite que ya impone el
/// parser SSE ([`crate::sse::MAX_SSE_LINE_BYTES`]): una línea más grande
/// nunca llega a clasificarse en el pipeline, así que un tope menor aquí
/// solo produciría subconteos silenciosos.
pub(crate) const MAX_PROBE_LINE_BYTES: usize = crate::sse::MAX_SSE_LINE_BYTES;

/// Prefiltro barato (substring, sin parsear JSON) antes de construir el
/// `Value`. Solo estas dos familias de delta aportan algo al probe; el resto
/// (`input_json_delta`, `signature_delta`, `citations_delta`, ...) se
/// descarta sin coste de parseo.
const DELTA_MARKERS: [&str; 2] = ["thinking_delta", "text_delta"];

/// Un evento de razonamiento identificado en una línea `data:` de Anthropic,
/// con metadatos acotados (sin contenido).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReasoningProbe {
    /// Tipo de delta del vendor (`thinking_delta`, `text_delta`).
    pub delta_type: String,
    /// Índice del content block que emite el delta.
    pub block_index: Option<u64>,
    /// `estimated_tokens` del vendor: solo se conserva cuando es un entero
    /// positivo (mismo criterio que el traductor). `None` cuando falta, es
    /// 0, negativo, decimal, no numérico o no cabe en `i64`.
    pub estimated_tokens: Option<i64>,
    /// Longitud en bytes del texto de `thinking` (0 si está vacío o falta).
    /// Nunca se conserva el texto.
    pub thinking_len: usize,
    /// Longitud en bytes del texto de `text` (0 si está vacío o falta).
    /// Nunca se conserva el texto.
    pub text_len: usize,
}

impl ReasoningProbe {
    /// Razonamiento retenido por el vendor: `thinking` vacío con una
    /// estimación positiva. Es el caso que el traductor convierte en
    /// liveness; se registra para que la actividad no sea invisible.
    #[inline]
    pub fn is_hidden_progress(&self) -> bool {
        self.delta_type == "thinking_delta"
            && self.thinking_len == 0
            && self.estimated_tokens.is_some_and(|n| n > 0)
    }

    /// `thinking_delta` con texto visible: razonamiento que sí se reenvía al
    /// cliente. Sirve para distinguir "razonamiento visible" de "progreso
    /// oculto" sin mirar el contenido.
    #[inline]
    pub fn is_visible_reasoning(&self) -> bool {
        self.delta_type == "thinking_delta" && self.thinking_len > 0
    }
}

/// Contadores agregados de razonamiento por request, más el estado mínimo
/// del evento en curso. Aritmética saturante: un stream adversativo no puede
/// desbordarlos.
#[derive(Debug, Default, Clone)]
pub(crate) struct ReasoningProgress {
    /// True mientras un `event: content_block_delta` esté pendiente de su
    /// línea `data:`. Se guarda un flag, **no** el nombre del evento: ese
    /// texto lo controla el upstream y no se conserva. Un evento oversized
    /// queda fuera por comparación (nunca iguala a `content_block_delta`),
    /// con la misma semántica que el parser. Se resetea a `false` en línea
    /// vacía, igual que el parser real con su evento en curso.
    delta_pending: bool,
    /// Eventos de progreso oculto (`thinking` vacío + estimación positiva):
    /// los que el traductor convierte en liveness.
    pub hidden_events: u64,
    /// Eventos de razonamiento visible (`thinking` con texto).
    pub visible_reasoning_events: u64,
    /// Deltas de texto vistos durante la clasificación (grupo de control:
    /// el texto normal no debe inflar los contadores de razonamiento).
    pub text_delta_events: u64,
    /// Suma saturada de las estimaciones positivas observadas. Es una
    /// estimación del vendor, nunca uso facturado.
    pub estimated_tokens_sum: u64,
    /// Block index del primer evento de razonamiento observado.
    pub first_block_index: Option<u64>,
    /// `elapsed_ms` del primer evento de razonamiento observado.
    pub first_activity_ms: Option<u64>,
    /// Pings de keepalive vistos: no son actividad de razonamiento.
    pub ping_events: u64,
}

impl ReasoningProgress {
    /// Alimenta una línea SSE cruda y devuelve el probe de razonamiento, o
    /// `None` para cualquier otra cosa: líneas `event:`, `id:`, `retry:`,
    /// comentarios, líneas vacías, `data:` de eventos que no son
    /// `content_block_delta`, JSON malformado y tipos de delta desconocidos.
    ///
    /// Efectos laterales: actualiza `delta_pending` y el contador de pings.
    /// Los contadores de razonamiento los actualiza [`Self::record`], para
    /// que la clasificación y el cómputo puedan verificarse por separado.
    pub fn observe(&mut self, line: &str) -> Option<ReasoningProbe> {
        if line.len() > MAX_PROBE_LINE_BYTES {
            // Misma defensa que el parser: sin evento en curso fiable.
            self.delta_pending = false;
            return None;
        }
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            self.delta_pending = false;
            return None;
        }
        if let Some(event) = line.strip_prefix("event: ") {
            let event = event.trim();
            if event == "ping" {
                // El ping de Anthropic es `event: ping` + `data: {...}`:
                // contarlo en la línea de evento es inequívoco y no cuesta
                // ningún parseo. No es actividad de razonamiento.
                self.ping_events = self.ping_events.saturating_add(1);
            }
            // Prefijo `"event: "` con espacio y comparación exacta, igual que
            // `parse_anthropic_sse_stream_line`: así un evento oversized o
            // con formato raro queda fuera sin comprobaciones extra, y el
            // probe clasifica exactamente lo que el traductor traduciría.
            self.delta_pending = event == "content_block_delta";
            return None;
        }
        let Some(data) = line.strip_prefix("data: ") else {
            // `id:`, `retry:` y comentarios no llevan payload ni consumen el
            // evento en curso, igual que en `parse_anthropic_sse_stream_line`.
            return None;
        };
        // Emparejamiento de un solo uso: la línea `data:` consume el flag.
        // El traductor resuelve como `unknown` una línea `data:` que llega
        // tras el delimitador en blanco (que este probe no ve, porque
        // `decode_and_record_line` lo filtra) y la descarta; consumir aquí
        // evita contar duplicados que nunca se tradujeron.
        let pending = std::mem::take(&mut self.delta_pending);
        if !pending {
            return None;
        }
        if !DELTA_MARKERS.iter().any(|m| data.contains(m)) {
            return None;
        }
        let value: Value = serde_json::from_str(data).ok()?;
        probe_content_block_delta(&value)
    }

    /// Registra el resultado de clasificar una línea. `elapsed_ms` es el
    /// tiempo transcurrido del request en el momento de la línea.
    pub fn record(&mut self, probe: &ReasoningProbe, elapsed_ms: u64) {
        if probe.is_hidden_progress() {
            self.hidden_events = self.hidden_events.saturating_add(1);
            if let Some(n) = probe.estimated_tokens {
                // `n` ya está filtrado a `> 0`, así que el cast a `u64` es
                // seguro y la suma satura en vez de desbordar.
                self.estimated_tokens_sum = self.estimated_tokens_sum.saturating_add(n as u64);
            }
            self.note_first_activity(probe, elapsed_ms);
        } else if probe.is_visible_reasoning() {
            self.visible_reasoning_events = self.visible_reasoning_events.saturating_add(1);
            self.note_first_activity(probe, elapsed_ms);
        } else if probe.delta_type == "text_delta" {
            self.text_delta_events = self.text_delta_events.saturating_add(1);
        }
    }

    fn note_first_activity(&mut self, probe: &ReasoningProbe, elapsed_ms: u64) {
        if self.first_activity_ms.is_none() {
            self.first_activity_ms = Some(elapsed_ms);
            self.first_block_index = probe.block_index;
        }
    }

    /// True si se observó alguna actividad de razonamiento (oculta o
    /// visible) en el request.
    #[inline]
    pub fn saw_reasoning(&self) -> bool {
        self.first_activity_ms.is_some()
    }
}

fn probe_content_block_delta(value: &Value) -> Option<ReasoningProbe> {
    let delta = value.get("delta")?;
    let delta_type = delta.get("type").and_then(Value::as_str)?;
    if delta_type != "thinking_delta" && delta_type != "text_delta" {
        return None;
    }
    let thinking = delta.get("thinking").and_then(Value::as_str);
    let text = delta.get("text").and_then(Value::as_str);
    // `estimated_tokens` solo cuenta cuando es un entero positivo: 0,
    // negativos, decimales, strings y valores fuera de rango no afirman
    // progreso (mismo criterio exacto que el traductor —`Option<i64>` con
    // `> 0`—, para que el contador no diverja de la clasificación real).
    let estimated_tokens = delta
        .get("estimated_tokens")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0);
    Some(ReasoningProbe {
        delta_type: delta_type.to_string(),
        block_index: value.get("index").and_then(Value::as_u64),
        estimated_tokens,
        thinking_len: thinking.map_or(0, str::len),
        text_len: text.map_or(0, str::len),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Línea `event:` cruda, como la entrega `SseParser::next_line`.
    fn ev(event: &str) -> String {
        format!("event: {event}")
    }

    /// Línea `data:` cruda, como la entrega `SseParser::next_line`.
    fn dat(json: &str) -> String {
        format!("data: {json}")
    }

    /// Recorre el par real `event:` + `data:` y devuelve el probe de la
    /// línea de datos.
    fn probe_pair(event: &str, json: &str) -> Option<ReasoningProbe> {
        let mut p = ReasoningProgress::default();
        assert!(
            p.observe(&ev(event)).is_none(),
            "an event line never probes"
        );
        p.observe(&dat(json))
    }

    /// Evento del vendor con `thinking` vacío y estimación positiva.
    const HIDDEN_JSON: &str =
        r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":120}}"#;

    #[test]
    fn hidden_progress_probe_keeps_only_metadata() {
        let probe =
            probe_pair("content_block_delta", HIDDEN_JSON).expect("hidden progress must be probed");
        assert!(probe.is_hidden_progress());
        assert!(!probe.is_visible_reasoning());
        assert_eq!(probe.delta_type, "thinking_delta");
        assert_eq!(probe.estimated_tokens, Some(120));
        assert_eq!(probe.thinking_len, 0);
        assert_eq!(probe.block_index, Some(0));
    }

    #[test]
    fn visible_reasoning_reports_length_not_content() {
        let probe = probe_pair(
            "content_block_delta",
            r#"{"index":1,"delta":{"type":"thinking_delta","thinking":"secret chain of thought","estimated_tokens":7}}"#,
        )
        .expect("visible reasoning must be probed");
        assert!(probe.is_visible_reasoning());
        assert!(!probe.is_hidden_progress());
        // Solo la longitud queda en el probe.
        assert_eq!(probe.thinking_len, "secret chain of thought".len());
        assert!(!format!("{probe:?}").contains("secret chain"));
    }

    #[test]
    fn text_delta_is_probed_as_control_group() {
        let probe = probe_pair(
            "content_block_delta",
            r#"{"index":2,"delta":{"type":"text_delta","text":"hola","estimated_tokens":9}}"#,
        )
        .expect("text delta must be probed");
        assert!(!probe.is_hidden_progress());
        assert!(!probe.is_visible_reasoning());
        assert_eq!(probe.delta_type, "text_delta");
        assert_eq!(probe.text_len, 4);
        // El texto real nunca queda en el probe.
        assert!(!format!("{probe:?}").contains("hola"));
    }

    #[test]
    fn probe_requires_the_content_block_delta_event() {
        // El mismo payload bajo otro evento: el traductor tampoco lo
        // clasificaría como delta de contenido.
        for event in [
            "message_delta",
            "content_block_start",
            "content_block_stop",
            "message_start",
            "ping",
        ] {
            assert!(
                probe_pair(event, HIDDEN_JSON).is_none(),
                "hidden payload under `{event}` must not be probed"
            );
        }
    }

    #[test]
    fn probe_ignores_non_reasoning_delta_types() {
        for json in [
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            r#"{"index":0,"delta":{"type":"signature_delta","signature":"abc"}}"#,
            r#"{"index":0,"delta":{"type":"citations_delta","citation":{}}}"#,
        ] {
            assert!(
                probe_pair("content_block_delta", json).is_none(),
                "non-reasoning delta must not be probed: {json}"
            );
        }
    }

    #[test]
    fn probe_ignores_non_delta_events() {
        for (event, json) in [
            ("message_stop", r#"{"type":"message_stop"}"#),
            ("content_block_stop", r#"{"index":0}"#),
            (
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"thinking"}}"#,
            ),
            (
                "message_delta",
                r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":42}}"#,
            ),
        ] {
            assert!(
                probe_pair(event, json).is_none(),
                "`{event}` must not be probed"
            );
        }
    }

    #[test]
    fn probe_rejects_malformed_and_missing_payloads() {
        let mut p = ReasoningProgress::default();
        assert!(p.observe(&ev("content_block_delta")).is_none());
        // JSON malformado: el probe falla en silencio, sin entrar en pánico.
        assert!(
            p.observe(&dat(
                r#"{"index":0,"delta":{"type":"thinking_delta","thinking":}}"#
            ))
            .is_none()
        );
        // Payload vacío.
        assert!(p.observe("data: ").is_none());
        // Línea sin separador evento/data.
        assert!(p.observe("content_block_delta").is_none());
        // Tipo de delta ausente.
        assert!(
            p.observe(&dat(r#"{"index":0,"delta":{"thinking":"x"}}"#))
                .is_none()
        );
        // JSON que no es objeto.
        assert!(p.observe(&dat("[1,2,3]")).is_none());
    }

    #[test]
    fn empty_line_resets_the_tracked_event() {
        let mut p = ReasoningProgress::default();
        assert!(p.observe(&ev("content_block_delta")).is_none());
        assert!(p.delta_pending);
        // Delimitador en blanco entre eventos (Anthropic lo emite siempre;
        // en producción lo filtra `decode_and_record_line`).
        assert!(p.observe("").is_none());
        assert!(!p.delta_pending);
        assert!(
            p.observe(&dat(HIDDEN_JSON)).is_none(),
            "a data line after the blank delimiter carries no event"
        );
    }

    #[test]
    fn data_line_consumes_the_event_once() {
        // Emparejamiento de un solo uso: el traductor resuelve una línea
        // `data:` tras el delimitador como `unknown` y la descarta, así que
        // el probe tampoco debe contarla. Sin el `take()`, un segundo
        // `data:` con el mismo payload se contaría dos veces.
        let mut p = ReasoningProgress::default();
        assert!(p.observe(&ev("content_block_delta")).is_none());
        let probe = p.observe(&dat(HIDDEN_JSON)).expect("first data probes");
        p.record(&probe, 1);
        assert!(!p.delta_pending, "the event must be consumed");
        assert!(
            p.observe(&dat(HIDDEN_JSON)).is_none(),
            "a bare data line after consumption carries no event"
        );
        assert_eq!(p.hidden_events, 1, "the duplicate must not be counted");
    }

    #[test]
    fn oversized_event_name_is_dropped_like_the_parser() {
        // Un evento oversized nunca iguala a `content_block_delta`, así que
        // queda fuera por la comparación exacta, como en el parser.
        let mut p = ReasoningProgress::default();
        let huge = format!("event: {}", "x".repeat(4096));
        assert!(p.observe(&huge).is_none());
        assert!(!p.delta_pending);
        assert!(p.observe(&dat(HIDDEN_JSON)).is_none());
    }

    #[test]
    fn event_line_without_space_is_not_tracked() {
        // El parser real exige `"event: "` con espacio; `event:foo` cae por
        // el fallthrough sin tocar el evento en curso. El probe iguala esa
        // semántica en vez de ser más permisivo.
        let mut p = ReasoningProgress::default();
        assert!(p.observe("event:content_block_delta").is_none());
        assert!(!p.delta_pending);
        assert!(p.observe(&dat(HIDDEN_JSON)).is_none());
    }

    #[test]
    fn oversized_line_is_ignored() {
        let mut p = ReasoningProgress::default();
        assert!(p.observe(&ev("content_block_delta")).is_none());
        let huge = format!("data: {{\"pad\":\"{}\"}}", "y".repeat(MAX_PROBE_LINE_BYTES));
        assert!(p.observe(&huge).is_none());
    }

    #[test]
    fn probe_ignores_non_positive_estimates() {
        // 0, negativo, string, null, ausente y fuera de i64 no afirman
        // progreso.
        for json in [
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":0}}"#,
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":-5}}"#,
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":"120"}}"#,
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":null}}"#,
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":""}}"#,
            // u64::MAX no cabe en el `i64` que lee el traductor.
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":18446744073709551615}}"#,
        ] {
            let probe =
                probe_pair("content_block_delta", json).expect("the delta itself is still probed");
            assert!(
                !probe.is_hidden_progress(),
                "no positive estimate must not count as hidden progress: {json}"
            );
            assert_eq!(probe.estimated_tokens, None, "estimate dropped: {json}");
        }
    }

    #[test]
    fn counters_are_saturating_and_content_free() {
        // 2^53 - 1: cabe en u64, en i64 y en el i64 que lee el traductor.
        const BIG: i64 = 9_007_199_254_740_991;
        let json = format!(
            r#"{{"index":3,"delta":{{"type":"thinking_delta","thinking":"","estimated_tokens":{BIG}}}}}"#
        );
        let mut progress = ReasoningProgress::default();
        let hidden = probe_pair("content_block_delta", &json).expect("probe");
        assert!(hidden.is_hidden_progress());
        progress.record(&hidden, 10);
        progress.record(&hidden, 20);

        assert_eq!(progress.hidden_events, 2);
        // La suma crece sin desbordar.
        assert_eq!(progress.estimated_tokens_sum, (BIG as u64) * 2);
        assert_eq!(progress.first_activity_ms, Some(10));
        assert_eq!(progress.first_block_index, Some(3));
        assert!(progress.saw_reasoning());
        assert!(!format!("{progress:?}").contains("thinking"));

        // Saturación real: un acumulador ya cerca de u64::MAX no hace panic
        // por overflow en debug, satura.
        let mut near_max = ReasoningProgress {
            estimated_tokens_sum: u64::MAX - 1,
            ..Default::default()
        };
        near_max.record(&hidden, 30);
        assert_eq!(near_max.hidden_events, 1);
        assert_eq!(near_max.estimated_tokens_sum, u64::MAX);
    }

    #[test]
    fn counters_separate_hidden_from_visible_and_text() {
        let visible_json = r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"pensando","estimated_tokens":9}}"#;
        let text_json = r#"{"index":0,"delta":{"type":"text_delta","text":"respuesta"}}"#;

        let mut progress = ReasoningProgress::default();
        let hidden = probe_pair("content_block_delta", HIDDEN_JSON).expect("probe");
        let visible = probe_pair("content_block_delta", visible_json).expect("probe");
        let text = probe_pair("content_block_delta", text_json).expect("probe");

        progress.record(&hidden, 1);
        progress.record(&visible, 2);
        progress.record(&text, 3);

        assert_eq!(progress.hidden_events, 1);
        assert_eq!(progress.visible_reasoning_events, 1);
        assert_eq!(progress.text_delta_events, 1);
        assert_eq!(progress.ping_events, 0);
        // El texto normal no aporta estimaciones: un `estimated_tokens` en
        // un text_delta no es una señal de razonamiento.
        assert_eq!(progress.estimated_tokens_sum, 120);
        assert_eq!(progress.first_activity_ms, Some(1));
        assert_eq!(progress.first_block_index, Some(0));
    }

    #[test]
    fn ping_is_counted_apart_from_reasoning() {
        let mut progress = ReasoningProgress::default();
        assert!(progress.observe(&ev("ping")).is_none());
        assert!(progress.observe(&dat(r#"{"type": "ping"}"#)).is_none());
        assert!(progress.observe(&ev("ping")).is_none());

        assert_eq!(progress.ping_events, 2);
        assert_eq!(progress.hidden_events, 0);
        assert_eq!(progress.text_delta_events, 0);
        assert!(
            !progress.saw_reasoning(),
            "a ping must never look like reasoning activity"
        );
    }

    #[test]
    fn full_hidden_stream_records_first_activity_once() {
        let mut progress = ReasoningProgress::default();
        for (i, json) in [
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":120}}"#,
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":80}}"#,
            r#"{"index":1,"delta":{"type":"thinking_delta","thinking":"","estimated_tokens":40}}"#,
        ]
        .into_iter()
        .enumerate()
        {
            let probe = probe_pair("content_block_delta", json).expect("probe");
            progress.record(&probe, 100 + i as u64);
            // Delimitador en blanco que emite Anthropic entre eventos.
            assert!(progress.observe("").is_none());
        }

        assert_eq!(progress.hidden_events, 3);
        assert_eq!(progress.estimated_tokens_sum, 240);
        assert_eq!(
            progress.first_activity_ms,
            Some(100),
            "first activity is stamped once, at the first event"
        );
        assert_eq!(progress.first_block_index, Some(0));
    }
}
