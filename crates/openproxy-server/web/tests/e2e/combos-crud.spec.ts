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
 *   PATCH  /admin/api/combos/:id    -> 200 {"id": n}   (applies: strategy, race_size,
 *                                    context_window, priority_mode,
 *                                    lkgp_exploration_rate, selection_window_secs,
 *                                    preventive_rate_limit, cooldown_mode,
 *                                    cooldown_base_secs, cooldown_max_secs,
 *                                    cooldown_factor). "strategy" is validated with the
 *                                    same set as CreateComboInput (priority |
 *                                    round_robin | shuffle); an unknown value answers
 *                                    400 with CoreError::Validation.
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
 * BUG-C2 (UI, handlers/combo-handlers.ts createCombo + views/combos.ts mountCombos):
 *   two halves: (a) `createCombo` only called `requestUpdate()` after the
 *   POST — `state.combos` was never refreshed, and (b) even when the state
 *   was refreshed externally, the grid view's `render` callback reassigned
 *   `state.combos` from its stale closure `data` on EVERY re-render,
 *   clobbering the refresh. The fix re-fetches GET /combos into
 *   `state.combos` (via `mutateAndRefresh.onSuccess`) and syncs the loader
 *   result in `onLoaded` (once per load) instead of inside `render`, so the
 *   card appears immediately after submit without any reload; the test
 *   asserts exactly that.
 *
 * BUG-C3 (backend, handlers/admin/combos.rs apply_combo_general_updates): the
 *   strategy `<select>` in the detail header fires
 *   `patchCombo(detailComboId!, { strategy })` on change, but the PATCH
 *   surface did not recognise the key and silently dropped it — the
 *   persisted Combo kept the create-time strategy and the UI lied after
 *   a reload. The fix routes `strategy` through the same
 *   `Strategy::parse` validation the create path uses (invalid values
 *   answer 400 CoreError::Validation) and persists it; the "edit" test
 *   asserts the GET response carries the new strategy after the PATCH.
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
    // Strategy sent at create time (the modal's default) — the edit step
    // PATCHes away from it, so the create-time GET can assert against it.
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

      // 3. BUG-C2 regression: the create POST must surface the new card
      //    in the grid WITHOUT any reload — `createCombo` re-fetches
      //    GET /combos into `state.combos` before the re-render, so the
      //    card appears in place, immediately after the modal closes.
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
      // Wait for the strategy PATCH to round-trip before we trigger the
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

      // 5b. Persisted state: both keys are in the PATCH surface now —
      //     `strategy` is validated with the create-time set and written
      //     by the server (BUG-C3 fix); `race_size` was already applied.
      //     The server-side Combo is the source of truth for the contract.
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
        strategy: newStrategy,    // PATCH {strategy} is persisted.
        race_size: newRaceSize,   // PATCH {race_size} is applied.
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
