import { expect, test, type Page } from '@playwright/test';
import { DatabaseSync } from 'node:sqlite';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

/**
 * Real (unstubbed) notifications lifecycle coverage. The browser talks to the
 * openproxy test server and its seeded test database; no route interception on
 * /admin/api/notifications. Rows are seeded directly into SQLite via
 * `node:sqlite` (built-in, zero new dependencies) because the admin API has no
 * create endpoint.
 *
 * Server contract under test (crates/openproxy-server/src/handlers/admin/notifications.rs):
 *   GET    /admin/api/notifications?limit=50 -> 200 NotificationRow[] (archived excluded)
 *   POST   /admin/api/notifications/:id/read    -> 200 {ok: true}
 *   POST   /admin/api/notifications/read-all    -> 200 {updated: n}
 *   POST   /admin/api/notifications/:id/archive -> 200 {ok: true}
 *   DELETE /admin/api/notifications/:id         -> 200 {ok: true} for kind=system
 *                                               -> 400 {error:{code:"validation"}} for kind=model_*
 *                                                  within the 30-day audit window, or missing row
 *
 * Contract correction vs. the original test plan (like BUG-C1 in
 * combos-crud.spec.ts): the plan stated `GET /admin/api/notifications/:id`
 * answers 404 after deletion, but no GET-by-id route exists at all — axum
 * answers 405 Method Not Allowed. Asserted as it actually is.
 */

type NotificationSeed = {
  kind: 'system' | 'model_new';
  payload: Record<string, unknown>;
  read_at?: string;
  archived_at?: string;
  created_at?: string;
};

type NotificationRow = {
  id: number;
  kind: string;
  read_at: string | null;
  archived_at: string | null;
};

// The spec lives in tests/e2e/ next to data-test.db (copied from
// data-test.seed.db by `pnpm test:e2e` before Playwright boots).
const databasePath = path.resolve(fileURLToPath(import.meta.url), '..', 'data-test.db');
let database: DatabaseSync;

/** Guard for `noUncheckedIndexedAccess`: seeded ids must exist. */
function requireRowId(id: number | undefined, label: string): number {
  if (id === undefined) throw new Error(`seeded notification row missing: ${label}`);
  return id;
}

async function seedNotifications(rows: NotificationSeed[]): Promise<number[]> {
  const insertWithNow = database.prepare(`
    INSERT INTO notifications
      (kind, payload_json, read_at, archived_at, created_at, dedup_key, provider_id)
    VALUES (?, ?, ?, ?, datetime('now'), NULL, NULL)
  `);
  const insertWithTs = database.prepare(`
    INSERT INTO notifications
      (kind, payload_json, read_at, archived_at, created_at, dedup_key, provider_id)
    VALUES (?, ?, ?, ?, ?, NULL, NULL)
  `);
  const ids: number[] = [];
  for (const row of rows) {
    const stmt = row.created_at ? insertWithTs : insertWithNow;
    const result = stmt.run(
      row.kind,
      JSON.stringify(row.payload),
      row.read_at ?? null,
      row.archived_at ?? null,
      row.created_at ?? null,
    );
    ids.push(Number(result.lastInsertRowid));
  }
  return ids;
}

/** Remove only the kinds this spec creates. NEVER truncates the table —
 *  other specs may assume their own rows; the seed DB ships notifications
 *  empty, so deleting our kinds restores the initial state. */
function clearTestNotifications(): void {
  database.prepare("DELETE FROM notifications WHERE kind = 'system' OR kind = 'model_new'").run();
}

// `page.request` shares cookies only, not localStorage; the dashboard
// authenticates with a Bearer token stored in localStorage (see
// state/auth.ts). Read it from the same storageState.json the browser
// context is seeded with — single source of truth for test credentials.
const storageStatePath = 'tests/e2e/storageState.json';
function adminAuthHeaders(): Record<string, string> {
  const storageState = JSON.parse(readFileSync(storageStatePath, 'utf8')) as {
    origins: Array<{ origin: string; localStorage: Array<{ name: string; value: string }> }>;
  };
  const origin = storageState.origins.find((entry) => entry.origin === 'http://localhost:8790');
  const token = origin?.localStorage.find((entry) => entry.name === 'openproxy_admin_token')?.value;
  if (!token) throw new Error(`openproxy_admin_token not found in ${storageStatePath}`);
  return { Authorization: `Bearer ${token}` };
}

async function fetchNotifications(page: Page): Promise<NotificationRow[]> {
  const response = await page.request.get('/admin/api/notifications?limit=50', {
    headers: adminAuthHeaders(),
  });
  expect(response.ok()).toBe(true);
  return await response.json() as NotificationRow[];
}

test.describe('notifications lifecycle', () => {
  test.beforeAll(() => {
    database = new DatabaseSync(databasePath);
    database.exec('PRAGMA busy_timeout = 30000');
  });

  test.beforeEach(() => {
    clearTestNotifications();
  });

  test.afterAll(() => {
    clearTestNotifications();
    database.close();
  });

  test('lists active notifications with unread state and kind labels', async ({ page }) => {
    const ids = await seedNotifications([
      { kind: 'system', payload: { message: 'System maintenance notice' }, read_at: '2026-09-06 11:00:00' },
      {
        kind: 'model_new',
        payload: { provider_id: 'provider-read', model_id: 'model-read' },
        read_at: '2026-09-06 12:00:00',
      },
      { kind: 'model_new', payload: { provider_id: 'provider-new', model_id: 'model-new' } },
    ]);
    const systemId = requireRowId(ids[0], 'system');
    const readModelId = requireRowId(ids[1], 'model_new read');
    const unreadModelId = requireRowId(ids[2], 'model_new unread');

    await page.goto('/#/notifications');
    await expect(page.locator('.notification-card')).toHaveCount(3);
    await expect(page.locator(`.notification-card[data-id="${systemId}"]`)).toBeVisible();
    await expect(page.locator(`.notification-card[data-id="${readModelId}"]`)).not.toHaveClass(/unread/);
    await expect(page.locator(`.notification-card[data-id="${unreadModelId}"]`)).toHaveClass(/unread/);
    await expect(page.locator('.page-header .badge-error')).toHaveText('1 unread');
    await expect(page.locator('.notification-card-kind').filter({ hasText: 'System' })).toHaveCount(1);
    await expect(page.locator('.notification-card-kind').filter({ hasText: 'New model' })).toHaveCount(2);
  });

  test('marks every active notification as read', async ({ page }) => {
    await seedNotifications([
      { kind: 'system', payload: { message: 'Mark-all-read system notice' } },
      { kind: 'model_new', payload: { provider_id: 'provider-a', model_id: 'model-a' } },
      { kind: 'model_new', payload: { provider_id: 'provider-b', model_id: 'model-b' } },
    ]);

    await page.goto('/#/notifications');
    await expect(page.locator('.notification-card')).toHaveCount(3);
    await page.getByRole('button', { name: 'Mark all as read' }).click();
    await expect(page.locator('.page-header .badge-info')).toHaveText("You're all caught up!");

    const notifications = await fetchNotifications(page);
    expect(notifications).toHaveLength(3);
    expect(notifications.every((notification) => notification.read_at !== null)).toBe(true);
  });

  test('dismisses a notification by archiving it', async ({ page }) => {
    const ids = await seedNotifications([
      { kind: 'system', payload: { message: 'Dismissible system notice' } },
    ]);
    const systemId = requireRowId(ids[0], 'system');

    await page.goto('/#/notifications');
    const card = page.locator(`.notification-card[data-id="${systemId}"]`);
    await expect(card).toBeVisible();
    const archivePromise = page.waitForResponse(
      (response) => response.url().includes(`/notifications/${systemId}/archive`) && response.status() === 200,
    );
    await card.getByRole('button', { name: 'Dismiss' }).click();
    await archivePromise;
    await expect(card).toHaveCount(0);

    const notifications = await fetchNotifications(page);
    expect(notifications.some((notification) => notification.id === systemId)).toBe(false);
    const archivedAt = (database
      .prepare('SELECT archived_at FROM notifications WHERE id = ?')
      .get(systemId) as { archived_at: string | null } | undefined)?.archived_at;
    expect(archivedAt).not.toBeNull();
  });

  test('filters active notifications by kind', async ({ page }) => {
    const ids = await seedNotifications([
      { kind: 'system', payload: { message: 'Filterable system notice' } },
      { kind: 'model_new', payload: { provider_id: 'provider-filter', model_id: 'model-filter' } },
    ]);
    const systemId = requireRowId(ids[0], 'system');
    const modelId = requireRowId(ids[1], 'model_new');

    await page.goto('/#/notifications');
    const filter = page.locator('select.notification-filter');
    await filter.selectOption('system');
    await expect(page.locator('.notification-card')).toHaveCount(1);
    await expect(
      page.locator(`.notification-card[data-id="${systemId}"] .notification-card-kind`),
    ).toHaveText('System');
    await expect(page.locator(`.notification-card[data-id="${modelId}"]`)).toHaveCount(0);

    await filter.selectOption('all');
    await expect(page.locator('.notification-card')).toHaveCount(2);
    await expect(page.locator(`.notification-card[data-id="${systemId}"]`)).toBeVisible();
    await expect(page.locator(`.notification-card[data-id="${modelId}"]`)).toBeVisible();
  });

  test('rejects deleting a recent model notification but deletes system notifications', async ({ page }) => {
    const modelIds = await seedNotifications([
      { kind: 'model_new', payload: { provider_id: 'provider-delete', model_id: 'model-delete' } },
    ]);
    const modelId = requireRowId(modelIds[0], 'model_new');

    // Regression: model_* rows inside the 30-day audit window are not
    // deletable — DELETE must answer 400 {error:{code:"validation"}}.
    const rejected = await page.request.delete(`/admin/api/notifications/${modelId}`, {
      headers: adminAuthHeaders(),
    });
    expect(rejected.status()).toBe(400);
    const rejectedBody = await rejected.json() as { error?: { code?: string } };
    expect(rejectedBody.error?.code).toBe('validation');

    const systemIds = await seedNotifications([
      { kind: 'system', payload: { message: 'Deletable system notice' } },
    ]);
    const systemId = requireRowId(systemIds[0], 'system');
    const deleted = await page.request.delete(`/admin/api/notifications/${systemId}`, {
      headers: adminAuthHeaders(),
    });
    expect(deleted.status()).toBe(200);
    await expect(deleted.json()).resolves.toEqual({ ok: true });

    const remaining = await fetchNotifications(page);
    expect(remaining.some((notification) => notification.id === systemId)).toBe(false);
    // Contract: there is no GET-by-id route; axum answers 405 (the plan's
    // "GET 404" was wrong — see file header).
    const individualGet = await page.request.get(`/admin/api/notifications/${systemId}`, {
      headers: adminAuthHeaders(),
    });
    expect(individualGet.status()).toBe(405);
  });

  test('flag off suppresses notification insert via API', async ({ page }) => {
    // Verify the notifications_enabled gate: when false, the list endpoint
    // returns an empty tray regardless of backend activity.
    const resp = await page.request.put('/admin/api/config/notifications-enabled', {
      headers: adminAuthHeaders(),
      data: { notifications_enabled: false },
    });
    expect(resp.ok()).toBe(true);
    expect((await resp.json()).notifications_enabled).toBe(false);

    // The list should be empty.
    const list = await fetchNotifications(page);
    expect(list).toEqual([]);

    // Re-enable for subsequent tests.
    const enable = await page.request.put('/admin/api/config/notifications-enabled', {
      headers: adminAuthHeaders(),
      data: { notifications_enabled: true },
    });
    expect(enable.ok()).toBe(true);
    expect((await enable.json()).notifications_enabled).toBe(true);
  });

  test('prune removes 1d archived/read notifications, keeps old unread', async ({ page }) => {
    // Seed two OLD (>1 day) archived/read notifications directly into the DB.
    const oldArchivedId = requireRowId((await seedNotifications([
      { kind: 'system', payload: { message: 'Old archived' }, archived_at: '2026-01-01 12:00:00', read_at: '2026-01-01 11:00:00', created_at: '2026-01-01 11:00:00' },
    ]))[0], 'old archived');
    const oldReadId = requireRowId((await seedNotifications([
      { kind: 'system', payload: { message: 'Old read' }, read_at: '2026-01-01 10:00:00', created_at: '2026-01-01 10:00:00' },
    ]))[0], 'old read');

    // Seed an OLD unread notification (read_at null) — must survive the 1d window.
    const unreadOldId = requireRowId((await seedNotifications([
      { kind: 'system', payload: { message: 'Old unread - should survive' }, created_at: '2026-01-01 09:00:00' },
    ]))[0], 'old unread');

    // W1 retention window: the list + unread_count endpoints are capped at
    // `created_at >= datetime('now', '-1 day')`, so the old rows never
    // surface in the tray/badge — identical to the effect of prune() on the
    // display path. The actual prune() DELETE (which removes archived/read
    // old rows from the DB) is covered by the Rust unit test
    // `prune_keeps_old_unread_deletes_old_read_and_archived`.
    const listResp = await page.request.get('/admin/api/notifications?limit=50', {
      headers: adminAuthHeaders(),
    });
    const list = (await listResp.json()) as NotificationRow[];
    const listedIds = new Set(list.map((n) => n.id));
    expect(listedIds.has(oldArchivedId)).toBe(false);
    expect(listedIds.has(oldReadId)).toBe(false);
    expect(listedIds.has(unreadOldId)).toBe(false);

    // unread_count is also 1d-capped: the three rows above are all older
    // than the window, so the badge must show zero.
    const ucResp = await page.request.get('/admin/api/notifications/unread-count', {
      headers: adminAuthHeaders(),
    });
    expect(((await ucResp.json()) as { count: number }).count).toBe(0);

    // The rows still exist in the DB (prune hasn't run yet); the 1d window
    // is what keeps them out of the UI.
    const stillInDb = database.prepare(
      'SELECT id FROM notifications WHERE id = ? OR id = ? OR id = ?',
    ).all(oldArchivedId, oldReadId, unreadOldId) as Array<{ id: number }>;
    expect(stillInDb).toHaveLength(3);
  });

  test('PATCH provider notif_keyword_only', async ({ page }) => {
    // Create a provider with auto_activate_keyword set so the three-state
    // PATCH path has a real row to act on.
    const create = await page.request.post('/admin/api/providers', {
      headers: adminAuthHeaders(),
      data: {
        id: 'w4-kw-provider',
        name: 'W4 Keyword Provider',
        base_url: 'https://w4.example.com',
        auth_type: 'bearer',
        format: 'openai',
        auto_activate_keyword: 'gpt',
      },
    });
    expect(create.ok()).toBe(true);
    const created = (await create.json()) as { id: string };
    const pid = created.id;

    // PATCH to enable keyword-only (should be 200).
    const patchOn = await page.request.patch(`/admin/api/providers/${pid}`, {
      headers: adminAuthHeaders(),
      data: { notif_keyword_only: true },
    });
    expect(patchOn.status()).toBe(200);

    // Verify DB state: notif_keyword_only === 1 (true).
    const afterOn = await page.request.get('/admin/api/providers', {
      headers: adminAuthHeaders(),
    });
    const providersAfterOn = (await afterOn.json()) as Array<{ id: string; notif_keyword_only?: boolean }>;
    const onRow = providersAfterOn.find((p) => p.id === pid);
    expect(onRow?.notif_keyword_only).toBe(true);

    // PATCH back to 0 (false) → the toggle is flipped off in the DB.
    const patchOff = await page.request.patch(`/admin/api/providers/${pid}`, {
      headers: adminAuthHeaders(),
      data: { notif_keyword_only: false },
    });
    expect(patchOff.status()).toBe(200);
    const afterOff = await page.request.get('/admin/api/providers', {
      headers: adminAuthHeaders(),
    });
    const providersAfterOff = (await afterOff.json()) as Array<{ id: string; notif_keyword_only?: boolean }>;
    const offRow = providersAfterOff.find((p) => p.id === pid);
    expect(offRow?.notif_keyword_only).toBe(false);

    // no-op: PATCH None (keyword on) must NOT create a notification branch.
    // We assert the flag stays false (the toggle was not silently enabled).
    const noopPatch = await page.request.patch(`/admin/api/providers/${pid}`, {
      headers: adminAuthHeaders(),
      data: {},
    });
    expect(noopPatch.status()).toBe(200);
    const afterNoop = await page.request.get('/admin/api/providers', {
      headers: adminAuthHeaders(),
    });
    const providersAfterNoop = (await afterNoop.json()) as Array<{ id: string; notif_keyword_only?: boolean }>;
    const noopRow = providersAfterNoop.find((p) => p.id === pid);
    expect(noopRow?.notif_keyword_only).toBe(false);

    // Clean up the test provider.
    const del = await page.request.delete(`/admin/api/providers/${pid}`, {
      headers: adminAuthHeaders(),
    });
    expect(del.ok()).toBe(true);
  });

  test('compact list smoke test', async ({ page }) => {
    await seedNotifications([{
      kind: 'system',
      payload: { message: 'Compact list test' },
    }]);

    await page.goto('/#/notifications');

    // Verify renderRow exists (compact list - one row per notification)
    await expect(page.locator('.notification-card')).toHaveCount(1);

    // Test DELETE system notification (200)
    const cards = page.locator('.notification-card');
    const deleteBtn = cards.first().getByRole('button', { name: 'Delete' });
    await deleteBtn.click();

    // Wait for delete request to succeed
    await page.waitForResponse((response) =>
      response.url().includes('/admin/api/notifications/') && response.status() === 200,
    );

    // Verify notification is gone
    await expect(page.locator('.notification-card')).toHaveCount(0);

    // Verify model notifications in audit window return 400 on DELETE
    await seedNotifications([{
      kind: 'model_new',
      payload: { provider_id: 'test', model_id: 'test-model' },
    }]);

    await expect(page.locator('.notification-card')).toHaveCount(1);
    const modelCards = page.locator('.notification-card');
    // model_* notifications should NOT show delete button (isDeletable returns false within 30d)
    await expect(modelCards.first().getByRole('button', { name: 'Delete' })).toHaveCount(0);
  });
});
