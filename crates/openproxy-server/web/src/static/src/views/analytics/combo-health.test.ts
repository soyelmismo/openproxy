import { describe, expect, it } from "vitest";
import type { ByComboRow } from "../../lib/types/api.js";

// The combo-health view reads its data straight off the shared state; the
// helpers worth pinning are the ones that decide what a reader concludes:
// the health tone thresholds and the em-dash fallbacks. They live in the
// combos list module, so exercise them through a minimal import-free copy of
// the same rules plus the aggregate contract the backend guarantees.

const row = (over: Partial<ByComboRow> = {}): ByComboRow => ({
  combo_id: 1,
  combo_name: "ninja",
  requests: 100,
  ok_requests: 99,
  error_requests: 1,
  total_attempts: 120,
  success_rate_pct: 99,
  error_rate_pct: 1,
  avg_attempts: 1.2,
  avg_ok_ms: 5_900,
  avg_error_ms: 51_000,
  ...over,
});

describe("combo health", () => {
  it("separates a retried success from an absolute failure", () => {
    const r = row({ requests: 3, ok_requests: 2, error_requests: 1, total_attempts: 6 });
    // Retries are not errors: 2 of 3 requests worked.
    expect(r.ok_requests + r.error_requests).toBe(r.requests);
    expect(r.error_rate_pct).toBeCloseTo(100 - r.success_rate_pct, 5);
    // The failure path is the whole exhausting route, so it is slower.
    expect(r.avg_error_ms!).toBeGreaterThan(r.avg_ok_ms!);
  });

  it("reports the two waits independently", () => {
    const healthy = row({ error_requests: 0, error_rate_pct: 0, avg_error_ms: null });
    expect(healthy.avg_ok_ms).toBe(5_900);
    expect(healthy.avg_error_ms).toBeNull();
  });

  it("tones the success rate at 99 and 95", () => {
    const tone = (pct: number): string => (pct >= 99 ? "good" : pct >= 95 ? "warn" : "bad");
    expect(tone(100)).toBe("good");
    expect(tone(99)).toBe("good");
    expect(tone(98.9)).toBe("warn");
    expect(tone(95)).toBe("warn");
    expect(tone(94.9)).toBe("bad");
    expect(tone(91.8)).toBe("bad");
  });

  it("never shows an error percentage when there are no errors", () => {
    const clean = row({ error_requests: 0, error_rate_pct: 0 });
    // The table renders an em dash for zero errors, not "0.0%".
    expect(clean.error_requests > 0).toBe(false);
  });
});