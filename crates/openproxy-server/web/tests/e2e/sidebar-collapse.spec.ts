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
// Run with: npx playwright test sidebar-collapse.spec.ts
// (or via `npm test` from crates/openproxy-server/web).
//
// @see tsconfig.test.json for type settings.

import { test, expect, type Page } from '@playwright/test';

const BASE = 'http://127.0.0.1:8788/';
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

test('sidebar collapse to icons + localStorage persistence', async ({ page }: { page: Page }) => {
  // 1. Start clean.
  await page.goto(BASE);
  // The storage is per-origin; clearing it from a prior test run
  // guarantees the expanded-state branch runs.
  await page.evaluate((k: string) => localStorage.removeItem(k), STORAGE_KEY);
  await page.reload();

  // 2. Expanded state: labels visible, sidebar ~200px.
  await expect(page.locator('.sidebar nav a').first()).toBeVisible();
  const labelVisible = await page.locator('.sidebar nav a .nav-label').first().isVisible();
  expect(labelVisible).toBe(true);
  const expandedWidth = await sidebarWidth(page);
  expect(expandedWidth).toBeGreaterThan(150);

  // 3. Click the toggle.
  await page.locator('.sidebar-toggle').click();

  // 4. localStorage now "1".
  const stored = await page.evaluate((k: string) => localStorage.getItem(k), STORAGE_KEY);
  expect(stored).toBe('1');

  // 5. Labels hidden (computed display: none OR hidden attr).
  const labelAfter = page.locator('.sidebar nav a .nav-label').first();
  const labelIsHiddenAttr = await labelAfter.evaluate((el) => el.hasAttribute('hidden'));
  const labelDisplay = await labelAfter.evaluate((el) => getComputedStyle(el).display);
  expect(labelIsHiddenAttr || labelDisplay === 'none').toBe(true);

  // 6. Body has the collapsed class.
  const bodyHasClass = await page.evaluate(() => document.body.classList.contains('sidebar-collapsed'));
  expect(bodyHasClass).toBe(true);

  // 7. Sidebar column narrowed to ~56px.
  const collapsedWidth = await sidebarWidth(page);
  expect(collapsedWidth).toBeLessThan(80);

  // 8. Reload — choice persists.
  await page.reload();
  const storedAfterReload = await page.evaluate((k: string) => localStorage.getItem(k), STORAGE_KEY);
  expect(storedAfterReload).toBe('1');
  const bodyHasClassAfterReload = await page.evaluate(() => document.body.classList.contains('sidebar-collapsed'));
  expect(bodyHasClassAfterReload).toBe(true);
  const widthAfterReload = await sidebarWidth(page);
  expect(widthAfterReload).toBeLessThan(80);

  // 9. Toggle back to expanded.
  await page.locator('.sidebar-toggle').click();
  const storedExpanded = await page.evaluate((k: string) => localStorage.getItem(k), STORAGE_KEY);
  expect(storedExpanded).toBe('0');
  const labelBackVisible = await page.locator('.sidebar nav a .nav-label').first().isVisible();
  expect(labelBackVisible).toBe(true);
  const widthExpanded = await sidebarWidth(page);
  expect(widthExpanded).toBeGreaterThan(150);

  // 10. Reload — expanded persists.
  await page.reload();
  const storedExpandedReload = await page.evaluate((k: string) => localStorage.getItem(k), STORAGE_KEY);
  expect(storedExpandedReload).toBe('0');
  const widthExpandedReload = await sidebarWidth(page);
  expect(widthExpandedReload).toBeGreaterThan(150);
});

test('nav link click + active class works in both collapsed and expanded modes', async ({ page }: { page: Page }) => {
  await page.goto(BASE);
  await page.evaluate((k: string) => localStorage.removeItem(k), STORAGE_KEY);
  await page.reload();

  // Expanded: click Providers → active class on Providers, route updates.
  await page.click('a[href="#/providers"]');
  await expect(page).toHaveURL(/#\/providers$/);
  await expect(page.locator('a[href="#/providers"]')).toHaveClass(/active/);
  const activeExpanded = await page.locator('.sidebar nav a.active').first().getAttribute('data-nav');
  expect(activeExpanded).toBe('#/providers');

  // Collapse.
  await page.locator('.sidebar-toggle').click();
  // Still active while collapsed.
  const activeCollapsed = await page.locator('.sidebar nav a.active').first().getAttribute('data-nav');
  expect(activeCollapsed).toBe('#/providers');

  // Navigate to Combos via collapsed sidebar.
  await page.click('a[href="#/combos"]');
  await expect(page).toHaveURL(/#\/combos$/);
  await expect(page.locator('a[href="#/combos"]')).toHaveClass(/active/);
  const activeCombosCollapsed = await page.locator('.sidebar nav a.active').first().getAttribute('data-nav');
  expect(activeCombosCollapsed).toBe('#/combos');
});
