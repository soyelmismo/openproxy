// components/log-row.test.ts — the Cost column of the live-logs table reports
// the money the ledger actually charged, and keeps the calculated price as
// secondary data (tooltip) so the two are never confused.

import { describe, it, expect } from "vitest";
import { render } from "lit-html";
import type { TemplateResult } from "lit-html";
import { renderLogRowHtml } from "./log-row.js";
import type { AttemptState } from "../state/live-logs-store.js";
import type { RecentUsageRow } from "../lib/types/usage.js";

function toString(tpl: TemplateResult): string {
  const container = document.createElement("div");
  render(tpl, container);
  return container.innerHTML.replace(/<!--[\s\S]*?-->/g, "");
}

function textOf(tpl: TemplateResult): string {
  const container = document.createElement("div");
  render(tpl, container);
  return container.textContent ?? "";
}

function row(overrides: Partial<RecentUsageRow>): RecentUsageRow {
  return {
    id: 1254244,
    request_id: "req-fixture",
    trace_id: "trace-fixture",
    provider_id: "openrouter",
    upstream_model_id: "openai/gpt-4o-mini",
    status_code: 200,
    total_ms: 1200,
    prompt_tokens: 1000,
    completion_tokens: 200,
    cached_tokens: null,
    cost_usd: 0.0146004,
    connect_ms: 40,
    ttft_ms: 120,
    request_body_json: null,
    response_body_json: null,
    request_headers: null,
    response_headers: null,
    error_message: null,
    race_total: 1,
    race_attempts: 1,
    is_streaming: true,
    stream_complete: true,
    race_lost: false,
    stop_reason: "stop",
    compression_savings_pct: null,
    compression_techniques: null,
    client_response: true,
    prompt_tokens_estimated: false,
    completion_tokens_estimated: false,
    proxy_url: null,
    proxy_status: null,
    is_proxy_rotated: false,
    endpoint_kind: "chat",
    api_key_id: 1000037,
    audio_ms: null,
    created_at: "2026-11-15T10:00:00Z",
    ...overrides,
  };
}

function attempt(usage: RecentUsageRow): AttemptState {
  return {
    attemptKey: "fixture-1",
    requestId: usage.request_id,
    traceId: usage.trace_id,
    providerId: usage.provider_id,
    upstreamModelId: usage.upstream_model_id,
    startedAtMs: Date.parse(usage.created_at),
    updatedAtMs: Date.parse(usage.created_at),
    stage: "completed",
    stageSeq: 1,
    stageRank: 1,
    elapsedMsAtEvent: usage.total_ms,
    connectMs: usage.connect_ms,
    ttftMs: usage.ttft_ms,
    statusCode: usage.status_code,
    terminal: true,
    terminalKind: "completed",
    error: null,
    rowId: usage.id,
    row: usage,
    source: "db",
    endpointKind: "chat",
    apiKeyId: usage.api_key_id ?? null,
    sessionHash: null,
  };
}

const ALL_COLUMNS = null;

describe("log row Cost column", () => {
  it("shows the charged amount and 'Cobrado' for a proven charge", () => {
    const usage = row({ charged_units: 14_600, credits_debited: true, charge_state: "charged" });
    const tpl = renderLogRowHtml(attempt(usage), ALL_COLUMNS, Date.now());
    const text = textOf(tpl);
    // Amount and verdict are two spans: they must be visibly separated, not
    // glued into "$0.0146Cobrado".
    expect(text).toContain("$0.0146 · Cobrado");
    expect(text).not.toContain("$0.0146Cobrado");
    // The tooltip keeps the calculated price as secondary data.
    expect(toString(tpl)).toContain("Coste calculado");
  });

  it("shows $0.0000 + 'No cobrado' when a priced row charged nothing", () => {
    // The row shape that motivated this: cost_usd 0.0146004, nothing debited.
    const usage = row({ charged_units: 0, credits_debited: true, charge_state: "not_charged" });
    const text = textOf(renderLogRowHtml(attempt(usage), ALL_COLUMNS, Date.now()));
    expect(text).toContain("$0.0000 · No cobrado");
    expect(text).not.toContain("$0.0000No cobrado");
    // The price must not leak into the visible cost cell.
    expect(text).not.toContain("$0.0146");
  });

  it("shows 'Desconocido' with no amount for a historical row", () => {
    const usage = row({ charged_units: null, credits_debited: true, charge_state: "unknown" });
    const text = textOf(renderLogRowHtml(attempt(usage), ALL_COLUMNS, Date.now()));
    expect(text).toContain("Desconocido");
    expect(text).not.toContain("$0.0146");
    expect(text).not.toContain("$0.0000");
  });

  it("shows 'No cobrado' when no debit was ever attempted", () => {
    const usage = row({ charged_units: null, credits_debited: false });
    const text = textOf(renderLogRowHtml(attempt(usage), ALL_COLUMNS, Date.now()));
    expect(text).toContain("$0.0000 · No cobrado");
  });

  // Regression: a 499 (client cancelled mid-stream) is priced by the proxy —
  // 73002 prompt tokens, estimated, because the final usage block never arrived
  // — but if nothing was debited the Cost column must say "No cobrado $0.0000",
  // never the calculated price, and the tooltip must carry the exact numbers
  // (not the rounded "73k", not an amount that looks like real money moved).
  it("499 cancel: not charged, exact quantity in the tooltip, no fake charge", () => {
    const usage = row({
      status_code: 499,
      prompt_tokens: 73_002,
      prompt_tokens_estimated: true,
      completion_tokens: 512,
      completion_tokens_estimated: true,
      cost_usd: 0.034721,
      client_response: false,
      stream_complete: false,
      charged_units: 0,
      credits_debited: true,
      charge_state: "not_charged",
    });
    const tpl = renderLogRowHtml(attempt(usage), ALL_COLUMNS, Date.now());
    const markup = toString(tpl);
    const text = textOf(tpl);

    // The visible verdict: nothing was charged, and the price is not shown as money.
    expect(text).toContain("$0.0000 · No cobrado");
    expect(text).not.toContain("0.0347");

    // The tooltip keeps the exact calculated price (micro precision)...
    expect(markup).toContain("Coste calculado: $0.034721");
    // ...and the exact token counts behind the rounded 73k cell.
    expect(markup).toContain("Prompt 73002 (estimated)");
    expect(markup).toContain("Completion 512 (estimated)");
    expect(text).toContain("73k");
    expect(text).not.toContain("73002");
  });

  it("renders the charged verdict in the mobile card badge too", () => {
    const usage = row({ charged_units: 0, credits_debited: true, cached_tokens: null });
    const markup = toString(renderLogRowHtml(attempt(usage), ALL_COLUMNS, Date.now()));
    expect(markup).toContain("m-card-view");
    expect(markup).toContain("No cobrado");
  });
});
