/// <reference types="node" />
import { test, expect } from '@playwright/test';
import { readFileSync } from 'node:fs';

const storage = JSON.parse(readFileSync('tests/e2e/storageState.json', 'utf8'));
const token = storage.origins[0].localStorage.find((item: { name: string }) => item.name === 'openproxy_admin_token').value;
const baseURL = process.env['OPENPROXY_TILING_TEST_URL'] || 'http://localhost:8790';
test.use({ baseURL, storageState: { cookies: [], origins: [{ origin: baseURL, localStorage: [{ name: 'openproxy_admin_token', value: token }] }] } });

const headers = { Authorization: `Bearer ${token}` };
test.beforeEach(async ({ request }) => {
  if (!['http://localhost:5174', 'http://localhost:8790'].includes(baseURL)) throw new Error('Only isolated test backends are allowed');
  for (const [label, apiKey] of [
    ['compact-layout-test-account-with-long-label@example.invalid', 'test-only-not-a-real-key'],
    ['compact-layout-second@example.invalid', 'test-only-not-a-real-key-2'],
  ] as const) {
    const response = await request.post('/admin/api/accounts', { headers, data: {
      provider_id: 'laya', label, api_key: apiKey, scopes: [],
    } });
    expect(response.ok()).toBe(true);
  }
});
test.afterEach(async ({ request }) => {
  const response = await request.get('/admin/api/accounts', { headers });
  const accounts = await response.json() as Array<{id: number; label: string}>;
  for (const account of accounts.filter(item => item.label === 'compact-layout-test-account-with-long-label@example.invalid' || item.label === 'compact-layout-second@example.invalid')) {
    await request.delete(`/admin/api/accounts/${account.id}`, { headers });
  }
});

for (const viewport of [{ width: 1600, height: 1000 }, { width: 390, height: 844 }]) {
  test(`provider tabs adapt to native panel width at viewport ${viewport.width}`, async ({ page }) => {
    await page.setViewportSize(viewport);
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.goto('/admin/#/providers/laya');
    const view = page.locator('.tile-view[data-view="provider-detail"]');
    await expect(view.locator('.account-card-row').first()).toBeVisible();
    const tabs = view.getByRole('tab');
    await expect(tabs).toHaveCount(3);
    const widths = viewport.width > 768 ? [280, 493, 858, 1200] : [280, 340];
    for (const width of widths) {
      await view.evaluate((element, width) => { element.style.width = `${width}px`; }, width);
      for (let index = 0; index < 3; index++) {
        await tabs.nth(index).click();
        await expect(tabs.nth(index)).toHaveAttribute('aria-selected', 'true');
        await expect(view.getByRole('tabpanel')).toHaveCount(1);
        if (index === 0) {
          const entry = view.getByRole('tabpanel').locator('.provider-entry').first();
          await expect(entry).toBeVisible();
          expect(await entry.evaluate(el => el.getBoundingClientRect().height), `account row too tall at ${width}`).toBeLessThan(width < 400 ? 240 : 200);
          await expect(entry.locator('.account-drag-handle')).toBeVisible();
          await expect(entry.locator('.account-health-select')).toBeVisible();
          await expect(entry.locator('.provider-entry-actions button').first()).toBeVisible();
          const nameButton = entry.locator('.account-name-button');
          await expect(nameButton).toBeVisible();
          await expect(entry.locator('.account-name-input')).toBeHidden();
          await nameButton.click();
          await expect(entry.locator('.account-name-input')).toBeVisible();
          await entry.locator('.account-name-input').press('Escape');
          await expect(entry.locator('.account-name-input')).toBeHidden();
          expect(await view.evaluate(el => el.scrollWidth > el.clientWidth + 1), `account overflow at ${width}`).toBe(false);
        }
        if (index === 1) {
          const entry = view.getByRole('tabpanel').locator('.provider-entry').first();
          await expect(entry).toBeVisible();
          expect(await entry.evaluate(el => el.getBoundingClientRect().height), `summary too tall at ${width}`).toBeLessThan(width < 400 ? 220 : 160);
          await expect(entry.locator('details')).not.toHaveAttribute('open');
          await entry.locator('summary').click();
          await expect(entry.locator('select').first()).toBeVisible();
          expect(await view.evaluate(el => el.scrollWidth > el.clientWidth + 1), `expanded editor overflow at ${width}`).toBe(false);
          await entry.locator('summary').click();
          await expect(view.getByRole('tabpanel').locator('thead')).toHaveCount(0);
        }
        if (index < 2 && [280, 858].includes(width)) {
          await view.screenshot({ path: `/root/.hermes/cache/scratch/openproxy-tiling/dense-${viewport.width}-${width}-${index}.png` });
        }
        const metrics = await view.evaluate(element => {
          const panel = element.querySelector<HTMLElement>('[role="tabpanel"]:not([hidden])')!;
          const tables = Array.from(panel.querySelectorAll<HTMLElement>('.table-wrap'));
          return { overflow: element.scrollWidth > element.clientWidth + 1,
            tableOverflow: tables.some(table => table.scrollWidth > table.clientWidth + 1) };
        });
        expect(metrics.overflow, `tile overflow at ${width}, tab ${index}`).toBe(false);
        if (width <= 760) expect(metrics.tableOverflow, `table overflow at ${width}, tab ${index}`).toBe(false);
      }
    }
    await tabs.nth(1).click();
    const search = view.getByPlaceholder('Search models...');
    await search.fill('gemini');
    await tabs.nth(2).click();
    await expect(view.locator('.provider-config')).toBeVisible();
    await tabs.nth(1).click();
    await expect(search).toHaveValue('gemini');
    await tabs.nth(1).press('ArrowRight');
    await expect(tabs.nth(2)).toBeFocused();
    await expect(tabs.nth(2)).toHaveAttribute('aria-selected', 'true');
    if (viewport.width > 768) {
      await tabs.nth(0).click();
      const first = view.locator('.account-card-row').filter({ hasText: 'compact-layout-test-account-with-long-label' });
      const second = view.locator('.account-card-row').filter({ hasText: 'compact-layout-second' });
      await expect(first).toBeVisible();
      await expect(second).toBeVisible();
      const handle = second.locator('.account-drag-handle');
      const start = await handle.boundingBox();
      const target = await first.boundingBox();
      expect(start && target).toBeTruthy();
      const [reordered] = await Promise.all([
        page.waitForResponse(r => r.url().includes('/accounts/reorder') && r.request().method() === 'POST'),
        (async () => {
          await page.mouse.move(start!.x + 4, start!.y + 4);
          await page.mouse.down();
          await page.mouse.move(start!.x + 8, start!.y - 14, { steps: 3 });
          await page.mouse.move(start!.x + 8, target!.y + 6, { steps: 8 });
          await page.mouse.up();
        })(),
      ]);
      expect(reordered.ok()).toBe(true);
      await page.waitForTimeout(450);
      const order = await view.locator('.accounts-list > .account-card-row').evaluateAll(rows => rows.map(r => r.getAttribute('data-drag-id')));
      const firstId = await first.getAttribute('data-drag-id');
      const secondId = await second.getAttribute('data-drag-id');
      expect(order.indexOf(firstId!), `dragged account should precede the other`).toBeGreaterThan(order.indexOf(secondId!));
      expect(await page.locator('.account-drag-ghost').count()).toBe(0);
      expect(await page.locator('.account-card-row.drag-slot').count()).toBe(0);
    }
    expect(errors).toEqual([]);
  });
}
