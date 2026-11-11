// views/combos/combos-list.test.ts — locks the compact combos header shape.
//
// The header used to be three stacked tiers (title row, filter bar, actions) and
// a `flex-direction: column` container query made it four rows in a narrow
// workspace tile. These assertions pin the flat, single-toolbar structure so a
// future edit cannot quietly reintroduce the vertical stack.

import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { render } from "lit-html";
import { state } from "../../state/index.js";
import type { Combo } from "../../lib/types/api.js";
import { renderCombosList } from "./combos-list.js";

function makeCombo(overrides: Partial<Combo> = {}): Combo {
  return {
    id: 1,
    name: "alpha",
    strategy: "priority",
    race_size: 1,
    created_at: "2026-01-01T00:00:00Z",
    context_window: null,
    priority_mode: "strict",
    ...overrides,
  } as Combo;
}

describe("combos list header structure", () => {
  let host: HTMLElement;

  beforeEach(() => {
    host = document.createElement("div");
    document.body.appendChild(host);
    state.combos = [
      makeCombo({ id: 1, name: "alpha", strategy: "priority", priority_mode: "strict" }),
      makeCombo({ id: 2, name: "beta", strategy: "round_robin", priority_mode: "lkgp" }),
    ] as Combo[];
  });

  afterEach(() => {
    host.remove();
    document.body.innerHTML = "";
    state.combos = [];
  });

  it("renders one flat toolbar: identity plus a single actions group", () => {
    render(renderCombosList(), host);
    const header = host.querySelector(".combos-page-header");
    expect(header).not.toBeNull();

    const directChildren = Array.from(header!.children);
    expect(directChildren).toHaveLength(2);
    expect(directChildren[0]!.tagName).toBe("H2");
    expect(directChildren[0]!.classList.contains("view-identity")).toBe(true);
    expect(directChildren[1]!.classList.contains("combos-header-actions")).toBe(true);

    // The old intermediate wrappers are gone: filters are direct children of
    // the actions group, not a nested bar that could re-stack on its own.
    expect(host.querySelector(".combos-filter-bar")).toBeNull();
    expect(host.querySelector(".header-title-wrap")).toBeNull();
  });

  it("keeps search, both selects and the create button in the actions group", () => {
    render(renderCombosList(), host);
    const actions = host.querySelector(".combos-header-actions");
    expect(actions).not.toBeNull();

    const search = actions!.querySelector("input.combos-search-input");
    expect(search).not.toBeNull();

    const selects = actions!.querySelectorAll("select.combos-filter-select");
    expect(selects).toHaveLength(2);

    const button = actions!.querySelector("button.combo-create-btn");
    expect(button).not.toBeNull();

    // Count badge lives in the toolbar, next to the filters it describes.
    expect(actions!.querySelector(".combos-count-badge")).not.toBeNull();
  });

  it("keeps every header control as a sibling in one flex row", () => {
    render(renderCombosList(), host);
    const actions = host.querySelector(".combos-header-actions")!;
    const controls = [
      actions.querySelector(".combos-search-wrap"),
      ...Array.from(actions.querySelectorAll("select.combos-filter-select")),
      actions.querySelector(".combos-count-badge"),
      actions.querySelector("button.combo-create-btn"),
    ];

    expect(controls.every((c) => c !== null)).toBe(true);
    // One flex parent for all of them: nothing nests a control in its own
    // sub-bar, so the row can only wrap — it can never become a column.
    for (const control of controls) {
      expect(control!.parentElement).toBe(actions);
    }
    // No inline flex axis overrides anywhere in the header.
    expect(host.querySelector(".combos-page-header [style*='flex-direction']")).toBeNull();
  });
});
