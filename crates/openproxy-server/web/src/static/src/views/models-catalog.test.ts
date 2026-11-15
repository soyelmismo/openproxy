// views/models-catalog.test.ts — schedule-aware pricing in the Models table:
// current band rates, per-second countdown, one timer/cleanup, server as_of
// offset, invalid → unavailable, and mobile card labels.

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { mountModelsCatalog } from "./models-catalog.js";
import * as apiModule from "../state/api.js";

const SCHEDULE = {
  timezone: "UTC",
  peak: { input_per_1m: 0.3, output_per_1m: 1.2, cached_per_1m: 0.006 },
  off_peak: { input_per_1m: 0.15, output_per_1m: 0.6, cached_per_1m: 0.003 },
  peak_windows: [{ days: [1], start: "01:00", end: "02:00" }],
  off_peak_dates: [],
};

interface TestEntry {
  id: string;
  display_name?: string;
  owned_by?: string;
  type?: string;
  family?: string;
  context_length?: number;
  max_output_tokens?: number;
  capabilities?: Record<string, boolean>;
  pricing_input_per_1m?: number;
  pricing_output_per_1m?: number;
  pricing_cached_per_1m?: number;
  pricing_schedule?: unknown;
  pricing_as_of?: number;
}

function entry(overrides: Partial<TestEntry> = {}): TestEntry {
  return {
    id: "combo:deepseek-chat",
    display_name: "deepseek-chat",
    owned_by: "combo",
    type: "combo",
    family: "combo",
    context_length: 128_000,
    max_output_tokens: 8_192,
    capabilities: { tools: true },
    pricing_input_per_1m: 0.3,
    pricing_output_per_1m: 1.2,
    pricing_cached_per_1m: 0.006,
    pricing_schedule: SCHEDULE,
    ...overrides,
  };
}

function cell(row: Element, price: "in" | "out" | "cached"): string {
  return row.querySelector(`[data-price="${price}"]`)?.textContent ?? "";
}
describe("models catalog pricing schedule", () => {
  let host: HTMLElement;
  let apiSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    host = document.createElement("div");
    document.body.appendChild(host);
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-01-05T01:59:59Z")); // Monday, one second before 02:00
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
    document.body.innerHTML = "";
  });

  it("shows the current band, flips locally at the transition, and never re-fetches", async () => {
    apiSpy = vi.spyOn(apiModule, "api").mockResolvedValue({ count: 1, data: [entry()] });
    const cleanup = await mountModelsCatalog(host);

    let row = host.querySelector('tr[data-mc-idx="0"]')!;
    expect(cell(row, "in")).toBe("$0.300");
    expect(cell(row, "out")).toBe("$1.20");
    expect(row.querySelector('[data-band]')?.textContent).toBe("peak");
    expect(row.querySelector('[data-countdown]')?.textContent).toBe("00:00:01");

    await vi.advanceTimersByTimeAsync(1_000);

    row = host.querySelector('tr[data-mc-idx="0"]')!;
    expect(cell(row, "in")).toBe("$0.150");
    expect(cell(row, "out")).toBe("$0.600");
    expect(row.querySelector('[data-band]')?.textContent).toBe("off-peak");
    expect(row.querySelector('[data-next]')?.textContent).toBe("→ peak");
    expect(apiSpy).toHaveBeenCalledTimes(1);

    cleanup?.();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("updates cells in place: the row node survives a per-second tick", async () => {
    vi.spyOn(apiModule, "api").mockResolvedValue({ count: 1, data: [entry()] });
    const cleanup = await mountModelsCatalog(host);
    const before = host.querySelector('tr[data-mc-idx="0"]');

    await vi.advanceTimersByTimeAsync(500);
    expect(host.querySelector('tr[data-mc-idx="0"]')).toBe(before);
    cleanup?.();
  });

  it("uses pricing_as_of as a server-clock offset (sampled once)", async () => {
    // Server clock is one hour behind local: 02:30 local is 01:30 server.
    vi.setSystemTime(new Date("2026-01-05T02:30:00Z"));
    const asOf = Math.floor(Date.parse("2026-01-05T01:30:00Z") / 1000);
    vi.spyOn(apiModule, "api").mockResolvedValue({
      count: 1,
      data: [entry({ pricing_as_of: asOf })],
    });
    const cleanup = await mountModelsCatalog(host);
    const row = host.querySelector('tr[data-mc-idx="0"]')!;
    expect(row.querySelector('[data-band]')?.textContent).toBe("peak");
    expect(cell(row, "in")).toBe("$0.300");
    cleanup?.();
  });

  it("renders a present-but-invalid schedule as unavailable, not as flat pricing", async () => {
    vi.spyOn(apiModule, "api").mockResolvedValue({
      count: 1,
      data: [entry({ pricing_schedule: { ...SCHEDULE, timezone: "Europe/Madrid" } })],
    });
    const cleanup = await mountModelsCatalog(host);
    const row = host.querySelector('tr[data-mc-idx="0"]')!;
    expect(row.querySelector('[data-band]')?.textContent).toBe("unavailable");
    expect(cell(row, "in")).toBe("—");
    expect(cell(row, "out")).toBe("—");
    cleanup?.();
  });

  it("keeps flat prices when no schedule is present and starts no timer", async () => {
    vi.spyOn(apiModule, "api").mockResolvedValue({
      count: 1,
      data: [entry({ pricing_schedule: null })],
    });
    const cleanup = await mountModelsCatalog(host);
    const row = host.querySelector('tr[data-mc-idx="0"]')!;
    expect(cell(row, "in")).toBe("$0.300");
    expect(row.querySelector('[data-band]')).toBeNull();
    expect(vi.getTimerCount()).toBe(0);
    cleanup?.();
  });

  it("renders mobile card labels for prices and band", async () => {
    vi.spyOn(apiModule, "api").mockResolvedValue({ count: 1, data: [entry()] });
    const cleanup = await mountModelsCatalog(host);
    const row = host.querySelector('tr[data-mc-idx="0"]')!;
    // Every price/band cell carries data-l so the <=760px card reflow labels it.
    expect(row.querySelector('td[data-l="$ in /M"]')).not.toBeNull();
    expect(row.querySelector('td[data-l="$ cached /M"]')).not.toBeNull();
    expect(row.querySelector('td[data-l="$ out /M"]')).not.toBeNull();
    expect(row.querySelector('td[data-l="Band"]')?.textContent).toContain("peak");
    expect(host.querySelector(".mc-card")?.className).toContain("density-normal");
    cleanup?.();
  });

  it("sorting by price uses the current band's rates", async () => {
    const cheap = entry({
      id: "cheap",
      owned_by: "x",
      pricing_input_per_1m: 0.1,
      pricing_output_per_1m: 0.2,
      pricing_schedule: null,
    });
    vi.spyOn(apiModule, "api").mockResolvedValue({ count: 2, data: [cheap, entry()] });
    const cleanup = await mountModelsCatalog(host);

    const header = Array.from(host.querySelectorAll("th")).find((th) => th.textContent?.includes("$ in /M"))!;
    header.dispatchEvent(new MouseEvent("click", { bubbles: true }));

    const ids = Array.from(host.querySelectorAll("tr[data-mc-idx] td.mc-model .mc-id")).map(
      (el) => el.textContent,
    );
    // Peak $0.30 (combo) vs flat $0.10 (cheap): cheap first.
    expect(ids).toEqual(["cheap", "combo:deepseek-chat"]);
    cleanup?.();
  });
});
