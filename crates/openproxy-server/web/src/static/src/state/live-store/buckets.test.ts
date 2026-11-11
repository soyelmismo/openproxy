import { beforeEach, afterEach, describe, expect, it, vi } from "vitest";
import type { RecentUsageRow } from "../../lib/types/api.js";

// Fixtures model usage broadcasts: an attempt can be published before the
// executor marks and republishes that same row as the client response.
function row(id: number, status: number, clientResponse: boolean): RecentUsageRow {
  return {
    id, request_id: "combo-request", trace_id: `attempt-${id}`,
    provider_id: "provider", upstream_model_id: "model", status_code: status,
    total_ms: 100, prompt_tokens: null, completion_tokens: null, cached_tokens: null,
    cost_usd: null, connect_ms: null, ttft_ms: null, request_body_json: null,
    response_body_json: null, request_headers: null, response_headers: null,
    error_message: null, race_total: 3, race_attempts: 3, is_streaming: false,
    stream_complete: true, race_lost: false, stop_reason: null,
    compression_savings_pct: null, compression_techniques: null,
    client_response: clientResponse, prompt_tokens_estimated: false,
    completion_tokens_estimated: false, proxy_url: null, proxy_status: null,
    is_proxy_rotated: false, created_at: "2026-11-08T09:00:00.000Z",
  };
}

beforeEach(() => {
  vi.resetModules();
  vi.useFakeTimers();
  vi.setSystemTime(new Date("2026-11-08T09:00:00.000Z"));
});
afterEach(() => vi.useRealTimers());

describe("dashboard client response status buckets", () => {
  it("excludes internal combo failures and race losers when a retry succeeds", async () => {
    const { writeRowToBuckets, collectWindow } = await import("./buckets.js");
    writeRowToBuckets(row(1, 429, false));
    writeRowToBuckets(row(2, 503, false));
    writeRowToBuckets({ ...row(3, 200, false), race_lost: true });
    writeRowToBuckets(row(4, 200, true));
    for (const window of [60, 300, 1800] as const) {
      const buckets = collectWindow(window).buckets;
      expect(buckets.reduce((n, b) => n + b.status_2xx, 0)).toBe(1);
      expect(buckets.reduce((n, b) => n + b.status_4xx + b.status_5xx, 0)).toBe(0);
      // Keep attempt-level diagnostics, costs and throughput outside this fix.
      expect(buckets.reduce((n, b) => n + b.count, 0)).toBe(4);
    }
  });

  it("counts final client errors once, not every exhausted retry", async () => {
    const { writeRowToBuckets, collectWindow } = await import("./buckets.js");
    writeRowToBuckets(row(1, 429, false));
    writeRowToBuckets(row(2, 500, false));
    writeRowToBuckets(row(3, 502, true));
    writeRowToBuckets({ ...row(4, 401, true), request_id: "direct-request" });
    const buckets = collectWindow(300).buckets;
    expect(buckets.reduce((n, b) => n + b.status_4xx, 0)).toBe(1);
    expect(buckets.reduce((n, b) => n + b.status_5xx, 0)).toBe(1);
  });

  it("accepts false-to-true promotion and deduplicates repeats across time buckets", async () => {
    const { writeRowToBuckets, collectWindow } = await import("./buckets.js");
    writeRowToBuckets(row(1, 200, false));
    vi.advanceTimersByTime(1100);
    writeRowToBuckets(row(1, 200, true));
    vi.advanceTimersByTime(5100);
    writeRowToBuckets(row(1, 200, true));
    for (const window of [60, 300, 1800] as const) {
      expect(collectWindow(window).buckets.reduce((n, b) => n + b.status_2xx, 0)).toBe(1);
    }
  });

  it("never infers a client response from a missing flag or a predictive skip", async () => {
    const { writeRowToBuckets, collectWindow } = await import("./buckets.js");
    const legacy = row(1, 500, false);
    Reflect.deleteProperty(legacy, "client_response");
    writeRowToBuckets(legacy);
    writeRowToBuckets(row(2, 0, false));
    expect(collectWindow(300).buckets.reduce((n, b) => n + b.status_2xx + b.status_4xx + b.status_5xx, 0)).toBe(0);
  });
});
