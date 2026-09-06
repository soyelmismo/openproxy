// handlers/key-handlers.test.ts — unit tests for buildKeyBodyFromForm().
//
// Scope: the pure form→JSON payload builder extracted from
// `createKey` / `updateKey`. jsdom supplies the `HTMLFormElement`
// contract; we never hit the network (the fetch-backed `api()` is
// only called by the surrounding async handlers, not by the builder).
//
// Covering here:
//   - " " sentinel → `[]` (explicit clear-all) vs "" → `null` (no-op)
//   - comma splitting with trim + dedupe of blanks
//   - expiry amount/unit → ISO timestamp or null
//   - validation failure → null (no scopes)

import { describe, it, expect, beforeEach } from "vitest";
import { buildKeyBodyFromForm } from "./key-handlers.js";

/** Build a minimal-but-realistic key form. Fields omitted here stay
 *  absent from the DOM, exercising the `querySelector → null` branch
 *  of the builder. */
function makeKeyForm(fields: {
  label?: string;
  scopes?: string[];
  allowedModels?: string;
  blProviders?: string;
  blModels?: string;
  expiresAmount?: string;
  expiresUnit?: string;
} = {}): HTMLFormElement {
  const form = document.createElement("form");
  const append = (name: string, value: string, tag: "input" | "select" = "input", type = "text"): void => {
    if (tag === "select") {
      const sel = document.createElement("select");
      sel.name = name;
      // jsdom resets `select.value` to "" unless an <option> matches,
      // so seed one option with the requested value.
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
  if (fields.allowedModels !== undefined) append("allowed_models", fields.allowedModels, "input", "hidden");
  if (fields.blProviders !== undefined) append("blacklisted_providers", fields.blProviders);
  if (fields.blModels !== undefined) append("blacklisted_models", fields.blModels, "input", "hidden");
  if (fields.expiresAmount !== undefined) append("expires_amount", fields.expiresAmount, "input", "number");
  if (fields.expiresUnit !== undefined) append("expires_unit", fields.expiresUnit, "select");
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
    expect(body?.allowed_models).toBeNull(); // "" → null (no touch)
    expect(body?.blacklisted_providers).toBeNull();
    expect(body?.blacklisted_models).toBeNull();
    expect(body?.expires_at).toBeNull();     // "never" default
  });

  it("encodes ' ' (single space) in allowed_models as an explicit empty list", () => {
    const form = track(makeKeyForm({ scopes: ["read"], allowedModels: " " }));
    const body = buildKeyBodyFromForm(form);
    expect(body?.allowed_models).toEqual([]);
  });

  it("splits and trims a comma-separated blacklist, dropping empty items", () => {
    const form = track(makeKeyForm({
      scopes: ["chat"],
      blProviders: " openai , groq , ,  ",
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
});
