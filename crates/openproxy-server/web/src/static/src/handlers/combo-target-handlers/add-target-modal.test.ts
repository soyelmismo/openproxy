// handlers/combo-target-handlers/add-target-modal.test.ts — unit tests
// for buildSubComboTargetBodyFromForm().
//
// Scope: pure form→JSON builder extracted from the "sub-combo" branch
// of `addTarget`. The "model multi-select" branch is intentionally
// NOT extracted because it depends on DOM scanning (queried row-level
// per-model selects/inputs) and `state.models` — too entangled with
// side-effects for a pure unit test; the integration / e2e path
// covers it.
//
// Covering here:
//   - `provider_id` is always the literal "combo" marker
//   - `account_id` and `model_row_id` are `null` (XOR with sub_combo_id)
//   - sub_combo_id / priority_order parseInt faithfully, including
//     NaN pass-through (the caller validates NaN before POST).

import { describe, it, expect, beforeEach } from "vitest";
import { buildSubComboTargetBodyFromForm } from "./add-target-modal.js";

function makeSubComboForm(fields: {
  subComboId?: string;
  priorityOrder?: string;
} = {}): HTMLFormElement {
  const form = document.createElement("form");
  document.body.appendChild(form);

  const input = (name: string, value: string): void => {
    const i = document.createElement("input");
    i.name = name;
    i.value = value;
    form.appendChild(i);
  };
  input("sub_combo_id", fields.subComboId ?? "7");
  input("priority_order", fields.priorityOrder ?? "100");
  return form;
}

describe("buildSubComboTargetBodyFromForm", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  it("builds the XOR-compliant body for a valid sub-combo selection", () => {
    const form = makeSubComboForm({ subComboId: "42", priorityOrder: "3" });
    expect(buildSubComboTargetBodyFromForm(form)).toEqual({
      provider_id: "combo",
      account_id: null,
      model_row_id: null,
      sub_combo_id: 42,
      priority_order: 3,
    });
  });

  it("preserves the literal 'combo' provider marker regardless of form content", () => {
    const form = makeSubComboForm();
    expect(buildSubComboTargetBodyFromForm(form).provider_id).toBe("combo");
  });

  it("explicitly nulls account_id and model_row_id (not undefined)", () => {
    const body = buildSubComboTargetBodyFromForm(makeSubComboForm());
    expect(body.account_id).toBeNull();
    expect(body.model_row_id).toBeNull();
  });

  it("parses sub_combo_id and priority_order as base-10 integers", () => {
    const form = makeSubComboForm({ subComboId: "8", priorityOrder: "12" });
    const body = buildSubComboTargetBodyFromForm(form);
    expect(body.sub_combo_id).toBe(8);
    expect(body.priority_order).toBe(12);
  });

  it("returns NaN when the sub_combo_id input is blank (caller validates)", () => {
    const form = makeSubComboForm({ subComboId: "" });
    // parseInt("") === NaN — the surrounding handler checks
    // `if (!body.sub_combo_id)` to bail with a toast. Pin the
    // behaviour so future "smart" coercions don't silently break it.
    expect(Number.isNaN(buildSubComboTargetBodyFromForm(form).sub_combo_id)).toBe(true);
  });
});