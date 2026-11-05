import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { state } from "../state/index.js";
import * as apiModule from "../state/api.js";
import { mountCombos } from "./combos.js";

describe("mountCombos native mount target and lifecycle", () => {
  let container: HTMLElement;

  beforeEach(() => {
    container = document.createElement("div");
    document.body.appendChild(container);
    state.combos = [];
    state.accounts = [];
  });

  afterEach(() => {
    container.remove();
    document.body.innerHTML = "";
    vi.restoreAllMocks();
  });

  it("mounts into an explicit target HTMLElement", async () => {
    vi.spyOn(apiModule, "api").mockResolvedValueOnce([
      { id: 1, name: "Combo 1", strategy: "priority", targets: [] },
    ]);

    const cleanup = await mountCombos({}, container);
    expect(typeof cleanup).toBe("function");

    // Container should contain combos list view content once loaded
    expect(container.innerHTML).not.toBe("");

    if (typeof cleanup === "function") {
      cleanup();
    }
  });

  it("falls back to #main when target is omitted", async () => {
    const main = document.createElement("main");
    main.id = "main";
    document.body.appendChild(main);

    vi.spyOn(apiModule, "api").mockResolvedValueOnce([
      { id: 2, name: "Combo Main", strategy: "priority", targets: [] },
    ]);

    const cleanup = await mountCombos();
    expect(typeof cleanup).toBe("function");
    expect(main.innerHTML).not.toBe("");

    if (typeof cleanup === "function") {
      cleanup();
    }
    main.remove();
  });
});
