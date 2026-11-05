import { describe, it, expect, beforeEach } from "vitest";
import { state } from "../state/index.js";
import {
  openModelPickerModal,
  closeModelPickerModal,
  toggleModelPicker,
  onFilterChange,
  loadMorePickerItems,
  getCurrentAllowedModels,
  getCurrentAllowedCombos,
} from "./model-picker.js";
import type { Combo } from "../lib/types/combos.js";
import type { Model, Provider } from "../lib/types/api.js";

describe("model-picker component", () => {
  beforeEach(() => {
    document.body.innerHTML = `
      <div id="modal-root"></div>
      <div id="model-picker-display"></div>
      <div id="blacklisted-models-display"></div>
      <div id="allowed-combos-display"></div>
      <input type="hidden" name="allowed_models" value="">
      <input type="hidden" name="blacklisted_models" value="">
      <input type="hidden" name="allowed_combos" value="">
    `;

    // Seed test combos and models
    state.combos = [
      { id: 1, name: "fast-smart", strategy: "priority" } as unknown as Combo,
      { id: 2, name: "fallback-all", strategy: "round_robin" } as unknown as Combo,
    ];

    const testModels: Model[] = [];
    for (let i = 1; i <= 120; i++) {
      testModels.push({
        id: `m-${i}`,
        model_id: `provider-a/gpt-${i}`,
        provider_id: i % 2 === 0 ? "provider-a" : "provider-b",
        created: 1234567,
        object: "model",
        owned_by: "system",
      } as unknown as Model);
    }
    state.models = testModels;
    state.providers = [
      { id: "provider-a", name: "Provider A", active: true } as unknown as Provider,
      { id: "provider-b", name: "Provider B", active: true } as unknown as Provider,
    ];
  });

  it("includes combos as 'combo:<name>' in allowed_models picker", () => {
    openModelPickerModal("allowed_models");
    const list = document.getElementById("model-picker-list");
    expect(list).not.toBeNull();

    // Combos should be rendered with virtual id combo:fast-smart and combo:fallback-all
    const rows = list!.querySelectorAll(".model-picker-row");
    expect(rows.length).toBe(50); // Default PAGE_SIZE windowing

    const text = list!.textContent || "";
    expect(text).toContain("combo:fast-smart");
    expect(text).toContain("combo:fallback-all");

    // Check remaining count button exists
    expect(text).toContain("Load more");
  });

  it("paginates items in batches of 50 to avoid DOM lag", () => {
    openModelPickerModal("allowed_models");
    const list = document.getElementById("model-picker-list");
    let rows = list!.querySelectorAll(".model-picker-row");
    expect(rows.length).toBe(50);

    loadMorePickerItems();
    rows = list!.querySelectorAll(".model-picker-row");
    expect(rows.length).toBe(100);

    loadMorePickerItems();
    rows = list!.querySelectorAll(".model-picker-row");
    // 120 models + 2 combos = 122 total items
    expect(rows.length).toBe(122);
    expect(list!.textContent).not.toContain("Load more");
  });

  it("filters by combo provider when 'combo' is selected in provider dropdown", () => {
    openModelPickerModal("allowed_models");
    const provSelect = document.getElementById("model-picker-provider") as HTMLSelectElement;
    expect(provSelect).not.toBeNull();

    provSelect.value = "combo";
    onFilterChange();

    const list = document.getElementById("model-picker-list");
    const rows = list!.querySelectorAll(".model-picker-row");
    expect(rows.length).toBe(2);
    expect(list!.textContent).toContain("combo:fast-smart");
    expect(list!.textContent).toContain("combo:fallback-all");
    expect(list!.textContent).not.toContain("provider-a/gpt-");
  });

  it("filters by search term across model id, combo name and combo virtual prefix", () => {
    openModelPickerModal("allowed_models");
    const searchInput = document.getElementById("model-picker-search") as HTMLInputElement;

    searchInput.value = "fast-smart";
    onFilterChange();

    const list = document.getElementById("model-picker-list");
    const rows = list!.querySelectorAll(".model-picker-row");
    expect(rows.length).toBe(1);
    expect(list!.textContent).toContain("combo:fast-smart");

    // Search model by number
    searchInput.value = "gpt-42";
    onFilterChange();
    const rowsGpt = list!.querySelectorAll(".model-picker-row");
    expect(rowsGpt.length).toBe(1);
    expect(list!.textContent).toContain("provider-a/gpt-42");
  });

  it("toggles and saves selected combo in allowed_models hidden input", () => {
    openModelPickerModal("allowed_models");
    toggleModelPicker("combo:fast-smart", { target: { checked: true } } as any);
    closeModelPickerModal();

    const hidden = document.querySelector('input[name="allowed_models"]') as HTMLInputElement;
    expect(hidden.value).toBe("combo:fast-smart");
    expect(getCurrentAllowedModels()).toEqual(["combo:fast-smart"]);
  });

  it("handles allowed_combos target correctly without virtual prefixes", () => {
    openModelPickerModal("allowed_combos");
    const list = document.getElementById("model-picker-list");
    const rows = list!.querySelectorAll(".model-picker-row");
    expect(rows.length).toBe(2);

    toggleModelPicker("1", { target: { checked: true } } as any);
    closeModelPickerModal();

    const hidden = document.querySelector('input[name="allowed_combos"]') as HTMLInputElement;
    expect(hidden.value).toBe("1");
    expect(getCurrentAllowedCombos()).toEqual([1]);
  });
});
