import { test, expect, type Page } from '@playwright/test';
import type { StageEvent, RecentUsageRow } from '../../src/static/src/lib/types/api.js';

const DUMMY_ADMIN_TOKEN = 'op_live_test_dummy_token_for_e2e';
const ADMIN_TOKEN_STORAGE_KEY = 'openproxy_admin_token';

function makeFinalizedRow(overrides: Partial<RecentUsageRow> = {}): RecentUsageRow {
  return {
    id: 999999, request_id: 'req-A', trace_id: 'tid-A', provider_id: 'openrouter',
    upstream_model_id: 'claude-3-5-sonnet', status_code: 200, total_ms: 1234, prompt_tokens: 100,
    completion_tokens: 50, cost_usd: 0.002, cached_tokens: null, connect_ms: 30, ttft_ms: 120,
    request_body_json: { messages: [{ role: 'user', content: 'hi' }] },
    response_body_json: { content: [{ text: 'hello' }] },
    request_headers: { 'content-type': 'application/json' },
    response_headers: { 'content-type': 'application/json' },
    error_message: null, race_total: null, race_attempts: null, is_streaming: false,
    stream_complete: true, race_lost: false, stop_reason: 'end_turn', compression_savings_pct: null,
    compression_techniques: null, client_response: true, prompt_tokens_estimated: false,
    completion_tokens_estimated: false, proxy_url: null, proxy_status: null, is_proxy_rotated: false,
    created_at: new Date().toISOString(), ...overrides,
  };
}

function makeInflightRow(overrides: Partial<RecentUsageRow> = {}): RecentUsageRow {
  return makeFinalizedRow({
    id: 0, status_code: 0, total_ms: 0, prompt_tokens: null, completion_tokens: null,
    cost_usd: 0, request_body_json: null, response_body_json: null, request_headers: null,
    response_headers: null, is_streaming: true, stream_complete: false, client_response: false, ...overrides,
  });
}

function makeStageEvent(overrides: Partial<StageEvent> = {}): StageEvent {
  return {
    request_id: 'req-A', trace_id: 'tid-A', stage: 'streaming', elapsed_ms: 200,
    connect_ms: 30, ttft_ms: 120, status_code: 200, error: null, stop_reason: null,
    compression_savings_pct: null, compression_techniques: null, timestamp: new Date().toISOString(),
    provider_id: 'openrouter', upstream_model_id: 'claude-3-5-sonnet', ...overrides,
  };
}

async function setupLogsView(page: Page): Promise<void> {
  await page.goto('http://localhost:8790/#/logs');
  await expect(page.locator('#logs')).toBeVisible();
  await expect(page.locator('#logs >> text=Phase').first()).toBeVisible({ timeout: 5000 });
}

async function injectRowAndOpenModal(page: Page, row: RecentUsageRow): Promise<void> {
  await page.evaluate((r: RecentUsageRow) => {
    const w = window as any;
    const store = w.__liveLogsStore;
    store.rowsById.clear(); store.attemptsByKey.clear(); store.requestGroups.clear(); store.attemptKeyByRowId.clear();
    store.applyUsageRow(r);
    const logs = w.__openproxyState.logs;
    logs.page = 1; logs.rowsPerPage = 50; logs.followTail = false;
    w.__openproxyLogsGoPage(1);
  }, row);
  const rowEl = page.locator(`#logs .log-row[data-request-id="${row.request_id}"]`);
  await rowEl.first().waitFor({ timeout: 5000 });
  await rowEl.first().click();
  await page.locator('.log-detail-modal').waitFor({ timeout: 5000 });
}

async function injectInflightAndOpenModal(page: Page, inflight: RecentUsageRow, stage: StageEvent): Promise<void> {
  await page.evaluate((args: { inflight: RecentUsageRow; stage: StageEvent }) => {
    const w = window as any;
    const store = w.__liveLogsStore;
    store.rowsById.clear(); store.attemptsByKey.clear(); store.requestGroups.clear(); store.attemptKeyByRowId.clear();
    store.applyUsageRow(args.inflight);
    store.dispatch({ type: 'stage', data: args.stage });
    const logs = w.__openproxyState.logs;
    logs.page = 1; logs.rowsPerPage = 50; logs.followTail = false;
    w.__openproxyLogsGoPage(1);
  }, { inflight, stage });
  const rowEl = page.locator(`#logs .log-row[data-request-id="${inflight.request_id}"]`);
  await rowEl.first().waitFor({ timeout: 5000 });
  await rowEl.first().click();
  await page.locator('.log-detail-modal').waitFor({ timeout: 5000 });
}

async function injectUpdateLogDetail(page: Page, row: Partial<RecentUsageRow>) {
  const payload = { created_at: new Date().toISOString(), ...row };
  await page.evaluate((r: Record<string, unknown>) => {
    const w = window as any;
    if (w.__liveLogsStore) w.__liveLogsStore.applyUsageRow(r);
  }, payload);
}

async function closeModal(page: Page): Promise<void> {
  await page.locator('.log-detail-modal .close-btn').click();
  await page.waitForSelector('.log-detail-modal', { state: 'detached' });
}

test.describe('Log detail modal — contamination regression (Fixes 4-b + 5-b)', () => {
  test.beforeEach(async ({ page }: { page: Page }) => {
    await page.addInitScript((args: { key: string; token: string }) => {
      try { localStorage.setItem(args.key, args.token); } catch (_e) {}
    }, { key: ADMIN_TOKEN_STORAGE_KEY, token: DUMMY_ADMIN_TOKEN });

    await page.route('**/admin/api/usage/detail*', async (route) => {
      const url = new URL(route.request().url());
      const [id, traceId] = [url.searchParams.get('id'), url.searchParams.get('trace_id')];
      const row = await page.evaluate((args: { id: string | null; traceId: string | null }) => {
        const w = window as any;
        if (!w.__liveLogsStore) return null;
        const store = w.__liveLogsStore;
        if (args.id) return store.rowsById.get(Number(args.id)) || null;
        if (args.traceId) {
          const attempts = Array.from(store.attemptsByKey.values()) as any[];
          const attempt = attempts.find((a: any) => a.traceId === args.traceId);
          if (attempt && attempt.row) return attempt.row;
        }
        return null;
      }, { id, traceId });

      if (row) {
        await route.fulfill({ status: 200, contentType: 'application/json', body: JSON.stringify({ row }) });
      } else {
        await route.fulfill({ status: 404, contentType: 'application/json', body: JSON.stringify({ error: 'Not found' }) });
      }
    });
  });

  test('1) modal stays on row A when WS row event for row B arrives', async ({ page }) => {
    await setupLogsView(page);
    const rowA = makeFinalizedRow({ request_id: 'req-A-contam-1', trace_id: 'tid-A-contam-1' });
    await injectRowAndOpenModal(page, rowA);

    const modal = page.locator('.log-detail-modal');
    await expect(modal).toContainText('claude-3-5-sonnet');
    await expect(modal).toContainText('200');
    await expect(modal).toContainText('1234 ms');

    await injectUpdateLogDetail(page, {
      id: 9999, request_id: 'req-B-contam-1', trace_id: 'tid-B-contam-1',
      upstream_model_id: 'gpt-4o-mini-FAKE', status_code: 500, total_ms: 9999,
      error_message: 'Server Error: fake contamination',
    });

    await expect(modal).toContainText('claude-3-5-sonnet');
    await expect(modal).toContainText('200');
    await expect(modal).toContainText('1234 ms');
    await expect(modal).not.toContainText('gpt-4o-mini-FAKE');
    await expect(modal).not.toContainText('9999 ms');
    await expect(modal).not.toContainText('fake contamination');
    await expect(modal.locator('.status-pill')).toContainText('200');

    const selReqId = await page.evaluate(() => {
      const w = window as any;
      const identity = w.__openproxyState?.logs?.selectedIdentity;
      return identity ? w.__liveLogsStore?.selectDetail(identity)?.requestId ?? null : null;
    });
    expect(selReqId).toBe('req-A-contam-1');
  });

  test('2) inflight A: real row A with error_message=null clears synthetic "Request in progress"', async ({ page }) => {
    await setupLogsView(page);
    const inflightA = makeInflightRow({ request_id: 'req-A-contam-2', trace_id: 'tid-A-contam-2' });
    const stageA = makeStageEvent({ request_id: 'req-A-contam-2', trace_id: 'tid-A-contam-2', stage: 'streaming', elapsed_ms: 200 });
    await injectInflightAndOpenModal(page, inflightA, stageA);

    const modal = page.locator('.log-detail-modal');
    await expect(modal).toContainText('Request in progress', { timeout: 5000 });

    await injectUpdateLogDetail(page, {
      id: 222, request_id: 'req-A-contam-2', trace_id: 'tid-A-contam-2', status_code: 200,
      total_ms: 5000, error_message: null, upstream_model_id: 'claude-3-5-sonnet',
    });

    await expect(modal).not.toContainText('Request in progress');
    await expect(modal.locator('.status-pill')).toContainText('200');
    await expect(modal).toContainText('claude-3-5-sonnet');
    await expect(modal).toContainText('5000 ms');
    const errorsSection = page.locator('#log-detail-content [data-log-tab="errors"]');
    await expect(errorsSection).toContainText('No errors recorded');
    await expect(errorsSection).not.toContainText('Request in progress');
  });

  test('3) WS event for same row A with upstream_model_id="" overlays empty string', async ({ page }) => {
    await setupLogsView(page);
    const rowA = makeFinalizedRow({ request_id: 'req-A-contam-3', trace_id: 'tid-A-contam-3' });
    await injectRowAndOpenModal(page, rowA);

    const modal = page.locator('.log-detail-modal');
    await expect(modal).toContainText('claude-3-5-sonnet');

    await injectUpdateLogDetail(page, {
      id: 999999, request_id: 'req-A-contam-3', trace_id: 'tid-A-contam-3',
      upstream_model_id: '', status_code: 200, total_ms: 1234, error_message: null,
    });

    const snapshotModel = await page.evaluate(() => {
      const w = window as any;
      const identity = w.__openproxyState?.logs?.selectedIdentity;
      if (!identity) return '<undefined>';
      const attempt = w.__liveLogsStore?.selectDetail(identity);
      return attempt?.upstreamModelId ?? attempt?.row?.upstream_model_id ?? '<undefined>';
    });
    expect(snapshotModel).toBe('');
    await expect(modal).not.toContainText('claude-3-5-sonnet');
    const modelLine = page.locator('.log-detail-summary div').filter({ hasText: 'Model:' });
    await expect(modelLine).toContainText('—');
    await expect(modelLine).not.toContainText('claude-3-5-sonnet');
  });

  test('4) modal closes — selectedRow is cleared', async ({ page }) => {
    await setupLogsView(page);
    const rowA = makeFinalizedRow({ request_id: 'req-A-contam-4', trace_id: 'tid-A-contam-4' });
    await injectRowAndOpenModal(page, rowA);
    expect(await page.locator('.log-detail-modal').count()).toBe(1);

    const selBefore = await page.evaluate(() => {
      const w = window as any;
      const identity = w.__openproxyState?.logs?.selectedIdentity;
      return identity ? w.__liveLogsStore?.selectDetail(identity)?.requestId ?? null : null;
    });
    expect(selBefore).toBe('req-A-contam-4');

    await closeModal(page);
    expect(await page.locator('.log-detail-modal').count()).toBe(0);
    const selAfter = await page.evaluate(() => (window as any).__openproxyState?.logs?.selectedIdentity ?? null);
    expect(selAfter).toBeNull();
  });

  test('5) modal re-opens for different row — pinned identity resets correctly', async ({ page }) => {
    await setupLogsView(page);
    const rowA = makeFinalizedRow({ id: 999999, request_id: 'req-A-contam-5', trace_id: 'tid-A-contam-5', upstream_model_id: 'claude-3-5-sonnet', status_code: 200, total_ms: 1234 });
    const rowB = makeFinalizedRow({ id: 999998, request_id: 'req-B-contam-5', trace_id: 'tid-B-contam-5', upstream_model_id: 'gpt-4o-mini', status_code: 201, total_ms: 5678 });

    await injectRowAndOpenModal(page, rowA);
    const modal = page.locator('.log-detail-modal');
    await expect(modal).toContainText('claude-3-5-sonnet');
    await expect(modal).toContainText('1234 ms');
    await closeModal(page);

    await page.evaluate((args: { a: RecentUsageRow; b: RecentUsageRow }) => {
      const w = window as any;
      const store = w.__liveLogsStore;
      store.rowsById.clear(); store.attemptsByKey.clear(); store.requestGroups.clear(); store.attemptKeyByRowId.clear();
      store.applyUsageRow(args.a); store.applyUsageRow(args.b);
      const logs = w.__openproxyState.logs;
      logs.page = 1; logs.rowsPerPage = 50; logs.followTail = false;
      w.__openproxyLogsGoPage(1);
    }, { a: rowA, b: rowB });

    const rowBEl = page.locator(`#logs .log-row[data-request-id="${rowB.request_id}"]`);
    await rowBEl.first().waitFor({ timeout: 5000 });
    await rowBEl.first().click();
    await page.locator('.log-detail-modal').waitFor({ timeout: 5000 });

    await expect(modal).toContainText('gpt-4o-mini');
    await expect(modal).toContainText('5678 ms');
    await expect(modal).not.toContainText('claude-3-5-sonnet');
    await expect(modal).not.toContainText('1234 ms');

    await injectUpdateLogDetail(page, {
      id: 999999, request_id: 'req-A-contam-5', trace_id: 'tid-A-contam-5',
      upstream_model_id: 'claude-3-5-sonnet-LATE', status_code: 200, total_ms: 1234, error_message: null,
    });

    await expect(modal).toContainText('gpt-4o-mini');
    await expect(modal).toContainText('5678 ms');
    await expect(modal).not.toContainText('claude-3-5-sonnet-LATE');
    await expect(modal).not.toContainText('1234 ms');
  });

  test('6) modal stays on A under burst of 50 WS events for other requests', async ({ page }) => {
    await setupLogsView(page);
    const rowA = makeFinalizedRow({ request_id: 'req-A-contam-6', trace_id: 'tid-A-contam-6' });
    await injectRowAndOpenModal(page, rowA);

    const modal = page.locator('.log-detail-modal');
    await expect(modal).toContainText('claude-3-5-sonnet');
    await expect(modal).toContainText('200');
    await expect(modal).toContainText('1234 ms');

    await page.evaluate(() => {
      const w = window as any;
      if (typeof w.__openproxyUpdateLogDetail !== 'function') return;
      for (let i = 0; i < 50; i++) {
        w.__openproxyUpdateLogDetail({
          id: 10_000 + i, request_id: `req-other-contam-6-${i}`, trace_id: `tid-other-contam-6-${i}`,
          upstream_model_id: `model-fake-${i}`, status_code: 500, total_ms: 9999, error_message: `Error fake ${i}`,
        });
      }
    });

    await expect(modal).toContainText('claude-3-5-sonnet');
    await expect(modal).toContainText('200');
    await expect(modal).toContainText('1234 ms');
    await expect(modal).not.toContainText('model-fake-0');
    await expect(modal).not.toContainText('model-fake-25');
    await expect(modal).not.toContainText('model-fake-49');
    await expect(modal).not.toContainText('Error fake');
    await expect(modal).not.toContainText('9999 ms');

    const selReqId = await page.evaluate(() => {
      const w = window as any;
      const identity = w.__openproxyState?.logs?.selectedIdentity;
      return identity ? w.__liveLogsStore?.selectDetail(identity)?.requestId ?? null : null;
    });
    expect(selReqId).toBe('req-A-contam-6');
  });

  test('7) Fix 4 edge case: modal open for inflight with empty trace_id', async ({ page }) => {
    await setupLogsView(page);
    const inflightNoTid = makeInflightRow({ request_id: 'req-A-contam-7', trace_id: '' });
    const stageNoTid = makeStageEvent({ request_id: 'req-A-contam-7', trace_id: '', stage: 'streaming' });
    await injectInflightAndOpenModal(page, inflightNoTid, stageNoTid);

    const modal = page.locator('.log-detail-modal');
    await expect(modal).toContainText('Request in progress', { timeout: 5000 });
    await expect(modal).toContainText('claude-3-5-sonnet');

    await injectUpdateLogDetail(page, {
      id: 0, request_id: 'req-A-contam-7', trace_id: '',
      upstream_model_id: 'gpt-4o-mini-SHOULD-NOT-APPEAR', status_code: 200, total_ms: 9999, error_message: null,
    });

    await expect(modal).not.toContainText('gpt-4o-mini-SHOULD-NOT-APPEAR');
    await expect(modal).not.toContainText('9999 ms');
    await expect(modal).toContainText('Request in progress');

    const selReqId = await page.evaluate(() => {
      const w = window as any;
      const identity = w.__openproxyState?.logs?.selectedIdentity;
      return identity ? w.__liveLogsStore?.selectDetail(identity)?.requestId ?? null : null;
    });
    expect(selReqId).toBe('req-A-contam-7');
  });
});
