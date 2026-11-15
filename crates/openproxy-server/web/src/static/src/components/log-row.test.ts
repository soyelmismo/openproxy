// One usage cost across desktop, mobile, detail and debug export.
import { describe, it, expect } from "vitest";
import { render } from "lit-html";
import { renderLogRowHtml } from "./log-row.js";
import { renderLogDetailModal } from "./log-detail/modal.js";
import { buildDebugBundle } from "./log-detail/debug-bundle.js";
import type { AttemptState } from "../state/live-logs-store.js";
import type { RecentUsageRow } from "../lib/types/usage.js";
import type { LogDetailLog } from "./log-detail/state.js";

function usage(overrides: Partial<RecentUsageRow> = {}): RecentUsageRow {
  return {
    id: 1254244,
    request_id: "cost-regression",
    trace_id: "cost-regression",
    provider_id: "fixture",
    upstream_model_id: "fixture",
    status_code: 200,
    total_ms: 1200,
    prompt_tokens: 73002,
    completion_tokens: 512,
    prompt_tokens_estimated: true,
    completion_tokens_estimated: true,
    cost_usd: 0.0251,
    charged_units: 0,
    credits_debited: true,
    charge_state: "not_charged",
    client_response: true,
    cached_tokens: null,
    is_streaming: true,
    stream_complete: true,
    race_lost: false,
    endpoint_kind: "chat",
    created_at: "2026-11-15T10:00:00Z",
    ...overrides,
  } as RecentUsageRow;
}

function renderRow(row: RecentUsageRow): HTMLDivElement {
  const attempt = {
    attemptKey: "cost-regression:1",
    requestId: row.request_id,
    traceId: row.trace_id,
    providerId: row.provider_id,
    upstreamModelId: row.upstream_model_id,
    startedAtMs: Date.parse(row.created_at),
    updatedAtMs: Date.parse(row.created_at),
    stage: "completed",
    stageSeq: 1,
    stageRank: 1,
    elapsedMsAtEvent: row.total_ms,
    terminal: true,
    terminalKind: "completed",
    statusCode: row.status_code,
    error: null,
    rowId: row.id,
    row,
    source: "db",
  } as AttemptState;
  const host = document.createElement("div");
  render(renderLogRowHtml(attempt, null, Date.now()), host);
  return host;
}

describe("single live log usage cost", () => {
  it.each([
    { charged_units: 0, credits_debited: true },
    { charged_units: null, credits_debited: false },
    { charged_units: null, credits_debited: true },
    { charged_units: 10000, credits_debited: true },
  ])("uses cost_usd regardless of prepaid ledger state %j", (facts) => {
    const host = renderRow(usage(facts));
    const cells = host.querySelectorAll(".log-cost");
    expect(cells.length).toBe(2);
    for (const cell of cells) {
      expect(cell.textContent).toBe("$0.0251");
      expect(cell.getAttribute("title")).toBe("Cost: $0.025100");
    }
    expect(host.textContent).not.toMatch(/No cobrado|Desconocido|Cobrado/);
    expect(host.innerHTML).not.toContain("Coste calculado");
  });

  it("keeps cancellation cost and exact estimated token quantities", () => {
    const host = renderRow(usage({ status_code: 499, cost_usd: 0.034721, stream_complete: false }));
    expect(host.querySelector(".log-cost")?.textContent).toBe("$0.0347");
    expect(host.innerHTML).toContain("Cost: $0.034721");
    expect(host.innerHTML).toContain("Prompt 73002 (estimated)");
    expect(host.innerHTML).toContain("Completion 512 (estimated)");
  });

  it.each([null, 0, 0.000045])("preserves missing, zero and micro costs %s", (cost) => {
    const host = renderRow(usage({ cost_usd: cost }));
    const expected = cost === null ? "—" : cost === 0 ? "$0.0000" : "$0.000045";
    expect(host.querySelector(".log-cost")?.textContent).toBe(expected);
  });

  it("shows a single Cost in the modal instead of a duplicate debit field", () => {
    const host = document.createElement("div");
    render(renderLogDetailModal(usage() as LogDetailLog), host);
    const labels = [...host.querySelectorAll("strong")].filter((node) => node.textContent === "Cost:");
    expect(labels.length).toBe(1);
    expect(labels[0]?.parentElement?.textContent).toContain("$0.0251");
    expect(host.textContent).not.toMatch(/No cobrado|Coste calculado|Desconocido/);
  });

  it("exports one cost without the misleading debit summary", () => {
    const bundle = buildDebugBundle(usage() as LogDetailLog);
    expect(bundle).toContain("**Cost USD:** 0.0251");
    expect(bundle).not.toContain("**Charged:**");
    expect(bundle).not.toContain("calculated price");
  });
});
