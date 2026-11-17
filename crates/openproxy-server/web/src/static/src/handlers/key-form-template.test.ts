// handlers/key-form-template.test.ts — structural contract for keyFormTemplate().
//
// The key editor is one lit-html form shared by the create/edit modal and the
// inline key-detail view. These tests pin the parts the async handlers rely on
// (field names, ids, hidden sentinels, expiry wiring) and the parts the redesign
// introduced (four task sections, valid HTML nesting, no duplicate ids), so a
// layout change cannot silently break buildKeyBodyFromForm or the pickers.

import { describe, it, expect, beforeEach } from "vitest";
import { render } from "lit-html";
import { keyFormTemplate, buildKeyBodyFromForm } from "./key-handlers.js";

type KeyRow = NonNullable<Parameters<typeof keyFormTemplate>[0]["key"]>;

function mountCreate(inline = false): HTMLElement {
  const host = document.createElement("div");
  document.body.appendChild(host);
  render(keyFormTemplate({ mode: "create", tiers: [], wrapper: host, inline }), host);
  return host;
}

function mountEdit(
  key: KeyRow,
  quota?: Parameters<typeof keyFormTemplate>[0]["quota"],
  inline = false,
  tiers: Parameters<typeof keyFormTemplate>[0]["tiers"] = [],
): HTMLElement {
  const host = document.createElement("div");
  document.body.appendChild(host);
  render(keyFormTemplate({ mode: "edit", key, tiers, quota, wrapper: host, inline }), host);
  return host;
}

function formOf(host: HTMLElement): HTMLFormElement {
  const form = host.querySelector("form");
  if (!(form instanceof HTMLFormElement)) throw new Error("key form missing");
  return form;
}

const FIELD_NAMES = [
  "label",
  "scopes",
  "tier_id",
  "blacklisted_providers",
  "blacklisted_models",
  "expires_amount",
  "expires_unit",
];

describe("keyFormTemplate structure", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  it("renders one form with the complete create-mode field contract", () => {
    const host = mountCreate();
    expect(host.querySelectorAll("form")).toHaveLength(1);
    const form = formOf(host);

    for (const name of FIELD_NAMES) {
      expect(form.querySelectorAll(`[name="${name}"]`).length, name).toBeGreaterThan(0);
    }
    expect(form.querySelectorAll('input[name="scopes"]')).toHaveLength(4);
    expect([...form.querySelectorAll<HTMLInputElement>('input[name="scopes"]')].map((i) => i.value))
      .toEqual(["chat", "analytics", "manage", "read"]);
    expect([...form.querySelectorAll<HTMLInputElement>('input[name="scopes"]:checked')].map((i) => i.value))
      .toEqual(["chat"]);

    const label = form.elements.namedItem("label");
    expect(label).toBeInstanceOf(HTMLInputElement);
    expect((label as HTMLInputElement).required).toBe(true);
    expect((label as HTMLInputElement).value).toBe("");

    expect((form.elements.namedItem("blacklisted_models") as HTMLInputElement).value).toBe("");
    // The plan picker defaults to "keep current" / no plan on create.
    const tier = form.elements.namedItem("tier_id") as HTMLSelectElement;
    expect(tier.value).toBe("");
    expect([...tier.options].map((o) => o.value)).toEqual(["", "0"]);

    const amount = form.elements.namedItem("expires_amount") as HTMLInputElement;
    expect(amount.type).toBe("number");
    expect(amount.disabled).toBe(true);
    const unit = form.elements.namedItem("expires_unit") as HTMLSelectElement;
    expect([...unit.options].map((o) => o.value)).toEqual(["days", "months", "years", "never"]);

    const submit = form.querySelector<HTMLButtonElement>('button[type="submit"]');
    expect(submit?.textContent).toBe("Create key");
    expect(form.querySelectorAll('button[type="button"]').length).toBeGreaterThanOrEqual(2);
  });

  it("preserves every id and control the pickers and quick-add selects query", () => {
    const form = formOf(mountCreate());
    const ids = [
      "key-label",
      "key-expires-amount",
      "blacklisted-models-display",
      "key-bl-providers",
      "key-bl-models-custom",
      "quick-bl-provider-select",
    ];
    for (const id of ids) {
      expect(form.querySelector(`#${id}`), id).not.toBeNull();
    }
    expect(form.querySelector("#key-label")?.getAttribute("name")).toBe("label");
    expect(form.querySelector("#key-bl-providers")?.getAttribute("name")).toBe("blacklisted_providers");
  });

  it("rehydrates edit-mode values and the three-state list sentinels", () => {
    const future = new Date(Date.now() + 10 * 24 * 60 * 60 * 1000 + 60 * 60 * 1000).toISOString();
    const form = formOf(mountEdit({
      id: 7,
      label: "prod",
      scopes: ["chat", "manage"],
      tier_id: 3,
      blacklisted_providers: ["openai", "groq"],
      blacklisted_models: ["gpt-4*", "*-preview"],
      expires_at: future,
    }));

    expect((form.elements.namedItem("label") as HTMLInputElement).value).toBe("prod");
    expect([...form.querySelectorAll<HTMLInputElement>('input[name="scopes"]:checked')].map((i) => i.value))
      .toEqual(["chat", "manage"]);
    expect((form.elements.namedItem("blacklisted_providers") as HTMLInputElement).value).toBe("openai, groq");
    expect((form.elements.namedItem("blacklisted_models") as HTMLInputElement).value).toBe("gpt-4*,*-preview");
    expect((form.elements.namedItem("expires_amount") as HTMLInputElement).value).toBe("10");
    expect((form.elements.namedItem("expires_amount") as HTMLInputElement).disabled).toBe(false);
    expect((form.elements.namedItem("expires_unit") as HTMLSelectElement).value).toBe("days");
    expect(form.querySelector<HTMLButtonElement>('button[type="submit"]')?.textContent).toBe("Save");
  });

  it("groups fields into four labelled task sections in reading order", () => {
    const form = formOf(mountCreate());
    const sections = [...form.querySelectorAll<HTMLElement>("section.key-form-section")];
    expect(sections).toHaveLength(4);

    const expected = [
      ["key-section-identity", "Identity"],
      ["key-section-scopes", "Scopes"],
      ["key-section-plan", "Plan"],
      ["key-section-restrictions", "Restrictions"],
    ];
    sections.forEach((section, index) => {
      const entry = expected[index];
      expect(entry).toBeDefined();
      if (!entry) return;
      const [id, title] = entry;
      expect(section.getAttribute("aria-labelledby")).toBe(id);
      const heading = form.querySelector(`#${id}`);
      expect(heading?.tagName).toBe("H3");
      expect(heading?.textContent).toBe(title);
      expect(section.contains(heading)).toBe(true);
    });

    for (const name of FIELD_NAMES) {
      for (const control of form.querySelectorAll(`[name="${name}"]`)) {
        expect(control.closest("section.key-form-section"), `${name} outside a section`).not.toBeNull();
      }
    }
    // Identity owns label + expiry; scopes owns the scope cards; plan owns the
    // tier picker (the tier carries access + caps); restrictions owns the two
    // remaining per-key subtractive lists.
    expect(sections[0]?.querySelectorAll('[name="label"], [name="expires_amount"], [name="expires_unit"]')).toHaveLength(3);
    expect(sections[1]?.querySelectorAll('input[name="scopes"]')).toHaveLength(4);
    expect(sections[2]?.querySelectorAll('select[name="tier_id"]')).toHaveLength(1);
    expect(sections[3]?.querySelectorAll('[name="blacklisted_providers"], [name="blacklisted_models"]')).toHaveLength(2);
  });

  it("never emits invalid nesting", () => {
    const form = formOf(mountCreate());
    const invalid = [
      "form form",
      "button button",
      "button input",
      "button select",
      "button textarea",
      "label label",
      "label div",
      "label section",
      "label p",
      "label h3",
      "p div",
      "p section",
      "p form",
      "p h1",
      "p h2",
      "p h3",
    ];
    for (const selector of invalid) {
      expect(form.querySelectorAll(selector), selector).toHaveLength(0);
    }
    // Each scope/toggle card label wraps exactly one labelable control.
    const wrapping = [...form.querySelectorAll("label.scope-card")];
    expect(wrapping).toHaveLength(4);
    for (const label of wrapping) {
      expect(label.querySelectorAll("input, select, textarea, button")).toHaveLength(1);
    }
    const forLabels = [...form.querySelectorAll<HTMLLabelElement>("label[for]")];
    expect(forLabels.map((label) => label.htmlFor)).toEqual(["key-label", "key-expires-amount"]);
    for (const label of forLabels) {
      expect(form.querySelector(`#${label.htmlFor}`), label.htmlFor).not.toBeNull();
    }
  });

  it("leaves the blacklist picker slot empty so the chip renderer owns its content", () => {
    const form = formOf(mountCreate());
    for (const id of ["blacklisted-models-display"]) {
      const slot = form.querySelector(`#${id}`);
      expect(slot, id).not.toBeNull();
      expect(slot!.childElementCount, id).toBe(0);
      expect(slot!.textContent?.trim(), id).toBe("");
    }
    // A static "Loading..." placeholder used to survive the chip render.
    expect(form.textContent).not.toContain("Loading...");
  });

  it("keeps help text out of the controls: one short hint per section, detail in title", () => {
    const form = formOf(mountCreate());
    const hints = [...form.querySelectorAll<HTMLElement>(".key-form-section-hint")];
    expect(hints).toHaveLength(4);
    for (const hint of hints) {
      expect(hint.textContent!.trim().length).toBeLessThanOrEqual(40);
      expect(hint.closest(".field"), "hint inside a control").toBeNull();
    }
    const notes = [...form.querySelectorAll<HTMLElement>(".key-form-field-note")];
    expect(notes.length).toBeGreaterThanOrEqual(3);
    for (const note of notes) expect(note.textContent!.trim().length).toBeLessThanOrEqual(20);
    // The full explanation lives in a tooltip, not in a paragraph inside a control.
    const titled = [...form.querySelectorAll("label.scope-card[title]")];
    expect(titled).toHaveLength(4);
    for (const el of titled) expect(el.getAttribute("title")!.length).toBeGreaterThan(20);
  });

  it("keeps the blocked-provider quick-add and gives model patterns the full row", () => {
    const grid = formOf(mountCreate()).querySelector(".key-form-grid--restrictions");
    expect(grid).not.toBeNull();
    const fields = [...grid!.querySelectorAll(":scope > .field")];
    expect(fields).toHaveLength(2);
    expect(fields.map((field) => field.querySelector(".field-label")?.textContent)).toEqual([
      "Blocked providers",
      "Blocked model patterns",
    ]);
    const wide = grid!.querySelectorAll(".field.key-form-field--wide");
    expect(wide).toHaveLength(1);
    expect(wide[0]!.querySelector('[name="blacklisted_models"]')).not.toBeNull();
    expect(grid!.querySelector("#quick-bl-provider-select")?.getAttribute("aria-label")).toContain("blocklist");
  });

  it("emits unique ids (sections, pickers, and controls)", () => {
    const form = formOf(mountCreate());
    const ids = [...form.querySelectorAll<HTMLElement>("[id]")].map((el) => el.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it("renders the wide modal shell with a scrollable body and pinned footer", () => {
    const host = mountCreate(false);
    const modal = host.querySelector<HTMLElement>(".modal-bg > .modal.modal-key-editor");
    expect(modal).not.toBeNull();
    if (!modal) return;
    expect(modal.querySelector(".modal-header h2")?.textContent).toBe("Create API key");
    const form = formOf(host);
    expect(form.parentElement).toBe(modal);
    expect(form.querySelector(".modal-body.key-form-body")).not.toBeNull();
    expect(form.querySelector(".modal-footer")).not.toBeNull();
    expect(modal.querySelector(".modal-footer")).toBe(form.querySelector(".modal-footer"));
    expect(host.querySelector(".key-properties-fields")).toBeNull();
  });

  it("renders the inline shell without modal chrome and keeps the shared field wrapper", () => {
    const host = mountCreate(true);
    expect(host.querySelector(".modal-bg")).toBeNull();
    expect(host.querySelector(".modal-header")).toBeNull();
    expect(host.querySelector(".key-properties-form.key-form")).not.toBeNull();
    expect(host.querySelector(".key-properties-content")).not.toBeNull();
    expect(host.querySelector(".key-properties-fields.key-form-body")).not.toBeNull();
    expect(host.querySelector(".key-properties-footer")).not.toBeNull();
    expect(host.querySelector(".modal-footer")).toBeNull();
    // The inline view sits under the page h1, so its sections are h2.
    expect([...host.querySelectorAll(".key-form-section-title")].map((el) => el.tagName))
      .toEqual(["H2", "H2", "H2", "H2"]);
  });
});

describe("keyFormTemplate dollar caps (custom quotas)", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  it("renders an unlocked caps fieldset on create", () => {
    const form = formOf(mountCreate());
    const fieldset = form.querySelector<HTMLFieldSetElement>("#key-quota-fields");
    expect(fieldset, "caps fieldset missing").not.toBeNull();
    expect(fieldset!.disabled).toBe(false);
    expect(form.querySelector('input[name="quota_weekly"]')).not.toBeNull();
    expect(form.querySelector('input[name="quota_five_hour"]')).not.toBeNull();
    expect((form.elements.namedItem("quota_weekly") as HTMLInputElement).value).toBe("");
    expect(form.querySelector("#key-quota-hint")?.textContent).toContain("Custom dollar caps");
  });

  it("rehydrates saved caps and keeps the fieldset unlocked for planless keys", () => {
    const form = formOf(mountEdit({ id: 7 }, {
      tier_id: null,
      caps: { weekly_usd_cap: 7.5, five_hour_usd_cap: null },
    }));
    expect((form.elements.namedItem("quota_weekly") as HTMLInputElement).value).toBe("7.5");
    expect((form.elements.namedItem("quota_five_hour") as HTMLInputElement).value).toBe("");
    expect(form.querySelector<HTMLFieldSetElement>("#key-quota-fields")!.disabled).toBe(false);
    expect(form.querySelector("#key-quota-hint")?.textContent).toContain("Custom dollar caps");
  });

  it("locks the caps fieldset while a plan is linked and shows its caps read-only", () => {
    const form = formOf(mountEdit({ id: 7 }, {
      tier_id: 3,
      caps: { weekly_usd_cap: 25, five_hour_usd_cap: 1 },
    }));
    const fieldset = form.querySelector<HTMLFieldSetElement>("#key-quota-fields")!;
    expect(fieldset.disabled).toBe(true);
    expect((form.elements.namedItem("quota_weekly") as HTMLInputElement).value).toBe("25");
    expect((form.elements.namedItem("quota_five_hour") as HTMLInputElement).value).toBe("1");
    expect(form.querySelector("#key-quota-hint")?.textContent).toContain("linked plan");
  });

  it("locks the caps when the quota state is unavailable so a save cannot clear them", () => {
    const form = formOf(mountEdit({ id: 7 }, { locked: true }));
    const fieldset = form.querySelector<HTMLFieldSetElement>("#key-quota-fields")!;
    expect(fieldset.disabled).toBe(true);
    expect(form.querySelector("#key-quota-hint")?.textContent).toContain("unavailable");
    // The body builder must omit quota entirely for a locked fieldset.
    expect("quota" in (buildKeyBodyFromForm(form) ?? {})).toBe(false);
  });

  it("unlocks the caps when the picker switches to detach", () => {
    const host = mountEdit(
      { id: 7 },
      { tier_id: 3, caps: { weekly_usd_cap: 25, five_hour_usd_cap: 1 } },
      false,
      [{ id: 3, name: "Pro", price_usd: 30, credit_usd: 30, ttl_days: 30 }],
    );
    const form = formOf(host);
    const tier = form.elements.namedItem("tier_id") as HTMLSelectElement;
    const fieldset = form.querySelector<HTMLFieldSetElement>("#key-quota-fields")!;
    expect(fieldset.disabled).toBe(true);
    tier.value = "0";
    tier.dispatchEvent(new Event("change", { bubbles: true }));
    expect(fieldset.disabled).toBe(false);
    expect(form.querySelector("#key-quota-hint")?.textContent).toContain("Custom dollar caps");
    tier.value = "3";
    tier.dispatchEvent(new Event("change", { bubbles: true }));
    expect(fieldset.disabled).toBe(true);
    expect(form.querySelector("#key-quota-hint")?.textContent).toContain("selected plan");
  });
});
