import { describe, expect, it } from "vitest";
import { activeTile, applyPreset, assignTileRoute, changeLayout, clampRect, clampSplit, closeTile, createWorkspace, GRID, moveTile, openSplit, overlaps, planDrop, placeTile, PRESETS, rectValid, resolveFit, restoreWorkspace, routeMeta, safeRoute, setTileRect, spawnTile, swapTiles, type Rect } from "./workspace.js";

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
  it("grows to six panels and focuses an already open view without duplication", () => {
    const s = createWorkspace();
    openSplit(s, "#/combos"); openSplit(s, "#/logs"); openSplit(s, "#/providers");
    expect(s.layout).toBe("grid"); expect(s.tiles).toHaveLength(4);
    openSplit(s, "#/logs"); expect(s.tiles).toHaveLength(4); expect(activeTile(s).route).toBe("#/logs");
    openSplit(s, "#/playground"); expect(s.tiles).toHaveLength(5); expect(s.layout).toBe("penta"); expect(activeTile(s).route).toBe("#/playground");
    openSplit(s, "#/keys"); expect(s.tiles).toHaveLength(6); expect(s.layout).toBe("hex"); expect(activeTile(s).route).toBe("#/keys");
    openSplit(s, "#/analytics"); expect(s.tiles).toHaveLength(6); expect(activeTile(s).route).toBe("#/analytics");
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
    // a repeated family drops the duplicate, not the whole arrange
    const deduped = restoreWorkspace(JSON.stringify({ ...s, tiles: [s.tiles[0], s.tiles[0]] }));
    expect(deduped.tiles).toHaveLength(1);
    expect(deduped.layout).toBe("single");
    // an unknown layout name infers from the tiles instead of nuking them
    // (s carries two tiles from the split preset, so split comes back)
    const inferred = restoreWorkspace(JSON.stringify({ ...s, layout: "toString" }));
    expect(inferred.layout).toBe("split");
    expect(inferred.tiles).toHaveLength(2);
    expect(inferred.tiles.map(t => t.route)).toEqual(s.tiles.map(t => t.route));
  });

  it("keeps the maximized tile across reload", () => {
    const s = createWorkspace("#/");
    applyPreset(s, PRESETS[0]!);
    const payload = JSON.stringify({ ...s, tiles: s.tiles.map((t, i) => ({ id: `tile-${i + 1}`, route: t.route, rect: t.rect })), focused: "tile-1", maximized: "tile-2" });
    const restored = restoreWorkspace(payload);
    expect(restored.maximized).toBe("tile-2");
    // a maximized id that names no restored tile falls back to none
    expect(restoreWorkspace(JSON.stringify({ ...JSON.parse(payload), maximized: "tile-9" })).maximized).toBeNull();
    expect(restoreWorkspace(JSON.stringify({ ...JSON.parse(payload), tiles: [JSON.parse(payload).tiles[0]] })).maximized).toBeNull();
  });
  it("resets invalid split sizes and resolves nested route metadata", () => {
    expect(clampSplit(NaN)).toBe(50); expect(clampSplit(-10)).toBe(25);
    expect(routeMeta("#/combos/42").label).toBe("Combos");
    expect(routeMeta("#/keys/2/usage").label).toBe("API Keys");
  });
  it("grows to six panels through penta and hex layouts", () => {
    const s = createWorkspace("#/combos");
    for (const route of ["#/logs", "#/providers", "#/keys", "#/analytics", "#/notifications"]) openSplit(s, route);
    expect(s.layout).toBe("hex"); expect(s.tiles).toHaveLength(6);
    const lastId = activeTile(s).id;
    openSplit(s, "#/playground");
    expect(s.tiles).toHaveLength(6);
    expect(s.tiles.find(tile => tile.id === lastId)!.route).toBe("#/playground");
    changeLayout(s, "penta"); expect(s.tiles).toHaveLength(5);
    changeLayout(s, "single"); expect(s.tiles).toHaveLength(1);
  });
  it("exchanges rects without rebinding ids to views", () => {
    const s = createWorkspace("#/combos");
    openSplit(s, "#/logs");
    const [a, b] = s.tiles;
    const rectA = { ...a!.rect }, rectB = { ...b!.rect };
    swapTiles(s, a!.id, b!.id);
    expect(a!.rect).toEqual(rectB); expect(b!.rect).toEqual(rectA);
    expect(s.tiles.map(t => t.id)).toEqual([a!.id, b!.id]);
    expect(a!.route).toBe("#/combos"); expect(b!.route).toBe("#/logs");
    swapTiles(s, a!.id, a!.id);
    expect(a!.rect).toEqual(rectB);
    swapTiles(s, a!.id, "missing");
    expect(a!.rect).toEqual(rectB);
  });
  it("moves a tile by exchanging rects with the neighbor at a clamped offset", () => {
    const s = createWorkspace("#/combos");
    changeLayout(s, "grid");
    const order = s.tiles.map(t => t.id);
    const rects = s.tiles.map(t => ({ ...t.rect }));
    const rectOf = (id: string): Rect => s.tiles.find(t => t.id === id)!.rect;
    moveTile(s, order[2]!, -2);
    expect(rectOf(order[2]!)).toEqual(rects[0]);
    expect(rectOf(order[0]!)).toEqual(rects[2]);
    moveTile(s, order[2]!, 99);
    expect(rectOf(order[2]!)).toEqual(rects[3]);
    moveTile(s, order[2]!, -99);
    expect(rectOf(order[2]!)).toEqual(rects[2]);
  });
  it("drops a view into a slot, swapping with an open family owner", () => {
    const s = createWorkspace("#/combos");
    openSplit(s, "#/logs");
    const combosId = s.tiles[0]!.id, logsId = s.tiles[1]!.id;
    // Providers is not open: the target panel loads it.
    assignTileRoute(s, logsId, "#/providers");
    expect(s.tiles.find(t => t.id === logsId)!.route).toBe("#/providers");
    expect(s.focused).toBe(logsId);
    // Combos already lives in panel one: dropping it on panel two exchanges rects.
    const rectCombos = { ...s.tiles.find(t => t.id === combosId)!.rect };
    const rectLogs = { ...s.tiles.find(t => t.id === logsId)!.rect };
    assignTileRoute(s, logsId, "#/combos");
    expect(s.tiles.find(t => t.id === combosId)!.rect).toEqual(rectLogs);
    expect(s.tiles.find(t => t.id === logsId)!.rect).toEqual(rectCombos);
    expect(s.tiles.find(t => t.id === logsId)!.route).toBe("#/providers");
    // Dropping onto the owning panel just retargets the route.
    assignTileRoute(s, combosId, "#/combos/7");
    expect(s.tiles.find(t => t.id === combosId)!.route).toBe("#/combos/7");
    assignTileRoute(s, combosId, "javascript:alert(1)");
    expect(s.tiles.find(t => t.id === combosId)!.route).toBe("#/combos/7");
    assignTileRoute(s, "missing", "#/keys");
    expect(s.tiles).toHaveLength(2);
  });
  it("restores five and six panel layouts and truncates overlong payloads to the layout's count", () => {
    const s = createWorkspace("#/combos");
    for (const route of ["#/logs", "#/providers", "#/keys", "#/analytics", "#/notifications"]) openSplit(s, route);
    const restored = restoreWorkspace(JSON.stringify(s));
    expect(restored.layout).toBe("hex"); expect(restored.tiles).toHaveLength(6);
    expect(restoreWorkspace(JSON.stringify({ ...s, layout: "penta", tiles: s.tiles.slice(0, 5) })).layout).toBe("penta");
    // A payload from a build with a bigger budget still restores, cut to the stated shape.
    const over = restoreWorkspace(JSON.stringify({ ...s, layout: "hex", tiles: [...s.tiles, { id: "tile-x", route: "#/config" }] }));
    expect(over.layout).toBe("hex"); expect(over.tiles).toHaveLength(6);
    expect(over.tiles.some(t => t.id === "tile-x")).toBe(false);
  });
  it("offers a six panel command center preset", () => {
    const s = createWorkspace();
    applyPreset(s, PRESETS[PRESETS.length - 1]!);
    expect(s.layout).toBe("hex"); expect(s.tiles).toHaveLength(6);
    expect(s.tiles.map(t => routeMeta(t.route).route)).toEqual(["#/logs", "#/analytics", "#/", "#/providers", "#/combos", "#/keys"]);
  });

  it("places tiles freely, relocating only what the new rect touches", () => {
    const s = createWorkspace();
    changeLayout(s, "grid");
    const [t0, t1, t2, t3] = s.tiles;
    // free the bottom-right corner first, so a 3x3 slot exists below the hero
    setTileRect(s, t3!.id, { x: 3, y: 5, w: 3, h: 1 });
    placeTile(s, t0!.id, { x: 0, y: 0, w: 6, h: 2 });
    expect(t0!.rect).toEqual({ x: 0, y: 0, w: 6, h: 2 });
    // the top-right quarter slid under the hero; the rest kept their rects
    expect(t1!.rect).toEqual({ x: 3, y: 2, w: 3, h: 3 });
    expect(t2!.rect).toEqual({ x: 0, y: 3, w: 3, h: 3 });
    expect(t3!.rect).toEqual({ x: 3, y: 5, w: 3, h: 1 });
    for (const t of s.tiles) expect(rectValid(t.rect)).toBe(true);
  });
  it("refuses placements that cannot resolve without overlap", () => {
    const s = createWorkspace();
    changeLayout(s, "hex");
    const id = s.tiles[0]!.id;
    const before = s.tiles.map(t => ({ ...t.rect }));
    // a 6x6 giant leaves no room for the other five tiles of 2x3
    placeTile(s, id, { x: 0, y: 0, w: 6, h: 6 });
    expect(s.tiles.map(t => ({ ...t.rect }))).toEqual(before);
  });
  it("resizes in place with setTileRect, never displacing neighbors", () => {
    const s = createWorkspace();
    changeLayout(s, "hex");
    const before = s.tiles.slice(1).map(t => ({ ...t.rect }));
    setTileRect(s, s.tiles[0]!.id, { x: 0, y: 0, w: 3, h: 6 });
    expect(s.tiles[0]!.rect).toEqual({ x: 0, y: 0, w: 3, h: 6 });
    expect(s.tiles.slice(1).map(t => ({ ...t.rect }))).toEqual(before);
    // a rect that would overlap a neighbor still applies verbatim: the grip
    // drag is responsible for clipping against neighbors before committing.
    setTileRect(s, s.tiles[0]!.id, { x: 0, y: 0, w: 6, h: 6 });
    expect(s.tiles[0]!.rect).toEqual({ x: 0, y: 0, w: 6, h: 6 });
  });
  it("validates rects and clamps them into the canvas", () => {
    expect(rectValid({ x: 0, y: 0, w: 6, h: 6 })).toBe(true);
    expect(rectValid({ x: 5, y: 5, w: 2, h: 2 })).toBe(false);
    expect(rectValid({ x: 0, y: 0, w: 1.5, h: 2 })).toBe(false);
    expect(rectValid(null)).toBe(false);
    expect(clampRect({ x: -3, y: 99, w: 9, h: 0 })).toEqual({ x: 0, y: 5, w: 6, h: 1 });
    expect(clampRect({ x: 4, y: 4, w: 3, h: 3 })).toEqual({ x: 3, y: 3, w: 3, h: 3 });
  });
  it("resolveFit reports the relocation map or null for hopeless drops", () => {
    const s = createWorkspace();
    changeLayout(s, "grid");
    setTileRect(s, s.tiles[3]!.id, { x: 3, y: 5, w: 3, h: 1 });
    const free = resolveFit(s.tiles, s.tiles[0]!.id, { x: 0, y: 0, w: 6, h: 2 });
    expect(free).not.toBeNull();
    expect(free!.size).toBe(1);
    expect(free!.get(s.tiles[1]!.id)).toEqual({ x: 3, y: 2, w: 3, h: 3 });
    const hopeless = resolveFit(s.tiles, s.tiles[0]!.id, { x: 0, y: 0, w: 6, h: 6 });
    expect(hopeless).toBeNull();
  });
  it("spawns a new tile on empty canvas and focuses an open family instead", () => {
    const s = createWorkspace();
    setTileRect(s, s.tiles[0]!.id, { x: 0, y: 0, w: 3, h: 3 });
    expect(spawnTile(s, "#/logs", { x: 3, y: 3, w: 3, h: 3 })).toBe(true);
    expect(s.tiles).toHaveLength(2);
    expect(routeMeta(s.tiles[1]!.route).route).toBe("#/logs");
    // same family again: focused, not duplicated
    expect(spawnTile(s, "#/logs?range=1h", { x: 3, y: 3, w: 3, h: 3 })).toBe(true);
    expect(s.tiles).toHaveLength(2);
    expect(activeTile(s).route).toBe("#/logs?range=1h");
    // a spawn that would not fit cleanly is refused
    changeLayout(s, "grid");
    const snapshot = s.tiles.map(t => ({ ...t.rect }));
    expect(spawnTile(s, "#/playground", { x: 0, y: 0, w: 6, h: 6 })).toBe(false);
    expect(s.tiles.map(t => ({ ...t.rect }))).toEqual(snapshot);
  });
  it("migrates legacy index-based payloads to preset shapes and honors rects when present", () => {
    const legacy = restoreWorkspace(JSON.stringify({ version: 1, layout: "split", tiles: [{ id: "a", route: "#/logs" }, { id: "b", route: "#/" }], focused: "a", column: 50, row: 50, contextCollapsed: false }));
    expect(legacy.tiles[0]!.rect).toEqual({ x: 0, y: 0, w: 3, h: 6 });
    expect(legacy.tiles[1]!.rect).toEqual({ x: 3, y: 0, w: 3, h: 6 });
    // rect payloads accept custom arrangements (count may differ from the preset)
    const custom = restoreWorkspace(JSON.stringify({
      version: 1, layout: "grid", focused: "a", column: 50, row: 50, contextCollapsed: false,
      tiles: [
        { id: "a", route: "#/logs", rect: { x: 0, y: 0, w: 4, h: 6 } },
        { id: "b", route: "#/", rect: { x: 4, y: 0, w: 2, h: 3 } },
      ],
    }));
    expect(custom.tiles).toHaveLength(2);
    expect(custom.tiles[1]!.rect).toEqual({ x: 4, y: 0, w: 2, h: 3 });
    // invalid rects fall back to the preset shape for the slot
    const broken = restoreWorkspace(JSON.stringify({
      version: 1, layout: "split", focused: "a", column: 50, row: 50, contextCollapsed: false,
      tiles: [{ id: "a", route: "#/logs", rect: { x: 9, y: 0, w: 2, h: 2 } }, { id: "b", route: "#/", rect: { x: 3, y: 0, w: 3, h: 6 } }],
    }));
    expect(broken.tiles[0]!.rect).toEqual({ x: 0, y: 0, w: 3, h: 6 });
    // overlap in a payload resolves to free space instead of stacking
    const overlapped = restoreWorkspace(JSON.stringify({
      version: 1, layout: "split", focused: "a", column: 50, row: 50, contextCollapsed: false,
      tiles: [
        { id: "a", route: "#/logs", rect: { x: 0, y: 0, w: 3, h: 6 } },
        { id: "b", route: "#/", rect: { x: 0, y: 0, w: 3, h: 6 } },
      ],
    }));
    expect(overlaps(overlapped.tiles[0]!.rect, overlapped.tiles[1]!.rect)).toBe(false);
    expect(GRID.cols).toBe(6);
    const probe: Rect = { x: 0, y: 0, w: 2, h: 3 };
    expect(probe.w).toBe(2);
  });

  it("carves neighbors when the canvas has no room, so a panel lands between two", () => {
    const s = createWorkspace();
    changeLayout(s, "split"); // {0,0,3,6} and {3,0,3,6}
    // Drop a 2-wide panel over the seam: the left tile is carved to 1 column.
    const plan = planDrop(s.tiles, "spawn-probe", { x: 1, y: 0, w: 2, h: 6 });
    expect(plan).not.toBeNull();
    expect(plan!.carves.size).toBe(1);
    expect(s.tiles[0]!.rect).toEqual({ x: 0, y: 0, w: 3, h: 6 });
    expect(plan!.carves.get(s.tiles[0]!.id)).toEqual({ x: 0, y: 0, w: 1, h: 6 });
    // applied through spawnTile
    expect(spawnTile(s, "#/analytics", { x: 1, y: 0, w: 2, h: 6 })).toBe(true);
    expect(s.tiles).toHaveLength(3);
    expect(s.tiles.find(t => t.id === s.tiles[0]!.id)!.rect).toEqual({ x: 0, y: 0, w: 1, h: 6 });
    const spawned = s.tiles[2]!;
    expect(spawned.rect).toEqual({ x: 1, y: 0, w: 2, h: 6 });
  });
  it("carves a full strip off two tiles at once", () => {
    const s = createWorkspace();
    changeLayout(s, "grid");
    const plan = planDrop(s.tiles, "spawn-probe", { x: 0, y: 0, w: 6, h: 1 });
    expect(plan).not.toBeNull();
    expect(plan!.carves.size).toBe(2);
    expect(plan!.carves.get(s.tiles[0]!.id)).toEqual({ x: 0, y: 1, w: 3, h: 2 });
    expect(plan!.carves.get(s.tiles[1]!.id)).toEqual({ x: 3, y: 1, w: 3, h: 2 });
    // untouched tiles keep their rects
    expect(plan!.carves.has(s.tiles[2]!.id)).toBe(false);
  });
  it("rejects drops that neither relocate nor carve cleanly", () => {
    const s = createWorkspace();
    changeLayout(s, "hex");
    // the whole canvas: every neighbour would be left with nothing to keep
    expect(planDrop(s.tiles, "spawn-probe", { x: 0, y: 0, w: 6, h: 6 })).toBeNull();
    // a drop that exactly covers one neighbour leaves it nothing to keep either
    expect(planDrop(s.tiles, "spawn-probe", { x: 0, y: 0, w: 2, h: 3 })).toBeNull();
    // placeTile therefore refuses instead of stacking tiles
    const before = s.tiles.map(t => ({ ...t.rect }));
    placeTile(s, s.tiles[0]!.id, { x: 0, y: 0, w: 2, h: 3 });
    expect(s.tiles.map(t => ({ ...t.rect }))).toEqual(before);
  });
  it("grows a seam drop into the strip that fits between two", () => {
    const s = createWorkspace();
    changeLayout(s, "split"); // {0,0,3,6} + {3,0,3,6}
    // A small ghost over the seam cannot cut clean strips: the plan grows it to
    // the full-height strip so each neighbour gives up one edge.
    const plan = planDrop(s.tiles, "spawn-probe", { x: 2, y: 3, w: 2, h: 1 });
    expect(plan).not.toBeNull();
    expect(plan!.rect).toEqual({ x: 2, y: 0, w: 2, h: 6 });
    expect(plan!.carves.get(s.tiles[0]!.id)).toEqual({ x: 0, y: 0, w: 2, h: 6 });
    expect(plan!.carves.get(s.tiles[1]!.id)).toEqual({ x: 4, y: 0, w: 2, h: 6 });
    // spawnTile commits the grown strip, not the requested 2x1
    expect(spawnTile(s, "#/analytics", { x: 2, y: 3, w: 2, h: 1 })).toBe(true);
    expect(s.tiles[2]!.rect).toEqual({ x: 2, y: 0, w: 2, h: 6 });
    const all = s.tiles.map(t => t.rect);
    for (let i = 0; i < all.length; i++) {
      for (let j = i + 1; j < all.length; j++) {
        expect(overlaps(all[i]!, all[j]!)).toBe(false);
      }
    }
  });

  it("carves only the wide side when the pair is asymmetric", () => {
    const s = createWorkspace();
    s.tiles[0]!.rect = { x: 0, y: 0, w: 4, h: 6 };
    const second = spawnTile(s, "#/config", { x: 4, y: 0, w: 2, h: 6 });
    expect(second).toBe(true);
    expect(s.tiles[1]!.rect).toEqual({ x: 4, y: 0, w: 2, h: 6 });
    // a strip that fits entirely in the wide side leaves the narrow one intact
    const plan = planDrop(s.tiles, "probe", { x: 2, y: 0, w: 2, h: 6 });
    expect(plan).not.toBeNull();
    expect(plan!.carves.get(s.tiles[0]!.id)).toEqual({ x: 0, y: 0, w: 2, h: 6 });
    expect(plan!.carves.has(s.tiles[1]!.id)).toBe(false);
    // straddling the seam squeezes both: the wide keeps three, the narrow one
    const straddle = planDrop(s.tiles, "probe", { x: 3, y: 0, w: 2, h: 6 });
    expect(straddle!.carves.get(s.tiles[0]!.id)).toEqual({ x: 0, y: 0, w: 3, h: 6 });
    expect(straddle!.carves.get(s.tiles[1]!.id)).toEqual({ x: 5, y: 0, w: 1, h: 6 });
  });
});
