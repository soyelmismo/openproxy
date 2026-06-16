// tests/e2e/columns-toggle.spec.js — verifies the user can show /
// hide columns in the /logs view and that the selection persists
// in localStorage across page reloads. This is the spec's
// verification section, automated via Playwright (the project
// already uses Playwright; the spec's mention of "puppeteer-core"
// was the concept, not the library name).

const { test, expect } = require('@playwright/test');

const STORAGE_KEY = 'openproxy:logs:visibleColumns';

// All 8 columns defined in lib/constants.js. Order matches the
// header rendering, so we can assert index-based positions.
const ALL_COLUMNS = ['time', 'phase', 'status', 'provider', 'model', 'tokens', 'latency', 'cost'];
const HEADER_LABELS = ['Time', 'Phase', 'Status', 'Provider', 'Model', 'Tokens', 'Latency', 'Cost'];

async function gotoLogs(page) {
  await page.goto('http://localhost:8788/#/logs');
  // Wait for the logs view to render the header row. Once a header
  // span with `data-col` exists, the view has fully mounted.
  await page.waitForSelector('#logs .log-row [data-col="time"]', { timeout: 10000 });
  // And give the WS a tick so any in-flight rows don't race the
  // first assertion (they shouldn't, since the table is empty on
  // a fresh load, but the existing e2e spec does this too).
  await expect(page.locator('#logs-connection-status')).toHaveText('🟢 connected', { timeout: 10000 });
}

test.beforeEach(async ({ page }) => {
  // Clear localStorage before the first navigation only. We can't
  // use addInitScript (it runs on every navigation, including
  // reloads, which would erase the user's choice). Instead, do a
  // one-shot visit to the origin with a clear query and then
  // proceed to /logs. The clear query string is a no-op in the
  // app code, it just gives us a hook to run code before the
  // test's real navigation.
  await page.goto('http://localhost:8788/');
  await page.evaluate((key) => {
    try { localStorage.removeItem(key); } catch (_) {}
  }, STORAGE_KEY);
});

test('Columns toggle: show/hide + localStorage persistence', async ({ page }) => {
  // 1. Navigate to /logs. The header has 8 columns.
  await gotoLogs(page);
  const header = page.locator('#logs > .log-row').first();
  const headerCols = header.locator('[data-col]');
  await expect(headerCols).toHaveCount(8);
  for (let i = 0; i < ALL_COLUMNS.length; i++) {
    await expect(headerCols.nth(i)).toHaveAttribute('data-col', ALL_COLUMNS[i]);
    await expect(headerCols.nth(i)).toHaveText(HEADER_LABELS[i]);
  }

  // 2. The "Columns" button exists, the menu exists, and the
  // menu starts closed (no .open class).
  const columnsBtn = page.locator('#logs-columns-toggle');
  await expect(columnsBtn).toBeVisible();
  await expect(columnsBtn).toHaveText(/Columns/);
  const menu = page.locator('.columns-menu');
  await expect(menu).toBeAttached();
  await expect(menu).not.toHaveClass(/open/);
  await expect(columnsBtn).toHaveAttribute('aria-expanded', 'false');

  // 3. Click the button → menu opens.
  await columnsBtn.click();
  await expect(menu).toHaveClass(/open/);
  await expect(columnsBtn).toHaveAttribute('aria-expanded', 'true');

  // 4. 8 checkboxes, all checked.
  const checkboxes = menu.locator('input[type="checkbox"]');
  await expect(checkboxes).toHaveCount(8);
  for (let i = 0; i < ALL_COLUMNS.length; i++) {
    await expect(checkboxes.nth(i)).toBeChecked();
    await expect(checkboxes.nth(i)).toHaveAttribute('data-arg1', ALL_COLUMNS[i]);
  }

  // 5. Uncheck "cost" → menu stays open, header no longer has
  // the .log-cost cell, body rows no longer have .log-cost cells.
  const costBox = menu.locator('input[data-arg1="cost"]');
  await costBox.click();
  // Menu must still be open after a checkbox click.
  await expect(menu).toHaveClass(/open/);
  // Header no longer renders the .log-cost cell.
  await expect(header.locator('.log-cost')).toHaveCount(0);
  // localStorage now omits "cost".
  const stored = await page.evaluate((k) => JSON.parse(localStorage.getItem(k) || '[]'), STORAGE_KEY);
  expect(stored).toEqual(['time', 'phase', 'status', 'provider', 'model', 'tokens', 'latency']);
  expect(stored).not.toContain('cost');

  // 6. Reload → Cost stays hidden, the menu's checkbox for "cost"
  // is unchecked, and the header has 7 cells now.
  await page.reload();
  await gotoLogs(page);
  const header2 = page.locator('#logs > .log-row').first();
  await expect(header2.locator('[data-col]')).toHaveCount(7);
  await expect(header2.locator('.log-cost')).toHaveCount(0);
  // Re-open menu and confirm cost is unchecked.
  await page.locator('#logs-columns-toggle').click();
  await expect(page.locator('.columns-menu')).toHaveClass(/open/);
  await expect(page.locator('.columns-menu input[data-arg1="cost"]')).not.toBeChecked();

  // 7. Check "cost" again → the column reappears.
  await page.locator('.columns-menu input[data-arg1="cost"]').click();
  await expect(page.locator('#logs > .log-row').first().locator('.log-cost')).toHaveCount(1);
  const storedAfter = await page.evaluate((k) => JSON.parse(localStorage.getItem(k) || '[]'), STORAGE_KEY);
  expect(storedAfter).toContain('cost');
  expect(storedAfter).toHaveLength(8);

  // 8. Guard: can't hide the last visible column. Hide all except
  // one by clicking each remaining checkbox. The last click is a
  // no-op (the size===1 guard in toggleColumn).
  // Click outside to close the menu.
  await page.locator('body').click({ position: { x: 5, y: 5 } });
  await expect(page.locator('.columns-menu')).not.toHaveClass(/open/);
  // Re-open and hide everything except "time".
  await page.locator('#logs-columns-toggle').click();
  for (const key of ['phase', 'status', 'provider', 'model', 'tokens', 'latency', 'cost']) {
    await page.locator(`.columns-menu input[data-arg1="${key}"]`).click();
  }
  // Now only "time" is visible. Try to hide it — must be a no-op.
  await page.locator('.columns-menu input[data-arg1="time"]').click();
  const storedMin = await page.evaluate((k) => JSON.parse(localStorage.getItem(k) || '[]'), STORAGE_KEY);
  expect(storedMin).toEqual(['time']);
  // And the time checkbox is still checked (clicking a checked box
  // when it's the only one visible is rejected by the size===1
  // guard, so the checkbox reverts to checked).
  await expect(page.locator('.columns-menu input[data-arg1="time"]')).toBeChecked();
});
