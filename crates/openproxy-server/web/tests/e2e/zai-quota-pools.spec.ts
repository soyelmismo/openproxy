/// <reference types="node" />
import { test, expect } from '@playwright/test';
import { createServer, type Server } from 'node:http';
import { readFile } from 'node:fs/promises';
import { build } from 'esbuild';
import type { Account, QuotaPool } from '../../src/static/src/lib/types/api.js';

// Real renderer + real CSS in Chromium, without accounts, tokens or a live API.
let server: Server;
let origin: string;

test.beforeAll(async () => {
  const result = await build({
    stdin: {
      contents: `import { render } from 'lit-html';
        import { renderQuotaCell } from './src/static/src/views/quota-cell.ts';
        window.renderAccount = (account) => render(renderQuotaCell(account), document.querySelector('#main'));`,
      resolveDir: process.cwd(),
      loader: 'ts',
    },
    bundle: true,
    format: 'esm',
    write: false,
  });
  const script = result.outputFiles[0]?.text ?? '';
  const css = await readFile('src/static/dist/app.css', 'utf8');
  server = createServer((req, res) => {
    if (req.url === '/render.js') {
      res.setHeader('Content-Type', 'text/javascript');
      res.end(script);
    } else if (req.url === '/style.css') {
      res.setHeader('Content-Type', 'text/css');
      res.end(css);
    } else {
      res.setHeader('Content-Type', 'text/html');
      res.end('<!doctype html><html data-theme="dark"><meta name="viewport" content="width=device-width"><link rel="stylesheet" href="/style.css"><body><main id="main" style="max-width:100%;padding:16px"></main><script type="module" src="/render.js"></script></body></html>');
    }
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const addr = server.address();
  if (!addr || typeof addr === 'string') throw new Error('missing test listener');
  origin = `http://127.0.0.1:${addr.port}`;
});

test.afterAll(async () => {
  if (server) await new Promise<void>((resolve, reject) => server.close((err) => err ? reject(err) : resolve()));
});

function pool(overrides: Partial<QuotaPool> = {}): QuotaPool {
  return {
    id: 'starter', source: 'zcode_starter', plan_name: 'Weekend Build',
    status: 'active', unit: 'tokens', used: 25000, limit: 300000000,
    remaining: 299975000, reset_at: null, expires_at: String(Math.floor(Date.now() / 1000) + 86400),
    starts_at: null, model_ids: ['GLM-5.3-Flash'], model_details: null,
    fetch_error: null, last_fetched_at: String(Math.floor(Date.now() / 1000)), ...overrides,
  };
}

async function renderAccount(page: import('@playwright/test').Page, pools: QuotaPool[]) {
  await page.goto(origin);
  await page.waitForFunction(() => typeof (window as unknown as { renderAccount?: unknown }).renderAccount === 'function');
  await page.evaluate((quotaPools) => {
    const render = (window as unknown as { renderAccount: (account: Partial<Account>) => void }).renderAccount;
    render({ provider_id: 'zai', quota_pools: quotaPools, quota_session_used: 0, quota_session_limit: null });
  }, pools);
}

for (const width of [1280, 375]) {
  test(`independent Starter and paid bars in dark theme at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 900 });
    await renderAccount(page, [pool({ id: 'paid', source: 'coding_plan', plan_name: 'Pro', unit: 'percentage', used: 80, limit: 100, remaining: 20, model_ids: [], expires_at: null }), pool()]);
    const pools = page.locator('.quota-pool');
    await expect(pools).toHaveCount(2);
    await expect(pools.nth(0)).toContainText('Starter · Free · Weekend Build');
    await expect(pools.nth(0)).toContainText('299,975,000 / 300,000,000 tokens left');
    await expect(pools.nth(0)).toContainText('GLM-5.3-Flash');
    await expect(pools.nth(1)).toContainText('Coding Plan · Pro');
    await expect(pools.nth(1)).toContainText('20% left');
    await expect(page.getByRole('progressbar')).toHaveCount(2);
    await expect(page.locator('#main')).not.toContainText('Session Window');
    const sizes = await page.evaluate(() => ({ scroll: document.body.scrollWidth, width: innerWidth }));
    expect(sizes.scroll).toBeLessThanOrEqual(sizes.width + 2);
  });
}

test('missing paid subscription does not manufacture a zero bar', async ({ page }) => {
  await renderAccount(page, [pool(), pool({ id: 'paid', source: 'coding_plan', status: 'absent', used: null, remaining: null, limit: null })]);
  await expect(page.locator('.quota-pool').nth(1)).toContainText('No active Coding Plan');
  await expect(page.getByRole('progressbar')).toHaveCount(1);
});

test('unreadable Starter stays independent and untrusted text stays escaped', async ({ page }) => {
  await renderAccount(page, [pool({ status: 'unavailable', used: null, remaining: null, limit: null, fetch_error: '<script>window.bad = true</script>' }), pool({ id: 'paid', source: 'coding_plan', unit: 'percentage', used: 20, remaining: 80, limit: 100 })]);
  await expect(page.locator('.quota-pool').nth(0)).toContainText('Unavailable');
  await expect(page.locator('.quota-pool').nth(0)).toContainText('usage data unavailable for this source');
  await expect(page.locator('.quota-pool').nth(1)).toContainText('80% left');
  await expect(page.getByRole('progressbar')).toHaveCount(1);
  expect(await page.evaluate(() => 'bad' in window)).toBe(false);
});
