---

# Gate G1 — Streaming Response Body Persistence

**Status:** Draft
**Scope:** Frontend (already shipped in SPEC_LOG_DETAIL_MODAL.md) + Backend (this spec)
**Bug location:** `crates/openproxy-core/src/pipeline.rs:2888-2900`

## Context

The user observed that the live-logs modal showed `(empty)` in the Response tab for streaming requests, even with `recording=ON`. Two investigations confirmed:

1. The frontend modal consumed `response_body_json` correctly, but the backend never persisted it for streaming responses.
2. The streaming success path at `pipeline.rs:2888-2900` hardcodes `response_body_json: None`.

The frontend was fixed in a previous commit (see `SPEC_LOG_DETAIL_MODAL.md` and `crates/openproxy-web/src/static/src/components/log-detail.ts:renderResponseTab`). This spec covers the backend fix.

## User design constraint (verbatim)

> "se pueden almacenar en memoria como buffer y luego construir ese json y dejarlo en los live logs, no? solo cuando recording es ON. recuerda priorizar la optimizacion de cpu."

Translation: "they can be stored in memory as a buffer and then construct that json and leave it in live logs, no? only when recording is ON. remember to prioritize CPU optimization."

The fix MUST:

- Accumulate chunks in memory during streaming.
- Build a reconstructed OpenAI-style JSON.
- Persist it to `response_body_json` only when `is_recording() == true`.
- **Not regress the OpenAI fast path (H6)** — most streaming chunks must continue to skip JSON parsing.

## Investigation findings (key file:line refs)

| Topic | File | Lines |
|---|---|---|
| `dispatch_upstream_streaming` | `crates/openproxy-core/src/pipeline.rs` | 2163-2908 |
| Streaming loop body | `crates/openproxy-core/src/pipeline.rs` | 2443-2783 |
| OpenAI fast path (H6) | `crates/openproxy-core/src/pipeline.rs` | 2606-2666 |
| Per-format dispatch | `crates/openproxy-core/src/pipeline.rs` | 2669-2709 |
| `record_attempt_raw_with_tokens` (BUG call site) | `crates/openproxy-core/src/pipeline.rs` | 2888-2900, 3142-3272 |
| `is_recording` gate | `crates/openproxy-core/src/pipeline.rs` | 235-241, 3197-3200 |
| `UpstreamSseChunk` struct | `crates/openproxy-core/src/sse.rs` | 10-37 |
| OpenAI parser (probe + line parser) | `crates/openproxy-core/src/sse.rs` | 73-136 |
| Gemini parser | `crates/openproxy-core/src/sse.rs` | 156-243 |
| Anthropic stateless translator | `crates/openproxy-core/src/sse.rs` | 287-410 |
| Anthropic stateful translator | `crates/openproxy-core/src/sse.rs` | 427-616 |
| `AnthropicToolUseAccumulator` | `crates/openproxy-core/src/sse.rs` | 54-64 |
| 32 MiB upstream cap | `crates/openproxy-core/src/upstream/client.rs` | 541 |
| `UsageInput.response_body_json` | `crates/openproxy-core/src/cost.rs` | 39 |
| `cost::record` (DB INSERT) | `crates/openproxy-core/src/cost.rs` | 96-218 |
| `OpenAIResponse` (target shape) | `crates/openproxy-core/src/translation.rs` | 80-89 |
| `OpenAIMessage` | `crates/openproxy-core/src/translation.rs` | 56-78 |
| `OpenAIUsage` | `crates/openproxy-core/src/translation.rs` | 98-103 |
| Multi-frame fake upstream test pattern | `crates/openproxy-core/src/pipeline.rs` | 6832-7122 |

## Design

### A. Architecture

A new module `crates/openproxy-core/src/sse_accumulator.rs` defines a `ResponseAccumulator` struct. The streaming loop in `dispatch_upstream_streaming` owns one instance.

Rationale:
- The pure parsers in `sse.rs` were designed stateless — extending their signatures would break the OpenAI fast path.
- The pipeline already owns `usage`, `stop_reason`, `tool_use_acc`; adding an accumulator is the natural extension.
- The final `serde_json::Value` mirrors the non-streaming `OpenAIResponse` (translation.rs:80-89), so the same `response_body_json` column accepts both.

### B. Recording gate (CPU optimization)

The accumulator is **only constructed when `is_recording() == true`**, checked once at the top of `dispatch_upstream_streaming`. When OFF: no `String` allocation, no `serde_json::Value` allocation, no field updates per chunk — the only cost is a single `bool` check at function entry.

A second safety gate at `pipeline.rs:3197-3200` (the `is_recording` gate inside `record_attempt_raw_with_tokens`) drops the body to `None` before storage when OFF. So the loop-level gate is purely a CPU optimization, not a correctness requirement.

### C. CPU optimization — preserve the OpenAI fast path

The OpenAI fast path at `pipeline.rs:2606-2666` currently avoids JSON parsing for the majority of chunks (pure content deltas). The accumulator MUST respect this:

- **Fast path** (`needs_parse == false`): the accumulator stores `raw_payload` from `UpstreamSseChunk.raw_payload` (already populated by the fast path) into its `content_parts: Vec<String>`. No JSON parsing happens here.
- **Slow path** (`needs_parse == true`): the accumulator also stores `raw_payload` AND the parsed `usage` / `stop_reason` fields.
- **On `finish()`**, `content_parts` are concatenated verbatim into the final `content` field. Each raw payload is a small OpenAI chunk JSON; concatenation is a faithful reconstruction because the frontend's `parseOpenAiChatResponse` parses each OpenAI chunk independently.

**The OpenAI probe is not extended.** It keeps extracting only `usage` + `finish_reason` cheaply. The accumulator does not need `delta.content` from the probe because it already has the raw payload from `UpstreamSseChunk.raw_payload`.

For Gemini and Anthropic, every chunk is already fully parsed. The accumulator just consumes the per-chunk fields.

For Anthropic tool_use, the accumulator owns its own `Vec<AccumulatedToolCall>` populated directly at the call site `pipeline.rs:2692-2699` from `translate_anthropic_sse_event`'s `Open | Delta | Close` events — see §D and §F. The pre-existing `AnthropicToolUseAccumulator` (`tool_use_acc`) stays as-is; the new accumulator runs in parallel.

The final assembly into `serde_json::Value` happens **once at the end** of the loop, not per-chunk.

### D. Data structure: `ResponseAccumulator`

```rust
// crates/openproxy-core/src/sse_accumulator.rs

/// Hard cap on total bytes the accumulator will hold. Below the 32 MiB
/// upstream cap so the reconstructed JSON stays well under SQLite's practical
/// row-size limit. When exceeded, the accumulator stops appending and sets
/// `truncated = true`.
pub const MAX_ACCUMULATED_BYTES: usize = 16 * 1024 * 1024; // 16 MiB

pub struct ResponseAccumulator {
    /// One entry per non-empty chunk. For OpenAI this is the verbatim
    /// upstream chunk JSON. For Anthropic/Gemini this is the extracted text
    /// segment (those providers' raw SSE payloads are not OpenAI-shaped).
    /// Joined verbatim into the final `content` field on `finish()`.
    content_parts: Vec<String>,

    /// Concatenated reasoning text. Populated from `delta.reasoning_content`
    /// (OpenAI o1-style, written via `message.extra` round-trip),
    /// `delta.thinking` (Anthropic `thinking_delta`), or Gemini `parts[].thought`.
    /// `None` if no reasoning was emitted.
    reasoning: Option<String>,

    /// Accumulated tool calls. OpenAI: from `delta.tool_calls[]`. Anthropic:
    /// populated at the call site from `translate_anthropic_sse_event`'s
    /// Open/Delta/Close events. Gemini: out of scope for G1.
    tool_calls: Vec<AccumulatedToolCall>,

    /// Inherited from the existing `usage` local in the loop.
    usage: Option<OpenAIUsage>,

    /// Inherited from the existing `stop_reason` local in the loop.
    stop_reason: Option<String>,

    /// Inherited from `done_sent` — upstream emitted `[DONE]` (or Anthropic equivalent).
    done_sent: bool,

    /// `true` if appending would have pushed `content_parts` past
    /// `MAX_ACCUMULATED_BYTES`. Surfaced in the final JSON.
    truncated: bool,
}

struct AccumulatedToolCall {
    id: String,
    /// OpenAI: `function.name`; Anthropic: `name` from content_block_start.
    name: String,
    /// OpenAI: JSON-encoded string per the spec; Anthropic: the accumulated
    /// `partial_json` fragments joined together.
    arguments: String,
}

/// What `translate_anthropic_sse_event` is doing with a tool_use block.
pub enum AnthropicToolEvent {
    /// `content_block_start{type:tool_use}` — open a new entry.
    Open,
    /// `content_block_delta{type:input_json_delta}` — append `partial_json`.
    Delta { partial_json: &'static str },
    /// `content_block_stop` — close the current entry (no-op for the vec).
    Close,
}

impl ResponseAccumulator {
    pub fn new() -> Self { /* ... */ }

    /// OpenAI-only: called once per parsed chunk with the upstream chunk's
    /// raw JSON payload. Fast-path chunks come here too (with `raw_payload`
    /// already populated by the fast path).
    pub fn update_openai(
        &mut self,
        raw_payload: Option<&str>,
        usage: Option<OpenAIUsage>,
        stop_reason: Option<&str>,
    ) { /* pushes raw_payload into content_parts (subject to MAX_ACCUMULATED_BYTES),
          stores usage + stop_reason if Some */ }

    /// Gemini: appends an extracted text segment to `content_parts`.
    pub fn update_gemini(
        &mut self,
        text_segment: Option<&str>,
        thought_segment: Option<&str>,
    ) { /* text_segment → content_parts, thought_segment → reasoning */ }

    /// Anthropic-only: called from the streaming loop when translate_anthropic_sse_event
    /// sees content_block_start (Open) or content_block_delta/input_json_delta (Delta)
    /// or content_block_stop (Close). Updates `tool_calls` directly.
    pub fn update_anthropic_tool_use(
        &mut self,
        event_kind: AnthropicToolEvent,
        id: Option<&str>,
        name: Option<&str>,
    ) {
        // Open  → push a new AccumulatedToolCall with id+name.
        // Delta → append `partial_json` to the last entry's `arguments`.
        // Close → no-op (entry is already complete in the vec).
    }

    /// Per-chunk update from a fully-parsed UpstreamSseChunk.
    /// Routes to update_openai / update_gemini / update_anthropic_* as appropriate.
    pub fn update_from_chunk(
        &mut self,
        provider: ProviderKind,
        chunk: &UpstreamSseChunk,
    ) { /* ... */ }

    /// Build the final OpenAI-style response JSON.
    pub fn finish(
        &self,
        chunk_id: &str,
        created: u64,
        model: &str,
    ) -> serde_json::Value {
        // Shape: { id, object:"chat.completion", created, model,
        //          choices: [{ index:0, message:{ role, content,
        //                                           reasoning_content?,
        //                                           tool_calls? },
        //                      finish_reason }],
        //          usage,
        //          truncated? }
        // content = content_parts joined (verbatim).
        // reasoning_content is written into message.extra (round-trips because
        // OpenAIMessage.extra is #[serde(flatten)] at translation.rs:77).
    }
}
```

The `finish()` shape is round-trip compatible with `OpenAIResponse` (translation.rs:80-89) — same field names, same nesting. The DB column stores it as `TEXT` (SQLite) so any future OpenAI field can be added without a migration.

### E. Reasoning content extension (provider-by-provider)

**OpenAI** — the cheap probe at `sse.rs:73-79` is **unchanged**. Reasoning flows through `message.extra`:

- When `parse_openai_sse_line` (sse.rs:97-136) takes the slow path (`needs_parse == true`) and finds `delta.reasoning_content`, it writes that field into `message.extra["reasoning_content"]` (the existing `extra: serde_json::Map` at `translation.rs:77`).
- On `finish()`, the accumulator concatenates the per-chunk `extra["reasoning_content"]` into a single string and writes it back into the final message's `extra`. Round-trip via `serde_json::from_value::<OpenAIResponse>(...)` succeeds only because `OpenAIMessage.extra` is `#[serde(flatten)]` (translation.rs:77), so `reasoning_content` becomes a top-level sibling of `content` / `role`.
- Fast-path chunks do not carry `reasoning_content` in practice (OpenAI emits it only on chunks that also carry `usage` or a final summary, which always trigger `needs_parse == true`).

**Anthropic** — extend the `content_block_delta` arm in `translate_anthropic_sse_payload` (sse.rs:329-358) to handle a new `delta.type == "thinking_delta"` case by extracting `delta.thinking` and emitting it as `delta_reasoning` on `UpstreamSseChunk`. The existing `text_delta` arm stays unchanged.

**Gemini** — extend `parse_gemini_sse_line` (sse.rs:156-243) to iterate `parts[]`; items with `thought: true` populate `delta_reasoning`, regular items concatenate into `delta_content`.

### F. Persistence integration

At `pipeline.rs:2888-2900`, replace `None` with the accumulator's output **only if the accumulator was constructed** (i.e. `is_recording() == true`). When recording is OFF, the existing `None` is unchanged.

**Anthropic tool_use threading** — the new `ResponseAccumulator` is passed into the Anthropic translator at `pipeline.rs:2692-2699` so that `translate_anthropic_sse_event` can call `acc.update_anthropic_tool_use(...)` on the relevant events. This mirrors how the existing `tool_use_acc: &mut AnthropicToolUseAccumulator` is threaded today:

```rust
// At the top of dispatch_upstream_streaming (pipeline.rs ~2443):
let mut acc: Option<ResponseAccumulator> = if is_recording() { Some(ResponseAccumulator::new()) } else { None };

// In the Anthropic branch (pipeline.rs:2692-2699), pass `acc.as_mut()` alongside
// `tool_use_acc`. The translator calls acc.update_anthropic_tool_use() on
// Open/Delta/Close events. `acc` and `tool_use_acc` are independent — both run.
let translated = translate_anthropic_sse_event(
    event,
    &mut tool_use_acc,
    acc.as_mut(), // NEW: passed in addition to tool_use_acc
);
```

**Final call site** (pipeline.rs:2888-2900) — extract into a named local before passing to `record_attempt_raw_with_tokens`. This avoids footgun patterns where an inline `.map(...).unwrap_or(None)` can drift across edits:

```rust
// pipeline.rs:2888-2900 (just before record_attempt_raw_with_tokens).
// `is_recording` is double-gated: once here (we built `acc` only when recording was on)
// and again at pipeline.rs:3197-3200 (the storage layer drops the body to None if recording flipped off mid-stream).
let response_body_json: Option<serde_json::Value> = if let Some(acc) = acc.as_ref() {
    Some(acc.finish(&chunk_id, created, &model_name))
} else {
    None
};

// Pass `response_body_json` explicitly into record_attempt_raw_with_tokens.
```

The downstream `is_recording` gate at `pipeline.rs:3197-3200` (inside `record_attempt_raw_with_tokens`) handles the recording=OFF case at the storage layer — but per the CPU constraint, we want to avoid building the JSON value at all when recording is OFF.

### G. Failure paths

Failure paths inside the streaming loop (per-chunk errors, client disconnects, timeouts) call `record_and_fail_with_trace_id` (pipeline.rs:3041). Those paths do not currently pass `response_body_json` and are out of scope. A partial body is acceptable for failures — the operator sees the error in the Errors tab.

> **Note (G2 follow-up):** Future spec G2 should address capturing partial response bodies for failed/cancelled streams. Out of scope for G1.

### H. Counters for test assertions (CPU regression)

The streaming loop maintains an `AtomicUsize parse_count` that increments each time `parse_openai_sse_line` is called. The test fixture reads it after the stream ends to assert that the fast path still skipped JSON parsing, replacing fragile wall-clock assertions. The counter is `#[cfg(test)]` only.

```rust
#[cfg(test)]
static OPENAI_PARSE_COUNT: AtomicUsize = AtomicUsize::new(0);

// inside parse_openai_sse_line:
#[cfg(test)]
{ OPENAI_PARSE_COUNT.fetch_add(1, Ordering::Relaxed); }
```

## Test plan

Tests live in `crates/openproxy-core/src/pipeline.rs` (the existing `#[cfg(test)] mod tests` block at line 3397). Tests that drive multiple SSE chunks on separate TCP writes use the multi-frame pattern from `streaming_dispatch_uses_upstream_client_end_to_end` (pipeline.rs:6832-7122), NOT `run_with_fake_upstream_and_capture_stages` (which is single-frame).

1. **`streaming_response_body_persists_reconstructed_openai_chat`** — drives a fake OpenAI upstream using the multi-frame pattern from pipeline.rs:6832-7122. Sends 3 content chunks on **separate TCP writes with explicit flushes**, then a final usage chunk. Asserts: `response_body_json` is non-NULL; round-trips through `OpenAIResponse`; `choices[0].message.content == "hi there!"`; `usage.prompt_tokens == 10`; `finish_reason == "stop"`.

2. **`streaming_response_body_persists_reconstructed_anthropic_message_with_tool_use`** — drives a fake Anthropic upstream with `message_start`, `content_block_start{type:tool_use}`, two `content_block_delta{type:input_json_delta}` fragments, `content_block_stop`, `message_delta`. Asserts: persisted body has `choices[0].message.tool_calls[]` with one entry; `function.name == "get_weather"`; `function.arguments` parses as JSON. (`update_anthropic_tool_use` runs at `content_block_start`, `content_block_delta`, and `content_block_stop` — the `content_block_stop` clear of `tool_use_acc` does NOT affect the accumulator's own vec.)

3. **`streaming_response_body_persists_reconstructed_gemini_response`** — drives a fake Gemini upstream with parts-text chunks. Asserts: persisted body has the concatenated content; `finish_reason` mapped correctly.

4. **`streaming_response_body_persists_reasoning_content_o1`** — OpenAI chunks include `delta.reasoning_content`. Asserts: `choices[0].message.reasoning_content` is present and equals the concatenated reasoning (round-trips via `message.extra`'s `#[serde(flatten)]`).

5. **`streaming_response_body_persists_anthropic_thinking`** — Anthropic extended thinking via `thinking_delta`. Asserts: `choices[0].message.reasoning_content` is present.

6. **`streaming_response_body_persists_gemini_thought_parts`** — Gemini `parts[].thought: true`. Asserts: reasoning content is captured.

7. **`recording_off_does_not_allocate_response_body`** — set `is_recording=false` before the call. Asserts: persisted `response_body_json` is NULL (the SQLite NULL check at the DB row is the real assertion); the accumulator is never constructed (verifiable by observing that the `Some(acc) = ...` branch was skipped).

8. **`openai_fast_path_no_regression`** — feed 100 OpenAI chunks of pure content (no `usage`, no `finish_reason`). Reset `OPENAI_PARSE_COUNT` to 0 before the call, run, then assert `OPENAI_PARSE_COUNT.load(Ordering::Relaxed) <= 2` (the final usage chunk may trigger one extra parse for `stop_reason`; 2 is a safe upper bound). This replaces the previous 50ms wall-clock timing assertion.

9. **`streaming_response_body_caps_at_16mib`** — feed enough content to exceed `MAX_ACCUMULATED_BYTES`. Asserts: `truncated: true` at the top level; `content` length stays under the cap; no panic.

10. **`existing_sse_parser_tests_still_pass`** — all tests in `sse.rs:630-1464` must continue to pass without modification (since the probe is not extended).

11. **`gemini_streaming_response_body_separates_thought_from_text`** — drive a fake Gemini upstream with parts `[{text:"r", thought:true}, {text:"a"}]`. Asserts: persisted body has `choices[0].message.content == "a"` and `choices[0].message.reasoning_content == "r"`.

## Out of scope

- The custom executor paths (`executor_antigravity.rs`, `executor_kiro.rs`) — they already return `OpenAIResponse` from the non-streaming call site; their `response_body_json` is also hardcoded to `None` at `pipeline.rs:1193` but that's a separate bug.
- The 32 MiB cap configuration — hardcoded; a config knob is a future enhancement.
- Sanitizing the response body for credentials (currently no sanitization, matches existing behavior for `response_body_json` in the non-streaming path).
- Per-provider feature detection (e.g. "this model doesn't support reasoning_content" — the proxy just doesn't extract it; the persisted JSON will simply lack the field).
- The streaming `request_body_json` — currently always `None` at `pipeline.rs:2893`. The non-streaming path persists it. A future spec can address streaming request body persistence.
- **Partial response body capture for failed/cancelled streams** (see §G note). Tracked as a G2 follow-up.

## Acceptance criteria

1. After a successful OpenAI streaming turn with recording=ON, the persisted `response_body_json` column is non-NULL and parses to a valid `OpenAIResponse` (round-trip via `serde_json::from_value`). The round-trip of `reasoning_content` succeeds only because `OpenAIMessage.extra` is declared `#[serde(flatten)]` at `translation.rs:77`, which allows `reasoning_content` to be written into `extra` and read back as a top-level `OpenAIMessage` field.

2. After a successful Anthropic streaming turn with recording=ON, the persisted body includes `choices[0].message.tool_calls[]` if any tool_use blocks were emitted. The accumulator's `update_anthropic_tool_use` populated the vec directly from `content_block_start` / `content_block_delta{type:input_json_delta}` events, independent of the existing `tool_use_acc` being cleared on `content_block_stop`.

3. After a successful Gemini streaming turn with recording=ON, the persisted body includes the concatenated text content.

4. When `delta.reasoning_content` (OpenAI) or `thinking_delta` (Anthropic) or `parts[].thought` (Gemini) is present in the upstream chunks, the persisted body includes `choices[0].message.reasoning_content`.

5. With recording=OFF, the SQLite `response_body_json` column is NULL (unchanged from today).

6. With recording=OFF, the `ResponseAccumulator` is never constructed (verifiable via the test `recording_off_does_not_allocate_response_body`).

7. The OpenAI fast path (H6) still does NOT parse the JSON for the majority of chunks; the test `openai_fast_path_no_regression` enforces `OPENAI_PARSE_COUNT <= 2` after 100 chunks. The cheap probe is not extended — `raw_payload` is the only per-chunk field the fast path populates.

8. All existing SSE parser tests in `sse.rs:630-1464` still pass.

9. `cargo build --release` succeeds.

10. `cargo test -p openproxy-core` passes.

11. When the accumulator's byte total would exceed `MAX_ACCUMULATED_BYTES` (16 MiB), the accumulator sets `truncated: true` in the final JSON and stops appending. The persisted body never exceeds the cap.

## Migration / rollout

No data migration. No schema change. No frontend change. The new column writes are additive and backward-compatible with the existing dashboard. Operators who upgrade will see streaming responses appear in the Response tab of the live-logs modal.

If recording=OFF, no behavioral change.

If recording=ON, `usage.response_body_json` starts being populated for streaming rows. The 16 MiB accumulator cap (below the 32 MiB upstream cap) bounds the per-row size.

---

End of spec.
