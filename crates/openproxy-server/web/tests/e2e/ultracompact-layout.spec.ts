/// <reference types="node" />
import { test, expect } from '@playwright/test';

// Opt-in, read-only preview checks. Never create accounts or change provider state.
const preview = process.env['OPENPROXY_LAYOUT_TEST_URL'];
const token = process.env['OPENPROXY_LAYOUT_TEST_TOKEN'];
const provider = process.env['OPENPROXY_LAYOUT_TEST_PROVIDER'] || 'antigravity';
test.skip(!preview || !token, 'Requires an authenticated frontend preview');
test.use({ storageState: { cookies: [], origins: [] }, viewport: { width: 1440, height: 1000 } });

test.beforeEach(async ({ page }) => {
  await page.addInitScript(({ token, provider }) => {
    localStorage.setItem('openproxy_admin_token', token!);
    localStorage.setItem('openproxy:workspace:v1', JSON.stringify({
      version: 1, layout: 'split',
      tiles: [{ id: 'compact', route: `#/providers/${provider}` }, { id: 'other', route: '#/logs' }],
      focused: 'compact', maximized: null, column: 30, row: 50, contextCollapsed: true,
    }));
  }, { token, provider });
  await page.route('**/admin/api/**', route => {
    const request = route.request();
    if (!['GET', 'HEAD', 'OPTIONS'].includes(request.method()) && !request.url().includes('ws-ticket')) {
      throw new Error(`Read-only layout test attempted ${request.method()} ${new URL(request.url()).pathname}`);
    }
    return route.continue();
  });
  await page.route('**/esbuild', route => route.abort());
  await page.goto(`${preview}/admin/#/providers/${provider}`);
  await expect(page.locator('.account-card-row').first()).toBeVisible();
});

for (const width of [250, 280, 320, 760]) {
  test(`accounts and models fit a native ${width}px tile`, async ({ page }) => {
    await page.locator('.workspace-grid').evaluate((grid, width) => {
      (grid as HTMLElement).style.width = `${Number(width) * 2 + 20}px`;
    }, width);
    const tile = page.locator('.workspace-tile[data-index="0"]');
    await tile.locator(`#provider-${provider}-tab-accounts`).click();
    const name = tile.locator('.account-name-text').first();
    await expect.poll(() => name.evaluate(element => element.getBoundingClientRect().width)).toBeGreaterThan(80);
    expect(await name.evaluate(element => getComputedStyle(element).whiteSpace)).toBe('nowrap');
    await expect(name).toHaveAttribute('title', /.+/);
    await tile.locator('.account-name-button').first().click();
    await expect(tile.locator('.account-name-input').first()).toBeVisible();
    await tile.locator('.account-name-input').first().press('Escape');
    await tile.locator(`#provider-${provider}-tab-models`).click();
    await expect(tile.locator('.dg').first()).toBeVisible();
    await expect.poll(() => tile.evaluate(tile => {
      const view = tile.querySelector<HTMLElement>('.tile-view')!;
      const content = tile.querySelector<HTMLElement>('.tile-content')!;
      return Math.max(view.scrollWidth - view.clientWidth, content.scrollWidth - content.clientWidth);
    })).toBeLessThanOrEqual(1);
    for (const button of await tile.locator('.filter-tab').all()) {
      expect(await button.evaluate(element => element.getBoundingClientRect().height)).toBeLessThanOrEqual(30);
    }
    const grid = tile.locator('.dg').first();
    await expect.poll(() => grid.evaluate(grid => {
      const bounds = grid.getBoundingClientRect();
      return [...grid.querySelectorAll<HTMLElement>('.dg-actions')].every(actions =>
        actions.getBoundingClientRect().right <= bounds.right + 1);
    })).toBe(true);
    const nameButton = tile.locator('.dg-row [title="Test model"]').first();
    await expect(nameButton).toBeVisible();
  });
}

test('live logs stay readable in a 200px tile', async ({ page }) => {
  await page.goto(`${preview}/admin/#/logs`);
  const tile = page.locator('.workspace-tile').filter({ has: page.locator('.tile-view[data-view="logs"]') }).first();
  await expect(tile.locator('button.log-row').first()).toBeVisible();
  for (const width of [200, 280, 420]) {
    // Both panes get the width under test: the logs tile may be either one.
    await page.locator('.workspace-grid').evaluate((grid, width) => {
      (grid as HTMLElement).style.width = `${Number(width) * 2 + 20}px`;
    }, width);
    // Nothing scrolls sideways: the deck and the rows must reflow, not overflow.
    await expect.poll(() => tile.evaluate(tile => {
      const view = tile.querySelector<HTMLElement>('.tile-view')!;
      const deck = tile.querySelector<HTMLElement>('.logs-control-deck');
      const pag = tile.querySelector<HTMLElement>('.logs-pagination');
      return Math.max(
        view.scrollWidth - view.clientWidth,
        deck ? deck.scrollWidth - deck.clientWidth : 0,
        pag ? pag.scrollWidth - pag.clientWidth : 0,
      );
    })).toBeLessThanOrEqual(1);
    // The model name is never clipped: it wraps instead of hiding the provider.
    expect(await tile.locator('.log-model-prov').first().evaluate(model =>
      model.scrollWidth - model.clientWidth)).toBeLessThanOrEqual(1);
    if (width <= 280) {
      expect(await tile.locator('.log-model-prov').first().evaluate(model =>
        getComputedStyle(model).overflowWrap)).toBe('anywhere');
    }
  }
});

test('shared grids refold and restore columns when a tile resizes', async ({ page }) => {
  const tile = page.locator('.workspace-tile[data-index="0"]');
  // Use the native route without a backend mutation.
  await page.goto(`${preview}/admin/#/providers`);
  const grid = tile.locator('.dg').first();
  await expect(grid).toBeVisible();
  for (const width of [760, 250, 760]) {
    await page.locator('.workspace-grid').evaluate((grid, width) => {
      (grid as HTMLElement).style.width = `${Number(width) * 2 + 20}px`;
    }, width);
    await expect.poll(() => grid.evaluate(grid => {
      const actions = grid.querySelector<HTMLElement>('.dg-row .dg-actions');
      return !actions || actions.getBoundingClientRect().right <= grid.getBoundingClientRect().right + 1;
    })).toBe(true);
    const visibleColumns = () => grid.evaluate(grid =>
      [...grid.querySelectorAll<HTMLElement>('.dg-head > .dg-col')]
        .filter(cell => getComputedStyle(cell).visibility !== 'hidden').length);
    if (width < 300) await expect.poll(visibleColumns).toBeLessThanOrEqual(2);
    else await expect.poll(visibleColumns).toBeGreaterThan(2);
  }
});
