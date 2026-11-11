/// <reference types="node" />
import { test, expect } from '@playwright/test';

// Read-only preview regression: no tests, reorder, or settings writes to the backend.
const preview = process.env['OPENPROXY_LAYOUT_TEST_URL'];
const token = process.env['OPENPROXY_LAYOUT_TEST_TOKEN'];
const comboId = process.env['OPENPROXY_LAYOUT_TEST_COMBO'] || '2';
test.skip(!preview || !token, 'Requires an authenticated frontend preview');
test.use({ storageState: { cookies: [], origins: [] }, viewport: { width: 1600, height: 1000 } });

test('target cards use available columns without stretching actions', async ({ page }) => {
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.addInitScript(({ token, comboId }) => {
    localStorage.setItem('openproxy_admin_token', token!);
    localStorage.setItem('openproxy:workspace:v1', JSON.stringify({
      version: 1, layout: 'split',
      tiles: [{ id: 'target', route: `#/combos/${comboId}` }, { id: 'other', route: '#/providers' }],
      focused: 'target', maximized: null, column: 40, row: 50, contextCollapsed: true,
    }));
  }, { token, comboId });
  await page.route('**/admin/api/**', route => {
    const request = route.request();
    if (!['GET', 'HEAD', 'OPTIONS'].includes(request.method()) && !request.url().includes('ws-ticket')) {
      throw new Error(`Read-only layout test attempted ${request.method()}`);
    }
    return route.continue();
  });
  await page.route('**/esbuild', route => route.abort());
  await page.goto(`${preview}/admin/#/combos/${comboId}`);
  const tile = page.locator('.workspace-tile[data-index="0"]');
  const row = tile.locator('.combo-target-card-row').first();
  await expect(row).toBeVisible();
  for (const width of [565, 420, 320, 250, 1000, 565]) {
    await page.locator('.workspace-grid').evaluate((grid, width) => {
      (grid as HTMLElement).style.width = `${Number(width) * 2 + 20}px`;
    }, width);
    await expect.poll(() => row.evaluate(row => getComputedStyle(row).display))
      .toBe(width > 840 ? 'table-row' : 'grid');
    if (width <= 840) {
      const fields = row.locator('.col-target-provider, .col-target-account, .col-target-context');
      const boxes = await fields.evaluateAll(fields => fields.map(field => field.getBoundingClientRect().top));
      if (width > 280) {
        // Two or three editable fields share a row.
        expect(Math.abs(boxes[0]! - boxes[1]!)).toBeLessThan(1);
      } else {
        // Micro tiles stack every field: a shared row would crush the values.
        expect(boxes[1]!).toBeGreaterThan(boxes[0]! + 1);
        expect(boxes[2]!).toBeGreaterThan(boxes[1]! + 1);
        const widths = await fields.evaluateAll(fields => fields.map(field => field.getBoundingClientRect().width));
        const rowWidth = await row.evaluate(row => row.getBoundingClientRect().width);
        expect(widths.every(fieldWidth => fieldWidth > rowWidth * 0.7)).toBe(true);
      }
      if (width > 480) {
        expect(Math.abs(boxes[1]! - boxes[2]!)).toBeLessThan(1);
        expect(await row.evaluate(row => row.getBoundingClientRect().height)).toBeLessThan(230);
      }
      // Actions keep an intrinsic size: they never stretch across the whole card.
      expect(await row.evaluate(row => {
        const rowWidth = row.getBoundingClientRect().width;
        return [...row.querySelectorAll('.target-actions-wrap button')]
          .every(button => button.getBoundingClientRect().width < rowWidth * 0.7);
      })).toBe(true);
    }
    expect(await tile.locator('.tile-view').evaluate(view => view.scrollWidth - view.clientWidth)).toBeLessThanOrEqual(1);
    expect(await row.evaluate(row => {
      const bounds = row.getBoundingClientRect();
      return [...row.querySelectorAll('td, button, select, input')].every(control => {
        const rect = control.getBoundingClientRect();
        return !rect.width || (rect.right <= bounds.right + 1 && rect.left >= bounds.left - 1);
      });
    })).toBe(true);
  }
  // Opening account selection must not mutate any configuration.
  const trigger = row.locator('.target-accounts-btn');
  if (await trigger.count()) {
    await trigger.click();
    await expect(row.locator('.target-accounts-menu')).toBeVisible();
    await row.locator('.target-accounts-backdrop').click({ position: { x: 1, y: 1 }, force: true });
    await expect(row.locator('.target-accounts-menu')).toHaveCount(0);
  }
  expect(errors).toEqual([]);
});
