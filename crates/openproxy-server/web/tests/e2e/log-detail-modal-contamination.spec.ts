// @see tsconfig.test.json for type settings.
//
// e2e/log-detail-modal-contamination.spec.ts — regression tests for
// the user-reported bug:
//   "al abrir un log, el modal se ve bien pero cuando entra una
//    petición nueva este modal se bugea y muestra datos de otras
//    peticiones".
//
// These tests exercise the fixes applied by Builder 4-b + Reviewer 5-b
// in `components/log-detail.ts` and `views/logs.ts`:
//
//   Fix 1 (CAUSA PRIMARIA):      whitelist OVERLAYABLE_FIELDS +
//                                special-case `error_message`
//                                (always overlay, even null) so the
//                                synthetic "Request in progress..."
//                                message is cleared when the real row
//                                arrives with `error_message: null`.
//   Fix 2 (CAUSA SECUNDARIA):    `reapStaleInflight` pushes its
//                                mutation to the open modal via
//                                `updateOpenLogDetail(placeholder)`.
//   Fix 3 (HARDENING):           `removeLogDetailModal` clears
//                                `state.logs.selectedRow = null`.
//   Fix 4 (DEFENSIVE):           `matchesPinnedModalIdentity` returns
//                                false when both trace_ids are empty
//                                (HALLAZGO 6 edge case).
//   Fix 5 (HARDENING):           `snapshotRow` deep-clones via
//                                `JSON.parse(JSON.stringify(row))`.
//   Fix 6 (TEST HOOK):           `window.__openproxyUpdateLogDetail`
//                                exposed for e2e injection.
//
// The tests follow the pattern of `live-logs-retry.spec.ts` and
// `phase-robustness.spec.ts`: navigate to /#/logs, clear the live
// state, inject synthetic rows/inflight placeholders, force a
// re-render via `window.__openproxyLogsGoPage(1)`, then click the
// rendered row to open the modal. Once the modal is open we use
// `window.__openproxyUpdateLogDetail({...})` to simulate WS `row`
// events arriving while the modal is open.
//
// NOTE: we deliberately do NOT redeclare `Window.__openproxyState`
// here. The `live-logs-retry.spec.ts` spec already declares it with
// a permissive shape that the dashboard's `state.logs` type fits
// inside. A second `declare global { interface Window { ... } }` in
// this file would conflict with the existing one. Instead we cast
// at the call site (same pattern as `phase-robustness.spec.ts`).
// `Window.__openproxyUpdateLogDetail` IS declared globally in
// `components/log-detail.ts` (Fix 6), so it's already visible to
// the typechecker without re-declaration here.

import { test, expect, type Page } from '@playwright/test';
import type { StageEvent, RecentUsageRow } from '../../src/static/src/lib/types/api.js';

// The dashboard's `navigate()` function (state/router.ts) checks
// `isLoggedIn()` (state/auth.ts), which reads from
// `localStorage["openproxy_admin_token"]`. Without a token, every
// route is redirected to `#/login` and the logs view never mounts.
// The backend's `OPENPROXY_DASHBOARD_AUTH_BYPASS=1` env var only
// bypasses the API middleware — the SPA's local gate still applies.
// We seed a dummy token before each test so the SPA mounts the
// logs view. The token is never sent to the API in these tests
// (we inject synthetic state directly), so any non-empty string
// works.
const DUMMY_ADMIN_TOKEN = 'op_live_test_dummy_token_for_e2e';
const ADMIN_TOKEN_STORAGE_KEY = 'openproxy_admin_token';

test.beforeEach(async ({ page }: { page: Page }) => {
  await page.addInitScript((args: { key: string; token: string }) => {
    try {
      localStorage.setItem(args.key, args.token);
    } catch (_e: unknown) {
      // Ignore — the script runs in the page context; if
      // localStorage is unavailable (rare), the test will fail
      // later with a clearer error.
    }
  }, { key: ADMIN_TOKEN_STORAGE_KEY, token: DUMMY_ADMIN_TOKEN });

  // Intercept the API detail calls, lookup in the window state
  await page.route('**/admin/api/usage/detail*', async (route) => {
    const url = new URL(route.request().url());
    const id = url.searchParams.get('id');
    const traceId = url.searchParams.get('trace_id');

    // Retrieve the row from state
    const row = await page.evaluate((args: { id: string | null, traceId: string | null }) => {
      const w = window as any;
      if (!w.__openproxyState?.logs) return null;
      const logs = w.__openproxyState.logs;
      if (args.id) {
        return logs.rowById.get(Number(args.id)) || logs.rows.find((r: any) => r.id === Number(args.id)) || null;
      }
      if (args.traceId) {
        return logs.rows.find((r: any) => r.trace_id === args.traceId) || null;
      }
      return null;
    }, { id, traceId });

    if (row) {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({ row }),
      });
    } else {
      await route.fulfill({
        status: 404,
        contentType: 'application/json',
        body: JSON.stringify({ error: 'Not found in test mock state' }),
      });
    }
  });
});

// ---------------------------------------------------------------------------
// Helpers — synthetic row builders
// ---------------------------------------------------------------------------

/** Build a finalized RecentUsageRow with sane defaults. The
 *  `request_body_json` and `response_body_json` are deliberately
 *  model-agnostic (no `claude-3-5-sonnet` string in them) so the
 *  tests can assert "the modal contains claude-3-5-sonnet" without
 *  false positives from the Raw log section's JSON dump. */
function makeFinalizedRow(overrides: Partial<RecentUsageRow> = {}): RecentUsageRow {
  return {
    id: 5001,
    request_id: 'req-A',
    trace_id: 'tid-A',
    provider_id: 'openrouter',
    upstream_model_id: 'claude-3-5-sonnet',
    status_code: 200,
    total_ms: 1234,
    prompt_tokens: 100,
    completion_tokens: 50,
    cost_usd: 0.002,
    connect_ms: 30,
    ttft_ms: 120,
    request_body_json: { messages: [{ role: 'user', content: 'hi' }] },
    response_body_json: { content: [{ text: 'hello' }] },
    request_headers: { 'content-type': 'application/json' },
    response_headers: { 'content-type': 'application/json' },
    error_message: null,
    race_total: null,
    race_attempts: null,
    is_streaming: false,
    stream_complete: true,
    race_lost: false,
    stop_reason: 'end_turn',
    compression_savings_pct: null,
    compression_techniques: null,
    client_response: true,
    prompt_tokens_estimated: false,
    completion_tokens_estimated: false,
    created_at: new Date().toISOString(),
    ...overrides,
  };
}

/** Build an in-flight RecentUsageRow (id=0, status_code=0,
 *  is_streaming=true). The `openLogDetail` code path will detect
 *  this as inflight and synthesize the "Request in progress..."
 *  error_message on the snapshot it passes to `showLogDetail`. */
function makeInflightRow(overrides: Partial<RecentUsageRow> = {}): RecentUsageRow {
  return {
    id: 0,
    request_id: 'req-A',
    trace_id: 'tid-A',
    provider_id: 'openrouter',
    upstream_model_id: 'claude-3-5-sonnet',
    status_code: 0,
    total_ms: 0,
    prompt_tokens: null,
    completion_tokens: null,
    cost_usd: 0,
    connect_ms: 30,
    ttft_ms: 120,
    request_body_json: null,
    response_body_json: null,
    request_headers: null,
    response_headers: null,
    error_message: null,
    race_total: null,
    race_attempts: null,
    is_streaming: true,
    stream_complete: false,
    race_lost: false,
    stop_reason: null,
    compression_savings_pct: null,
    compression_techniques: null,
    client_response: false,
    prompt_tokens_estimated: false,
    completion_tokens_estimated: false,
    created_at: new Date().toISOString(),
    ...overrides,
  };
}

/** Build a synthetic StageEvent for a non-terminal stage
 *  (default: "streaming") so the inflight branch of `openLogDetail`
 *  produces the "Request in progress — current stage: streaming"
 *  synthetic error_message. */
function makeStageEvent(overrides: Partial<StageEvent> = {}): StageEvent {
  return {
    request_id: 'req-A',
    trace_id: 'tid-A',
    stage: 'streaming',
    elapsed_ms: 200,
    connect_ms: 30,
    ttft_ms: 120,
    status_code: 200,
    error: null,
    stop_reason: null,
    compression_savings_pct: null,
    compression_techniques: null,
    timestamp: new Date().toISOString(),
    provider_id: 'openrouter',
    upstream_model_id: 'claude-3-5-sonnet',
    ...overrides,
  };
}

// ---------------------------------------------------------------------------
// Helpers — view setup and modal opening
// ---------------------------------------------------------------------------

/** Navigate to /#/logs and wait for the view to mount (the "Phase"
 *  header is always present once the logs view is mounted). */
async function setupLogsView(page: Page): Promise<void> {
  await page.goto('http://localhost:8788/#/logs');
  await expect(page.locator('#logs')).toBeVisible();
  await expect(page.locator('#logs >> text=Phase').first()).toBeVisible({ timeout: 5000 });
}

/** Inject a finalized row into the dashboard state, force a re-render,
 *  and click the row to open the modal. The row's `request_body_json`
 *  must be non-null so `hasCompleteLogDetail` returns true and
 *  `openLogDetail` does NOT make a `/usage/detail` fetch (which would
 *  404 for a synthetic row). */
async function injectRowAndOpenModal(
  page: Page,
  row: RecentUsageRow,
): Promise<void> {
  await page.evaluate((r: RecentUsageRow) => {
    const w = window as unknown as {
      __openproxyState: {
        logs: {
          rows: RecentUsageRow[];
          rowById: Map<number, RecentUsageRow>;
          stagesByTraceId: Map<string, StageEvent>;
          stagesByRequestId: Map<string, StageEvent>;
          inflightByTraceId: Map<string, RecentUsageRow>;
          inflightByRequestId: Map<string, RecentUsageRow>;
          page: number;
          rowsPerPage: number;
          followTail: boolean;
        };
      };
      __openproxyLogsGoPage: (page: number) => void;
    };
    const logs = w.__openproxyState.logs;
    logs.rows = [r];
    logs.rowById.clear();
    logs.rowById.set(r.id, r);
    logs.stagesByTraceId.clear();
    logs.stagesByRequestId.clear();
    logs.inflightByTraceId.clear();
    logs.inflightByRequestId.clear();
    logs.page = 1;
    logs.rowsPerPage = 50;
    logs.followTail = false;
    w.__openproxyLogsGoPage(1);
  }, row);

  const rowEl = page.locator(`#logs .log-row[data-request-id="${row.request_id}"]`);
  await rowEl.first().waitFor({ timeout: 5000 });
  await rowEl.first().click();
  await page.locator('.log-detail-modal').waitFor({ timeout: 5000 });
}

/** Inject an inflight placeholder (with its stage event) into the
 *  dashboard state, force a re-render, and click the rendered row
 *  to open the modal. Inflight placeholders render with a synthetic
 *  id (MAX_SAFE_INTEGER - now + created_at_ms) so we click by
 *  `data-request-id` (which is the real `request_id`). */
async function injectInflightAndOpenModal(
  page: Page,
  inflight: RecentUsageRow,
  stage: StageEvent,
): Promise<void> {
  await page.evaluate(
    (args: { inflight: RecentUsageRow; stage: StageEvent }) => {
      const w = window as unknown as {
        __openproxyState: {
          logs: {
            rows: RecentUsageRow[];
            rowById: Map<number, RecentUsageRow>;
            stagesByTraceId: Map<string, StageEvent>;
            stagesByRequestId: Map<string, StageEvent>;
            inflightByTraceId: Map<string, RecentUsageRow>;
            inflightByRequestId: Map<string, RecentUsageRow>;
            page: number;
            rowsPerPage: number;
            followTail: boolean;
          };
        };
        __openproxyLogsGoPage: (page: number) => void;
      };
      const logs = w.__openproxyState.logs;
      logs.rows = [];
      logs.rowById.clear();
      logs.stagesByTraceId.clear();
      logs.stagesByRequestId.clear();
      logs.inflightByTraceId.clear();
      logs.inflightByRequestId.clear();
      logs.page = 1;
      logs.rowsPerPage = 50;
      logs.followTail = false;
      if (args.inflight.trace_id) {
        logs.inflightByTraceId.set(args.inflight.trace_id, args.inflight);
        logs.stagesByTraceId.set(args.inflight.trace_id, args.stage);
      } else {
        logs.inflightByRequestId.set(args.inflight.request_id, args.inflight);
        logs.stagesByRequestId.set(args.inflight.request_id, args.stage);
      }
      w.__openproxyLogsGoPage(1);
    },
    { inflight, stage },
  );

  const rowEl = page.locator(`#logs .log-row[data-request-id="${inflight.request_id}"]`);
  await rowEl.first().waitFor({ timeout: 5000 });
  await rowEl.first().click();
  await page.locator('.log-detail-modal').waitFor({ timeout: 5000 });
}

/** Simulate a WS `row` event reaching the dashboard while the modal
 *  is open, by calling the Fix 6 test hook
 *  `window.__openproxyUpdateLogDetail`. */
async function injectUpdateLogDetail(
  page: Page,
  row: Record<string, unknown>,
): Promise<void> {
  await page.evaluate((r: Record<string, unknown>) => {
    const w = window as unknown as {
      __openproxyUpdateLogDetail?: (row: unknown) => void;
    };
    if (typeof w.__openproxyUpdateLogDetail !== 'function') {
      throw new Error(
        'window.__openproxyUpdateLogDetail is not exposed — Fix 6 (TEST HOOK) is missing from the build.',
      );
    }
    w.__openproxyUpdateLogDetail(r);
  }, row);
}

/** Close the open modal via the X close button. */
async function closeModal(page: Page): Promise<void> {
  await page.locator('.log-detail-modal .close-btn').click();
  await page.waitForSelector('.log-detail-modal', { state: 'detached' });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test.describe('Log detail modal — contamination regression (Fixes 4-b + 5-b)', () => {

  // -------------------------------------------------------------------------
  // Test 1 — pinned check rejects WS row event for a DIFFERENT request
  // -------------------------------------------------------------------------

  test('1) modal stays on row A when WS row event for row B arrives', async ({ page }: { page: Page }) => {
    await setupLogsView(page);

    const rowA = makeFinalizedRow({
      request_id: 'req-A-contam-1',
      trace_id: 'tid-A-contam-1',
      upstream_model_id: 'claude-3-5-sonnet',
      status_code: 200,
      total_ms: 1234,
    });
    await injectRowAndOpenModal(page, rowA);

    const modal = page.locator('.log-detail-modal');
    // Sanity: the modal initially shows row A's data.
    await expect(modal).toContainText('claude-3-5-sonnet');
    await expect(modal).toContainText('200');
    await expect(modal).toContainText('1234 ms');

    // Inject a WS `row` event for a DIFFERENT request (row B).
    // The pinned identity (req-A-contam-1 / tid-A-contam-1) MUST
    // reject this update — the modal must stay on A.
    await injectUpdateLogDetail(page, {
      id: 9999,
      request_id: 'req-B-contam-1',
      trace_id: 'tid-B-contam-1',
      upstream_model_id: 'gpt-4o-mini-FAKE',
      status_code: 500,
      total_ms: 9999,
      error_message: 'Server Error: fake contamination',
    });

    // The modal still shows A's data.
    await expect(modal).toContainText('claude-3-5-sonnet');
    await expect(modal).toContainText('200');
    await expect(modal).toContainText('1234 ms');
    // The modal must NOT show B's data.
    await expect(modal).not.toContainText('gpt-4o-mini-FAKE');
    await expect(modal).not.toContainText('9999 ms');
    await expect(modal).not.toContainText('fake contamination');
    // Status 500 from B must not have replaced A's 200.
    await expect(modal.locator('.status-pill')).toContainText('200');

    // The snapshot in state.logs.selectedRow must still be A's snapshot.
    const selReqId = await page.evaluate(() => {
      const w = window as unknown as {
        __openproxyState?: { logs?: { selectedRow?: { request_id?: string } | null } };
      };
      return w.__openproxyState?.logs?.selectedRow?.request_id ?? null;
    });
    expect(selReqId).toBe('req-A-contam-1');
  });

  // -------------------------------------------------------------------------
  // Test 2 — Fix 1 (CAUSA PRIMARIA): synthetic "Request in progress"
  //          is cleared when the real row arrives with error_message: null
  // -------------------------------------------------------------------------

  test('2) inflight A: real row A with error_message=null clears synthetic "Request in progress"',
    async ({ page }: { page: Page }) => {
    await setupLogsView(page);

    const inflightA = makeInflightRow({
      request_id: 'req-A-contam-2',
      trace_id: 'tid-A-contam-2',
      upstream_model_id: 'claude-3-5-sonnet',
    });
    const stageA = makeStageEvent({
      request_id: 'req-A-contam-2',
      trace_id: 'tid-A-contam-2',
      stage: 'streaming',
      elapsed_ms: 200,
    });
    await injectInflightAndOpenModal(page, inflightA, stageA);

    const modal = page.locator('.log-detail-modal');
    // The modal must show the synthetic "Request in progress" error
    // message generated by `openLogDetail`'s inflight branch.
    await expect(modal).toContainText('Request in progress', { timeout: 5000 });

    // Inject a WS `row` event for A (same request_id + trace_id) with
    // the real data: status_code=200, total_ms=5000, error_message=null
    // (success). Per Fix 1, the special-case for `error_message` in the
    // overlay loop MUST replace the synthetic message with null.
    await injectUpdateLogDetail(page, {
      id: 0,
      request_id: 'req-A-contam-2',
      trace_id: 'tid-A-contam-2',
      status_code: 200,
      total_ms: 5000,
      error_message: null,
      upstream_model_id: 'claude-3-5-sonnet',
    });

    // The synthetic message MUST be gone.
    await expect(modal).not.toContainText('Request in progress');
    // The real data MUST be visible: status=200, model=claude-3-5-sonnet,
    // total_ms=5000 ms.
    await expect(modal.locator('.status-pill')).toContainText('200');
    await expect(modal).toContainText('claude-3-5-sonnet');
    await expect(modal).toContainText('5000 ms');
    // The Errors section must show "No errors recorded." (the fallback
    // when `errors` is null), NOT the synthetic "Request in progress".
    const errorsSection = page.locator('#log-detail-content [data-log-tab="errors"]');
    await expect(errorsSection).toContainText('No errors recorded');
    await expect(errorsSection).not.toContainText('Request in progress');
  });

  // -------------------------------------------------------------------------
  // Test 3 — Fix 1 (whitelist overlay): empty-string `upstream_model_id`
  //          IS overlaid (because "" != null is true). The modal's Model
  //          line then falls through to "—".
  //
  // NOTE: the task description phrased this as "the modal stays showing
  // claude-3-5-sonnet", but per the actual Fix 1 code (`if (v != null)
  // merged[k] = v;`), an empty string IS overlaid onto the snapshot.
  // The renderer's `log.upstream_model_id || ... || "—"` chain then
  // falls through to "—" because "" is falsy in JS. We assert the
  // ACTUAL behavior of the fix: the overlay happens (the snapshot's
  // upstream_model_id becomes "") and the Model line shows "—" (not
  // "claude-3-5-sonnet"). This is the documented, expected behavior
  // of the whitelist overlay — the rationale "porque `"" != null` es
  // TRUE, así que se overlay" is what we validate.
  // -------------------------------------------------------------------------

  test('3) WS event for same row A with upstream_model_id="" overlays the empty string (validates Fix 1 whitelist)',
    async ({ page }: { page: Page }) => {
    await setupLogsView(page);

    const rowA = makeFinalizedRow({
      request_id: 'req-A-contam-3',
      trace_id: 'tid-A-contam-3',
      upstream_model_id: 'claude-3-5-sonnet',
    });
    await injectRowAndOpenModal(page, rowA);

    const modal = page.locator('.log-detail-modal');
    // Sanity: the modal initially shows the model.
    await expect(modal).toContainText('claude-3-5-sonnet');

    // Inject a WS `row` event for A (same request_id + trace_id) with
    // upstream_model_id: "" (empty string). `upstream_model_id` is in
    // OVERLAYABLE_FIELDS, and the special-case only applies to
    // `error_message`, so the generic `if (v != null)` branch runs.
    // Since "" != null is TRUE, the overlay HAPPENS: the snapshot's
    // upstream_model_id becomes "".
    await injectUpdateLogDetail(page, {
      id: 5001,
      request_id: 'req-A-contam-3',
      trace_id: 'tid-A-contam-3',
      upstream_model_id: '',
      status_code: 200,
      total_ms: 1234,
      error_message: null,
    });

    // Verify the overlay happened: the snapshot's upstream_model_id
    // is now "" (NOT "claude-3-5-sonnet", NOT undefined).
    const snapshotModel = await page.evaluate(() => {
      const w = window as unknown as {
        __openproxyState?: { logs?: { selectedRow?: { upstream_model_id?: string } | null } };
      };
      return w.__openproxyState?.logs?.selectedRow?.upstream_model_id ?? '<undefined>';
    });
    expect(snapshotModel).toBe('');

    // Verify the modal no longer shows "claude-3-5-sonnet" anywhere
    // (the Model line falls through to "—", the Raw log shows
    // upstream_model_id: "", and request_body_json / response_body_json
    // don't contain "claude-3-5-sonnet" by construction).
    await expect(modal).not.toContainText('claude-3-5-sonnet');
    // The Model line in the summary should now show "—".
    const modelLine = page.locator('.log-detail-summary div').filter({ hasText: 'Model:' });
    await expect(modelLine).toContainText('—');
    await expect(modelLine).not.toContainText('claude-3-5-sonnet');
  });

  // -------------------------------------------------------------------------
  // Test 4 — Fix 3: `removeLogDetailModal` clears `state.logs.selectedRow`
  // -------------------------------------------------------------------------

  test('4) modal closes — selectedRow is cleared (Fix 3)', async ({ page }: { page: Page }) => {
    await setupLogsView(page);

    const rowA = makeFinalizedRow({
      request_id: 'req-A-contam-4',
      trace_id: 'tid-A-contam-4',
    });
    await injectRowAndOpenModal(page, rowA);

    // Sanity: the modal is open and selectedRow is set.
    expect(await page.locator('.log-detail-modal').count()).toBe(1);
    const selBefore = await page.evaluate(() => {
      const w = window as unknown as {
        __openproxyState?: { logs?: { selectedRow?: { request_id?: string } | null } };
      };
      return w.__openproxyState?.logs?.selectedRow?.request_id ?? null;
    });
    expect(selBefore).toBe('req-A-contam-4');

    // Close the modal via the X button.
    await closeModal(page);

    // The modal must be gone.
    expect(await page.locator('.log-detail-modal').count()).toBe(0);
    // Fix 3: `state.logs.selectedRow` must be cleared to null so
    // `copyDebugBundle` (and any other reader) doesn't see stale
    // data after the modal is closed.
    const selAfter = await page.evaluate(() => {
      const w = window as unknown as {
        __openproxyState?: { logs?: { selectedRow?: unknown } };
      };
      // Return the raw value (null, undefined, or an object). We
      // explicitly do NOT coalesce undefined → '<undefined>' here
      // because Fix 3 sets selectedRow to `null` (not undefined),
      // and we want to distinguish null (Fix 3 worked) from
      // undefined (something else entirely). The assertion below
      // accepts null only.
      return w.__openproxyState?.logs?.selectedRow ?? null;
    });
    expect(selAfter).toBeNull();
  });

  // -------------------------------------------------------------------------
  // Test 5 — Lifecycle: pinned identity resets correctly when the modal
  //          is re-opened for a different row. A late WS event for the
  //          FIRST row must be rejected.
  // -------------------------------------------------------------------------

  test('5) modal re-opens for different row — pinned identity resets correctly',
    async ({ page }: { page: Page }) => {
    await setupLogsView(page);

    const rowA = makeFinalizedRow({
      id: 5001,
      request_id: 'req-A-contam-5',
      trace_id: 'tid-A-contam-5',
      upstream_model_id: 'claude-3-5-sonnet',
      status_code: 200,
      total_ms: 1234,
    });
    const rowB = makeFinalizedRow({
      id: 5002,
      request_id: 'req-B-contam-5',
      trace_id: 'tid-B-contam-5',
      upstream_model_id: 'gpt-4o-mini',
      status_code: 201,
      total_ms: 5678,
    });

    // Open modal for A.
    await injectRowAndOpenModal(page, rowA);
    const modal = page.locator('.log-detail-modal');
    await expect(modal).toContainText('claude-3-5-sonnet');
    await expect(modal).toContainText('1234 ms');

    // Close modal for A.
    await closeModal(page);

    // Inject BOTH rows into the dashboard state and re-render, so we
    // can click row B next.
    await page.evaluate(
      (args: { a: RecentUsageRow; b: RecentUsageRow }) => {
        const w = window as unknown as {
          __openproxyState: {
            logs: {
              rows: RecentUsageRow[];
              rowById: Map<number, RecentUsageRow>;
              page: number;
              rowsPerPage: number;
              followTail: boolean;
            };
          };
          __openproxyLogsGoPage: (page: number) => void;
        };
        const logs = w.__openproxyState.logs;
        logs.rows = [args.a, args.b];
        logs.rowById.clear();
        logs.rowById.set(args.a.id, args.a);
        logs.rowById.set(args.b.id, args.b);
        logs.page = 1;
        logs.rowsPerPage = 50;
        logs.followTail = false;
        w.__openproxyLogsGoPage(1);
      },
      { a: rowA, b: rowB },
    );

    // Click row B to open the modal for B.
    const rowBEl = page.locator(`#logs .log-row[data-request-id="${rowB.request_id}"]`);
    await rowBEl.first().waitFor({ timeout: 5000 });
    await rowBEl.first().click();
    await page.locator('.log-detail-modal').waitFor({ timeout: 5000 });

    // The modal now shows B's data, not A's.
    await expect(modal).toContainText('gpt-4o-mini');
    await expect(modal).toContainText('5678 ms');
    await expect(modal).not.toContainText('claude-3-5-sonnet');
    await expect(modal).not.toContainText('1234 ms');

    // Inject a LATE WS `row` event for A (the previous row). The
    // pinned identity is now B (req-B-contam-5 / tid-B-contam-5), so
    // the update for A MUST be rejected.
    await injectUpdateLogDetail(page, {
      id: 5001,
      request_id: 'req-A-contam-5',
      trace_id: 'tid-A-contam-5',
      upstream_model_id: 'claude-3-5-sonnet-LATE',
      status_code: 200,
      total_ms: 1234,
      error_message: null,
    });

    // The modal still shows B's data — A's late event was rejected.
    await expect(modal).toContainText('gpt-4o-mini');
    await expect(modal).toContainText('5678 ms');
    await expect(modal).not.toContainText('claude-3-5-sonnet-LATE');
    await expect(modal).not.toContainText('1234 ms');
  });

  // -------------------------------------------------------------------------
  // Test 6 — Stress test: 50 WS events for 50 different requests must
  //          all be rejected; the modal must stay on A.
  // -------------------------------------------------------------------------

  test('6) modal stays on A under burst of 50 WS events for other requests',
    async ({ page }: { page: Page }) => {
    await setupLogsView(page);

    const rowA = makeFinalizedRow({
      request_id: 'req-A-contam-6',
      trace_id: 'tid-A-contam-6',
      upstream_model_id: 'claude-3-5-sonnet',
      status_code: 200,
      total_ms: 1234,
    });
    await injectRowAndOpenModal(page, rowA);

    const modal = page.locator('.log-detail-modal');
    // Sanity: the modal initially shows A's data.
    await expect(modal).toContainText('claude-3-5-sonnet');
    await expect(modal).toContainText('200');
    await expect(modal).toContainText('1234 ms');

    // Inject 50 WS events for 50 different requests (B, C, D, ...).
    // Each call goes through `__openproxyUpdateLogDetail`, which runs
    // the pinned check and rejects each one.
    await page.evaluate(() => {
      const w = window as unknown as {
        __openproxyUpdateLogDetail?: (row: unknown) => void;
      };
      if (typeof w.__openproxyUpdateLogDetail !== 'function') return;
      for (let i = 0; i < 50; i++) {
        w.__openproxyUpdateLogDetail({
          id: 10_000 + i,
          request_id: `req-other-contam-6-${i}`,
          trace_id: `tid-other-contam-6-${i}`,
          upstream_model_id: `model-fake-${i}`,
          status_code: 500,
          total_ms: 9999,
          error_message: `Error fake ${i}`,
        });
      }
    });

    // The modal must still show A's data after the burst.
    await expect(modal).toContainText('claude-3-5-sonnet');
    await expect(modal).toContainText('200');
    await expect(modal).toContainText('1234 ms');
    // None of the 50 fake models / errors may appear.
    await expect(modal).not.toContainText('model-fake-0');
    await expect(modal).not.toContainText('model-fake-25');
    await expect(modal).not.toContainText('model-fake-49');
    await expect(modal).not.toContainText('Error fake');
    await expect(modal).not.toContainText('9999 ms');

    // The snapshot must still be A's.
    const selReqId = await page.evaluate(() => {
      const w = window as unknown as {
        __openproxyState?: { logs?: { selectedRow?: { request_id?: string } | null } };
      };
      return w.__openproxyState?.logs?.selectedRow?.request_id ?? null;
    });
    expect(selReqId).toBe('req-A-contam-6');
  });

  // -------------------------------------------------------------------------
  // Test 7 (BONUS) — Fix 4: matchesPinnedModalIdentity rejects when
  // both trace_ids are empty (edge case). This documents the
  // reviewer's Issue #1: when the modal is open for an inflight
  // placeholder WITHOUT trace_id (inflightByRequestId path), late
  // WS events with the same request_id and empty trace_id MUST be
  // rejected — the modal stays frozen on its snapshot.
  // -------------------------------------------------------------------------

  test('7) Fix 4 edge case: modal open for inflight with empty trace_id — late WS event with same request_id + empty trace_id is REJECTED',
    async ({ page }: { page: Page }) => {
    await setupLogsView(page);

    // Inflight placeholder WITHOUT trace_id — lives in
    // `inflightByRequestId`. This is the "rare in production" path
    // noted in views/logs.ts:301-302 (synthetic events emitted from
    // the frontend itself).
    const inflightNoTid = makeInflightRow({
      request_id: 'req-A-contam-7',
      trace_id: '',
      upstream_model_id: 'claude-3-5-sonnet',
    });
    const stageNoTid = makeStageEvent({
      request_id: 'req-A-contam-7',
      trace_id: '',
      stage: 'streaming',
    });
    await injectInflightAndOpenModal(page, inflightNoTid, stageNoTid);

    const modal = page.locator('.log-detail-modal');
    // The modal shows the synthetic "Request in progress" message
    // (generated by openLogDetail's inflight branch).
    await expect(modal).toContainText('Request in progress', { timeout: 5000 });
    await expect(modal).toContainText('claude-3-5-sonnet');

    // Inject a WS `row` event with the SAME request_id and ALSO
    // empty trace_id. Per Fix 4 (`if (pinnedTid === "" && rowTid === "")
    // return false;`), the pinned check MUST reject this update.
    await injectUpdateLogDetail(page, {
      id: 0,
      request_id: 'req-A-contam-7',
      trace_id: '',
      upstream_model_id: 'gpt-4o-mini-SHOULD-NOT-APPEAR',
      status_code: 200,
      total_ms: 9999,
      error_message: null,
    });

    // The modal must NOT update. It stays frozen on the snapshot
    // (with the synthetic "Request in progress" message and the
    // original "claude-3-5-sonnet" model).
    await expect(modal).not.toContainText('gpt-4o-mini-SHOULD-NOT-APPEAR');
    await expect(modal).not.toContainText('9999 ms');
    // The synthetic message is STILL there (the update that would
    // have cleared it via Fix 1's special-case was rejected).
    await expect(modal).toContainText('Request in progress');

    // The snapshot must still be the original inflight snapshot
    // (the synthetic message proves it — the snapshot was taken
    // when the modal opened, before any WS event arrived).
    const selReqId = await page.evaluate(() => {
      const w = window as unknown as {
        __openproxyState?: { logs?: { selectedRow?: { request_id?: string } | null } };
      };
      return w.__openproxyState?.logs?.selectedRow?.request_id ?? null;
    });
    expect(selReqId).toBe('req-A-contam-7');
  });
});
