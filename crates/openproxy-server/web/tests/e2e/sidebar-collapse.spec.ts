// tests/e2e/sidebar-collapse.spec.ts
// Exercises the sidebar collapse-to-icons feature:
//   - labels visible by default
//   - click toggle → labels hidden, body class set, localStorage
//     value set, sidebar column narrowed
//   - reload → still collapsed
//   - click toggle → expanded again
//   - reload → still expanded
//   - data-nav still works in both states (active class applied
//     to the right link)
//
// @see tsconfig.test.json for type settings.

import { test, expect, type Page } from '@playwright/test';

const BASE = 'http://localhost:8790/';
const STORAGE_KEY = 'openproxy:sidebarCollapsed';

async function sidebarWidth(page: Page): Promise<number | null> {
  return page.evaluate(() => {
    const el = document.querySelector('#app');
    if (!el) return null;
    // grid-template-columns is "200px 1fr" (or "56px 1fr" when
    // collapsed). Parse the first explicit value.
    const tpl = getComputedStyle(el).gridTemplateColumns || '';
    const first = tpl.split(' ')[0] ?? '';
    const px = parseFloat(first);
    return Number.isFinite(px) ? px : null;
  });
}

test('sidebar defaults to the compact rail; toggle expands + localStorage persistence', async ({ page }: { page: Page }) => {
  await page.goto(BASE);
  // The storage is per-origin; clearing it from a prior test run
  // guarantees the compact-default branch runs.
  await page.evaluate((k: string) => localStorage.removeItem(k), STORAGE_KEY);
  await page.reload();

  await expect(page.locator('.sidebar nav a').first()).toBeVisible();
  // No stored preference => the compact rail is the product default
  // (7a138c0d8): labels hidden, narrow width, collapsed body class.
  const labelHidden = await page.locator('.sidebar nav a .nav-label').first().evaluate((el) => el.hasAttribute('hidden') || getComputedStyle(el).display === 'none');
  expect(labelHidden).toBe(true);
  await expect(page.locator('body')).toHaveClass(/sidebar-collapsed/);
  const collapsedWidth = await sidebarWidth(page);
  expect(collapsedWidth).toBeLessThan(80);

  await page.locator('.sidebar-toggle').click();

  const stored = await page.evaluate((k: string) => localStorage.getItem(k), STORAGE_KEY);
  expect(stored).toBe('0');

  const labelVisible = page.locator('.sidebar nav a .nav-label').first();
  await expect(labelVisible).toBeVisible();
  await expect(page.locator('body')).not.toHaveClass(/sidebar-collapsed/);
  const expandedWidth = await sidebarWidth(page);
  expect(expandedWidth).toBeGreaterThan(150);

  await page.reload();
  const storedAfterReload = await page.evaluate((k: string) => localStorage.getItem(k), STORAGE_KEY);
  expect(storedAfterReload).toBe('0');
  await expect(page.locator('body')).not.toHaveClass(/sidebar-collapsed/);
  const widthAfterReload = await sidebarWidth(page);
  expect(widthAfterReload).toBeGreaterThan(150);

  await page.locator('.sidebar-toggle').click();
  const storedCollapsed = await page.evaluate((k: string) => localStorage.getItem(k), STORAGE_KEY);
  expect(storedCollapsed).toBe('1');
  const labelBackHidden = await page.locator('.sidebar nav a .nav-label').first().evaluate((el) => el.hasAttribute('hidden') || getComputedStyle(el).display === 'none');
  expect(labelBackHidden).toBe(true);
  const widthCollapsed = await sidebarWidth(page);
  expect(widthCollapsed).toBeLessThan(80);

  await page.reload();
  await expect(page.locator('body')).toHaveClass(/sidebar-collapsed/);
  const storedCollapsedReload = await page.evaluate((k: string) => localStorage.getItem(k), STORAGE_KEY);
  expect(storedCollapsedReload).toBe('1');
  const widthCollapsedReload = await sidebarWidth(page);
  expect(widthCollapsedReload).toBeLessThan(80);
});

test('nav link click + active class works in both collapsed and expanded modes', async ({ page }: { page: Page }) => {
  await page.goto(BASE);
  await page.evaluate((k: string) => localStorage.removeItem(k), STORAGE_KEY);
  await page.reload();

  await page.click('a[href="#/providers"]');
  await expect(page).toHaveURL(/#\/providers$/);
  await expect(page.locator('a[href="#/providers"]')).toHaveClass(/active/);
  const activeExpanded = await page.locator('.sidebar nav a.active').first().getAttribute('data-nav');
  expect(activeExpanded).toBe('#/providers');

  await page.locator('.sidebar-toggle').click();
  const activeCollapsed = await page.locator('.sidebar nav a.active').first().getAttribute('data-nav');
  expect(activeCollapsed).toBe('#/providers');

  await page.click('a[href="#/combos"]');
  await expect(page).toHaveURL(/#\/combos$/);
  await expect(page.locator('a[href="#/combos"]')).toHaveClass(/active/);
  const activeCombosCollapsed = await page.locator('.sidebar nav a.active').first().getAttribute('data-nav');
  expect(activeCombosCollapsed).toBe('#/combos');
});
