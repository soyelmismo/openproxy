import { describe, it, expect, beforeEach } from "vitest";
import type { ComboTargetWithModel, Combo } from "../../lib/types/api.js";
import {
  resolveTargetEffectiveCw,
  invalidateSubCombo,
  clearSubComboCache,
  setSubComboCacheForTest,
  getSubComboData,
} from "./subcombo-group.js";

function makeTarget(overrides: Partial<ComboTargetWithModel> = {}): ComboTargetWithModel {
  return {
    id: 1,
    combo_id: 10,
    provider_id: "openai",
    account_id: null,
    model_row_id: 1,
    sub_combo_id: null,
    sub_combo_name: null,
    model_id: "gpt-4o",
    model_display_name: "GPT-4o",
    priority_order: 1,
    weight: 1,
    in_cooldown: false,
    cooldown_until: null,
    cooldown_reason: null,
    active: true,
    provider_active: true,
    context_length: 128_000,
    max_output_tokens: 4_096,
    ...overrides,
  };
}

function makeCombo(overrides: Partial<Combo> = {}): Combo {
  return {
    id: 20,
    name: "sub_test",
    strategy: "priority",
    race_size: 1,
    created_at: "2026-01-01T00:00:00Z",
    context_window: null,
    priority_mode: "strict",
    ...overrides,
  };
}

describe("subcombo-group context window resolution", () => {
  beforeEach(() => {
    clearSubComboCache();
  });

  it("returns model context length for flat model target", () => {
    const t = makeTarget({ context_length: 256_000, sub_combo_id: null });
    expect(resolveTargetEffectiveCw(t)).toBe(256_000);
  });

  it("returns null for model target with 0, negative, or null context length", () => {
    const tZero = makeTarget({ context_length: 0, sub_combo_id: null });
    const tNeg = makeTarget({ context_length: -1, sub_combo_id: null });
    const tNull = makeTarget({ context_length: null, sub_combo_id: null });
    expect(resolveTargetEffectiveCw(tZero)).toBeNull();
    expect(resolveTargetEffectiveCw(tNeg)).toBeNull();
    expect(resolveTargetEffectiveCw(tNull)).toBeNull();
  });

  it("falls back to target.context_length when sub-combo is not in cache", () => {
    const t = makeTarget({ sub_combo_id: 99, context_length: 512_000 });
    expect(resolveTargetEffectiveCw(t)).toBe(512_000);
  });

  it("recursively computes sub-combo natural context window from active leaf targets", () => {
    const subCombo = makeCombo({ id: 20, context_window: null });
    const subTargets = [
      makeTarget({ id: 101, combo_id: 20, context_length: 512_000, active: true, provider_active: true }),
      makeTarget({ id: 102, combo_id: 20, context_length: 256_000, active: true, provider_active: true }),
      makeTarget({ id: 103, combo_id: 20, context_length: 1_000_000, active: true, provider_active: true }),
    ];
    setSubComboCacheForTest(20, {
      combo: subCombo,
      targets: subTargets,
      loading: false,
      error: null,
    });

    const parentTarget = makeTarget({ sub_combo_id: 20, context_length: 128_000 });
    expect(resolveTargetEffectiveCw(parentTarget)).toBe(256_000);
  });

  it("excludes inactive targets and inactive providers in sub-combo", () => {
    const subCombo = makeCombo({ id: 20, context_window: null });
    const subTargets = [
      makeTarget({ id: 101, combo_id: 20, context_length: 64_000, active: false, provider_active: true }),
      makeTarget({ id: 102, combo_id: 20, context_length: 128_000, active: true, provider_active: false }),
      makeTarget({ id: 103, combo_id: 20, context_length: 256_000, active: true, provider_active: true }),
      makeTarget({ id: 104, combo_id: 20, context_length: 512_000, active: true, provider_active: true }),
    ];
    setSubComboCacheForTest(20, {
      combo: subCombo,
      targets: subTargets,
      loading: false,
      error: null,
    });

    const parentTarget = makeTarget({ sub_combo_id: 20 });
    // Active min is 256k, since 64k (inactive target) and 128k (inactive provider) are filtered out
    expect(resolveTargetEffectiveCw(parentTarget)).toBe(256_000);
  });

  it("caps override to natural context window when override is higher", () => {
    const subCombo = makeCombo({ id: 20, context_window: 1_000_000 });
    const subTargets = [
      makeTarget({ id: 101, combo_id: 20, context_length: 256_000, active: true, provider_active: true }),
      makeTarget({ id: 102, combo_id: 20, context_length: 512_000, active: true, provider_active: true }),
    ];
    setSubComboCacheForTest(20, {
      combo: subCombo,
      targets: subTargets,
      loading: false,
      error: null,
    });

    const parentTarget = makeTarget({ sub_combo_id: 20 });
    expect(resolveTargetEffectiveCw(parentTarget)).toBe(256_000);
  });

  it("applies lower override when sub-combo operator sets smaller context window", () => {
    const subCombo = makeCombo({ id: 20, context_window: 64_000 });
    const subTargets = [
      makeTarget({ id: 101, combo_id: 20, context_length: 256_000, active: true, provider_active: true }),
      makeTarget({ id: 102, combo_id: 20, context_length: 512_000, active: true, provider_active: true }),
    ];
    setSubComboCacheForTest(20, {
      combo: subCombo,
      targets: subTargets,
      loading: false,
      error: null,
    });

    const parentTarget = makeTarget({ sub_combo_id: 20 });
    expect(resolveTargetEffectiveCw(parentTarget)).toBe(64_000);
  });

  it("returns null when sub-combo has zero active targets and no override, not falling back to stale context_length", () => {
    const subCombo = makeCombo({ id: 20, context_window: null });
    const subTargets = [
      makeTarget({ id: 101, combo_id: 20, context_length: 128_000, active: false }),
    ];
    setSubComboCacheForTest(20, {
      combo: subCombo,
      targets: subTargets,
      loading: false,
      error: null,
    });

    const parentTarget = makeTarget({ sub_combo_id: 20, context_length: 128_000 });
    // Must return null, NOT 128k from parentTarget.context_length
    expect(resolveTargetEffectiveCw(parentTarget)).toBeNull();
  });

  it("supports multi-level nested sub-combos (parent -> sub -> leaf)", () => {
    // Sub-combo 30: leaf models with min 300k
    const sub30 = makeCombo({ id: 30, context_window: null });
    const targets30 = [
      makeTarget({ id: 301, combo_id: 30, context_length: 300_000, active: true, provider_active: true }),
      makeTarget({ id: 302, combo_id: 30, context_length: 400_000, active: true, provider_active: true }),
    ];
    setSubComboCacheForTest(30, { combo: sub30, targets: targets30, loading: false, error: null });

    // Sub-combo 20: contains sub-combo 30 and a 500k model
    const sub20 = makeCombo({ id: 20, context_window: null });
    const targets20 = [
      makeTarget({ id: 201, combo_id: 20, sub_combo_id: 30, active: true, provider_active: true }),
      makeTarget({ id: 202, combo_id: 20, context_length: 500_000, active: true, provider_active: true }),
    ];
    setSubComboCacheForTest(20, { combo: sub20, targets: targets20, loading: false, error: null });

    // Top parent target referencing sub-combo 20
    const topTarget = makeTarget({ sub_combo_id: 20 });
    expect(resolveTargetEffectiveCw(topTarget)).toBe(300_000);
  });

  it("handles cycle detection gracefully without infinite loop", () => {
    // 20 points to 21, 21 points to 20
    const sub20 = makeCombo({ id: 20, context_window: null });
    const sub21 = makeCombo({ id: 21, context_window: null });
    const targets20 = [
      makeTarget({ id: 201, combo_id: 20, sub_combo_id: 21, context_length: 200_000, active: true, provider_active: true }),
    ];
    const targets21 = [
      makeTarget({ id: 211, combo_id: 21, sub_combo_id: 20, context_length: 200_000, active: true, provider_active: true }),
    ];
    setSubComboCacheForTest(20, { combo: sub20, targets: targets20, loading: false, error: null });
    setSubComboCacheForTest(21, { combo: sub21, targets: targets21, loading: false, error: null });

    const topTarget = makeTarget({ sub_combo_id: 20, context_length: 200_000 });
    expect(resolveTargetEffectiveCw(topTarget)).toBe(200_000);
  });

  it("invalidates specific sub-combo and clears entire cache correctly", () => {
    setSubComboCacheForTest(20, { combo: makeCombo({ id: 20 }), targets: [], loading: false, error: null });
    setSubComboCacheForTest(21, { combo: makeCombo({ id: 21 }), targets: [], loading: false, error: null });
    expect(getSubComboData(20)).toBeDefined();
    expect(getSubComboData(21)).toBeDefined();

    invalidateSubCombo(20);
    expect(getSubComboData(20)).toBeUndefined();
    expect(getSubComboData(21)).toBeDefined();

    clearSubComboCache();
    expect(getSubComboData(21)).toBeUndefined();
  });
});
