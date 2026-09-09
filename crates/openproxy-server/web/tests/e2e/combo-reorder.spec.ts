import { expect, test } from '@playwright/test';
import { readFileSync } from 'node:fs';

/**
 * Real (unstubbed) reorder coverage for the combo detail view's target
 * table. The browser talks to the openproxy test server and its seeded
 * test database; no route interception, no admin API mocks.
 *
 * Server contract under test (crates/openproxy-server/src/handlers/admin/):
 *   POST   /admin/api/providers              -> 200 {"id": "..."}
 *   POST   /admin/api/models/custom          -> 200 {"row_id": n}
 *   POST   /admin/api/accounts               -> 200 {"id": n}
 *   POST   /admin/api/combos                 -> 200 {"id": n}
 *   POST   /admin/api/combos/:id/targets     -> 200 {"id": n}
 *   POST   /admin/api/combos/:id/targets/reorder -> 200 {"reordered": n, "count": n}
 *   GET    /admin/api/combos/:id/targets     -> 200 ComboTargetWithModel[]
 *   DELETE /admin/api/combos/:id             -> 200 {"deleted": n}
 *   DELETE /admin/api/accounts/:id           -> 200
 *   DELETE /admin/api/providers/:id          -> 200 (FK-cascades models+accounts)
 *
 * BUG-C4 (UI, views/combos.ts mountCombos detail view): clicking
 * "Move Up"/"Move Down" (onChangePriority) or dragging a row
 * (executeTargetReorder) POSTs the reorder, re-fetches
 * GET /combos/:id/targets into the module-local `detailTargets`, and
 * calls `requestUpdate()` — but the re-render was defeated by the
 * detail view's mount closure, which re-assigned `detailTargets` from
 * its stale loader `data` on EVERY render. The fresh order was thrown
 * away before renderComboDetail() could read it, so the table kept
 * showing the pre-reorder order until a full reload. Same root cause
 * as BUG-C2 (grid's state.combos, fixed in commit abc76e565); the
 * fix syncs loader data in `onLoaded` (once per load) instead of
 * inside `render`. This test asserts the reorder is visible in the
 * DOM without any reload, and that the persisted priority_order
 * matches the new visual order (double regression: UI + DB).
 */

// `page.request` shares cookies only, not localStorage; the dashboard
// authenticates with a Bearer token stored in localStorage (see
// state/auth.ts). Read it from the same storageState.json the browser
// context is seeded with — single source of truth for test credentials.
const storageStatePath = 'tests/e2e/storageState.json';
function adminAuthHeaders(): Record<string, string> {
  const storageState = JSON.parse(readFileSync(storageStatePath, 'utf8')) as {
    origins: Array<{ origin: string; localStorage: Array<{ name: string; value: string }> }>;
  };
  const origin = storageState.origins.find((o) => o.origin === 'http://localhost:8790');
  const token = origin?.localStorage.find((e) => e.name === 'openproxy_admin_token')?.value;
  if (!token) throw new Error(`openproxy_admin_token not found in ${storageStatePath}`);
  return { Authorization: `Bearer ${token}` };
}

test.describe('Combo target reorder', () => {
  test('BUG-C4: Move Down reorders rows in the DOM without reload and persists priority_order', async ({ page }, testInfo) => {
    test.setTimeout(60_000);
    const suffix = `${testInfo.workerIndex}-${testInfo.repeatEachIndex}-${Date.now()}`;
    const providerId = `e2e-reorder-p-${suffix}`;
    const comboName = `e2e-reorder-c-${suffix}`;
    // Unreachable upstream: nothing in this test dispatches a chat call,
    // but account creation triggers a background model refresh, and a
    // real-looking URL keeps that noise deterministic and harmless.
    const baseUrl = 'http://127.0.0.1:9/v1';
    const headers = adminAuthHeaders();
    const labels = ['A', 'B', 'C'] as const;

    let comboId: number | undefined;
    let accountId: number | undefined;
    let providerCreated = false;

    try {
      // ---- Setup: seed provider -> models -> account -> combo -> 3 targets ----
      const providerResp = await page.request.post('/admin/api/providers', {
        headers,
        data: { id: providerId, name: `E2E Reorder ${suffix}`, base_url: baseUrl, auth_type: 'bearer', format: 'openai' },
      });
      expect(providerResp.ok(), await providerResp.text()).toBe(true);
      providerCreated = true;

      const rowIds: number[] = [];
      for (const label of labels) {
        const r = await page.request.post('/admin/api/models/custom', {
          headers,
          data: {
            provider_id: providerId,
            // No display_name: the row's model cell renders model_id,
            // which doubles as a human-readable per-target marker.
            model_id: `e2e-reorder-model-${label.toLowerCase()}-${suffix}`,
            target_format: 'openai',
            ttl_seconds: 0, // never expires
          },
        });
        expect(r.ok(), await r.text()).toBe(true);
        rowIds.push(((await r.json()) as { row_id: number }).row_id);
      }
      expect(new Set(rowIds).size).toBe(3);

      const accountResp = await page.request.post('/admin/api/accounts', {
        headers,
        data: { provider_id: providerId, api_key: `e2e-reorder-key-${suffix}`, label: `e2e-reorder-${suffix}` },
      });
      expect(accountResp.ok(), await accountResp.text()).toBe(true);
      accountId = ((await accountResp.json()) as { id: number }).id;

      const comboResp = await page.request.post('/admin/api/combos', {
        headers,
        data: { name: comboName, strategy: 'priority', race_size: 1 },
      });
      expect(comboResp.ok(), await comboResp.text()).toBe(true);
      comboId = ((await comboResp.json()) as { id: number }).id;

      // 3 targets in order A,B,C with priority_order 0,1,2.
      const targetIds: number[] = [];
      for (let i = 0; i < rowIds.length; i++) {
        const r = await page.request.post(`/admin/api/combos/${comboId}/targets`, {
          headers,
          data: { provider_id: providerId, account_id: accountId, model_row_id: rowIds[i], priority_order: i },
        });
        expect(r.ok(), await r.text()).toBe(true);
        targetIds.push(((await r.json()) as { id: number }).id);
      }
      const [idA, idB, idC] = targetIds;

      // ---- Action: open the detail view, click "Move Down" on target A ----
      await page.goto(`/#/combos/${comboId}`);
      const rows = page.locator('table.combo-targets-table tbody tr.combo-target-card-row');
      await expect(rows).toHaveCount(3);

      // [target id, priority cell text] per row, in DOM order.
      const readRows = () =>
        rows.evaluateAll((trs) =>
          trs.map((tr) => [
            Number(tr.getAttribute('data-drag-id')),
            tr.querySelector('td.col-target-order')?.textContent?.trim() ?? '',
          ]),
        );

      // Initial DOM order: A,B,C (priority cells 0,1,2 as seeded).
      expect(await readRows()).toEqual([
        [idA, '0'],
        [idB, '1'],
        [idC, '2'],
      ]);

      // Reload marker: anything that navigates or reloads the page
      // wipes this window property, so the post-reorder assertions
      // below double as a "no reload happened" proof.
      await page.evaluate(() => {
        (window as unknown as Record<string, unknown>)['__e2eNoReload'] = true;
      });

      const rowA = page.locator(`tr.combo-target-card-row[data-drag-id="${idA}"]`);
      const reorderPost = page.waitForResponse(
        (r) => r.url().endsWith(`/admin/api/combos/${comboId}/targets/reorder`) && r.request().method() === 'POST',
      );
      await rowA.locator('button[title="Move Down"]').click();
      const reorderResp = await reorderPost;
      expect(reorderResp.ok()).toBe(true);

      // ---- Assertion (UI, no reload): rows visibly re-order to B,A,C ----
      await expect(rows).toHaveCount(3);
      // After reorder the server reassigns priority_order 1,2,3
      // (1-indexed — see combos.rs apply_target_priority_chunks), and
      // the view's post-POST re-fetch must be what the table shows.
      await expect.poll(readRows, { timeout: 10_000 }).toEqual([
        [idB, '1'],
        [idA, '2'],
        [idC, '3'],
      ]);
      expect(await page.evaluate(() => (window as unknown as Record<string, unknown>)['__e2eNoReload'])).toBe(true);

      // ---- Assertion (DB): GET targets returns priority_order consistent
      //      with the new visual order (B=1, A=2, C=3). ----
      const targetsResp = await page.request.get(`/admin/api/combos/${comboId}/targets`, { headers });
      expect(targetsResp.ok()).toBe(true);
      const targets = (await targetsResp.json()) as Array<{ id: number; priority_order: number }>;
      const byPriority = [...targets].sort((a, b) => a.priority_order - b.priority_order);
      expect(byPriority.map((t) => t.id)).toEqual([idB, idA, idC]);
      expect(byPriority.map((t) => t.priority_order)).toEqual([1, 2, 3]);
    } finally {
      // Cleanup only what this test created (unique timestamped names).
      // DELETE provider FK-cascades its models and accounts; the combo
      // delete cascades its targets. All idempotent.
      if (comboId !== undefined) {
        await page.request.delete(`/admin/api/combos/${comboId}`, { headers });
      }
      if (accountId !== undefined) {
        await page.request.delete(`/admin/api/accounts/${accountId}`, { headers });
      }
      if (providerCreated) {
        await page.request.delete(`/admin/api/providers/${providerId}`, { headers });
      }
    }
  });
});
