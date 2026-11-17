// tests/e2e/responsive.spec.ts — base responsive CSS at the 768px and
// 480px breakpoints: the sidebar becomes a top-bar on mobile, the body
// never scrolls horizontally, and the 1280px desktop layout is unchanged.
//
// The tiling workspace replaced the old #main content host: views live in
// .tile-view elements inside .workspace-grid tiles, so the shell-level
// assertions target the sidebar and the desk instead of #main.
//
import { test, expect, type Page } from '@playwright/test';

const DUMMY_ADMIN_TOKEN = 'op_live_test_dummy_token_for_e2e';
const ADMIN_TOKEN_STORAGE_KEY = 'openproxy_admin_token';
const NAV_LINKS: readonly string[] = ['Home', 'Providers', 'Combos', 'API Keys', 'Analytics', 'Live Logs', 'Config'];

test.beforeEach(async ({ page }: { page: Page }) => {
  await page.addInitScript((args: { key: string; token: string }) => {
    try {
      localStorage.setItem(args.key, args.token);
    } catch (_e: unknown) {
      // Ignore
    }
  }, { key: ADMIN_TOKEN_STORAGE_KEY, token: DUMMY_ADMIN_TOKEN });
});

async function gotoHome(page: Page): Promise<void> {
  page.on('console', msg => console.log('PAGE LOG:', msg.text()));
  page.on('pageerror', err => console.error('PAGE ERROR:', err.message));
  page.on('response', response => {
    if (response.status() === 404) {
      console.log(`PAGE RESOURCE 404: ${response.url()}`);
    }
  });
  await page.goto('http://localhost:8790/');
  // Wait for the shell to mount: sidebar + workspace desk must exist.
  await page.waitForSelector('.sidebar', { timeout: 10000 });
  await page.waitForSelector('.workspace-grid', { timeout: 10000 });
}

test.describe('Responsive — mobile (375x667)', () => {
  test.use({ viewport: { width: 375, height: 667 } });

  test('home: sidebar is a top-bar, no horizontal body scroll, all nav links visible', async ({ page }: { page: Page }) => {
    await gotoHome(page);

    // The sidebar renders as the mobile top-bar: its bottom edge must
    // sit at or above the workspace grid's top edge, and it spans the
    // full inner width of the viewport (1px page-frame border per side
    // on a 375px viewport => 373px).
    const sidebar = page.locator('.sidebar');
    const grid = page.locator('.workspace-grid');
    const sbBox = await sidebar.boundingBox();
    const gridBox = await grid.boundingBox();
    expect(sbBox).not.toBeNull();
    expect(gridBox).not.toBeNull();
    expect(sbBox!.y + sbBox!.height).toBeLessThanOrEqual(gridBox!.y + 1);
    expect(sbBox!.width).toBe(373);

    // Desktop nav links live in the drawer on mobile; the burger opens it.
    await page.getByRole('button', { name: /open.*menu|open.*navigation/i }).click();
    await expect(page.locator('.sidebar-drawer')).toBeVisible();
    for (const label of NAV_LINKS) {
      const link = page.locator('.sidebar-drawer nav a', { hasText: label });
      await expect(link).toBeVisible();
    }

    // tolerance for sub-pixel rounding on different platforms.
    const { scrollW, innerW } = await page.evaluate(() => ({
      scrollW: document.body.scrollWidth,
      innerW: window.innerWidth,
    }));
    expect(scrollW).toBeLessThanOrEqual(innerW + 2);
  });

  test('logs: table is horizontally scrollable inside its tile, not on body', async ({ page }: { page: Page }) => {
    await gotoHome(page);
    // Navigate through the drawer: a bare hash change does not route the
    // tiling workspace.
    await page.getByRole('button', { name: /open.*menu|open.*navigation/i }).click();
    await page.locator('.sidebar-drawer a[data-nav="#/logs"]').click();
    const logsView = page.locator('.tile-view[data-route="#/logs"]');
    await expect(logsView.locator('.logs-control-deck')).toBeVisible({ timeout: 10000 });

    // Body must not horizontally scroll (the logs table is wide, but it
    // scrolls inside its tile container).
    const { scrollW, innerW } = await page.evaluate(() => ({
      scrollW: document.body.scrollWidth,
      innerW: window.innerWidth,
    }));
    expect(scrollW).toBeLessThanOrEqual(innerW + 2);

    await expect(logsView).toBeVisible();
  });

  test('480px: tile padding is reduced to the compact mobile rhythm', async ({ page }: { page: Page }) => {
    // Use a viewport that triggers the 480px breakpoint (which
    // is a subset of the 768px breakpoint).
    await page.setViewportSize({ width: 360, height: 720 });
    await gotoHome(page);

    // The mobile block of workspace.css pins the tile padding to 12px.
    const tilePad = await page.locator('.tile-view').first().evaluate(
      (el) => getComputedStyle(el).paddingTop,
    );
    expect(tilePad).toBe('12px');
  });
});

test.describe('Responsive — desktop (1280x800)', () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test('desktop layout intact: sidebar is left column, no regression', async ({ page }: { page: Page }) => {
    // Expand the rail: the compact rail is the stored-preference default,
    // this test pins the expanded desktop column.
    await page.addInitScript(() => {
      try { localStorage.setItem('openproxy:sidebarCollapsed', '0'); } catch (_e) { void _e; }
    });
    await gotoHome(page);

    const sidebar = page.locator('.sidebar');
    const grid = page.locator('.workspace-grid');
    const sbBox = await sidebar.boundingBox();
    const gridBox = await grid.boundingBox();
    expect(sbBox).not.toBeNull();
    expect(gridBox).not.toBeNull();
    // Sidebar is in the left column: its left edge is at x=0
    // (or x=1 due to the page-frame border on #app), and the desk
    // starts at or after the sidebar's right edge.
    expect(sbBox!.x).toBeLessThanOrEqual(1);
    expect(gridBox!.x).toBeGreaterThanOrEqual(sbBox!.x + sbBox!.width - 1);

    // Expanded rail width matches --layout-sidebar-w (200px) and the
    // sidebar spans the desk vertically.
    expect(sbBox!.width).toBeGreaterThan(150);
    expect(sbBox!.height).toBeGreaterThanOrEqual(gridBox!.height);
  });

  test('desktop: sidebar collapse toggle still works (regression check)', async ({ page }: { page: Page }) => {
    // Start from a clean state: no stored preference => the compact rail
    // is the product default.
    await page.goto('http://localhost:8790/');
    await page.evaluate(() => {
      try { localStorage.removeItem('openproxy:sidebarCollapsed'); } catch (_e) { void _e; }
    });
    await page.reload();
    await page.waitForSelector('.workspace-grid', { timeout: 10000 });

    const sidebar = page.locator('.sidebar');
    const toggle = page.locator('.sidebar-toggle');

    // Sidebar is collapsed by default (compact rail).
    await expect(page.locator('body.sidebar-collapsed')).toHaveCount(1);
    const collapsedBox = await sidebar.boundingBox();
    expect(collapsedBox).not.toBeNull();
    expect(collapsedBox!.width).toBeLessThan(80);
    await expect(toggle).toBeVisible();

    // Click toggle => sidebar expands. The width should reach the
    // expanded --layout-sidebar-w (larger than the compact rail).
    await toggle.click();
    await expect(page.locator('body.sidebar-collapsed')).toHaveCount(0);
    const expandedBox = await sidebar.boundingBox();
    expect(expandedBox).not.toBeNull();
    expect(expandedBox!.width).toBeGreaterThan(150);

    // Click again => collapses back.
    await toggle.click();
    await expect(page.locator('body.sidebar-collapsed')).toHaveCount(1);
    const reCollapsedBox = await sidebar.boundingBox();
    expect(reCollapsedBox).not.toBeNull();
    expect(reCollapsedBox!.width).toBeLessThan(80);
  });
});
