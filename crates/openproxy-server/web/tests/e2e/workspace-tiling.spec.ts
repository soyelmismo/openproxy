import { expect, test, type Page } from '@playwright/test';

// Desktop tiling on the free 6x6 canvas: presets write rects, header drags
// preview with a gliding ghost (swap or free placement), sidebar drops load or
// spawn, grips resize without displacing neighbors. Read-only for the API.
const preview = process.env['OPENPROXY_LAYOUT_TEST_URL'];
const token = process.env['OPENPROXY_LAYOUT_TEST_TOKEN'];
test.skip(!preview || !token, 'Requires an authenticated frontend preview');
test.use({ viewport: { width: 1500, height: 1100 }, storageState: { cookies: [], origins: [] } });

const rect = (x: number, y: number, w: number, h: number) => ({ x, y, w, h });
const HEX = {
  version: 1, layout: 'hex',
  tiles: [
    { id: 'tile-1', route: '#/logs', rect: rect(0, 0, 2, 3) }, { id: 'tile-2', route: '#/analytics', rect: rect(2, 0, 2, 3) },
    { id: 'tile-3', route: '#/', rect: rect(4, 0, 2, 3) }, { id: 'tile-4', route: '#/providers', rect: rect(0, 3, 2, 3) },
    { id: 'tile-5', route: '#/combos', rect: rect(2, 3, 2, 3) }, { id: 'tile-6', route: '#/keys', rect: rect(4, 3, 2, 3) },
  ],
  focused: 'tile-1', maximized: null, column: 50, row: 50, contextCollapsed: false,
};
// Two corner panels leave two big holes: room for free moves, growth and spawns.
const SPLIT_SEED = {
  version: 1, layout: 'split',
  tiles: [
    { id: 'tile-1', route: '#/logs', rect: rect(0, 0, 3, 6) },
    { id: 'tile-2', route: '#/config', rect: rect(3, 0, 3, 6) },
  ],
  focused: 'tile-1', maximized: null, column: 50, row: 50, contextCollapsed: false,
};
// Asymmetric pair: a 4-wide and a 2-wide panel. Only the wide side can give.
const LOPSIDED = {
  version: 1, layout: 'split',
  tiles: [
    { id: 'tile-1', route: '#/logs', rect: rect(0, 0, 4, 6) },
    { id: 'tile-2', route: '#/config', rect: rect(4, 0, 2, 6) },
  ],
  focused: 'tile-1', maximized: null, column: 50, row: 50, contextCollapsed: false,
};
// A single panel: maximize stays available even with nothing else on canvas.
const SOLO = {
  version: 1, layout: 'single',
  tiles: [{ id: 'tile-1', route: '#/logs', rect: rect(0, 0, 6, 6) }],
  focused: 'tile-1', maximized: null, column: 50, row: 50, contextCollapsed: false,
};
const SPARSE = {
  version: 1, layout: 'grid',
  tiles: [
    { id: 'tile-a', route: '#/logs', rect: rect(0, 0, 3, 3) },
    { id: 'tile-b', route: '#/keys', rect: rect(3, 3, 3, 3) },
  ],
  focused: 'tile-a', maximized: null, column: 50, row: 50, contextCollapsed: false,
};

interface Seed { tiles: { id: string; route: string; rect: { x: number; y: number; w: number; h: number } }[]; maximized?: string | null }
async function boot(page: Page, seed: Seed): Promise<void> {
  // addInitScript serializes the function: free variables do not survive, so
  // every value crosses as an argument.
  await page.addInitScript(({ seed, token }) => {
    localStorage.setItem('openproxy_admin_token', token!);
    localStorage.setItem('openproxy:workspace:v1', JSON.stringify(seed));
  }, { seed, token });
  await page.route('**/admin/api/**', r => {
    const req = r.request();
    if (!['GET', 'HEAD', 'OPTIONS'].includes(req.method()) && !req.url().includes('ws-ticket')) {
      throw new Error(`Tiling sweep attempted ${req.method()} ${new URL(req.url()).pathname}`);
    }
    return r.continue();
  });
  await page.route('**/esbuild', r => r.abort());
  // A goto to the same URL is a same-document navigation: no reload, no init
  // scripts. Break the sameness so every boot re-seeds from scratch.
  if (page.url().startsWith(preview!)) await page.goto('about:blank');
  await page.goto(`${preview}/admin/#/logs`, { waitUntil: 'domcontentloaded' });
  await expect(page.locator('.workspace-tile')).toHaveCount(seed.tiles.length);
}

const slotRoute = (page: Page, index: number) =>
  page.locator(`.workspace-tile[data-index="${index}"] .tile-view`).evaluate(el => (el as HTMLElement).dataset['route']);
const center = (locator: ReturnType<Page['locator']>) =>
  locator.evaluate((el: Element) => {
    const r = el.getBoundingClientRect();
    return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
  });
// The browser normalizes inline grid-column/grid-row into the grid-area
// shorthand: "row / col / span rowSpan / span colSpan".
const tileRect = async (page: Page, index: number) => {
  const area = await page.locator(`.workspace-tile[data-index="${index}"]`).evaluate(el => (el as HTMLElement).style.gridArea);
  const [rowStart, colStart, rowEnd, colEnd] = area.split('/').map(part => parseInt(part.replace('span', '').trim(), 10));
  return rect(colStart! - 1, rowStart! - 1, colEnd!, rowEnd!);
};
const stored = (page: Page) => page.evaluate(() => JSON.parse(localStorage.getItem('openproxy:workspace:v1') || '{}'));
/** Center of a grid cell (6x6, pad 6, gap 8 — keep in sync with workspace.css). */
const cellPoint = (page: Page, cx: number, cy: number) =>
  page.locator('.workspace-grid').evaluate((el, { cx, cy }) => {
    const r = el.getBoundingClientRect();
    const colW = (r.width - 12 - 40) / 6;
    const rowH = (r.height - 12 - 40) / 6;
    return { x: r.left + 6 + cx * (colW + 8) + colW / 2, y: r.top + 6 + cy * (rowH + 8) + rowH / 2 };
  }, { cx, cy });

test.describe('desktop tiling workspace', () => {
  test('dragging a header previews the landing spot with a ghost and swaps on drop', async ({ page }) => {
    await boot(page, HEX);
    const header = page.locator('.workspace-tile[data-index="0"] .tile-header');
    const target = await center(page.locator('.workspace-tile[data-index="5"]'));
    await page.mouse.move((await center(header)).x, (await center(header)).y);
    await page.mouse.down();
    await page.mouse.move(target.x, target.y, { steps: 14 });
    const ghost = page.locator('.workspace-drop-ghost');
    await expect(ghost).toBeVisible();
    await expect(ghost).toHaveClass(/mode-swap/);
    await expect(page.locator('.workspace-drag-badge')).toContainText('Swap with panel 6');
    await expect(page.locator('.workspace-tile[data-index="5"]')).toHaveClass(/drop-target/);
    await page.mouse.up();
    await expect(ghost).toHaveCount(0);
    // Rects are exchanged; ids keep their views, so the mounted panels travel.
    expect(await tileRect(page, 0)).toEqual(rect(4, 3, 2, 3));
    expect(await tileRect(page, 5)).toEqual(rect(0, 0, 2, 3));
    expect(await slotRoute(page, 0)).toBe('#/logs');
    const data = await stored(page);
    expect(data['tiles'].map((t: { route: string }) => t.route)[0]).toBe('#/logs');
    expect(data['tiles'][0]['rect']).toEqual(rect(4, 3, 2, 3));
    await expect(page.locator('.tile-view[data-route="#/logs"] .logs-control-deck')).toBeVisible();
  });

  test('a header drag into empty space relocates the panel freely', async ({ page }) => {
    await boot(page, SPARSE);
    const header = page.locator('.workspace-tile[data-index="0"] .tile-header');
    const hole = await cellPoint(page, 4, 1);
    await page.mouse.move((await center(header)).x, (await center(header)).y);
    await page.mouse.down();
    await page.mouse.move(hole.x, hole.y, { steps: 12 });
    const ghost = page.locator('.workspace-drop-ghost');
    await expect(ghost).toBeVisible();
    await expect(ghost).toHaveClass(/mode-move/);
    await expect(page.locator('.workspace-drag-badge')).toContainText('Place here');
    await page.mouse.up();
    expect(await tileRect(page, 0)).toEqual(rect(3, 0, 3, 3));
    const data = await stored(page);
    expect(data['tiles'][0]['rect']).toEqual(rect(3, 0, 3, 3));
    expect(data['tiles'][1]['rect']).toEqual(rect(3, 3, 3, 3));
  });

  test('sidebar drops load into a panel or spawn one on empty canvas', async ({ page }) => {
    await boot(page, SPARSE);
    // Spawn: the top-right hole takes a brand-new panel.
    const link = page.locator('.sidebar nav a[data-nav="#/debug-logs"]');
    const hole = await cellPoint(page, 4, 1);
    await page.mouse.move((await center(link)).x, (await center(link)).y);
    await page.mouse.down();
    await page.mouse.move(hole.x, hole.y, { steps: 12 });
    await expect(page.locator('.workspace-drop-ghost')).toHaveClass(/mode-free/);
    await page.mouse.up();
    await expect(page.locator('.workspace-tile')).toHaveCount(3);
    const data = await stored(page);
    const spawned = data['tiles'].find((t: { route: string }) => t.route === '#/debug-logs');
    expect(spawned['rect']).toEqual(rect(3, 0, 3, 3));
    // Split: dropping onto an existing panel halves it and spawns a new one
    // (dragging means placing; loading a view into a panel is a plain click).
    const again = page.locator('.sidebar nav a[data-nav="#/playground"]');
    // Aim high inside the panel: the exact center is axis-ambiguous (dx ties
    // dy) and the chosen axis would depend on sub-pixel geometry.
    const pb = (await page.locator('.workspace-tile[data-index="1"]').boundingBox())!;
    const panel = { x: pb.x + pb.width / 2, y: pb.y + pb.height * 0.2 };
    await page.mouse.move((await center(again)).x, (await center(again)).y);
    await page.mouse.down();
    await page.mouse.move(panel.x, panel.y, { steps: 12 });
    await expect(page.locator('.workspace-drop-ghost')).toHaveClass(/mode-free/);
    await page.mouse.up();
    await expect(page.locator('.workspace-tile')).toHaveCount(4);
    const data2 = await stored(page);
    const spawnedP = data2['tiles'].find((t: { route: string }) => t.route === '#/playground');
    const keys = data2['tiles'].find((t: { route: string }) => t.route === '#/keys');
    // tile-b (3x3) gives half of its cells to the new panel: no hole, smaller side to the drop.
    const area = (r: { x: number; y: number; w: number; h: number }) => r.w * r.h;
    expect(area(spawnedP['rect']) + area(keys['rect'])).toBe(9);
    expect(area(keys['rect'])).toBeGreaterThanOrEqual(area(spawnedP['rect']));
    expect(keys['rect']['w']).toBe(3);
    expect(spawnedP['rect']['w']).toBe(3);
  });

  test('edge grips resize one axis and a neighbor stops the edge', async ({ page }) => {
    await boot(page, SPARSE);
    await page.locator('.workspace-tile[data-index="0"]').click({ position: { x: 8, y: 8 } });
    // Stretch the right edge of tile-a (3x3 at cols 0-2): free space allows 4 cols.
    const grip = page.locator('.workspace-tile[data-index="0"] .grip-e');
    const from = await center(grip);
    await page.mouse.move(from.x, from.y);
    await page.mouse.down();
    const wide = await cellPoint(page, 3, 1);
    await page.mouse.move(wide.x + 30, wide.y, { steps: 10 });
    await expect(page.locator('.workspace-drop-ghost')).toHaveClass(/mode-move/);
    await page.mouse.up();
    expect(await tileRect(page, 0)).toEqual(rect(0, 0, 4, 3));
    const data = await stored(page);
    expect(data['tiles'][0]['rect']).toEqual(rect(0, 0, 4, 3));
    // Shrinking from the same grip clamps at the two-column pixel floor.
    const grip2 = page.locator('.workspace-tile[data-index="0"] .grip-e');
    const from2 = await center(grip2);
    await page.mouse.move(from2.x, from2.y);
    await page.mouse.down();
    const narrow = await cellPoint(page, 1, 1);
    await page.mouse.move(narrow.x - 60, narrow.y, { steps: 10 });
    await page.mouse.up();
    const shrunk = (await stored(page))['tiles'][0]['rect'];
    expect(shrunk['w']).toBeGreaterThanOrEqual(2);
    expect(shrunk['w']).toBeLessThanOrEqual(4);
  });

  test('west grip grows left through free space; north grip stops at the neighbor', async ({ page }) => {
    await boot(page, SPARSE);
    // tile-b {3,3,3,3}: dragging its W grip into the bottom-left hole grows it.
    await page.locator('.workspace-tile[data-index="1"]').click({ position: { x: 8, y: 8 } });
    const grip = page.locator('.workspace-tile[data-index="1"] .grip-w');
    const from = await center(grip);
    await page.mouse.move(from.x, from.y);
    await page.mouse.down();
    const hole = await cellPoint(page, 0, 4);
    await page.mouse.move(hole.x - 30, hole.y, { steps: 10 });
    await page.mouse.up();
    expect(await tileRect(page, 1)).toEqual(rect(0, 3, 6, 3));
    // Packed canvas: the tile-1|tile-4 seam handle moves that shared border.
    // A wild overshoot clamps at the pixel floor (the top panel keeps one row).
    await boot(page, HEX);
    const seam = page.locator('.workspace-seam[data-a="tile-1"][data-b="tile-4"]');
    await expect(seam).toHaveCount(1);
    const box2 = (await seam.boundingBox())!;
    await page.mouse.move(box2.x + box2.width / 2, box2.y + box2.height / 2);
    await page.mouse.down();
    await page.mouse.move(box2.x + box2.width / 2, box2.y - 800, { steps: 10 });
    await page.mouse.up();
    const data = await stored(page);
    expect(data['tiles'].find((t: { id: string }) => t.id === 'tile-1')['rect']).toEqual(rect(0, 0, 2, 1));
    expect(data['tiles'].find((t: { id: string }) => t.id === 'tile-4')['rect']).toEqual(rect(0, 1, 2, 5));
  });

  test('a corner grip reshapes in place and never displaces the other panel', async ({ page }) => {
    await boot(page, SPARSE);
    const before = (await stored(page))['tiles'][1]['rect'];
    await page.locator('.workspace-tile[data-index="0"]').click({ position: { x: 8, y: 8 } });
    const grip = page.locator('.workspace-tile[data-index="0"] .grip-se');
    const from = await center(grip);
    await page.mouse.move(from.x, from.y);
    await page.mouse.down();
    const shrink = await cellPoint(page, 1, 1);
    await page.mouse.move(shrink.x - 20, shrink.y - 20, { steps: 10 });
    await page.mouse.up();
    const data = await stored(page);
    expect(data['tiles'][0]['rect']).toEqual(rect(0, 0, 2, 2));
    expect(data['tiles'][1]['rect']).toEqual(before);
  });

  test('a maximized tile keeps its decision through navigation and view picks', async ({ page }) => {
    await boot(page, HEX);
    // Maximize tile-4 (providers): the arrange stays behind it.
    await page.locator('.workspace-tile[data-index="3"] .tile-actions .workspace-icon-button').first().click();
    await expect(page.locator('.workspace-grid')).toHaveClass(/is-maximized/);
    await expect(page.locator('body')).toHaveClass(/workspace-maximized/);
    // A sidebar click loads INTO the hero: maximization survives.
    await page.locator('.sidebar nav a[data-nav="#/config"]').click();
    await expect(page.locator('.workspace-tile[data-index="3"] .tile-view')).toHaveAttribute('data-route', '#/config');
    await expect(page.locator('.workspace-grid')).toHaveClass(/is-maximized/);
    // The view picker on the hero swaps its view without focusing the arrange.
    await page.locator('.workspace-tile[data-index="3"] .tile-title').click();
    await page.locator('.workspace-tile[data-index="3"] .view-menu button').filter({ hasText: 'Playground' }).click();
    await expect(page.locator('.workspace-tile[data-index="3"] .tile-view')).toHaveAttribute('data-route', '#/playground');
    await expect(page.locator('.workspace-grid')).toHaveClass(/is-maximized/);
    const data = await stored(page);
    expect(data['maximized']).toBe('tile-4');
    expect(data['tiles'].find((t: { id: string }) => t.id === 'tile-4')['route']).toBe('#/playground');
  });

  test('a maximized tile swaps routes with the family owner instead of stealing focus', async ({ page }) => {
    await boot(page, HEX);
    // Maximize the logs tile, then navigate to combos (owned by tile-5).
    await page.locator('.workspace-tile[data-index="0"] .tile-actions .workspace-icon-button').first().click();
    await page.locator('.sidebar nav a[data-nav="#/combos"]').click();
    const data = await stored(page);
    expect(data['maximized']).toBe('tile-1');
    expect(data['tiles'].find((t: { id: string }) => t.id === 'tile-1')['route']).toBe('#/combos');
    // The family owner took over the hero's previous route, no duplicate.
    expect(data['tiles'].find((t: { id: string }) => t.id === 'tile-5')['route']).toBe('#/logs');
    await expect(page.locator('.workspace-grid')).toHaveClass(/is-maximized/);
  });

  test('a view dropped on the seam inserts between two tiles', async ({ page }) => {
    await boot(page, SPLIT_SEED);
    // The seam is the gap at the left edge of cell 3: aim the pointer there.
    const grid = (await page.locator('.workspace-grid').boundingBox())!;
    const colW = (grid.width - 12 - 8 * 5) / 6;
    const seamX = grid.x + 6 + 3 * (colW + 8) + 2;
    const seamY = grid.y + grid.height * 0.5;
    const link = page.locator('.sidebar nav a[data-nav="#/providers"]');
    const src = await center(link);
    await page.mouse.move(src.x, src.y);
    await page.mouse.down();
    await page.mouse.move(seamX, seamY, { steps: 12 });
    const ghost = page.locator('.workspace-drop-ghost');
    await expect(ghost).toHaveClass(/mode-free/);
    await page.mouse.up();
    await expect(page.locator('.workspace-tile')).toHaveCount(3);
    const data = await stored(page);
    const spawned = data['tiles'].find((t: { route: string }) => t.route === '#/providers');
    expect(spawned['rect']).toEqual(rect(2, 0, 2, 6));
    // both neighbours gave up one edge, nothing overlaps
    expect(data['tiles'].find((t: { id: string }) => t.id === 'tile-1')!['rect']).toEqual(rect(0, 0, 2, 6));
    expect(data['tiles'].find((t: { id: string }) => t.id === 'tile-2')!['rect']).toEqual(rect(4, 0, 2, 6));
    const all = data['tiles'].map((t: { rect: { x: number; y: number; w: number; h: number } }) => t.rect);
    for (let i = 0; i < all.length; i++) {
      for (let j = i + 1; j < all.length; j++) {
        const [a, b] = [all[i]!, all[j]!];
        expect(a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h).toBe(false);
      }
    }
  });

  test('a seam drop on an asymmetric pair carves only the side that can give', async ({ page }) => {
    await boot(page, LOPSIDED);
    const grid = (await page.locator('.workspace-grid').boundingBox())!;
    const colW = (grid.width - 12 - 8 * 5) / 6;
    // left edge of track 4: the seam between the 4-wide and the 2-wide panel
    const seamX = grid.x + 6 + 4 * (colW + 8) + 2;
    const link = page.locator('.sidebar nav a[data-nav="#/providers"]');
    const src = await center(link);
    await page.mouse.move(src.x, src.y);
    await page.mouse.down();
    await page.mouse.move(seamX, grid.y + grid.height * 0.5, { steps: 12 });
    const ghost = page.locator('.workspace-drop-ghost');
    await expect(ghost).toHaveClass(/mode-free/);
    await page.mouse.up();
    await expect(page.locator('.workspace-tile')).toHaveCount(3);
    const data = await stored(page);
    // only the 4-wide neighbour gave up two columns; the 2-wide one is intact
    expect(data['tiles'].find((t: { id: string }) => t.id === 'tile-1')!['rect']).toEqual(rect(0, 0, 2, 6));
    expect(data['tiles'].find((t: { id: string }) => t.id === 'tile-2')!['rect']).toEqual(rect(4, 0, 2, 6));
    const spawned = data['tiles'].find((t: { route: string }) => t.route === '#/providers');
    expect(spawned['rect']).toEqual(rect(2, 0, 2, 6));
  });

  test('a lone tile can still be maximized and restored', async ({ page }) => {
    await boot(page, SOLO);
    await expect(page.locator('.workspace-tile')).toHaveCount(1);
    // the maximize button exists even when there is no second panel to close
    const max = page.locator('.workspace-tile .tile-actions .workspace-icon-button').first();
    await expect(max).toHaveAttribute('aria-label', 'Maximize panel');
    await max.click();
    await expect(page.locator('.workspace-grid')).toHaveClass(/is-maximized/);
    await expect(page.locator('.workspace-tile')).toHaveCount(1);
    const box = (await page.locator('.workspace-tile').boundingBox())!;
    const grid = (await page.locator('.workspace-grid').boundingBox())!;
    expect(box.width).toBeGreaterThan(grid.width * 0.9);
    expect(box.height).toBeGreaterThan(grid.height * 0.9);
    await expect(page.locator('.workspace-tile .tile-view')).toHaveAttribute('data-route', '#/logs');
    // restoring brings the free canvas back, still with one panel
    await page.locator('.workspace-tile .tile-actions .workspace-icon-button').first().click();
    await expect(page.locator('.workspace-grid')).not.toHaveClass(/is-maximized/);
    await expect(page.locator('.workspace-tile')).toHaveCount(1);
    const data = await stored(page);
    expect(data['maximized']).toBeNull();
  });

  test('a maximized dashboard splits at the pointer and unmaximizes on drop', async ({ page }) => {
    await boot(page, { ...SOLO, maximized: 'tile-1' });
    await expect(page.locator('.workspace-grid.is-maximized')).toHaveCount(1);
    const grid = page.locator('.workspace-grid').first();
    const box = await grid.boundingBox();
    if (!box) throw new Error('no grid');
    const link = page.locator('a[href="#/providers"]').first();
    const sb = await link.boundingBox();
    if (!sb) throw new Error('no sidebar link');
    await page.mouse.move(sb.x + sb.width / 2, sb.y + sb.height / 2);
    await page.mouse.down();
    for (let i = 1; i <= 12; i++) {
      await page.mouse.move(sb.x + sb.width / 2 + (box.x + box.width * 0.25 - sb.x) * i / 12, sb.y + sb.height / 2 + (box.y + box.height / 2 - sb.y) * i / 12);
      await page.waitForTimeout(25);
    }
    await expect(page.locator('.workspace-drop-ghost')).toHaveClass(/mode-free/, { timeout: 3000 });
    await page.mouse.up();
    await expect(page.locator('.workspace-tile')).toHaveCount(2);
    const data = await stored(page);
    expect(data['maximized']).toBeNull();
    const providers = data['tiles'].find((t: { route: string }) => t.route === '#/providers')!;
    const hero = data['tiles'].find((t: { id: string }) => t.id === 'tile-1')!;
    // the drop took the left half of the FULL canvas, the hero took the right half
    expect(providers['rect']).toEqual(rect(0, 0, 3, 6));
    expect(hero['rect']).toEqual(rect(3, 0, 3, 6));
  });

  test('a sidebar drop on a lone tile splits it into two clean halves', async ({ page }) => {
    await boot(page, SOLO);
    const grid = page.locator('.workspace-grid').first();
    const box = await grid.boundingBox();
    if (!box) throw new Error('no grid');
    const link = page.locator('a[href="#/providers"]').first();
    const sb = await link.boundingBox();
    if (!sb) throw new Error('no sidebar link');
    await page.mouse.move(sb.x + sb.width / 2, sb.y + sb.height / 2);
    await page.mouse.down();
    for (let i = 1; i <= 12; i++) {
      await page.mouse.move(sb.x + sb.width / 2 + (box.x + box.width * 0.12 - sb.x) * i / 12, sb.y + sb.height / 2 + (box.y + box.height / 2 - sb.y) * i / 12);
      await page.waitForTimeout(25);
    }
    await expect(page.locator('.workspace-drop-ghost')).toHaveClass(/mode-free/, { timeout: 3000 });
    await page.mouse.up();
    await expect(page.locator('.workspace-tile')).toHaveCount(2);
    const rects = await page.evaluate(() =>
      [...document.querySelectorAll('.workspace-tile')].map(el => {
        const m = /(\d+)\s*\/\s*(\d+)\s*\/\s*span\s*(\d+)\s*\/\s*span\s*(\d+)/.exec((el as HTMLElement).style.gridArea || '');
        return m ? `${m[2]},${m[1]},${m[4]},${m[3]}` : '';
      }),
    );
    // the two halves cover the canvas exactly: cols 1-3 and 4-6, full height
    expect([...rects].sort()).toEqual(['1,1,3,6', '4,1,3,6']);
    const routes = await page.evaluate(() =>
      [...document.querySelectorAll('.workspace-tile')].map(el => el.querySelector('.tile-view')?.getAttribute('data-route')),
    );
    expect(routes).toContain('#/providers');
  });

  test('a view dropped on a panel splits that panel, not the whole canvas', async ({ page }) => {
    await boot(page, SPLIT_SEED);
    const grid = (await page.locator('.workspace-grid').boundingBox())!;
    const colW = (grid.width - 12 - 8 * 5) / 6;
    // middle of the left panel: far from both edges
    const link = page.locator('.sidebar nav a[data-nav="#/providers"]');
    const src = await center(link);
    const px = grid.x + 6 + 1.5 * (colW + 8);
    await page.mouse.move(src.x, src.y);
    await page.mouse.down();
    // Near the top edge: the exact middle is axis-ambiguous on wide canvases
    // (a 1-column half clears the px floors there and the nearest-edge rule
    // would pick the vertical axis); a top-anchored drop always splits height.
    await page.mouse.move(px, grid.y + grid.height * 0.08, { steps: 12 });
    await expect(page.locator('.workspace-drop-ghost')).toHaveClass(/mode-free/);
    await page.mouse.up();
    await expect(page.locator('.workspace-tile')).toHaveCount(3);
    const data = await stored(page);
    const logs = data['tiles'].find((t: { id: string }) => t.id === 'tile-1')!;
    const config = data['tiles'].find((t: { id: string }) => t.id === 'tile-2')!;
    const providers = data['tiles'].find((t: { route: string }) => t.route === '#/providers')!;
    // the tall left panel gave half its height to the new view (top or bottom
    // depending on the pointer's row); the right panel is untouched
    expect(logs['rect']['w']).toBe(3);
    expect(logs['rect']['h']).toBe(3);
    expect(providers['rect']['w']).toBe(3);
    expect(providers['rect']['h']).toBe(3);
    expect(config['rect']).toEqual(rect(3, 0, 3, 6));
  });

  test('a plain sidebar click still navigates the focused panel after drag support', async ({ page }) => {
    await boot(page, SPARSE);
    await page.locator('.sidebar nav a[data-nav="#/playground"]').click();
    await expect(page.locator('.workspace-tile[data-index="0"] .tile-view')).toHaveAttribute('data-route', '#/playground');
  });


  test('a tile dropped on the CENTER of a neighbour swaps their rects', async ({ page }) => {
    await boot(page, SPLIT_SEED);
    const target = (await page.locator('.workspace-tile[data-panel="tile-2"]').boundingBox())!;
    // The header's button-free middle starts the drag; the title button keeps its menu click.
    const header = (await page.locator('.workspace-tile[data-panel="tile-1"] .tile-header').boundingBox())!;
    await page.mouse.move(header.x + header.width * 0.55, header.y + header.height / 2);
    await page.mouse.down();
    await page.mouse.move(target.x + target.width / 2, target.y + target.height / 2, { steps: 10 });
    await expect(page.locator('.workspace-drop-ghost')).toHaveClass(/mode-swap/, { timeout: 3000 });
    await page.mouse.up();
    const data = await stored(page);
    const logs = data['tiles'].find((t: { id: string }) => t.id === 'tile-1')!;
    const config = data['tiles'].find((t: { id: string }) => t.id === 'tile-2')!;
    // full place exchange: each panel now lives where the other one lived
    expect(logs['rect']).toEqual(rect(3, 0, 3, 6));
    expect(config['rect']).toEqual(rect(0, 0, 3, 6));
  });

  test('a seam handle resizes two neighbours at once without moving the rest', async ({ page }) => {
    await boot(page, SPLIT_SEED);
    const seam = page.locator('.workspace-seam.seam-v');
    await expect(seam).toHaveCount(1);
    const box = (await seam.boundingBox())!;
    const grid = (await page.locator('.workspace-grid').boundingBox())!;
    const colW = (grid.width - 12 - 8 * 5) / 6;
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await page.mouse.down();
    // one full column to the right: the left panel grows, the right one shrinks
    await page.mouse.move(box.x + box.width / 2 + colW + 8, box.y + box.height / 2, { steps: 8 });
    await page.mouse.up();
    const data = await stored(page);
    expect(data['tiles'].find((t: { id: string }) => t.id === 'tile-1')!['rect']).toEqual(rect(0, 0, 4, 6));
    expect(data['tiles'].find((t: { id: string }) => t.id === 'tile-2')!['rect']).toEqual(rect(4, 0, 2, 6));
    // keyboard works too: focus the handle and grow the left panel once more
    await seam.focus();
    await page.keyboard.press('ArrowRight');
    const data2 = await stored(page);
    expect(data2['tiles'].find((t: { id: string }) => t.id === 'tile-1')!['rect']).toEqual(rect(0, 0, 5, 6));
    expect(data2['tiles'].find((t: { id: string }) => t.id === 'tile-2')!['rect']).toEqual(rect(5, 0, 1, 6));
  });

  test('closing panels walks the count back down and keeps rects', async ({ page }) => {
    await boot(page, HEX);
    for (let i = 6; i >= 2; i--) {
      await page.locator('.workspace-tile[data-index="0"] .tile-actions .workspace-icon-button').last().click();
      await expect(page.locator('.workspace-tile')).toHaveCount(i - 1);
    }
    await expect(page.locator('.workspace-grid')).toHaveClass(/layout-single/);
    const data = await stored(page);
    expect(data['tiles']).toHaveLength(1);
    expect(data['tiles'][0]['id']).toBe('tile-6');
    expect(data['tiles'][0]['rect']).toEqual(rect(4, 3, 2, 3));
  });
});
