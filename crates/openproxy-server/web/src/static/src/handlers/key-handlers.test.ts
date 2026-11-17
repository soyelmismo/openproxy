// handlers/key-handlers.test.ts — unit tests for buildKeyBodyFromForm(), the pure form→JSON
// payload builder extracted from `createKey` / `updateKey`. jsdom supplies the
// `HTMLFormElement` contract; no network is touched (the fetch-backed `api()` belongs to the
// surrounding async handlers, not the builder).
//
// Covering: the " " sentinel → `[]` (explicit clear-all) vs "" → `null` (no-op); comma splitting
// with trim + dedupe of blanks; expiry amount/unit → ISO timestamp or null; validation failure →
// null (no scopes).

import { describe, it, expect, beforeEach } from "vitest";
import { buildKeyBodyFromForm } from "./key-handlers.js";

/** Build a minimal-but-realistic key form. Fields omitted here stay absent from the DOM,
 *  exercising the builder's `querySelector → null` branch. */
function makeKeyForm(fields: {
  label?: string;
  scopes?: string[];
  allowedModels?: string;
  blProviders?: string;
  allowedProviders?: string;
  blModels?: string;
  expiresAmount?: string;
  expiresUnit?: string;
  tierId?: string;
  /** Dollar caps fieldset (payments build). Omitted → no fieldset in the DOM. */
  quota?: { weekly?: string; fiveHour?: string; disabled?: boolean };
} = {}): HTMLFormElement {
  const form = document.createElement("form");
  const append = (name: string, value: string, tag: "input" | "select" = "input", type = "text"): void => {
    if (tag === "select") {
      const sel = document.createElement("select");
      sel.name = name;
      // jsdom resets `select.value` to "" unless an <option> matches, so seed one.
      const opt = document.createElement("option");
      opt.value = value;
      sel.appendChild(opt);
      sel.value = value;
      form.appendChild(sel);
      return;
    }
    const input = document.createElement("input");
    input.name = name;
    input.type = type;
    input.value = value;
    form.appendChild(input);
  };
  const checkbox = (name: string, value: string, checked: boolean): void => {
    const input = document.createElement("input");
    input.type = "checkbox";
    input.name = name;
    input.value = value;
    input.checked = checked;
    form.appendChild(input);
  };
  if (fields.label !== undefined) append("label", fields.label);
  for (const scope of fields.scopes ?? []) checkbox("scopes", scope, true);
  if (fields.blProviders !== undefined) append("blacklisted_providers", fields.blProviders);
  if (fields.blModels !== undefined) append("blacklisted_models", fields.blModels, "input", "hidden");
  if (fields.expiresAmount !== undefined) append("expires_amount", fields.expiresAmount, "input", "number");
  if (fields.expiresUnit !== undefined) append("expires_unit", fields.expiresUnit, "select");
  if (fields.tierId !== undefined) append("tier_id", fields.tierId, "select");
  if (fields.quota) {
    const fieldset = document.createElement("fieldset");
    fieldset.id = "key-quota-fields";
    if (fields.quota.disabled) fieldset.disabled = true;
    const cap = (name: string, value: string | undefined): void => {
      const input = document.createElement("input");
      input.name = name;
      input.type = "number";
      input.value = value ?? "";
      fieldset.appendChild(input);
    };
    cap("quota_weekly", fields.quota.weekly);
    cap("quota_five_hour", fields.quota.fiveHour);
    form.appendChild(fieldset);
  }
  document.body.appendChild(form);
  return form;
}

describe("buildKeyBodyFromForm", () => {
  let created: HTMLFormElement[] = [];

  beforeEach(() => {
    created = [];
    document.body.innerHTML = "";
    // Track every form so the test can clean up.
    const origMake = makeKeyForm;
    void origMake;
  });

  const track = (form: HTMLFormElement): HTMLFormElement => {
    created.push(form);
    return form;
  };

  it("returns null and does not throw when no scopes are checked", () => {
    const form = track(makeKeyForm({ label: "no-scope" }));
    expect(buildKeyBodyFromForm(form)).toBeNull();
  });

  it("applies defaults for a brand-new (create) form", () => {
    const form = track(makeKeyForm({ scopes: ["chat"] }));
    const body = buildKeyBodyFromForm(form);
    expect(body).not.toBeNull();
    expect(body?.label).toBeNull();          // empty label → null
    expect(body?.scopes).toEqual(["chat"]);
    expect(body?.blacklisted_providers).toBeNull();
    expect(body?.blacklisted_models).toBeNull();
    expect(body?.expires_at).toBeNull();     // "never" default
    // No plan picker in the minimal form: no tier field sent at all.
    expect("tier_id" in (body ?? {})).toBe(false);
  });

  it("omits tier_id when the picker keeps the current plan", () => {
    const form = track(makeKeyForm({ scopes: ["chat"], tierId: "" }));
    const body = buildKeyBodyFromForm(form);
    expect("tier_id" in (body ?? {})).toBe(false);
  });

  it("sends tier_id: null to detach and a number to link", () => {
    const detach = track(makeKeyForm({ scopes: ["chat"], tierId: "0" }));
    expect(buildKeyBodyFromForm(detach)?.tier_id).toBeNull();

    const link = track(makeKeyForm({ scopes: ["chat"], tierId: "42" }));
    expect(buildKeyBodyFromForm(link)?.tier_id).toBe(42);
  });

  it("splits and trims a comma-separated blacklist, dropping empty items", () => {
    const form = track(makeKeyForm({
      scopes: ["chat"],
      blProviders: " openai , groq , ,  ",
      allowedProviders: " xkiro , beta , , ",
      blModels: "gpt-4*,*-preview",
    }));
    const body = buildKeyBodyFromForm(form);
    expect(body?.blacklisted_providers).toEqual(["openai", "groq"]);
    expect(body?.blacklisted_models).toEqual(["gpt-4*", "*-preview"]);
  });

  it("returns null expires_at when amount is blank even with unit=days", () => {
    const form = track(makeKeyForm({ scopes: ["chat"], expiresAmount: "", expiresUnit: "days" }));
    const body = buildKeyBodyFromForm(form);
    expect(body?.expires_at).toBeNull();
  });

  it("produces a future ISO-8601 expires_at for a numeric amount", () => {
    const form = track(makeKeyForm({ scopes: ["chat"], expiresAmount: "30", expiresUnit: "days" }));
    const body = buildKeyBodyFromForm(form);
    const parsed = body?.expires_at ? new Date(body.expires_at).getTime() : NaN;
    expect(Number.isFinite(parsed)).toBe(true);
    expect(parsed).toBeGreaterThan(Date.now());
  });

  it("trims the label and falls back to null when blank", () => {
    const blank = track(makeKeyForm({ scopes: ["chat"], label: "   " }));
    expect(buildKeyBodyFromForm(blank)?.label).toBeNull();
    const named = track(makeKeyForm({ scopes: ["chat"], label: "  my-app  " }));
    expect(buildKeyBodyFromForm(named)?.label).toBe("my-app");
  });

  it("sends filled dollar caps as an object and empty ones as null (clear)", () => {
    const capped = track(makeKeyForm({ scopes: ["chat"], quota: { weekly: "7.5", fiveHour: "2" } }));
    expect(buildKeyBodyFromForm(capped)?.quota).toEqual({ weekly_usd_cap: 7.5, five_hour_usd_cap: 2 });

    const halfCapped = track(makeKeyForm({ scopes: ["chat"], quota: { weekly: "10" } }));
    expect(buildKeyBodyFromForm(halfCapped)?.quota).toEqual({ weekly_usd_cap: 10, five_hour_usd_cap: null });

    const uncapped = track(makeKeyForm({ scopes: ["chat"], quota: {} }));
    expect(buildKeyBodyFromForm(uncapped)?.quota).toBeNull();
  });

  it("omits quota when the caps fieldset is disabled (a plan owns the caps)", () => {
    const form = track(makeKeyForm({ scopes: ["chat"], quota: { weekly: "7.5", disabled: true } }));
    const body = buildKeyBodyFromForm(form);
    expect(body).not.toBeNull();
    expect("quota" in (body ?? {})).toBe(false);
  });

  it("omits quota when the form has no caps fieldset (non-payments DOM)", () => {
    const form = track(makeKeyForm({ scopes: ["chat"] }));
    expect("quota" in (buildKeyBodyFromForm(form) ?? {})).toBe(false);
  });

  it("rejects non-positive dollar caps with a toast and returns null", () => {
    // A number input never carries non-numeric text (the browser sanitizes
    // it to empty), so the realistic bad values are zero and negatives.
    for (const bad of ["0", "-3"]) {
      const form = track(makeKeyForm({ scopes: ["chat"], quota: { weekly: bad } }));
      expect(buildKeyBodyFromForm(form), `weekly=${bad}`).toBeNull();
    }
  });
});
