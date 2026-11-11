/// <reference types="node" />
import { test, expect } from '@playwright/test';

// Read-only preview sweep: every dashboard view, every workspace width, plus the
// phone layout. It never writes: any non-GET admin call is aborted.
const preview = process.env['OPENPROXY_LAYOUT_TEST_URL'];
const token = process.env['OPENPROXY_LAYOUT_TEST_TOKEN'];
test.skip(!preview || !token, 'Requires an authenticated frontend preview');
test.use({ storageState: { cookies: [], origins: [] } });

const ROUTES: ReadonlyArray<readonly [string, string]> = [
  ['home', '#/'], ['providers', '#/providers'], ['provider-detail', '#/providers/agentrouter'],
  ['combos', '#/combos'], ['combo-detail', '#/combos/2'], ['keys', '#/keys'],
  ['key-detail', '#/keys/1000038'], ['key-usage', '#/keys/1000038/usage'],
  ['analytics', '#/analytics'], ['logs', '#/logs'], ['debug-logs', '#/debug-logs'],
  ['config', '#/config'], ['proxies', '#/proxies'], ['proxy-sources', '#/proxy-sources'],
  ['playground', '#/playground'], ['notifications', '#/notifications'],
];
const WIDTHS = [250, 320, 420, 560, 760, 1000, 1440];

const MEASURE = () => {
  const view = document.querySelector('.workspace-tile.mobile-active .tile-view')
    || document.querySelector('.workspace-tile .tile-view');
  const content = view?.closest('.tile-content');
  if (!view || !content) return null;
  const vb = view.getBoundingClientRect();
  const visible = (el: Element) => {
    if (el.closest('details:not([open])')) return false;
    const cs = getComputedStyle(el);
    if (cs.display === 'none' || cs.visibility === 'hidden' || Number(cs.opacity) === 0) return false;
    if (cs.contentVisibility === 'hidden') return false;
    const b = el.getBoundingClientRect();
    return b.width > 1 && b.height > 1;
  };
  const scrollable = (el: Element) => {
    for (let p = el.parentElement; p && p !== view; p = p.parentElement) {
      const cs = getComputedStyle(p);
      if (/(auto|scroll)/.test(cs.overflowX) || /(auto|scroll)/.test(cs.overflowY)) return true;
    }
    return false;
  };
  const srOnly = (el: Element) =>
    getComputedStyle(el).clipPath === 'inset(50%)' || el.getBoundingClientRect().width <= 2;
  const result = {
    hOverflow: Math.max(view.scrollWidth - view.clientWidth, content.scrollWidth - content.clientWidth),
    outside: [] as string[],
    occluded: [] as string[],
    tiny: [] as string[],
    cut: [] as string[],
  };
  const controls = [...view.querySelectorAll('input,select,textarea,button,a[href]')].filter(visible);
  for (const el of controls) {
    const b = el.getBoundingClientRect();
    if ((b.right > vb.right + 1.5 || b.left < vb.left - 1.5) && !scrollable(el)) {
      result.outside.push(`${el.tagName}.${String(el.className).slice(0, 22)}`);
    }
  }
  for (let i = 0; i < controls.length; i++) {
    for (let j = i + 1; j < controls.length; j++) {
      const a = controls[i]!, b = controls[j]!;
      if (a.contains(b) || b.contains(a)) continue;
      const ra = a.getBoundingClientRect(), rb = b.getBoundingClientRect();
      const ox = Math.min(ra.right, rb.right) - Math.max(ra.left, rb.left);
      const oy = Math.min(ra.bottom, rb.bottom) - Math.max(ra.top, rb.top);
      if (ox <= 2 || oy <= 2) continue;
      const small = ra.width * ra.height <= rb.width * rb.height ? a : b;
      const rs = small.getBoundingClientRect();
      const cx = rs.left + rs.width / 2, cy = rs.top + rs.height / 2;
      if (cx < 0 || cy < 0 || cx > innerWidth || cy > innerHeight) continue;
      const hit = document.elementFromPoint(cx, cy);
      if (!hit || hit === small || small.contains(hit) || hit.contains(small)) continue;
      result.occluded.push(`${small.tagName}.${String(small.className).slice(0, 18)}`);
    }
  }
  for (const el of view.querySelectorAll('*')) {
    if (!visible(el)) continue;
    const cs = getComputedStyle(el);
    const fs = parseFloat(cs.fontSize);
    if (fs > 0 && fs < 10 && (el.textContent || '').trim()) {
      result.tiny.push(`${el.tagName}.${String(el.className).slice(0, 16)}:${fs.toFixed(1)}`);
    }
    const hasText = [...el.childNodes].some(n => n.nodeType === 3 && n.textContent!.trim());
    if (hasText && !srOnly(el) && el.scrollWidth > el.clientWidth + 2
      && (cs.textOverflow === 'ellipsis' || cs.overflow === 'hidden')) {
      result.cut.push(`${el.tagName}.${String(el.className).slice(0, 18)}`);
    }
  }
  for (const k of ['outside', 'occluded', 'tiny', 'cut'] as const) {
    result[k] = [...new Set(result[k])].slice(0, 8);
  }
  return result;
};

async function boot(page: import('@playwright/test').Page, route: string) {
  await page.addInitScript(({ token, route }) => {
    localStorage.setItem('openproxy_admin_token', token!);
    localStorage.setItem('openproxy:workspace:v1', JSON.stringify({
      version: 1, layout: 'single', tiles: [{ id: 'tile-1', route }],
      focused: 'tile-1', maximized: null, column: 40, row: 50, contextCollapsed: true,
    }));
  }, { token, route });
  await page.route('**/admin/api/**', r => {
    const req = r.request();
    if (!['GET', 'HEAD', 'OPTIONS'].includes(req.method()) && !req.url().includes('ws-ticket')) {
      throw new Error(`Read-only sweep attempted ${req.method()} ${new URL(req.url()).pathname}`);
    }
    return r.continue();
  });
  await page.route('**/esbuild', r => r.abort());
  await page.goto(`${preview}/admin/${route}`, { waitUntil: 'domcontentloaded' });
  await expect(page.locator('.workspace-tile .tile-view').first()).toBeVisible({ timeout: 20000 });
}

test.describe('dashboard fits every pane width', () => {
  test.use({ viewport: { width: 1500, height: 1100 } });
  for (const [name, route] of ROUTES) {
    test(`${name} holds at 250-1440px`, async ({ page }) => {
      const errors: string[] = [];
      page.on('pageerror', e => errors.push(e.message));
      await boot(page, route);
      await page.waitForTimeout(1500);
      for (const width of WIDTHS) {
        // Free 6x6 canvas: fix the grid width instead of the template.
        // A single tile spans all six tracks, so tile width = grid width - 12 padding.
        await page.locator('.workspace-grid').evaluate((g, w) => {
          (g as HTMLElement).style.width = `${Number(w) + 12}px`;
        }, width);
        await expect.poll(async () => (await page.evaluate(MEASURE))?.hOverflow ?? 0).toBeLessThanOrEqual(1);
        const m = await page.evaluate(MEASURE);
        expect(m, `${name}@${width}`).not.toBeNull();
        expect(m!.outside, `${name}@${width} controls outside the pane`).toEqual([]);
        expect(m!.occluded, `${name}@${width} occluded controls`).toEqual([]);
        expect(m!.tiny, `${name}@${width} type below 10px`).toEqual([]);
        expect(m!.cut, `${name}@${width} truncated text`).toEqual([]);
      }
      expect(errors).toEqual([]);
    });
  }
});

test.describe('dashboard fits a phone', () => {
  test.use({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true, deviceScaleFactor: 2 });
  for (const [name, route] of ROUTES) {
    test(`${name} holds at 390x844`, async ({ page }) => {
      await boot(page, route);
      await page.waitForTimeout(1500);
      expect(await page.evaluate(() => document.documentElement.scrollWidth - innerWidth)).toBeLessThanOrEqual(1);
      const m = await page.evaluate(MEASURE);
      expect(m, name).not.toBeNull();
      expect(m!.outside, `${name} controls outside the pane`).toEqual([]);
      expect(m!.occluded, `${name} occluded controls`).toEqual([]);
      expect(m!.tiny, `${name} type below 10px`).toEqual([]);
      expect(m!.cut, `${name} truncated text`).toEqual([]);
    });
  }
});
