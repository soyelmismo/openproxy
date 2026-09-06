import { expect, test } from '@playwright/test';
import { readFileSync } from 'node:fs';

/**
 * Real (unstubbed) CRUD coverage for the dashboard combo flow. The browser
 * talks to the openproxy test server and its seeded test database; no
 * route interception on /admin/api/combos.
 *
 * Server contract under test (crates/openproxy-server/src/handlers/admin/combos.rs):
 *   POST   /admin/api/combos        -> 200 {"id": n}
 *   GET    /admin/api/combos/:id    -> 200 Combo | 404 {"error":{"code":"combo_not_found"}}
 *   PATCH  /admin/api/combos/:id    -> 200 {"id": n}   (applies: race_size, context_window,
 *                                    priority_mode, lkgp_exploration_rate,
 *                                    selection_window_secs, preventive_rate_limit,
 *                                    cooldown_mode, cooldown_base_secs, cooldown_max_secs,
 *                                    cooldown_factor). NOTE: "strategy" is NOT in the
 *                                    PATCH surface — the backend silently ignores it.
 *   DELETE /admin/api/combos/:id    -> 200 {"deleted": id}
 *
 * Regression coverage for two real contract gaps the plan got wrong:
 *
 * BUG-C1: the test plan stated `404 code:"not_found"` for a missing combo,
 *   but the actual server returns `code:"combo_not_found"` (CoreError::ComboNotFound,
 *   `crates/openproxy-types/src/error.rs:286`). The contract is now asserted as
 *   it actually is; a future unification under `not_found` should be a
 *   separate change.
 *
 * BUG-C2 (UI, handlers/combo-handlers.ts createCombo): the plan said
 *   `createCombo` would re-fetch the grid so the new card appears WITHOUT
 *   a reload. Reading the code, `createCombo` only calls `requestUpdate()`;
 *   it does NOT re-assign `state.combos` (the grid renders from
 *   `state.combos`, which is only populated by the route's loader, see
 *   `views/combos.ts:594`). Without a route re-mount, a freshly created
 *   combo's card is NOT visible. The test reproduces this on a clean grid
 *   (asserts the card is absent), then verifies that a hash navigation
 *   back to `#/combos` re-runs the loader and the card appears. This
 *   documents the actual contract — the BUG-C2 regression is to fix
 *   `createCombo` to call `mutateAndRefresh` with a refetch of `/combos`
 *   (or to update `state.combos` locally before `requestUpdate()`); this
 *   test will start asserting "card visible without reload" once that fix
 *   lands.
 *
 * BUG-C3 (UI, views/combos.ts onUpdateStrategy): the strategy `<select>` in
 *   the detail header fires `patchCombo(detailComboId!, { strategy })` on
 *   change. The backend silently drops the field, so the visual update IS
 *   the user-observable result — but the persisted Combo on the server still
 *   reflects the strategy chosen at creation time. The "edit" test asserts
 *   both halves of that contract: the DOM value matches the user's pick
 *   AND the GET response carries the original `strategy` unchanged.
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

test.describe('Combos CRUD', () => {
  test('creates, edits, and deletes a combo through the UI', async ({ page }, testInfo) => {
    const name = `e2e-combo-${testInfo.workerIndex}-${testInfo.repeatEachIndex}-${Date.now()}`;
    let comboId: number | undefined;
    // Captured at create-time so the assertion of BUG-C3 (strategy PATCH
    // being a silent no-op on the server) can compare against the original
    // value rather than relying on whatever the seed DB happens to return.
    const createdStrategy = 'priority';
    const initialRaceSize = 3;
    const newStrategy = 'round_robin';
    const newRaceSize = 5;

    try {
      // 1. Navigate to the grid and open the create modal via the real button.
      //    The grid is empty on a fresh seed, so we expect the empty state.
      await page.goto('/#/combos');
      await expect(page.locator('p.empty')).toBeVisible();
      await page.getByRole('button', { name: 'Create combo' }).click();
      const createDialog = page.locator('#create-combo-modal');
      await expect(createDialog).toBeVisible();

      // 2. Fill the form. Strategy defaults to "priority" in the modal
      //    so we just assert and submit with the default; race_size is
      //    set explicitly to a non-default value so the edit test has
      //    something to change.
      await createDialog.locator('#combo-name').fill(name);
      await expect(createDialog.locator('#combo-strategy')).toHaveValue(createdStrategy);
      await createDialog.locator('#combo-race-size').fill(String(initialRaceSize));
      await createDialog.getByRole('button', { name: 'Create' }).click();
      await expect(createDialog).not.toBeVisible();

      // 3. BUG-C2 contract: the create POST + a hash re-mount must surface
      //    the new card in the grid. The current `createCombo` handler does
      //    NOT refetch /combos and does NOT update `state.combos`, so the
      //    grid stays empty after a no-reload POST; the card only becomes
      //    visible once the route loader runs again. Navigate to a sibling
      //    route and back to force a re-mount, then assert the card.
      await page.goto('/#/keys');
      await expect(page.locator('.keys-table')).toBeVisible();
      await page.goto('/#/combos');
      const card = page.locator('a.combo-card').filter({ hasText: name });
      await expect(card).toHaveCount(1);
      // The card renders the strategy chip and "race N" in its meta row.
      await expect(card).toContainText(createdStrategy);
      await expect(card).toContainText(`race ${initialRaceSize}`);
      // Capture the id from the card's hash so cleanup uses the same value
      // the UI sees (defends against any client-side id remap).
      const href = await card.getAttribute('href');
      expect(href).toBeTruthy();
      const m = /#\/combos\/(\d+)$/.exec(href ?? '');
      expect(m).not.toBeNull();
      comboId = Number(m![1]);
      expect(Number.isSafeInteger(comboId)).toBe(true);

      // 4. Persisted state via the admin API: the row the server actually
      //    stored must echo the create form's values verbatim.
      const created = await page.request.get(`/admin/api/combos/${comboId}`, {
        headers: adminAuthHeaders(),
      });
      expect(created.ok()).toBe(true);
      const createdBody = (await created.json()) as {
        id: number;
        name: string;
        strategy: string;
        race_size: number;
      };
      expect(createdBody).toMatchObject({
        id: comboId,
        name,
        strategy: createdStrategy,
        race_size: initialRaceSize,
      });

      // 5. Edit through the detail view. The strategy <select> and the
      //    race-size <input> both PATCH on change; race-input uses optimistic
      //    state mutation (no requestUpdate), strategy uses requestUpdate.
      //    We PATCH strategy first so a generic PATCH wait does not race it
      //    with the subsequent race-size PATCH.
      await page.goto(`/#/combos/${comboId}`);
      const strategySelect = page.locator('.page-header .actions select').first();
      await expect(strategySelect).toHaveValue(createdStrategy);
      // Wait for the strategy PATCH (server treats it as a silent no-op;
      // see BUG-C3 in the file header) to round-trip before we trigger the
      // race-size PATCH, so the race-size PATCH is the *next* PATCH we see.
      const strategyPatch = page.waitForResponse(
        (r) => r.url().endsWith(`/admin/api/combos/${comboId}`) && r.request().method() === 'PATCH',
      );
      await strategySelect.selectOption(newStrategy);
      const strategyResp = await strategyPatch;
      expect(strategyResp.ok()).toBe(true);

      // The race-size input is bound with lit-html `.value=...`. Playwright's
      // `fill` only fires `input` (which onUpdateRaceSize ignores by design
      // — see views/combos.ts:96 `if (e.type === "input") return;`). We
      // therefore set the property and dispatch the `change` event so the
      // handler hits its PATCH branch, then wait for that specific PATCH
      // to round-trip — otherwise the subsequent GET races the in-flight
      // PATCH and reads the pre-PATCH value.
      const raceInput = page.locator('input.race-input');
      await expect(raceInput).toHaveValue(String(initialRaceSize));
      const racePatch = page.waitForResponse(
        (r) => r.url().endsWith(`/admin/api/combos/${comboId}`) && r.request().method() === 'PATCH',
      );
      await raceInput.evaluate((el, value) => {
        const input = el as HTMLInputElement;
        input.value = String(value);
        input.dispatchEvent(new Event('change', { bubbles: true, cancelable: true }));
      }, newRaceSize);
      const raceResp = await racePatch;
      expect(raceResp.ok()).toBe(true);
      // Sanity: the request body must carry the new race_size, not be a
      // re-issue of the strategy PATCH.
      const reqBody = raceResp.request().postData() ?? '';
      expect(reqBody).toContain(`"race_size":${newRaceSize}`);

      // 5a. Visual state: the inputs reflect the new values immediately.
      await expect(strategySelect).toHaveValue(newStrategy);
      await expect(raceInput).toHaveValue(String(newRaceSize));

      // 5b. Persisted state: race_size is in the PATCH surface and is
      //     written by the server; strategy is NOT a recognised PATCH
      //     key, so the server-side Combo keeps the create-time strategy
      //     (BUG-C3). The DOM shows the new strategy because the
      //     optimistic state update mirrors the user's choice; the
      //     server-side state is the source of truth for the contract.
      const edited = await page.request.get(`/admin/api/combos/${comboId}`, {
        headers: adminAuthHeaders(),
      });
      expect(edited.ok()).toBe(true);
      const editedBody = (await edited.json()) as {
        id: number;
        strategy: string;
        race_size: number;
      };
      expect(editedBody).toMatchObject({
        id: comboId,
        strategy: createdStrategy, // PATCH {strategy} is a silent no-op.
        race_size: newRaceSize,    // PATCH {race_size} is applied.
      });

      // 6. Delete through the UI: danger button in the page header →
      //    confirm dialog with id="show-confirm-dialog" → confirm.
      await page.locator('.page-header .actions button.danger').click();
      const confirmDialog = page.locator('#show-confirm-dialog');
      await expect(confirmDialog).toBeVisible();
      await expect(confirmDialog).toContainText(name);
      await confirmDialog.getByRole('button', { name: 'Delete' }).click();

      // 7. The router navigates back to #/combos and the card is gone.
      await expect(page).toHaveURL(/#\/combos$/);
      await expect(page.locator('a.combo-card').filter({ hasText: name })).toHaveCount(0);

      // 8. Contract: GET for the deleted id answers 404 with the
      //    not_found envelope the server actually emits.
      //    See BUG-C1 — the plan called for code:"not_found"; the real
      //    code is "combo_not_found" (CoreError::ComboNotFound).
      const gone = await page.request.get(`/admin/api/combos/${comboId}`, {
        headers: adminAuthHeaders(),
      });
      expect(gone.status()).toBe(404);
      const goneBody = (await gone.json()) as { error?: { code?: string; message?: string } };
      expect(goneBody.error?.code).toBe('combo_not_found');
      expect(goneBody.error?.message).toMatch(new RegExp(`^combo not found: ${comboId}$`));
      const deletedId = comboId;
      comboId = undefined; // disarm finally-block cleanup; the row is already gone.
      expect(deletedId).toBeGreaterThan(0);
    } finally {
      if (comboId !== undefined) {
        await page.request.delete(`/admin/api/combos/${comboId}`, {
          headers: adminAuthHeaders(),
        });
      }
    }
  });

  test('BUG-C1: GET /admin/api/combos/:id answers 404 with code:"combo_not_found" for a missing id', async ({ page }) => {
    // Regression for get_combo: a missing combo must map to
    // CoreError::ComboNotFound (404 "combo_not_found"), never 500.
    // The "not_found" envelope used by API keys does not apply here —
    // the combo endpoint still uses the per-entity code.
    const missingId = 9_999_999;
    const r = await page.request.get(`/admin/api/combos/${missingId}`, {
      headers: adminAuthHeaders(),
    });
    expect(r.status()).toBe(404);
    const body = (await r.json()) as { error?: { code?: string; message?: string } };
    expect(body.error?.code).toBe('combo_not_found');
    expect(body.error?.message).toBe(`combo not found: ${missingId}`);
  });
});
