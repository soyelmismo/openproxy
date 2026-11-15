// views/combos/pricing-schedule-editor.test.ts — DOM wiring for the UTC
// schedule editor: hydration, preset (fills only), validation, disable→null,
// holiday rows, Escape, and the exact PATCH payload.

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import type { Combo } from "../../lib/types/api.js";
import type { PricingSchedule } from "../../lib/pricing-schedule.js";
import { showPricingScheduleEditor } from "./pricing-schedule-editor.js";
import * as apiModule from "../../state/api.js";
import * as toastModule from "../../components/toast.js";

const STORED = {
  timezone: "UTC",
  peak: { input_per_1m: 0.3, output_per_1m: 1.2, cached_per_1m: 0.006 },
  off_peak: { input_per_1m: 0.15, output_per_1m: 0.6, cached_per_1m: 0.003 },
  peak_windows: [
    { days: [1, 2, 3, 4, 5], start: "01:00", end: "04:00" },
    { days: [1, 2, 3, 4, 5], start: "06:00", end: "10:00" },
  ],
  off_peak_dates: ["2026-02-17"],
};

function makeCombo(overrides: Partial<Combo> = {}): Combo {
  return {
    id: 7,
    name: "deepseek-chat",
    strategy: "priority",
    race_size: 1,
    created_at: "2026-01-01T00:00:00Z",
    pricing_input_per_1m: 1,
    pricing_output_per_1m: 2,
    pricing_cached_per_1m: null,
    pricing_schedule: STORED,
    ...overrides,
  } as Combo;
}

function input(selector: string): HTMLInputElement {
  const el = document.querySelector<HTMLInputElement>(selector);
  if (!el) throw new Error(`missing input ${selector}`);
  return el;
}

function setInput(selector: string, value: string): void {
  const el = input(selector);
  el.value = value;
  el.dispatchEvent(new Event("input", { bubbles: true }));
}

function setCheckbox(selector: string, checked: boolean): void {
  const el = input(selector);
  el.checked = checked;
  el.dispatchEvent(new Event("change", { bubbles: true }));
}

function buttonByText(text: string): HTMLButtonElement {
  const el = Array.from(document.querySelectorAll("button")).find(
    (b) => (b.textContent ?? "").trim().includes(text),
  );
  if (!el) throw new Error(`missing button ${text}`);
  return el;
}

async function flush(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 0));
  await new Promise((resolve) => setTimeout(resolve, 0));
}

function patchBodies(spy: { mock: { calls: unknown[][] } }): Record<string, unknown>[] {
  return spy.mock.calls.map((call) => {
    const init = call[1] as { body?: string } | undefined;
    return JSON.parse(init?.body ?? "{}") as Record<string, unknown>;
  });
}

describe("pricing schedule editor", () => {
  let apiSpy: ReturnType<typeof vi.spyOn>;
  let toastSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    document.body.innerHTML = "";
    apiSpy = vi.spyOn(apiModule, "api").mockResolvedValue({});
    toastSpy = vi.spyOn(toastModule, "showToast").mockReturnValue(undefined);
  });

  afterEach(() => {
    vi.restoreAllMocks();
    document.body.innerHTML = "";
  });

  it("hydrates from the stored schedule and is labelled as a modal", () => {
    showPricingScheduleEditor({ combo: makeCombo() });
    const dialog = document.querySelector('[role="dialog"]');
    expect(dialog).not.toBeNull();
    expect(dialog?.getAttribute("aria-modal")).toBe("true");
    expect(document.getElementById("ps-title")?.textContent).toContain("deepseek-chat");
    expect(input('[data-rate="peak.input"]').value).toBe("0.3");
    expect(input('[data-rate="off_peak.cached"]').value).toBe("0.003");
    expect(document.querySelectorAll(".ps-window")).toHaveLength(2);
    expect(input("#ps-date-0").value).toBe("2026-02-17");
    expect(document.activeElement?.id).toBe("ps-enabled");
  });

  it("saves the edited schedule and calls onSaved", async () => {
    const onSaved = vi.fn();
    showPricingScheduleEditor({ combo: makeCombo(), onSaved });
    setInput('[data-rate="peak.input"]', "0.4");
    buttonByText("Save schedule").click();
    await flush();

    expect(apiSpy).toHaveBeenCalledTimes(1);
    expect(apiSpy.mock.calls[0]?.[0]).toBe("/combos/7");
    const body = patchBodies(apiSpy)[0]!;
    const schedule = body["pricing_schedule"] as typeof STORED;
    expect(schedule.timezone).toBe("UTC");
    expect(schedule.peak.input_per_1m).toBe(0.4);
    expect(schedule.off_peak.input_per_1m).toBe(0.15);
    expect(schedule.off_peak_dates).toEqual(["2026-02-17"]);
    expect(onSaved).toHaveBeenCalledTimes(1);
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    expect(toastSpy).toHaveBeenCalledWith("Pricing schedule saved", "success");
  });

  it("disable + save sends pricing_schedule: null", async () => {
    showPricingScheduleEditor({ combo: makeCombo() });
    setCheckbox("#ps-enabled", false);
    buttonByText("Save schedule").click();
    await flush();

    expect(apiSpy).toHaveBeenCalledTimes(1);
    expect(patchBodies(apiSpy)[0]).toEqual({ pricing_schedule: null });
    expect(toastSpy).toHaveBeenCalledWith("Pricing schedule cleared", "success");
  });

  it("DeepSeek preset fills the form without saving, then saves exactly it", async () => {
    showPricingScheduleEditor({ combo: makeCombo({ pricing_schedule: null }) });
    buttonByText("Load DeepSeek preset").click();

    expect(apiSpy).not.toHaveBeenCalled();
    expect(input('[data-rate="peak.input"]').value).toBe("0.3");
    expect(input('[data-rate="peak.cached"]').value).toBe("0.006");
    expect(input('[data-rate="peak.output"]').value).toBe("1.2");
    expect(input('[data-rate="off_peak.input"]').value).toBe("0.15");
    expect(input('[data-rate="off_peak.output"]').value).toBe("0.6");
    expect(document.querySelectorAll(".ps-window")).toHaveLength(2);
    expect(input('[data-window-start="0"]').value).toBe("01:00");
    expect(input('[data-window-end="1"]').value).toBe("10:00");
    expect(document.querySelectorAll(".ps-date-row")).toHaveLength(0);

    buttonByText("Save schedule").click();
    await flush();
    const body = patchBodies(apiSpy)[0]!;
    expect(body["pricing_schedule"]).toEqual({
      timezone: "UTC",
      peak: { input_per_1m: 0.3, output_per_1m: 1.2, cached_per_1m: 0.006 },
      off_peak: { input_per_1m: 0.15, output_per_1m: 0.6, cached_per_1m: 0.003 },
      peak_windows: [
        { days: [1, 2, 3, 4, 5], start: "01:00", end: "04:00" },
        { days: [1, 2, 3, 4, 5], start: "06:00", end: "10:00" },
      ],
      off_peak_dates: [],
    });
  });

  it("adds and removes holiday dates and peak windows", async () => {
    showPricingScheduleEditor({ combo: makeCombo({ pricing_schedule: null }) });
    setCheckbox("#ps-enabled", true);

    buttonByText("Add date").click();
    setInput("#ps-date-0", "2026-10-01");
    buttonByText("Add date").click();
    setInput("#ps-date-1", "2026-10-02");
    expect(document.querySelectorAll(".ps-date-row")).toHaveLength(2);

    document.querySelector<HTMLButtonElement>('[aria-label="Remove off-peak date 1"]')?.click();
    expect(document.querySelectorAll(".ps-date-row")).toHaveLength(1);

    buttonByText("Add window").click();
    expect(document.querySelectorAll(".ps-window")).toHaveLength(1);
    document.querySelector<HTMLButtonElement>('[aria-label="Remove peak window 1"]')?.click();
    expect(document.querySelectorAll(".ps-window")).toHaveLength(0);

    // One remaining window is needed for a meaningful save; re-add it.
    buttonByText("Add window").click();
    setInput('[data-rate="peak.input"]', "0.3");
    setInput('[data-rate="peak.output"]', "1.2");
    setInput('[data-rate="off_peak.input"]', "0.15");
    setInput('[data-rate="off_peak.output"]', "0.6");
    buttonByText("Save schedule").click();
    await flush();

    const schedule = patchBodies(apiSpy)[0]!["pricing_schedule"] as typeof STORED;
    expect(schedule.off_peak_dates).toEqual(["2026-10-02"]);
    expect(schedule.peak_windows).toEqual([{ days: [1, 2, 3, 4, 5], start: "09:00", end: "17:00" }]);
  });

  it("blocks save and marks the field when validation fails", async () => {
    showPricingScheduleEditor({ combo: makeCombo() });
    setInput('[data-rate="peak.input"]', "");
    buttonByText("Save schedule").click();
    await flush();

    expect(apiSpy).not.toHaveBeenCalled();
    expect(document.querySelector(".ps-errors")?.getAttribute("role")).toBe("alert");
    expect(input('[data-rate="peak.input"]').getAttribute("aria-invalid")).toBe("true");
    expect(document.querySelector('[role="dialog"]')).not.toBeNull();
  });

  it("surfaces a stored invalid schedule as unavailable, not as flat", () => {
    const invalid = JSON.parse(
      JSON.stringify(STORED).replace('"UTC"', '"Europe/Madrid"'),
    ) as PricingSchedule;
    const combo = makeCombo({ pricing_schedule: invalid });
    showPricingScheduleEditor({ combo });
    expect(document.querySelector(".ps-warning")?.textContent).toContain("could not be read");
    expect(input("#ps-enabled").checked).toBe(false);
  });

  it("Escape closes without a request", () => {
    showPricingScheduleEditor({ combo: makeCombo() });
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    expect(apiSpy).not.toHaveBeenCalled();
  });
});
