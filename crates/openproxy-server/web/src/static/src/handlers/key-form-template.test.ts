// handlers/key-form-template.test.ts — structural contract for keyFormTemplate().
//
// The key editor is one lit-html form shared by the create/edit modal and the
// inline key-detail view. These tests pin the parts the async handlers rely on
// (field names, ids, hidden sentinels, expiry wiring) and the parts the redesign
// introduced (four task sections, valid HTML nesting, no duplicate ids), so a
// layout change cannot silently break buildKeyBodyFromForm or the pickers.

import { describe, it, expect, beforeEach } from "vitest";
import { render } from "lit-html";
import { keyFormTemplate } from "./key-handlers.js";

type KeyRow = NonNullable<Parameters<typeof keyFormTemplate>[0]["key"]>;

function mountCreate(inline = false): HTMLElement {
  const host = document.createElement("div");
  document.body.appendChild(host);
  render(keyFormTemplate({ mode: "create", wrapper: host, inline }), host);
  return host;
}

function mountEdit(key: KeyRow, inline = false): HTMLElement {
  const host = document.createElement("div");
  document.body.appendChild(host);
  render(keyFormTemplate({ mode: "edit", key, wrapper: host, inline }), host);
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
  "allowed_models",
  "allowed_combos",
  "only_whitelisted",
  "allowed_providers",
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

    expect((form.elements.namedItem("allowed_models") as HTMLInputElement).value).toBe("");
    expect((form.elements.namedItem("allowed_combos") as HTMLInputElement).value).toBe("");
    expect((form.elements.namedItem("blacklisted_models") as HTMLInputElement).value).toBe("");
    expect((form.elements.namedItem("only_whitelisted") as HTMLInputElement).checked).toBe(false);

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
      "model-picker-display",
      "allowed-combos-display",
      "blacklisted-models-display",
      "key-allowed-providers",
      "key-bl-providers",
      "key-bl-models-custom",
      "quick-wl-provider-select",
      "quick-bl-provider-select",
    ];
    for (const id of ids) {
      expect(form.querySelector(`#${id}`), id).not.toBeNull();
    }
    expect(form.querySelector("#key-label")?.getAttribute("name")).toBe("label");
    expect(form.querySelector("#key-allowed-providers")?.getAttribute("name")).toBe("allowed_providers");
    expect(form.querySelector("#key-bl-providers")?.getAttribute("name")).toBe("blacklisted_providers");
  });

  it("rehydrates edit-mode values and the three-state list sentinels", () => {
    const future = new Date(Date.now() + 10 * 24 * 60 * 60 * 1000 + 60 * 60 * 1000).toISOString();
    const form = formOf(mountEdit({
      id: 7,
      label: "prod",
      scopes: ["chat", "manage"],
      allowed_models: ["gpt-4o", "claude-3"],
      allowed_combos: [1, 2],
      blacklisted_providers: ["openai", "groq"],
      allowed_providers: ["xkiro"],
      blacklisted_models: ["gpt-4*", "*-preview"],
      only_whitelisted: true,
      expires_at: future,
    }));

    expect((form.elements.namedItem("label") as HTMLInputElement).value).toBe("prod");
    expect([...form.querySelectorAll<HTMLInputElement>('input[name="scopes"]:checked')].map((i) => i.value))
      .toEqual(["chat", "manage"]);
    expect((form.elements.namedItem("allowed_models") as HTMLInputElement).value).toBe("gpt-4o,claude-3");
    expect((form.elements.namedItem("allowed_combos") as HTMLInputElement).value).toBe("1,2");
    expect((form.elements.namedItem("allowed_providers") as HTMLInputElement).value).toBe("xkiro");
    expect((form.elements.namedItem("blacklisted_providers") as HTMLInputElement).value).toBe("openai, groq");
    expect((form.elements.namedItem("blacklisted_models") as HTMLInputElement).value).toBe("gpt-4*,*-preview");
    expect((form.elements.namedItem("only_whitelisted") as HTMLInputElement).checked).toBe(true);
    expect((form.elements.namedItem("expires_amount") as HTMLInputElement).value).toBe("10");
    expect((form.elements.namedItem("expires_amount") as HTMLInputElement).disabled).toBe(false);
    expect((form.elements.namedItem("expires_unit") as HTMLSelectElement).value).toBe("days");
    expect(form.querySelector<HTMLButtonElement>('button[type="submit"]')?.textContent).toBe("Save");
  });

  it("keeps the explicit empty-list sentinel (single space) for empty arrays", () => {
    const form = formOf(mountEdit({
      id: 8,
      scopes: ["chat"],
      allowed_models: [],
      allowed_combos: [],
      expires_at: null,
    }));
    expect((form.elements.namedItem("allowed_models") as HTMLInputElement).value).toBe(" ");
    expect((form.elements.namedItem("allowed_combos") as HTMLInputElement).value).toBe(" ");
  });

  it("groups fields into five labelled task sections in reading order", () => {
    const form = formOf(mountCreate());
    const sections = [...form.querySelectorAll<HTMLElement>("section.key-form-section")];
    expect(sections).toHaveLength(5);

    const expected = [
      ["key-section-identity", "Identity"],
      ["key-section-scopes", "Scopes"],
      ["key-section-model-access", "Model access"],
      ["key-section-restrictions", "Restrictions"],
      ["key-section-quotas", "Dollar quotas"],
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
    // Identity owns label + expiry; access owns both model fields; restrictions
    // owns both provider lists + blacklisted models.
    expect(sections[0]?.querySelectorAll('[name="label"], [name="expires_amount"], [name="expires_unit"]')).toHaveLength(3);
    expect(sections[1]?.querySelectorAll('input[name="scopes"]')).toHaveLength(4);
    expect(sections[2]?.querySelectorAll('[name="allowed_models"], [name="allowed_combos"], [name="only_whitelisted"]')).toHaveLength(3);
    expect(sections[3]?.querySelectorAll('[name="allowed_providers"], [name="blacklisted_providers"], [name="blacklisted_models"]')).toHaveLength(3);
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
    expect(wrapping).toHaveLength(8);
    for (const label of wrapping) {
      expect(label.querySelectorAll("input, select, textarea, button")).toHaveLength(1);
    }
    const forLabels = [...form.querySelectorAll<HTMLLabelElement>("label[for]")];
    expect(forLabels.map((label) => label.htmlFor)).toEqual(["key-label", "key-expires-amount"]);
    for (const label of forLabels) {
      expect(form.querySelector(`#${label.htmlFor}`), label.htmlFor).not.toBeNull();
    }
  });

  it("leaves the picker slots empty so the chip renderers own their content", () => {
    const form = formOf(mountCreate());
    for (const id of ["model-picker-display", "allowed-combos-display", "blacklisted-models-display"]) {
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
    expect(hints).toHaveLength(5);
    for (const hint of hints) {
      expect(hint.textContent!.trim().length).toBeLessThanOrEqual(60);
      expect(hint.closest(".field"), "hint inside a control").toBeNull();
    }
    const notes = [...form.querySelectorAll<HTMLElement>(".key-form-field-note")];
    expect(notes.length).toBeGreaterThanOrEqual(5);
    for (const note of notes) expect(note.textContent!.trim().length).toBeLessThanOrEqual(20);
    // The full explanation lives in a tooltip, not in a paragraph inside a control.
    const titled = [...form.querySelectorAll("label.scope-card[title]")];
    expect(titled.length).toBeGreaterThanOrEqual(4);
    for (const el of titled) expect(el.getAttribute("title")!.length).toBeGreaterThan(20);
  });

  it("pairs the provider lists and gives the model-pattern field the full row", () => {
    const grid = formOf(mountCreate()).querySelector(".key-form-grid--restrictions");
    expect(grid).not.toBeNull();
    const fields = [...grid!.querySelectorAll(":scope > .field")];
    expect(fields).toHaveLength(3);
    expect(fields.map((field) => field.querySelector(".field-label")?.textContent)).toEqual([
      "Allowed providers",
      "Blocked providers",
      "Blocked model patterns",
    ]);
    const wide = grid!.querySelectorAll(".field.key-form-field--wide");
    expect(wide).toHaveLength(1);
    expect(wide[0]!.querySelector('[name="blacklisted_models"]')).not.toBeNull();
    // The quick-add selects keep their ids for the provider handlers.
    expect(grid!.querySelector("#quick-wl-provider-select")?.getAttribute("aria-label")).toContain("allowlist");
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
      .toEqual(["H2", "H2", "H2", "H2", "H2"]);
  });
});
