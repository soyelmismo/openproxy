import { expect, test, type Page } from '@playwright/test';
import { DatabaseSync } from 'node:sqlite';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

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

const databasePath = path.resolve(fileURLToPath(import.meta.url), '..', 'data-test.db');
let database: DatabaseSync;

function requireRowId(id: number | undefined, label: string): number {
  if (id === undefined) throw new Error(`seeded notification row missing: ${label}`);
  return id;
}

async function seedNotifications(rows: NotificationSeed[]): Promise<number[]> {
  const insertWithNow = database.prepare("INSERT INTO notifications (kind, payload_json, read_at, archived_at, created_at, dedup_key, provider_id) VALUES (?, ?, ?, ?, datetime('now'), NULL, NULL)");
  const insertWithTs = database.prepare("INSERT INTO notifications (kind, payload_json, read_at, archived_at, created_at, dedup_key, provider_id) VALUES (?, ?, ?, ?, ?, NULL, NULL)");
  const ids: number[] = [];
  for (const row of rows) {
    if (row.created_at) {
      insertWithTs.run(row.kind, JSON.stringify(row.payload), row.read_at ?? null, row.archived_at ?? null, row.created_at);
    } else {
      insertWithNow.run(row.kind, JSON.stringify(row.payload), row.read_at ?? null, row.archived_at ?? null);
    }
    const last = database.prepare("SELECT last_insert_rowid() AS id").get() as { id: number };
    ids.push(Number(last.id));
  }
  return ids;
}

function clearTestNotifications(): void {
  database.prepare("DELETE FROM notifications WHERE kind = 'system' OR kind = 'model_new'").run();
}

function adminAuthHeaders(): Record<string, string> {
  const storageState = JSON.parse(readFileSync('tests/e2e/storageState.json', 'utf8')) as any;
  const token = storageState.origins.find((e: any) => e.origin === 'http://localhost:8790')?.localStorage.find((e: any) => e.name === 'openproxy_admin_token')?.value;
  return { Authorization: `Bearer ${token}` };
}

async function fetchNotifications(page: Page): Promise<NotificationRow[]> {
  const response = await page.request.get('/admin/api/notifications?limit=50', { headers: adminAuthHeaders() });
  expect(response.ok()).toBe(true);
  return await response.json() as NotificationRow[];
}

test.describe('notifications lifecycle', () => {
  test.beforeAll(() => {
    database = new DatabaseSync(databasePath);
    database.exec('PRAGMA busy_timeout = 30000');
  });

  test.beforeEach(() => clearTestNotifications());
  test.afterAll(() => { clearTestNotifications(); database.close(); });

  test('lists active notifications with unread state and kind labels', async ({ page }) => {
    const ids = await seedNotifications([
      { kind: 'system', payload: { message: 'System maintenance notice' }, read_at: '2026-09-06 11:00:00' },
      { kind: 'model_new', payload: { provider_id: 'provider-read', model_id: 'model-read' }, read_at: '2026-09-06 12:00:00' },
      { kind: 'model_new', payload: { provider_id: 'provider-new', model_id: 'model-new' } },
    ]);
    const [systemId, readModelId, unreadModelId] = [requireRowId(ids[0], 's'), requireRowId(ids[1], 'mr'), requireRowId(ids[2], 'mu')];

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
    expect(notifications.every((n) => n.read_at !== null)).toBe(true);
  });

  test('dismisses a notification by archiving it', async ({ page }) => {
    const ids = await seedNotifications([{ kind: 'system', payload: { message: 'Dismissible system notice' } }]);
    const systemId = requireRowId(ids[0], 'system');

    await page.goto('/#/notifications');
    const card = page.locator(`.notification-card[data-id="${systemId}"]`);
    await expect(card).toBeVisible();
    const archivePromise = page.waitForResponse(
      (r) => r.url().includes(`/notifications/${systemId}/archive`) && r.status() === 200,
    );
    await card.getByRole('button', { name: 'Dismiss' }).click();
    await archivePromise;
    await expect(card).toHaveCount(0);

    const notifications = await fetchNotifications(page);
    expect(notifications.some((n) => n.id === systemId)).toBe(false);
    const archivedAt = (database.prepare('SELECT archived_at FROM notifications WHERE id = ?').get(systemId) as any)?.archived_at;
    expect(archivedAt).not.toBeNull();
  });

  test('filters active notifications by kind', async ({ page }) => {
    const ids = await seedNotifications([
      { kind: 'system', payload: { message: 'Filterable system notice' } },
      { kind: 'model_new', payload: { provider_id: 'provider-filter', model_id: 'model-filter' } },
    ]);
    const [systemId, modelId] = [requireRowId(ids[0], 's'), requireRowId(ids[1], 'm')];

    await page.goto('/#/notifications');
    const filter = page.locator('select.notification-filter');
    await filter.selectOption('system');
    await expect(page.locator('.notification-card')).toHaveCount(1);
    await expect(page.locator(`.notification-card[data-id="${systemId}"] .notification-card-kind`)).toHaveText('System');
    await expect(page.locator(`.notification-card[data-id="${modelId}"]`)).toHaveCount(0);

    await filter.selectOption('all');
    await expect(page.locator('.notification-card')).toHaveCount(2);
    await expect(page.locator(`.notification-card[data-id="${systemId}"]`)).toBeVisible();
    await expect(page.locator(`.notification-card[data-id="${modelId}"]`)).toBeVisible();
  });

  test('rejects deleting a recent model notification but deletes system notifications', async ({ page }) => {
    const modelIds = await seedNotifications([{ kind: 'model_new', payload: { provider_id: 'provider-delete', model_id: 'model-delete' } }]);
    const modelId = requireRowId(modelIds[0], 'model_new');

    const rejected = await page.request.delete(`/admin/api/notifications/${modelId}`, { headers: adminAuthHeaders() });
    expect(rejected.status()).toBe(400);
    const rejectedBody = await rejected.json() as { error?: { code?: string } };
    expect(rejectedBody.error?.code).toBe('validation');

    const systemIds = await seedNotifications([{ kind: 'system', payload: { message: 'Deletable system notice' } }]);
    const systemId = requireRowId(systemIds[0], 'system');
    const deleted = await page.request.delete(`/admin/api/notifications/${systemId}`, { headers: adminAuthHeaders() });
    expect(deleted.status()).toBe(200);
    await expect(deleted.json()).resolves.toEqual({ ok: true });

    const remaining = await fetchNotifications(page);
    expect(remaining.some((n) => n.id === systemId)).toBe(false);
    const individualGet = await page.request.get(`/admin/api/notifications/${systemId}`, { headers: adminAuthHeaders() });
    expect(individualGet.status()).toBe(405);
  });

  test('flag off suppresses notification insert via API', async ({ page }) => {
    const resp = await page.request.put('/admin/api/config/notifications-enabled', {
      headers: adminAuthHeaders(), data: { notifications_enabled: false },
    });
    expect(resp.ok()).toBe(true);
    expect((await resp.json()).notifications_enabled).toBe(false);

    const list = await fetchNotifications(page);
    expect(list).toEqual([]);

    const enable = await page.request.put('/admin/api/config/notifications-enabled', {
      headers: adminAuthHeaders(), data: { notifications_enabled: true },
    });
    expect(enable.ok()).toBe(true);
    expect((await enable.json()).notifications_enabled).toBe(true);
  });

  test('prune removes 1d archived/read notifications, keeps old unread', async ({ page }) => {
    const oldArchivedId = requireRowId((await seedNotifications([
      { kind: 'system', payload: { message: 'Old archived' }, archived_at: '2026-01-01 12:00:00', read_at: '2026-01-01 11:00:00', created_at: '2026-01-01 11:00:00' },
    ]))[0], 'old archived');
    const oldReadId = requireRowId((await seedNotifications([
      { kind: 'system', payload: { message: 'Old read' }, read_at: '2026-01-01 10:00:00', created_at: '2026-01-01 10:00:00' },
    ]))[0], 'old read');
    const unreadOldId = requireRowId((await seedNotifications([
      { kind: 'system', payload: { message: 'Old unread - should survive' }, created_at: '2026-01-01 09:00:00' },
    ]))[0], 'old unread');

    const listResp = await page.request.get('/admin/api/notifications?limit=50', { headers: adminAuthHeaders() });
    const list = (await listResp.json()) as NotificationRow[];
    const listedIds = new Set(list.map((n) => n.id));
    expect(listedIds.has(oldArchivedId)).toBe(false);
    expect(listedIds.has(oldReadId)).toBe(false);
    expect(listedIds.has(unreadOldId)).toBe(false);

    const ucResp = await page.request.get('/admin/api/notifications/unread-count', { headers: adminAuthHeaders() });
    expect(((await ucResp.json()) as { count: number }).count).toBe(0);

    const stillInDb = database.prepare('SELECT id FROM notifications WHERE id = ? OR id = ? OR id = ?').all(oldArchivedId, oldReadId, unreadOldId) as any[];
    expect(stillInDb).toHaveLength(3);
  });

  test('PATCH provider notif_keyword_only', async ({ page }) => {
    const create = await page.request.post('/admin/api/providers', {
      headers: adminAuthHeaders(),
      data: {
        id: 'w4-kw-provider', name: 'W4 Keyword Provider', base_url: 'https://w4.example.com',
        auth_type: 'bearer', format: 'openai', auto_activate_keyword: 'gpt',
      },
    });
    expect(create.ok()).toBe(true);
    const pid = ((await create.json()) as { id: string }).id;

    const patchOn = await page.request.patch(`/admin/api/providers/${pid}`, {
      headers: adminAuthHeaders(), data: { notif_keyword_only: true },
    });
    expect(patchOn.status()).toBe(200);

    const afterOn = await page.request.get('/admin/api/providers', { headers: adminAuthHeaders() });
    const onRow = ((await afterOn.json()) as any[]).find((p) => p.id === pid);
    expect(onRow?.notif_keyword_only).toBe(true);

    const patchOff = await page.request.patch(`/admin/api/providers/${pid}`, {
      headers: adminAuthHeaders(), data: { notif_keyword_only: false },
    });
    expect(patchOff.status()).toBe(200);

    const afterOff = await page.request.get('/admin/api/providers', { headers: adminAuthHeaders() });
    const offRow = ((await afterOff.json()) as any[]).find((p) => p.id === pid);
    expect(offRow?.notif_keyword_only).toBe(false);

    const noopPatch = await page.request.patch(`/admin/api/providers/${pid}`, { headers: adminAuthHeaders(), data: {} });
    expect(noopPatch.status()).toBe(200);

    const del = await page.request.delete(`/admin/api/providers/${pid}`, { headers: adminAuthHeaders() });
    expect(del.ok()).toBe(true);
  });

  test('compact list smoke test', async ({ page }) => {
    await seedNotifications([{ kind: 'system', payload: { message: 'Compact list test' } }]);
    await page.goto('/#/notifications');
    await expect(page.locator('.notification-card')).toHaveCount(1);

    const deleteBtn = page.locator('.notification-card').first().getByRole('button', { name: 'Delete' });
    await deleteBtn.click();
    await page.waitForResponse((r) => r.url().includes('/admin/api/notifications/') && r.status() === 200);
    await expect(page.locator('.notification-card')).toHaveCount(0);

    await seedNotifications([{ kind: 'model_new', payload: { provider_id: 'test', model_id: 'test-model' } }]);
    await page.reload();
    await expect(page.locator('.notification-card')).toHaveCount(1);
    await expect(page.locator('.notification-card').first().getByRole('button', { name: 'Delete' })).toHaveCount(0);
  });
});
