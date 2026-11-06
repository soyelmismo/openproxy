import { expect, test } from '@playwright/test';
import { readFileSync } from 'node:fs';

// Real test database/server. No API interception or simulated dashboard data.
const storage = JSON.parse(readFileSync('tests/e2e/storageState.json', 'utf8'));
const token = storage.origins[0].localStorage.find((v: { name: string }) => v.name === 'openproxy_admin_token').value;
const baseURL = process.env['OPENPROXY_TILING_TEST_URL'] || 'http://localhost:8790';
test.use({ baseURL, storageState: { cookies: [], origins: [{ origin: baseURL, localStorage: [{ name: 'openproxy_admin_token', value: token }, { name: 'openproxy-theme', value: 'dark' }] }] } });

test.beforeEach(async ({ page }) => {
  await page.goto('/admin/#/combos');
  await expect(page.locator('.workspace-tile .tile-view')).toHaveCount(1);
});

test('real combo clone is visible, copied and reflected in the contextual sidebar', async ({ page }) => {
  const name = `tiling-${Date.now()}`;
  const headers = { Authorization: `Bearer ${token}` };
  const created = await page.request.post('/admin/api/combos', { headers, data: { name, strategy: 'priority', race_size: 2 } });
  expect(created.ok()).toBe(true);
  const id = (await created.json()).id;
  let copyId: number | undefined;
  try {
    const frame = page.locator('.workspace-tile .tile-view').first();
    await page.goto(`/admin/#/combos/${id}`);
    await expect(frame.getByRole('button', { name: 'Clone', exact: true })).toBeVisible();
    const response = page.waitForResponse(r => r.url().endsWith(`/combos/${id}/clone`) && r.request().method() === 'POST');
    await frame.getByRole('button', { name: 'Clone', exact: true }).click();
    const result = await response;
    expect(result.ok()).toBe(true);
    copyId = (await result.json()).id;
    await expect(page).toHaveURL(new RegExp(`#/combos/${copyId}$`));
    const copied = await page.request.get(`/admin/api/combos/${copyId}`, { headers });
    expect(await copied.json()).toMatchObject({ name: `${name}-copy`, race_size: 2 });
    await expect(page.locator('.context-resources')).toContainText(`${name}-copy`);
  } finally {
    if (copyId) await page.request.delete(`/admin/api/combos/${copyId}`, { headers });
    await page.request.delete(`/admin/api/combos/${id}`, { headers });
  }
});

test('tiling keeps independent live views, routes active panel, resizes, maximizes, closes and persists', async ({ page }) => {
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.getByRole('button', { name: '2 panels', exact: true }).click();
  await expect(page.locator('.workspace-tile')).toHaveCount(2);
  const first = page.locator('.workspace-tile').nth(0);
  const second = page.locator('.workspace-tile').nth(1);
  await expect(first.locator('.tile-view')).toBeVisible();
  await expect(second.locator('.tile-view')).toBeVisible();
  await expect(page.locator('.tile-view').nth(0)).toContainText('Combos');
  await expect(page.locator('.tile-view').nth(1)).toContainText('Logs');
  const originalUrl = await first.locator('.tile-view').evaluate(el => (el as HTMLElement).dataset['route']);
  await second.locator('.tile-title').click();
  await page.getByRole('menuitem', { name: 'API Keys', exact: true }).click();
  await expect(page.locator('.tile-view').nth(1)).toContainText('API Keys');
  expect(await first.locator('.tile-view').evaluate(el => (el as HTMLElement).dataset['route'])).toBe(originalUrl);
  await expect(page).toHaveURL(/#\/keys$/);

  const separator = page.getByRole('separator', { name: 'Resize panel columns' });
  await separator.focus();
  await page.keyboard.press('ArrowRight');
  await expect(separator).toHaveAttribute('aria-valuenow', '56');
  await second.getByRole('button', { name: 'Maximize panel' }).click();
  await expect(first).toBeHidden();
  await second.getByRole('button', { name: 'Restore panel' }).click();
  await expect(first).toBeVisible();
  await page.getByRole('button', { name: '4 panels', exact: true }).click();
  await expect(page.locator('.workspace-tile')).toHaveCount(4);
  await page.reload();
  await expect(page.locator('.workspace-tile')).toHaveCount(4);
  await page.locator('.workspace-tile').last().getByRole('button', { name: 'Close panel' }).click();
  await expect(page.locator('.workspace-grid')).toHaveClass(/layout-triple/);
  await expect(page.locator('.workspace-tile')).toHaveCount(3);
  await expect(page.locator('.workspace-tile .tile-view')).toHaveCount(3);
  expect(errors).toEqual([]);
});

test('presets, split shortcuts, context toggle, theme sync and mobile panel switching', async ({ page }) => {
  await page.getByRole('button', { name: 'Layouts', exact: true }).click();
  await page.getByRole('menuitem', { name: 'Routing desk' }).click();
  await expect(page.locator('.workspace-tile')).toHaveCount(2);
  await expect(page.locator('.tile-view').nth(1)).toContainText('Playground');
  await page.locator('.sidebar a[data-nav="#/keys"]').click({ modifiers: ['Shift'] });
  await expect(page.locator('.workspace-tile')).toHaveCount(3);
  await expect(page).toHaveURL(/#\/keys$/);
  await page.getByRole('button', { name: 'Hide context sidebar' }).click();
  await expect(page.locator('.workspace-context')).toBeHidden();
  await page.getByRole('button', { name: 'Show context sidebar' }).click();
  await expect(page.locator('.workspace-context')).toBeVisible();
  await page.evaluate(() => { document.querySelector<HTMLButtonElement>('#theme-toggle-slot button')!.click(); });
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'light');
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(page.locator('.workspace-context')).toBeHidden();
  await expect(page.locator('.workspace-tile:visible')).toHaveCount(1);
  await page.getByRole('button', { name: '1. Combos', exact: true }).click();
  await expect(page.locator('.workspace-tile:visible')).toHaveCount(1);
  await expect(page.locator('.workspace-tile:visible .tile-title')).toContainText('Combos');
  await page.getByRole('button', { name: /open.*menu|open.*navigation/i }).click();
  await expect(page.locator('.sidebar-drawer')).toBeVisible();
  await page.locator('.sidebar-drawer a[data-nav="#/providers"]').click();
  await expect(page.locator('.sidebar-drawer')).toBeHidden();
  await expect(page.locator('.workspace-tile:visible .tile-title')).toContainText('Providers');
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
});

test('API keys appear in context and edit in an integrated properties view with real persistence', async ({ page }) => {
  const headers = { Authorization: `Bearer ${token}` };
  const label = `workspace-key-${Date.now()}`;
  const expiresAt = new Date(Date.now() + 5 * 86400000 + 123456).toISOString();
  const created = await page.request.post('/admin/api/keys', { headers, data: { label, scopes: ['chat'], expires_at: expiresAt } });
  expect(created.ok()).toBe(true);
  const id = (await created.json()).key.id;
  try {
    await page.goto('/admin/#/keys');
    const resources = page.locator('.context-resources');
    await resources.getByRole('button', { name: 'Refresh resources' }).click();
    const link = resources.locator(`a[href="#/keys/${id}"]`);
    await expect(link).toContainText(label);
    await resources.getByRole('textbox', { name: 'Search resources' }).fill(label);
    await expect(resources.locator('.context-resource-row')).toHaveCount(1);
    await link.click();
    await expect(page).toHaveURL(new RegExp(`#/keys/${id}$`));
    const frame = page.locator('.workspace-tile .tile-view').first();
    await expect(frame.locator('.key-detail h1')).toHaveText(label);
    await expect(frame.locator('.modal-bg')).toHaveCount(0);
    await expect(link).toHaveClass('active');
    await frame.getByLabel('Label', { exact: true }).fill(`${label}-edited`);
    const saved = page.waitForResponse(r => r.url().endsWith(`/keys/${id}`) && r.request().method() === 'PATCH');
    await frame.getByRole('button', { name: 'Save', exact: true }).click();
    expect((await saved).ok()).toBe(true);
    await expect(frame.locator('.key-detail h1')).toHaveText(`${label}-edited`);
    await expect(link).toContainText(`${label}-edited`);
    const persisted = await page.request.get(`/admin/api/keys/${id}`, { headers });
    const key = await persisted.json();
    expect(key.label).toBe(`${label}-edited`);
    expect(new Date(key.expires_at).getTime()).toBe(new Date(expiresAt).getTime());
    await page.reload();
    await expect(frame.getByLabel('Label', { exact: true })).toHaveValue(`${label}-edited`);
    await frame.getByRole('button', { name: 'Cancel', exact: true }).click();
    await expect(page).toHaveURL(/#\/keys$/);
    const row = frame.locator('[data-row-key]').filter({ hasText: `${label}-edited` });
    await row.getByRole('button', { name: 'Edit', exact: true }).click();
    await expect(page).toHaveURL(new RegExp(`#/keys/${id}$`));
    await expect(frame.locator('.key-detail')).toBeVisible();
    await expect(frame.locator('.modal-bg')).toHaveCount(0);
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(frame.getByLabel('Label', { exact: true })).toBeVisible();
    expect(await page.locator('html').evaluate(el => el.scrollWidth <= window.innerWidth)).toBe(true);
    await page.setViewportSize({ width: 1600, height: 1000 });
    await page.screenshot({ path: '/root/.hermes/cache/scratch/openproxy-tiling/key-properties.png' });
  } finally {
    await page.request.delete(`/admin/api/keys/${id}`, { headers });
  }
});

test('provider toolbar, SVG controls and tile first paint remain integrated', async ({ page }) => {
  await page.goto('/admin/#/providers');
  const frame = page.locator('.workspace-tile .tile-view').first();
  await expect(frame.locator('.providers-toolbar')).toBeVisible();
  await expect(frame.getByRole('heading', { name: 'Providers', exact: true })).toHaveCount(0);
  const toolbar = await frame.locator('.providers-toolbar').evaluate(el => {
    const filters = el.querySelector('.filter-tabs')!.getBoundingClientRect();
    const actions = el.querySelector('.actions')!.getBoundingClientRect();
    return { aligned: Math.abs(filters.y + filters.height / 2 - actions.y - actions.height / 2) < 2,
      activeBackground: getComputedStyle(el.querySelector('.filter-tab.active')!).backgroundColor };
  });
  expect(toolbar.aligned).toBe(true);
  expect(toolbar.activeBackground).toBe('rgba(0, 0, 0, 0)');
  await frame.getByRole('button', { name: /^Inactive/ }).click();
  await expect(frame.getByRole('button', { name: /^Inactive/ })).toHaveAttribute('aria-pressed', 'true');
  await frame.locator('.providers-toolbar').getByRole('button', { name: /^All/ }).click();
  await page.getByRole('button', { name: '2 panels', exact: true }).click();
  await expect(page.locator('iframe')).toHaveCount(0);
  const second = page.locator('.tile-view').nth(1);
  const svgPath = page.locator('.workspace-tile').first().getByRole('button', { name: 'Maximize panel' }).locator('svg path');
  expect(await svgPath.evaluate(el => ({ ns: el.namespaceURI, width: (el as SVGGraphicsElement).getBBox().width }))).toEqual({ ns: 'http://www.w3.org/2000/svg', width: 12 });
  await expect(second).toBeVisible();
  expect(await page.locator('html').getAttribute('data-theme')).toBe('dark');
  await page.screenshot({ path: '/root/.hermes/cache/scratch/openproxy-tiling/comments-desktop.png' });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByRole('button', { name: '1. Providers', exact: true }).click();
  await expect(frame.locator('.providers-toolbar')).toBeVisible();
  expect(await page.locator('html').evaluate(el => el.scrollWidth <= window.innerWidth)).toBe(true);
  expect(await frame.locator('.providers-toolbar').evaluate(el => el.querySelector('.filter-tabs')!.getBoundingClientRect().right <= el.querySelector('.actions')!.getBoundingClientRect().left)).toBe(true);
  await page.screenshot({ path: '/root/.hermes/cache/scratch/openproxy-tiling/comments-mobile.png' });
});

test('logout tears down all native panels', async ({ page }) => {
  await page.getByRole('button', { name: '2 panels', exact: true }).click();
  await page.getByRole('button', { name: 'Expand sidebar', exact: true }).click();
  await page.getByRole('button', { name: 'Logout', exact: true }).click();
  await expect(page).toHaveURL(/#\/login$/);
  await expect(page.locator('.workspace-tile:visible')).toHaveCount(0);
});

test('native routes survive rapid changes without stale mounts, duplicate owners or lost sibling state', async ({ page }) => {
  const errors: string[] = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.goto('/admin/#/providers');
  await page.getByRole('button', { name: '2 panels', exact: true }).click();
  const logs = page.locator('.tile-view[data-view="logs"]');
  await expect(logs.locator('.logs-search-input')).toBeVisible();
  await logs.locator('.logs-search-input').fill('preserved');
  await logs.getByRole('button', { name: 'Pause', exact: true }).click();
  await page.locator('.workspace-tile').first().locator('.tile-header').click();
  for (const route of ['#/keys', '#/combos', '#/providers', '#/keys', '#/providers']) {
    await page.locator(`.sidebar a[data-nav="${route}"]`).click();
  }
  await expect(page.locator('.tile-view[data-view="providers"] .providers-toolbar')).toBeVisible();
  await expect(logs.locator('.logs-search-input')).toHaveValue('preserved');
  await expect(logs.getByRole('button', { name: 'Resume', exact: true })).toBeVisible();
  // Shift on an already open family focuses that owner instead of duplicating IDs/state.
  await page.locator('.sidebar a[data-nav="#/providers"]').click({ modifiers: ['Shift'] });
  await expect(page.locator('.workspace-tile')).toHaveCount(2);
  expect(await page.locator('.tile-view').evaluateAll(els => els.map(el => (el as HTMLElement).dataset['view']))).toEqual(['providers', 'logs']);
  await page.locator('.workspace-tile').filter({ has: logs }).getByRole('button', { name: 'Close panel' }).click();
  await expect(page.locator('.tile-view[data-view="logs"]')).toHaveCount(0);
  await page.getByRole('button', { name: '2 panels', exact: true }).click();
  await expect(page.locator('.tile-view[data-view="logs"] .logs-search-input')).toBeVisible();
  expect(errors).toEqual([]);
});

test('native desktop panels expose individual elements and provider modal spans the canvas', async ({ page }) => {
  const errors: string[] = [];
  page.on('pageerror', e => errors.push(e.message));
  await page.setViewportSize({ width: 1600, height: 1000 });
  await page.goto('/admin/#/providers');
  await page.getByRole('button', { name: '2 panels', exact: true }).click();
  const providers = page.locator('.tile-view[data-view="providers"]');
  const logs = page.locator('.tile-view[data-view="logs"]');
  await expect(providers.locator('.providers-toolbar')).toBeVisible();
  await expect(logs.locator('.logs-search-input')).toBeVisible();
  await expect(page.locator('iframe')).toHaveCount(0);
  await expect(page.locator('#main')).toHaveCount(1);
  // The logs panel really mounted its own view rather than an embedded
  // frame. Above a 1400px tile the rows render as a column table, below it
  // as cards, and a two-panel layout at this viewport is well under that,
  // so pin the section chrome that both layouts share instead of the table
  // header, and leave the row rendering to the specs that seed rows.
  await expect(logs.locator('.logs-section-header').first()).toBeVisible();
  const search = logs.locator('.logs-search-input');
  await search.fill('keep-this-filter');
  // Unrelated providers updates must not replace or reset logs controls.
  await providers.getByRole('button', { name: /^Inactive/ }).click();
  await expect(search).toHaveValue('keep-this-filter');
  await providers.getByRole('button', { name: 'Add provider', exact: true }).click();
  const modal = page.locator('#create-provider-modal');
  await expect(modal).toBeVisible();
  expect(await modal.evaluate(el => ({ width: el.getBoundingClientRect().width, left: el.getBoundingClientRect().left, native: el.ownerDocument === document, outsideTile: !el.closest('.workspace-tile') }))).toEqual({ width: 1600, left: 0, native: true, outsideTile: true });
  const id = modal.getByLabel('ID', { exact: true });
  await expect(id).toBeFocused();
  const rects = await modal.evaluate(el => {
    const id = el.querySelector('#provider-id')!.getBoundingClientRect();
    const name = el.querySelector('#provider-name')!.getBoundingClientRect();
    return { aligned: id.y === name.y, separate: id.right < name.left };
  });
  expect(rects).toEqual({ aligned: true, separate: true });
  await modal.getByRole('button', { name: 'Create', exact: true }).focus();
  await page.keyboard.press('Tab');
  await expect(modal.getByRole('button', { name: 'Close', exact: true })).toBeFocused();
  await page.screenshot({ path: '/root/.hermes/cache/scratch/openproxy-tiling/native-modal.png' });
  await page.keyboard.press('Escape');
  await expect(modal).toHaveCount(0);
  await expect(providers.getByRole('button', { name: 'Add provider', exact: true })).toBeFocused();
  await expect(search).toHaveValue('keep-this-filter');
  await search.click();
  await page.keyboard.press('Control+A');
  await page.keyboard.press('Backspace');
  await logs.getByRole('button', { name: 'Pause', exact: true }).click();
  await expect(logs.getByRole('button', { name: 'Resume', exact: true })).toBeVisible();
  expect(errors).toEqual([]);
});
