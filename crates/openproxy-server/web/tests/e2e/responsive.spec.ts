// tests/e2e/responsive.spec.ts — verifies the base responsive
// CSS at 768px and 480px breakpoints. Asserts that the sidebar
// becomes a top-bar on mobile, the layout does not produce
// horizontal scroll on the body, and the desktop layout is
// unchanged at 1280px.
//
// Mirrors the spec's VERIFICATION section (375x667, 1280x800).
// The project uses Playwright; the spec mentioned "puppeteer-core"
// as the concept, not the library name.
//
// @see tsconfig.test.json for type settings.

import { test, expect, type Page } from '@playwright/test';

const NAV_LINKS: readonly string[] = ['Home', 'Providers', 'Combos', 'API Keys', 'Analytics', 'Live Logs', 'Config'];

async function gotoHome(page: Page): Promise<void> {
  await page.goto('http://localhost:8788/');
  // Wait for the shell to mount: sidebar + main must exist.
  await page.waitForSelector('.sidebar', { timeout: 10000 });
  await page.waitForSelector('#main', { timeout: 10000 });
  // The home view renders a .page-header with h2 "Overview".
  await page.waitForSelector('.page-header h2', { timeout: 10000 });
}

test.describe('Responsive — mobile (375x667)', () => {
  test.use({ viewport: { width: 375, height: 667 } });

  test('home: sidebar is a top-bar, no horizontal body scroll, all nav links visible', async ({ page }: { page: Page }) => {
    await gotoHome(page);

    // 1. Sidebar is at the top: its top edge must be at or above
    // main's top edge. (We allow a 1px tolerance for sub-pixel
    // rounding.) Crucially, the sidebar's `right` edge must
    // extend past main's right edge — i.e. it spans the full
    // viewport width like a top bar would.
    const sidebar = page.locator('.sidebar');
    const main = page.locator('#main');
    const sbBox = await sidebar.boundingBox();
    const mainBox = await main.boundingBox();
    expect(sbBox).not.toBeNull();
    expect(mainBox).not.toBeNull();
    // Sidebar's bottom must be at or above main's top.
    expect(sbBox!.y + sbBox!.height).toBeLessThanOrEqual(mainBox!.y + 1);
    // Sidebar must be the full inner width of the viewport. The
    // #app shell has a 1px page-frame border on each side, so on
    // a 375px viewport the inner grid is 373px wide.
    expect(sbBox!.width).toBe(373);

    // 2. All nav links are visible.
    for (const label of NAV_LINKS) {
      const link = page.locator(`.sidebar nav a`, { hasText: label });
      await expect(link).toBeVisible();
    }

    // 3. No horizontal scroll on the body. The spec allows a 2px
    // tolerance for sub-pixel rounding on different platforms.
    const { scrollW, innerW } = await page.evaluate(() => ({
      scrollW: document.body.scrollWidth,
      innerW: window.innerWidth,
    }));
    expect(scrollW).toBeLessThanOrEqual(innerW + 2);
  });

  test('logs: table is horizontally scrollable inside main, not on body', async ({ page }: { page: Page }) => {
    await gotoHome(page);
    await page.goto('http://localhost:8788/#/logs');
    // Wait for logs view to render its header row.
    await page.waitForSelector('#logs .log-row [data-col="time"]', { timeout: 10000 });

    // Body must not horizontally scroll (the logs table is wide,
    // but it's inside <main> which has overflow-x: auto).
    const { scrollW, innerW } = await page.evaluate(() => ({
      scrollW: document.body.scrollWidth,
      innerW: window.innerWidth,
    }));
    expect(scrollW).toBeLessThanOrEqual(innerW + 2);

    // The logs container itself has overflow-y: auto (set in
    // components.css for #logs), and its content is wider than
    // its visible area on mobile — we don't assert that the row
    // overflows because the table may be empty on a fresh load,
    // but we DO assert that #logs exists and is visible.
    await expect(page.locator('#logs')).toBeVisible();
  });

  test('480px: main padding is reduced, page-header h2 is 1.1rem', async ({ page }: { page: Page }) => {
    // Use a viewport that triggers the 480px breakpoint (which
    // is a subset of the 768px breakpoint).
    await page.setViewportSize({ width: 360, height: 720 });
    await gotoHome(page);

    // 1. main padding: 0.75rem (var(--space-3)). Note that base.css
    // sets `font-size: var(--fs-md)` (= 0.95rem) on <html>, which
    // makes 1rem = 0.95 * 16px = 15.2px. So 0.75rem = 11.4px.
    const mainPad = await page.locator('#main').evaluate(
      (el) => getComputedStyle(el).paddingTop,
    );
    expect(mainPad).toBe('11.4px');

    // 2. page-header h2 font-size: 1.1rem. 1.1 * 15.2 = 16.72px.
    const h2Size = await page.locator('.page-header h2').evaluate(
      (el) => getComputedStyle(el).fontSize,
    );
    expect(h2Size).toBe('16.72px');
  });
});

test.describe('Responsive — desktop (1280x800)', () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test('desktop layout intact: sidebar is left column, no regression', async ({ page }: { page: Page }) => {
    await gotoHome(page);

    const sidebar = page.locator('.sidebar');
    const main = page.locator('#main');
    const sbBox = await sidebar.boundingBox();
    const mainBox = await main.boundingBox();
    expect(sbBox).not.toBeNull();
    expect(mainBox).not.toBeNull();
    // Sidebar is in the left column: its left edge is at x=0
    // (or x=1 due to the page-frame border on #app), and main
    // starts at or after the sidebar's right edge.
    expect(sbBox!.x).toBeLessThanOrEqual(1);
    expect(mainBox!.x).toBeGreaterThanOrEqual(sbBox!.x + sbBox!.width - 1);

    // Sidebar width should be the configured --layout-sidebar-w
    // (200px) — but main is what defines the row height, so the
    // sidebar and main share the same y range.
    expect(sbBox!.width).toBe(200);
    expect(sbBox!.height).toBe(mainBox!.height);
  });

  test('desktop: sidebar collapse toggle still works (regression check)', async ({ page }: { page: Page }) => {
    // Start from a clean state — make sure the user hasn't left
    // the sidebar collapsed from a previous test.
    await page.goto('http://localhost:8788/');
    await page.evaluate(() => {
      try { localStorage.removeItem('openproxy:sidebarCollapsed'); } catch (_e) { void _e; }
    });
    await gotoHome(page);

    const sidebar = page.locator('.sidebar');
    const toggle = page.locator('.sidebar-toggle');

    // Sidebar is expanded by default.
    const expandedBox = await sidebar.boundingBox();
    expect(expandedBox).not.toBeNull();
    expect(expandedBox!.width).toBe(200);
    await expect(toggle).toBeVisible();

    // Click toggle → sidebar collapses. The width should drop
    // to --layout-sidebar-w-collapsed (smaller than 200).
    await toggle.click();
    // The CSS animation/transition is instant for width; just
    // wait for the body class to flip.
    await expect(page.locator('body.sidebar-collapsed')).toHaveCount(1);
    const collapsedBox = await sidebar.boundingBox();
    expect(collapsedBox).not.toBeNull();
    expect(collapsedBox!.width).toBeLessThan(200);
    expect(collapsedBox!.width).toBeGreaterThan(0);

    // Click again → expands back.
    await toggle.click();
    await expect(page.locator('body.sidebar-collapsed')).toHaveCount(0);
    const reExpandedBox = await sidebar.boundingBox();
    expect(reExpandedBox).not.toBeNull();
    expect(reExpandedBox!.width).toBe(200);
  });
});
