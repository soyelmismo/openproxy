// Browser regression against the real checkout and its actual response CSP.
// CHECKOUT_LOCAL_ASSETS=1 tests working-tree HTML/CSS before the release build.
// Only GETs are allowed: this probe must never create an order or move money.
import { chromium } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { resolve, dirname } from 'node:path';
import assert from 'node:assert/strict';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const origin = process.env.CHECKOUT_ORIGIN || 'http://127.0.0.1:8787';
const local = process.env.CHECKOUT_LOCAL_ASSETS === '1';
const browser = await chromium.connectOverCDP(process.env.CHECKOUT_CDP || 'http://127.0.0.1:9222');
const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, serviceWorkers: 'block' });
const violations = [];
const errors = [];
try {
  const page = await context.newPage();
  await page.addInitScript(() => {
    window.__csp = [];
    document.addEventListener('securitypolicyviolation', e => window.__csp.push({ directive: e.effectiveDirective, blocked: e.blockedURI }));
  });
  page.on('pageerror', e => errors.push(e.message));
  await context.route('**/*', async route => {
    const req = route.request();
    if (req.method() !== 'GET') throw new Error(`Non-GET blocked: ${req.method()} ${req.url()}`);
    const url = new URL(req.url());
    if (url.origin !== new URL(origin).origin) return route.abort();
    if (local && url.pathname === '/checkout') {
      const response = await route.fetch();
      const html = readFileSync(resolve(root, 'src/static/checkout.html'), 'utf8')
        .replace('<!--TERMS_BODY-->', readFileSync(resolve(root, 'src/static/terms-body.html'), 'utf8'));
      return route.fulfill({ response, body: html });
    }
    if (local && url.pathname === '/admin/styles/checkout.css') {
      return route.fulfill({ contentType: 'text/css', body: readFileSync(resolve(root, 'src/static/styles/checkout.css'), 'utf8') });
    }
    return route.continue();
  });
  const response = await page.goto(`${origin}/checkout`, { waitUntil: 'networkidle' });
  assert.equal(response.status(), 200);
  assert.match(response.headers()['content-security-policy'], /style-src-elem 'self';/);
  assert.ok(await page.locator('#usd .plan-card').count(), 'real plan catalog rendered');
  for (const size of [{width:1280,height:900},{width:390,height:844}]) {
    await page.setViewportSize(size);
    for (const id of ['terms-modal','faq-modal','step-pay','step-key','step-mykey','step-done']) {
      assert.equal(await page.locator(`#${id}`).isVisible(), false, `${id} initially hidden at ${size.width}`);
    }
    await page.locator('.checkout-foot [data-modal="terms"]').click();
    assert.equal(await page.locator('#terms-modal').isVisible(), true);
    assert.equal(await page.locator('#faq-modal').isVisible(), false);
    await page.keyboard.press('Escape');
    assert.equal(await page.locator('#terms-modal').isVisible(), false);
    await page.locator('.checkout-foot [data-modal="faq"]').click();
    assert.equal(await page.locator('#faq-modal').isVisible(), true);
    await page.locator('#faq-modal summary').first().click();
    assert.equal(await page.locator('#faq-modal details').first().getAttribute('open'), '');
    await page.locator('#faq-modal summary').first().click();
    await page.locator('#faq-modal .close-btn').click();
    assert.equal(await page.locator('#faq-modal').isVisible(), false);
    await page.locator('#tab-mykey').click();
    assert.equal(await page.locator('#step-mykey').isVisible(), true);
    assert.equal(await page.locator('#step-buy').isVisible(), false);
    await page.locator('#tab-buy').click();
    assert.equal(await page.locator('#step-buy').isVisible(), true);
    const label = page.locator('#usd .plan-card:not(.sold-out) .plan-card-select').first();
    await label.click();
    const selected = await page.locator('#usd input:checked').inputValue();
    // Token pills use their own listener and must not select a different plan.
    const chips = page.locator('#usd button.chip');
    if (await chips.count()) {
      await chips.first().click();
      assert.equal(await page.locator('.plan-est-pop').isVisible(), true);
      assert.equal(await page.locator('#usd input:checked').inputValue(), selected);
      await page.keyboard.press('Escape');
      assert.equal(await page.locator('.plan-est-pop').count(), 0);
    }
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, `no overflow at ${size.width}`);
    if (process.env.CHECKOUT_SCREENSHOT) await page.screenshot({ path: `${process.env.CHECKOUT_SCREENSHOT}-${size.width}.png`, fullPage: true });
    console.log(`PASS ${size.width}: hidden states, modal open/close, tabs, selection, token tooltip, no overflow`);
  }
  violations.push(...await page.evaluate(() => window.__csp));
  assert.deepEqual(errors, [], 'no uncaught JavaScript errors');
  assert.deepEqual(violations, [], 'no CSP violations on origin');
  console.log(`PASS CSP and JS errors: 0. Mode=${local ? 'working-tree assets + live CSP/API' : 'deployed assets, no override'}`);
} finally {
  await context.close();
  await browser.close();
}
