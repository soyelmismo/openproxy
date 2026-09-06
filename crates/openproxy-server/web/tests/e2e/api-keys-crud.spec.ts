import { expect, test } from '@playwright/test';
import { readFileSync } from 'node:fs';

/**
 * Real (unstubbed) CRUD coverage for the dashboard API-key flow. The browser
 * talks to the openproxy test server and its seeded test database; no route
 * interception on /admin/api/keys.
 *
 * Server contract under test (crates/openproxy-server/src/handlers/admin/api_keys.rs):
 *   POST   /admin/api/keys          -> 200 {key, plaintext}
 *   GET    /admin/api/keys/:id      -> 200 ApiKey | 404 {"error":{"code":"not_found"}} when missing
 *   PATCH  /admin/api/keys/:id      -> 200 {id}
 *   DELETE /admin/api/keys/:id      -> 200 {id, deleted:true}
 *
 * Regression coverage for two previously-known bugs (both fixed):
 *
 * BUG-1 (UI, key-handlers.ts createKey): after creating a key the handler
 *   must refresh state.apiKeys and call requestUpdate() so the new row
 *   appears without a reload; the test asserts exactly that (no reload).
 *
 * BUG-2 (server, get_api_key): GET /admin/api/keys/:id for a missing id
 *   must answer 404 {"error":{"code":"not_found"}} (CoreError::NotFound),
 *   never 500. The second test in this file is the dedicated regression.
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
test.describe('API keys CRUD', () => {
  test('creates, edits, and deletes an API key through the UI', async ({ page }, testInfo) => {
    const label = `e2e-key-${testInfo.workerIndex}-${testInfo.repeatEachIndex}-${Date.now()}`;
    const editedLabel = `${label}-edited`;
    let keyId: number | undefined;

    try {
      // 1. Navigate and wait for the view to mount.
      await page.goto('/#/keys');
      await expect(page.locator('.keys-table')).toBeVisible();

      // 2. Open the create modal via the real header button.
      await page.getByRole('button', { name: 'Create key' }).click();
      const createDialog = page.locator('.modal-bg').filter({ has: page.locator('form') });
      await expect(createDialog).toBeVisible();

      // 3. Fill label; the form defaults to the valid minimum scope (chat).
      await createDialog.getByLabel('Label').fill(label);
      await expect(createDialog.locator('input[name="scopes"][value="chat"]')).toBeChecked();

      // 4. Submit.
      await createDialog.getByRole('button', { name: 'Create key' }).click();

      // 5. Plaintext modal: shown exactly once, with a Copy button, and the
      //    prefix — never the full secret — is what the table persists.
      const plaintextCode = page.locator('#plaintext-key');
      await expect(plaintextCode).toBeVisible();
      await expect(page.getByRole('button', { name: 'Copy' })).toBeVisible();
      await expect(page.getByText("This is the only time you'll see this key.")).toBeVisible();

      // Capture every secret-derived value before we close the modal; the
      // modal removes itself and the plaintext disappears with it.
      const plaintext = (await plaintextCode.textContent()) ?? '';
      expect(plaintext).toBeTruthy();
      const plaintextDialog = page.locator('.modal-bg').filter({ has: plaintextCode });
      await expect(plaintextDialog).toContainText(label);
      const prefix = (await plaintextDialog.locator('code').last().textContent()) ?? '';
      expect(prefix).toBeTruthy();
      expect(prefix).not.toBe(plaintext);

      // 6. Close the modal. createKey refreshes state.apiKeys and re-renders,
      //    so the new row appears WITHOUT a reload (BUG-1 regression); the
      //    table persists the prefix — never the plaintext secret.
      await page.getByRole('button', { name: "I've saved it" }).click();
      const row = page.locator('tr[data-row-key]').filter({ hasText: label });
      await expect(row).toHaveCount(1);
      await expect(page.locator('.keys-table')).toContainText(prefix);
      await expect(page.locator('.keys-table')).not.toContainText(plaintext);

      keyId = Number(await row.getAttribute('data-row-key'));
      expect(Number.isSafeInteger(keyId)).toBe(true);

      // 7. Edit the label through the edit modal (PATCH via UI).
      await row.getByRole('button', { name: 'Edit' }).click();
      const editDialog = page.locator('.modal-bg').filter({ has: page.locator('form') });
      await expect(editDialog).toBeVisible();
      await editDialog.getByLabel('Label').fill(editedLabel);
      await editDialog.getByRole('button', { name: 'Save' }).click();
      await expect(editDialog).not.toBeVisible();

      // 8. Persisted state via the admin API: new label, same prefix, no secret.
      const persisted = await page.request.get(`/admin/api/keys/${keyId}`, {
        headers: adminAuthHeaders(),
      });
      expect(persisted.ok()).toBe(true);
      const persistedBody = await persisted.json() as {
        id: number;
        label: string;
        key_prefix: string;
        is_active: boolean;
        plaintext?: string;
      };
      expect(persistedBody).toMatchObject({
        id: keyId,
        label: editedLabel,
        key_prefix: prefix,
        is_active: true,
      });
      expect(persistedBody).not.toHaveProperty('plaintext');

      // 9. Regenerate/revoke have unambiguous UI flows but require the list to
      //    be fresh; PATCH + DELETE already cover the mutation contract here.
      // 10. Delete through the UI (confirm dialog) and verify absence.
      const editedRow = page.locator('tr[data-row-key]').filter({ hasText: editedLabel });
      await expect(editedRow).toHaveCount(1);
      await editedRow.getByRole('button', { name: 'Delete' }).click();
      const confirmDialog = page.locator('#show-confirm-dialog');
      await expect(confirmDialog).toBeVisible();
      await confirmDialog.getByRole('button', { name: 'Delete' }).click();
      const deletedId = keyId;
      keyId = undefined;

      await expect(page.locator('.keys-table')).not.toContainText(editedLabel);
      // Contract: 404 once deleted (BUG-2 regression).
      const gone = await page.request.get(`/admin/api/keys/${deletedId}`, {
        headers: adminAuthHeaders(),
      });
      expect(gone.status()).toBe(404);
    } finally {
      if (keyId !== undefined) {
        // Cleanup is API-only and never logs the secret.
        await page.request.delete(`/admin/api/keys/${keyId}`, {
          headers: adminAuthHeaders(),
        });
      }
    }
  });

  test('BUG-2: GET /admin/api/keys/:id answers 404 for a missing id', async ({ page }) => {
    // Missing id, not derived from any seed row. Regression for the
    // get_api_key handler: not-found must map to CoreError::NotFound
    // (404 "not_found"), never CoreError::Internal (500).
    const missingId = 9_999_999;
    const r = await page.request.get(`/admin/api/keys/${missingId}`, {
      headers: adminAuthHeaders(),
    });
    expect(r.status()).toBe(404);
    const body = await r.json() as { error?: { code?: string } };
    expect(body.error?.code).toBe('not_found');
  });
});
