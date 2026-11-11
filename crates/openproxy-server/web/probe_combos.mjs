import { chromium } from 'playwright';

const AUTH = process.env.AUTH_KEY;
const browser = await chromium.launch({
  executablePath: '/usr/local/bin/google-chrome',
  args: ['--no-sandbox', '--disable-dev-shm-usage'],
});
const ctx = await browser.newContext({ viewport: { width: 1500, height: 1000 } });
const page = await ctx.newPage();
const errs = [];
page.on('console', (m) => { if (m.type() === 'error') errs.push(m.text()); });
page.on('pageerror', (e) => errs.push('pageerror: ' + e.message));

// Seed the admin key the same way the app stores it.
await page.addInitScript((key) => {
  localStorage.setItem('openproxy_admin_key', key);
  sessionStorage.setItem('openproxy_admin_key', key);
}, AUTH);

await page.goto('http://100.103.80.69:5173/admin/#/analytics', { waitUntil: 'networkidle' });
await page.waitForTimeout(4000);
const text = await page.evaluate(() => document.body.innerText);
console.log('--- ANALYTICS TEXT (combo section) ---');
const i = text.indexOf('Combo health');
console.log(i >= 0 ? text.slice(i, i + 900) : 'COMBO HEALTH SECTION NOT FOUND\n' + text.slice(0, 800));
await page.screenshot({ path: '/tmp/analytics_combos.png', fullPage: false });
console.log('--- console errors ---', errs.slice(0, 8));
await browser.close();
