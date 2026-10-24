// tests/e2e/columns-toggle.spec.ts — the user can show / hide columns in
// the /logs view and the selection persists in localStorage across reloads.
// @see tsconfig.test.json for type settings.

import { test, expect, type Page } from '@playwright/test';

const STORAGE_KEY = 'openproxy:logs:visibleColumns';

// All 12 columns defined in lib/constants.ts. Order matches the
// header rendering, so we can assert index-based positions.
const ALL_COLUMNS: readonly string[] = ['time', 'phase', 'type', 'client', 'status', 'provider', 'model', 'tokens', 'latency', 'cost', 'cache', 'compression'];
const HEADER_LABELS: readonly string[] = ['Time', 'Phase', 'Endpoint', 'Client', 'Status', 'Provider', 'Model', 'Tokens', 'Latency', 'Cost', 'API Cache', 'Compress'];

async function gotoLogs(page: Page): Promise<void> {
  await page.goto('http://localhost:8790/#/logs');
  // Wait for the logs view to render the header row. Once a header
  // span with `data-col` exists, the view has fully mounted.
  await page.waitForSelector('#logs .log-row [data-col="time"]', { timeout: 10000 });
  // And give the WS a tick so any in-flight rows don't race the
  // first assertion (they shouldn't, since the table is empty on
  // a fresh load, but the existing e2e spec does this too).
  await expect(page.locator('#logs-connection-status')).toHaveText('🟢 connected', { timeout: 10000 });
}

test.beforeEach(async ({ page }: { page: Page }) => {
  // Clear localStorage before the first navigation only. We can't
  // use addInitScript (it runs on every navigation, including
  // reloads, which would erase the user's choice). Instead, do a
  // one-shot visit to the origin with a clear query and then
  // proceed to /logs. The clear query string is a no-op in the
  // app code, it just gives us a hook to run code before the
  // test's real navigation.
  await page.goto('http://localhost:8790/');
  await page.evaluate((key: string) => {
    try { localStorage.removeItem(key); } catch (_e) { void _e; }
  }, STORAGE_KEY);
});

test('Columns toggle: show/hide + localStorage persistence', async ({ page }: { page: Page }) => {
  await gotoLogs(page);
  const header = page.locator('#logs .log-row').first();
  const headerCols = header.locator('[data-col]');
  await expect(headerCols).toHaveCount(12);
  for (let i = 0; i < ALL_COLUMNS.length; i++) {
    await expect(headerCols.nth(i)).toHaveAttribute('data-col', ALL_COLUMNS[i]!);
    await expect(headerCols.nth(i)).toHaveText(HEADER_LABELS[i]!);
  }

  const columnsBtn = page.locator('#logs-columns-toggle');
  await expect(columnsBtn).toBeVisible();
  await expect(columnsBtn).toHaveText(/Columns/);
  const menu = page.locator('.columns-menu');
  await expect(menu).toBeAttached();
  await expect(menu).not.toHaveClass(/open/);
  await expect(columnsBtn).toHaveAttribute('aria-expanded', 'false');

  await columnsBtn.click();
  await expect(menu).toHaveClass(/open/);
  await expect(columnsBtn).toHaveAttribute('aria-expanded', 'true');

  const checkboxes = menu.locator('input[type="checkbox"]');
  await expect(checkboxes).toHaveCount(12);
  for (let i = 0; i < ALL_COLUMNS.length; i++) {
    await expect(checkboxes.nth(i)).toBeChecked();
    await expect(checkboxes.nth(i)).toHaveAttribute('data-arg1', ALL_COLUMNS[i]!);
  }

  const costBox = menu.locator('input[data-arg1="cost"]');
  await costBox.click();
  await expect(menu).toHaveClass(/open/);
  await expect(header.locator('.log-cost')).toHaveCount(0);
  const stored = await page.evaluate((k: string) => JSON.parse(localStorage.getItem(k) || '[]'), STORAGE_KEY);
  expect(stored).toEqual(['time', 'phase', 'type', 'client', 'status', 'provider', 'model', 'tokens', 'latency', 'cache', 'compression']);
  expect(stored).not.toContain('cost');

  await page.reload();
  await gotoLogs(page);
  const header2 = page.locator('#logs .log-row').first();
  await expect(header2.locator('[data-col]')).toHaveCount(11);
  await expect(header2.locator('.log-cost')).toHaveCount(0);
  await page.locator('#logs-columns-toggle').click();
  await expect(page.locator('.columns-menu')).toHaveClass(/open/);
  await expect(page.locator('.columns-menu input[data-arg1="cost"]')).not.toBeChecked();

  await page.locator('.columns-menu input[data-arg1="cost"]').click();
  await expect(page.locator('#logs .log-row').first().locator('.log-cost')).toHaveCount(1);
  const storedAfter = await page.evaluate((k: string) => JSON.parse(localStorage.getItem(k) || '[]'), STORAGE_KEY);
  expect(storedAfter).toContain('cost');
  expect(storedAfter).toHaveLength(12);

  await page.locator('body').click({ position: { x: 5, y: 5 } });
  await expect(page.locator('.columns-menu')).not.toHaveClass(/open/);
  await page.locator('#logs-columns-toggle').click();
  for (const key of ['phase', 'type', 'client', 'status', 'provider', 'model', 'tokens', 'latency', 'cost', 'cache', 'compression']) {
    await page.locator(`.columns-menu input[data-arg1="${key}"]`).click();
  }
  await page.locator('.columns-menu input[data-arg1="time"]').click();
  // Ensure the DOM reflects that unchecking the last remaining column is prevented
  await page.waitForTimeout(100);
  const storedMin = await page.evaluate((k: string) => JSON.parse(localStorage.getItem(k) || '[]'), STORAGE_KEY);
  expect(storedMin).toEqual(['time']);
  await expect(page.locator('.columns-menu input[data-arg1="time"]')).toBeChecked();
});
