import { describe, expect, it } from "vitest";
import { activeTile, applyPreset, changeLayout, clampSplit, closeTile, createWorkspace, openSplit, PRESETS, restoreWorkspace, routeMeta, safeRoute } from "./workspace.js";

describe("workspace state", () => {
  it("starts with one real route, not a simulated dashboard", () => {
    const s = createWorkspace("#/combos/42");
    expect(s.layout).toBe("single");
    expect(activeTile(s).route).toBe("#/combos/42");
  });
  it("validates routes before persistence or frame navigation", () => {
    for (const route of ["#/", "#/providers/a%20b", "#/combos/2", "#/keys/3", "#/keys/3/usage", "#/logs?range=24h", "#/playground"]) expect(safeRoute(route)).toBe(true);
    for (const route of ["javascript:alert(1)", "https://evil.test/", "#/login", "#/providers/x?token=a", "#/combos/NaN", "#/unknown"]) expect(safeRoute(route)).toBe(false);
  });
  it("grows to four panels and focuses an already open view without duplication", () => {
    const s = createWorkspace();
    openSplit(s, "#/combos"); openSplit(s, "#/logs"); openSplit(s, "#/providers");
    expect(s.layout).toBe("grid"); expect(s.tiles).toHaveLength(4);
    openSplit(s, "#/logs"); expect(s.tiles).toHaveLength(4); expect(activeTile(s).route).toBe("#/logs");
    openSplit(s, "#/playground"); expect(s.tiles).toHaveLength(4); expect(activeTile(s).route).toBe("#/playground");
  });
  it("retains tile identities and preserves the focused view when returning to single", () => {
    const s = createWorkspace("#/combos/8"); const id = activeTile(s).id;
    changeLayout(s, "grid"); expect(s.tiles[0]!.id).toBe(id);
    s.focused = s.tiles[2]!.id; const route = activeTile(s).route;
    changeLayout(s, "single"); expect(activeTile(s).route).toBe(route); expect(s.tiles).toHaveLength(1);
  });
  it("reuses one owner per view family, including nested resource routes", () => {
    const s = createWorkspace("#/providers/a");
    openSplit(s, "#/logs");
    openSplit(s, "#/providers/b");
    expect(s.tiles).toHaveLength(2);
    expect(activeTile(s).route).toBe("#/providers/b");
    changeLayout(s, "grid");
    expect(new Set(s.tiles.map(tile => routeMeta(tile.route).route)).size).toBe(4);
  });
  it("closing updates the layout, clears maximization and never removes the last panel", () => {
    const s = createWorkspace(); changeLayout(s, "grid");
    s.maximized = s.tiles[0]!.id; closeTile(s, s.focused);
    expect(s.layout).toBe("triple"); expect(s.maximized).toBeNull(); expect(s.tiles.some(t => t.id === s.focused)).toBe(true);
    closeTile(s, s.focused); closeTile(s, s.focused); closeTile(s, s.focused);
    expect(s.tiles).toHaveLength(1); expect(s.layout).toBe("single");
  });
  it("restores only a bounded valid layout and ignores unexpected properties", () => {
    const s = createWorkspace(); applyPreset(s, PRESETS[0]!); s.column = 90;
    const restored = restoreWorkspace(JSON.stringify({ ...s, token: "not persisted" }));
    expect(restored.column).toBe(75); expect(restored.tiles).toEqual(s.tiles); expect(restored).not.toHaveProperty("token");
    expect(restoreWorkspace("broken").tiles).toHaveLength(1);
    expect(restoreWorkspace(JSON.stringify({ ...s, tiles: [{ id: "bad\"id", route: "#/" }] })).layout).toBe("single");
    expect(restoreWorkspace(JSON.stringify({ ...s, tiles: [s.tiles[0], s.tiles[0]] })).layout).toBe("single");
    expect(restoreWorkspace(JSON.stringify({ ...s, layout: "toString" })).layout).toBe("single");
  });
  it("resets invalid split sizes and resolves nested route metadata", () => {
    expect(clampSplit(NaN)).toBe(50); expect(clampSplit(-10)).toBe(25);
    expect(routeMeta("#/combos/42").label).toBe("Combos");
    expect(routeMeta("#/keys/2/usage").label).toBe("API Keys");
  });
});
