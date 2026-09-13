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
import {
  buildSubComboTargetBodyFromForm,
  modelMatchesSearch,
  buildGlobalSearchGroups,
} from "./add-target-modal.js";

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

describe("modelMatchesSearch", () => {
  const qwenModel = {
    provider_id: "hcnsec",
    model_id: "Qwen3.8-Flash-Next",
    display_name: "Qwen 3.8 Flash Next",
  };

  const llamaModel = {
    provider_id: "openrouter",
    model_id: "meta-llama/llama-3-70b-instruct",
  };

  it("matches full provider/model_id query (e.g. hcnsec/Qwen3.8-Flash-Next)", () => {
    expect(modelMatchesSearch(qwenModel, "hcnsec/Qwen3.8-Flash-Next")).toBe(true);
  });

  it("matches case-insensitively and handles surrounding quotes", () => {
    expect(modelMatchesSearch(qwenModel, '"hcnsec/qwen3.8-flash-next"')).toBe(true);
    expect(modelMatchesSearch(qwenModel, "'HCNSEC/QWEN3.8-FLASH-NEXT'")).toBe(true);
  });

  it("matches colon syntax provider:model_id", () => {
    expect(modelMatchesSearch(qwenModel, "hcnsec:Qwen3.8-Flash-Next")).toBe(true);
  });

  it("matches provider with trailing slash", () => {
    expect(modelMatchesSearch(qwenModel, "hcnsec/")).toBe(true);
  });

  it("matches provider and partial model token (e.g. hcnsec/flash)", () => {
    expect(modelMatchesSearch(qwenModel, "hcnsec/flash")).toBe(true);
  });

  it("matches space-separated provider and model", () => {
    expect(modelMatchesSearch(qwenModel, "hcnsec qwen flash")).toBe(true);
    expect(modelMatchesSearch(qwenModel, "hcnsec / Qwen3.8-Flash-Next")).toBe(true);
  });

  it("matches plain model ID or display name without provider prefix", () => {
    expect(modelMatchesSearch(qwenModel, "Qwen3.8")).toBe(true);
    expect(modelMatchesSearch(qwenModel, "Flash Next")).toBe(true);
  });

  it("rejects mismatched provider prefix", () => {
    expect(modelMatchesSearch(qwenModel, "openai/Qwen3.8-Flash-Next")).toBe(false);
    expect(modelMatchesSearch(qwenModel, "anthropic/flash")).toBe(false);
  });

  it("handles models with slashes in their own model_id", () => {
    expect(modelMatchesSearch(llamaModel, "openrouter/meta-llama/llama-3-70b-instruct")).toBe(true);
    expect(modelMatchesSearch(llamaModel, "meta-llama/llama-3-70b-instruct")).toBe(true);
    expect(modelMatchesSearch(llamaModel, "openrouter/llama")).toBe(true);
  });
});

describe("buildGlobalSearchGroups", () => {
  const models = [
    {
      row_id: 1,
      provider_id: "hcnsec",
      model_id: "Qwen3.8-Flash-Next",
      display_name: "Qwen 3.8",
      target_format: "openai" as const,
      discovered_at: "2026-01-01T00:00:00Z",
      expires_at: null,
      timeout_overrides_json: null,
      active: true,
      last_test_status: 200,
      last_test_at: null,
      custom: false,
      context_length: null,
      max_output_tokens: null,
      capabilities_json: null,
      family: null,
      model_type: "chat",
      reasoning_effort: false,
      is_active: true,
      input_modalities_json: null,
      output_modalities_json: null,
    },
    {
      row_id: 2,
      provider_id: "openai",
      model_id: "gpt-4o",
      display_name: null,
      target_format: "openai" as const,
      discovered_at: "2026-01-01T00:00:00Z",
      expires_at: null,
      timeout_overrides_json: null,
      active: true,
      last_test_status: 200,
      last_test_at: null,
      custom: false,
      context_length: null,
      max_output_tokens: null,
      capabilities_json: null,
      family: null,
      model_type: "chat",
      reasoning_effort: false,
      is_active: true,
      input_modalities_json: null,
      output_modalities_json: null,
    },
    {
      row_id: 3,
      provider_id: "hcnsec",
      model_id: "inactive-model",
      display_name: null,
      target_format: "openai" as const,
      discovered_at: "2026-01-01T00:00:00Z",
      expires_at: null,
      timeout_overrides_json: null,
      active: false,
      last_test_status: null,
      last_test_at: null,
      custom: false,
      context_length: null,
      max_output_tokens: null,
      capabilities_json: null,
      family: null,
      model_type: "chat",
      reasoning_effort: false,
      is_active: false,
      input_modalities_json: null,
      output_modalities_json: null,
    },
  ];

  it("finds models by provider/model query and groups by provider", () => {
    const groups = buildGlobalSearchGroups("hcnsec/Qwen3.8-Flash-Next", models, new Set());
    expect(groups.has("hcnsec")).toBe(true);
    expect(groups.get("hcnsec")?.length).toBe(1);
    expect(groups.get("hcnsec")?.[0]?.model_id).toBe("Qwen3.8-Flash-Next");
    expect(groups.has("openai")).toBe(false);
  });

  it("skips excluded existing combo target row IDs", () => {
    const groups = buildGlobalSearchGroups("hcnsec/Qwen3.8-Flash-Next", models, new Set([1]));
    expect(groups.has("hcnsec")).toBe(false);
  });

  it("skips inactive models", () => {
    const groups = buildGlobalSearchGroups("inactive-model", models, new Set());
    expect(groups.size).toBe(0);
  });
});