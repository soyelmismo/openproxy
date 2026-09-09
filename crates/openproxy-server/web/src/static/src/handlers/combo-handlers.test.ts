// handlers/combo-handlers.test.ts — unit tests for buildComboBodyFromForm().
//
// Scope: the pure form→JSON payload builder extracted from `createCombo`.
// We DO NOT exercise the surrounding async handler (network +
// `requestUpdate` are not the builder's responsibility).
//
// Covering here:
//   - defaults when every field is blank
//   - omitting optional params when the user left them blank
//   - parsing `parseFloat` / `parseInt` for the conditional fields
//   - NaN fallback (junk in the box → field dropped)
//   - checkbox semantics: `preventive_rate_limit` only true when "on"

import { describe, it, expect, beforeEach } from "vitest";
import { buildComboBodyFromForm } from "./combo-handlers.js";

/** Build a minimal create-combo form. The defaults mirror the
 *  template rendered by `createComboTemplate()`. */
function makeComboForm(fields: {
  name?: string;
  strategy?: string;
  raceSize?: string;
  preventiveRateLimit?: boolean;
  priorityMode?: string;
  lkgpExplorationRate?: string;
  selectionWindowSecs?: string;
  cooldownMode?: string;
  cooldownBaseSecs?: string;
  cooldownFactor?: string;
  cooldownMaxSecs?: string;
} = {}): HTMLFormElement {
  const form = document.createElement("form");
  document.body.appendChild(form);

  const text = (name: string, value: string, type = "text"): void => {
    const input = document.createElement("input");
    input.name = name;
    input.type = type;
    input.value = value;
    form.appendChild(input);
  };
  const sel = (name: string, value: string): void => {
    const s = document.createElement("select");
    s.name = name;
    const opt = document.createElement("option");
    opt.value = value;
    s.appendChild(opt);
    s.value = value;
    form.appendChild(s);
  };
  const cb = (name: string, on: boolean): void => {
    const c = document.createElement("input");
    c.type = "checkbox";
    c.name = name;
    c.checked = on;
    form.appendChild(c);
  };

  text("name", fields.name ?? "");
  sel("strategy", fields.strategy ?? "priority");
  text("race_size", fields.raceSize ?? "1", "number");
  cb("preventive_rate_limit", fields.preventiveRateLimit ?? false);
  sel("priority_mode", fields.priorityMode ?? "strict");
  if (fields.lkgpExplorationRate !== undefined) text("lkgp_exploration_rate", fields.lkgpExplorationRate, "number");
  if (fields.selectionWindowSecs !== undefined) text("selection_window_secs", fields.selectionWindowSecs, "number");
  sel("cooldown_mode", fields.cooldownMode ?? "flat");
  if (fields.cooldownBaseSecs !== undefined) text("cooldown_base_secs", fields.cooldownBaseSecs, "number");
  if (fields.cooldownFactor !== undefined) text("cooldown_factor", fields.cooldownFactor, "number");
  if (fields.cooldownMaxSecs !== undefined) text("cooldown_max_secs", fields.cooldownMaxSecs, "number");
  return form;
}

describe("buildComboBodyFromForm", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  it("applies defaults for a freshly-rendered create-combo form", () => {
    const form = makeComboForm();
    const body = buildComboBodyFromForm(form);
    expect(body.name).toBe("");
    expect(body.strategy).toBe("priority");
    expect(body.race_size).toBe(1);
    expect(body.preventive_rate_limit).toBe(false);
    expect(body.priority_mode).toBe("strict");
    expect(body.cooldown_mode).toBe("flat");
    // Optional fields are omitted (undefined) when blank.
    expect(body.lkgp_exploration_rate).toBeUndefined();
    expect(body.selection_window_secs).toBeUndefined();
    expect(body.cooldown_base_secs).toBeUndefined();
    expect(body.cooldown_factor).toBeUndefined();
    expect(body.cooldown_max_secs).toBeUndefined();
  });

  it("parses race_size and the lkgp exploration rate", () => {
    const form = makeComboForm({
      raceSize: "4",
      priorityMode: "lkgp",
      lkgpExplorationRate: "0.25",
    });
    const body = buildComboBodyFromForm(form);
    expect(body.race_size).toBe(4);
    expect(body.priority_mode).toBe("lkgp");
    expect(body.lkgp_exploration_rate).toBe(0.25);
  });

  it("omits lkgp_exploration_rate when blank (would fail Rust f64 parse)", () => {
    const form = makeComboForm({ priorityMode: "lkgp", lkgpExplorationRate: "" });
    const body = buildComboBodyFromForm(form);
    expect(body.lkgp_exploration_rate).toBeUndefined();
  });

  it("drops NaN-coercing values from the body instead of sending NaN to Rust", () => {
    const form = makeComboForm({
      priorityMode: "lkgp",
      lkgpExplorationRate: "abc",
    });
    const body = buildComboBodyFromForm(form);
    expect(body.lkgp_exploration_rate).toBeUndefined();
  });

  it("includes cooldown_* only when cooldown_mode is exponential", () => {
    const flat = makeComboForm({
      cooldownMode: "flat",
      cooldownBaseSecs: "60",
      cooldownFactor: "2",
      cooldownMaxSecs: "3600",
    });
    expect(buildComboBodyFromForm(flat).cooldown_base_secs).toBeUndefined();
    expect(buildComboBodyFromForm(flat).cooldown_factor).toBeUndefined();
    expect(buildComboBodyFromForm(flat).cooldown_max_secs).toBeUndefined();

    const exp = makeComboForm({
      cooldownMode: "exponential",
      cooldownBaseSecs: "120",
      cooldownFactor: "3",
      cooldownMaxSecs: "7200",
    });
    const body = buildComboBodyFromForm(exp);
    expect(body.cooldown_base_secs).toBe(120);
    expect(body.cooldown_factor).toBe(3);
    expect(body.cooldown_max_secs).toBe(7200);
  });

  it("includes selection_window_secs only for least_used / p2c modes", () => {
    const strict = makeComboForm({
      priorityMode: "strict",
      selectionWindowSecs: "3600",
    });
    expect(buildComboBodyFromForm(strict).selection_window_secs).toBeUndefined();

    const p2c = makeComboForm({
      priorityMode: "p2c",
      selectionWindowSecs: "1800",
    });
    expect(buildComboBodyFromForm(p2c).selection_window_secs).toBe(1800);
  });

  it("captures preventive_rate_limit as true only when the checkbox is ticked", () => {
    expect(buildComboBodyFromForm(makeComboForm({ preventiveRateLimit: false })).preventive_rate_limit).toBe(false);
    expect(buildComboBodyFromForm(makeComboForm({ preventiveRateLimit: true })).preventive_rate_limit).toBe(true);
  });
});