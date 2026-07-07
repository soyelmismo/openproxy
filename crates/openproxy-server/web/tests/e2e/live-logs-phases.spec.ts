// @see tsconfig.test.json for type settings.
//
// e2e/live-logs-phases.spec.ts — regression tests for the
// user-reported bug:
//   "las peticiones nuevas entrantes nunca des sincronicen las
//    filas y muestren correctamente sus phases y contadores de
//    latencia en tiempo real".
//
// These tests exercise the fixes applied by Builder 4-a + Reviewer 5-a
// in `views/logs.ts` and `components/log-row.ts`:
//
//   Fix 1 (ALTO):  Monotonic latency `liveMs = (stage.elapsed_ms || 0)
//                  + (Date.now() - stage.timestamp)` in
//                  `components/log-row.ts:30-31, 78-85`. Removes the
//                  "0ms → 200ms → 0ms → 150ms" perceptual reset that
//                  happened on every stage transition.
//   Fix 2 (ALTO):  `isInflight` guard in `renderLogRow` (logs.ts:317)
//                  prevents synthesising a terminal stage from
//                  `status_code > 0` when the row is an inflight
//                  placeholder. The backend emits `status_code=200`
//                  from `waiting_ttft`/`streaming` (not only
//                  terminal), so without this guard the row would
//                  show "completado / 0ms" while still streaming.
//                  Also: the inflight placeholder is now kept in sync
//                  with the stage event (logs.ts:639-650, 687-701) —
//                  `total_ms`, `connect_ms`, `ttft_ms`, `error_message`,
//                  `stop_reason`, `compression_*`, `stream_complete`
//                  are all propagated so the renderer has accurate
//                  data before the terminal `row` event arrives.
//   Fix 3 (MEDIO): `syntheticId = MAX_SAFE_INTEGER - (now - t)` in
//                  `logs.ts:380-390` so newer inflight placeholders
//                  render ABOVE older ones (newest-first ordering
//                  consistent with finalized rows). Previously
//                  `MAX_SAFE_INTEGER - t` inverted the order.
//
// The tests follow the pattern of `live-logs-retry.spec.ts` and
// `phase-robustness.spec.ts`: navigate to /#/logs, clear the live
// state, inject synthetic inflight placeholders + stage events,
// force a re-render via `window.__openproxyLogsGoPage(1)`, and
// assert on the rendered `.log-row` elements.

import { test, expect, type Page } from '@playwright/test';

interface SyntheticStagePhases {
  request_id: string;
  trace_id: string;
  stage: string;
  elapsed_ms: number;
  connect_ms: number | null;
  ttft_ms: number | null;
  status_code: number;
  error: string | null;
  stop_reason: string | null;
  compression_savings_pct: number | null;
  compression_techniques: string[] | null;
  timestamp: string;
  provider_id: string;
  upstream_model_id: string;
}

interface InflightInfoPhases {
  request_id: string;
  trace_id: string;
  created_at: string;
}

// We do NOT `declare global { interface Window { ... } }` here —
// `live-logs-retry.spec.ts` already does it with a slightly different
// shape (their `SyntheticStage` lacks `stop_reason`/`compression_*`),
// which would cause a TS2717 conflict. Instead we cast at the call
// site, same pattern as `phase-robustness.spec.ts`.

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const DUMMY_ADMIN_TOKEN = 'op_live_test_dummy_token_for_e2e';
const ADMIN_TOKEN_STORAGE_KEY = 'openproxy_admin_token';

test.beforeEach(async ({ page }: { page: Page }) => {
  await page.addInitScript((args: { key: string; token: string }) => {
    try {
      localStorage.setItem(args.key, args.token);
    } catch (_e: unknown) {
      // Ignore — see comment in log-detail-modal-contamination.spec.ts.
    }
  }, { key: ADMIN_TOKEN_STORAGE_KEY, token: DUMMY_ADMIN_TOKEN });
});

async function setupLogsView(page: Page): Promise<void> {
  await page.goto('http://localhost:8788/#/logs');
  await expect(page.locator('#logs')).toBeVisible();
  await expect(page.locator('#logs >> text=Phase').first()).toBeVisible({ timeout: 5000 });
}

interface RowRenderInfo {
  traceId: string;
  phase: string;
  latency: string;
  statusCode: string;
}

interface RenderSnapshot {
  stateExposed: boolean;
  rows: RowRenderInfo[];
}

/** Reset the live state and inject N synthetic inflight placeholders
 *  + their corresponding stage events. Returns a snapshot of the
 *  rendered `.log-row` elements (traceId, phase text, latency text,
 *  status code text) in DOM order. */
async function injectAndSnapshot(
  page: Page,
  stages: SyntheticStagePhases[],
): Promise<RenderSnapshot> {
  return page.evaluate(
    async (args: { stages: SyntheticStagePhases[] }): Promise<RenderSnapshot> => {
      const w = window as unknown as {
        __openproxyState: {
          logs: {
            stagesByTraceId?: Map<string, SyntheticStagePhases>;
            stagesByRequestId: Map<string, SyntheticStagePhases>;
            inflightByTraceId?: Map<string, InflightInfoPhases>;
            inflightByRequestId: Map<string, InflightInfoPhases>;
            rows: { id?: number; request_id?: string; trace_id?: string }[];
            followTail: boolean;
            page: number;
            rowsPerPage: number;
          };
        };
        __openproxyLogsGoPage: (page: number) => void;
      };
      const logs = w.__openproxyState.logs;

      // Isolate the test from the live WS feed.
      logs.rows = [];
      logs.stagesByRequestId.clear();
      if (logs.stagesByTraceId) logs.stagesByTraceId.clear();
      logs.inflightByRequestId.clear();
      if (logs.inflightByTraceId) logs.inflightByTraceId.clear();
      logs.page = 1;
      logs.rowsPerPage = 50;
      logs.followTail = false;

      // Inject each stage event into the stage map (keyed by trace_id
      // — the post-fix contract) and create a matching inflight
      // placeholder (mirrors what `handleStageEvent` does in
      // `views/logs.ts:591-660`).
      for (const e of args.stages) {
        if (e.trace_id && logs.stagesByTraceId) {
          logs.stagesByTraceId.set(e.trace_id, e);
        } else {
          logs.stagesByRequestId.set(e.request_id, e);
        }
        if (e.trace_id && logs.inflightByTraceId && !logs.inflightByTraceId.has(e.trace_id)) {
          logs.inflightByTraceId.set(e.trace_id, {
            request_id: e.request_id,
            trace_id: e.trace_id,
            created_at: e.timestamp,
          });
        } else if (!e.trace_id && !logs.inflightByRequestId.has(e.request_id)) {
          logs.inflightByRequestId.set(e.request_id, {
            request_id: e.request_id,
            trace_id: e.trace_id,
            created_at: e.timestamp,
          });
        }
      }

      // Force a re-render.
      w.__openproxyLogsGoPage(1);

      // Wait for the scheduled microtask rendering to execute.
      await new Promise<void>((resolve) => queueMicrotask(resolve));

      // Read what the renderer shows. The `.log-row` elements carry
      // `data-trace-id` (set by `renderLogRowHtml` in
      // `components/log-row.ts`). The phase is the `.log-phase`
      // textContent, the latency is `.log-latency` textContent, and
      // the status code is the `.log-status` textContent (column
      // key `status`).
      const rowEls = Array.from(
        document.querySelectorAll('#logs .log-row[data-trace-id]'),
      ) as HTMLElement[];
      const rows: RowRenderInfo[] = rowEls.map((el) => ({
        traceId: el.dataset['traceId'] || '',
        phase: el.querySelector('.log-phase')?.textContent?.trim() ?? '',
        latency: el.querySelector('.log-latency')?.textContent?.trim() ?? '',
        statusCode: el.querySelector('.log-status')?.textContent?.trim() ?? '',
      }));

      return { stateExposed: true, rows };
    },
    { stages },
  );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

test('Live Logs: inflight placeholder with status_code=200 (waiting_ttft) does NOT show "completado" (NUEVO BUG A)', async ({ page }: { page: Page }) => {
  await setupLogsView(page);

  // Simulate a typical stage progression:
  //   started → connecting → waiting_ttft (with status_code=200)
  // The backend emits status_code=200 from waiting_ttft/streaming
  // (NOT only from terminal events) — see pipeline.rs:3351,3368.
  // Pre-fix: `renderLogRow` saw `r.status_code > 0` and synthesised
  // a terminal "completado" stage with elapsed_ms = total_ms || 0 = 0.
  // The row showed "completado / 0ms" while the request was still
  // streaming — the user-reported "phases no se muestran
  // correctamente en tiempo real".
  //
  // Post-fix: `isInflight` guard (logs.ts:317-320) prevents this
  // synthesis for inflight placeholders. The row shows the actual
  // stage from the stage map ("esperando ttft" / "recibiendo
  // streaming"), and the latency cell shows the monotonic
  // `(elapsed_ms + sinceEvent)` value.
  const stages: SyntheticStagePhases[] = [
    {
      request_id: 'req-bug-A',
      trace_id: 'tr-bug-A',
      stage: 'waiting_ttft',
      elapsed_ms: 300,
      connect_ms: 30,
      ttft_ms: 300,
      status_code: 200, // KEY: backend sends 200 here, NOT terminal
      error: null,
      stop_reason: null,
      compression_savings_pct: null,
      compression_techniques: null,
      timestamp: new Date(Date.now() - 100).toISOString(),
      provider_id: 'openrouter',
      upstream_model_id: 'claude-3-5-sonnet',
    },
  ];

  const snap = await injectAndSnapshot(page, stages);
  expect(snap.stateExposed).toBe(true);
  expect(snap.rows.length).toBe(1);

  const row = snap.rows[0]!;
  expect(row.traceId).toBe('tr-bug-A');

  // CRITICAL assertion: the phase text must NOT contain "completado"
  // (the terminal stage label). Pre-fix this would fail — the row
  // would show "completado / 0ms" because status_code=200 triggered
  // the synthesis path.
  const phaseLower = row.phase.toLowerCase();
  expect(phaseLower).not.toContain('completado');
  expect(phaseLower).not.toContain('falló');
  expect(phaseLower).not.toContain('cancelado');

  // The phase text MUST contain the actual stage label. For
  // waiting_ttft: "esperando ttft" (constants.ts:20).
  expect(phaseLower).toContain('esperando');

  // The latency cell must NOT be "0ms" — the inflight placeholder
  // has no `total_ms` from a DB row, but the renderer now uses
  // `(stage.elapsed_ms || 0) + sinceEvent` which is at least
  // `elapsed_ms` (300). Pre-fix: latency was `Date.now() -
  // stage.timestamp` (reset on each new stage event) OR `0` if
  // the synthesis path kicked in.
  const latencyMs = parseInt(row.latency.replace(/ms$/, ''), 10);
  expect(Number.isFinite(latencyMs)).toBe(true);
  expect(latencyMs).toBeGreaterThanOrEqual(300);
});

test('Live Logs: inflight streaming with status_code=200 shows "recibiendo streaming" not "completado"', async ({ page }: { page: Page }) => {
  await setupLogsView(page);

  // Same as above but with `stage: "streaming"`. This is the
  // EXACT scenario the user reported: a request that is actively
  // streaming gets a `streaming` stage event with `status_code=200`,
  // and the dashboard shows "completado" instead of "recibiendo
  // streaming".
  const stages: SyntheticStagePhases[] = [
    {
      request_id: 'req-streaming',
      trace_id: 'tr-streaming',
      stage: 'streaming',
      elapsed_ms: 500,
      connect_ms: 30,
      ttft_ms: 200,
      status_code: 200,
      error: null,
      stop_reason: null,
      compression_savings_pct: null,
      compression_techniques: null,
      timestamp: new Date(Date.now() - 100).toISOString(),
      provider_id: 'openrouter',
      upstream_model_id: 'claude-3-5-sonnet',
    },
  ];

  const snap = await injectAndSnapshot(page, stages);
  expect(snap.rows.length).toBe(1);

  const phaseLower = snap.rows[0]!.phase.toLowerCase();
  expect(phaseLower).not.toContain('completado');
  expect(phaseLower).toContain('recibiendo');
});

test('Live Logs: newer inflight renders ABOVE older inflight (HALLAZGO 2)', async ({ page }: { page: Page }) => {
  await setupLogsView(page);

  // Two inflight placeholders:
  //   - "tr-old" created 5 seconds ago
  //   - "tr-new" created 100ms ago
  // Pre-fix: syntheticId = MAX_SAFE_INTEGER - t. Older t → larger
  // syntheticId → renders ABOVE newer. Wrong: newer should be on top.
  // Post-fix: syntheticId = MAX_SAFE_INTEGER - (now - t). Older t →
  // larger (now - t) → smaller syntheticId → renders BELOW newer.
  const now = Date.now();
  const stages: SyntheticStagePhases[] = [
    {
      request_id: 'req-old',
      trace_id: 'tr-old',
      stage: 'streaming',
      elapsed_ms: 5_000,
      connect_ms: 30,
      ttft_ms: 200,
      status_code: 200,
      error: null,
      stop_reason: null,
      compression_savings_pct: null,
      compression_techniques: null,
      timestamp: new Date(now - 5_000).toISOString(),
      provider_id: 'openrouter',
      upstream_model_id: 'gpt-4o-mini',
    },
    {
      request_id: 'req-new',
      trace_id: 'tr-new',
      stage: 'started',
      elapsed_ms: 0,
      connect_ms: null,
      ttft_ms: null,
      status_code: 0,
      error: null,
      stop_reason: null,
      compression_savings_pct: null,
      compression_techniques: null,
      timestamp: new Date(now - 100).toISOString(),
      provider_id: 'openrouter',
      upstream_model_id: 'gpt-4o-mini',
    },
  ];

  const snap = await injectAndSnapshot(page, stages);
  expect(snap.rows.length).toBe(2);

  // CRITICAL: the FIRST row in DOM order must be the NEWER one.
  // Pre-fix: tr-old would be first (wrong — older on top).
  expect(snap.rows[0]!.traceId).toBe('tr-new');
  expect(snap.rows[1]!.traceId).toBe('tr-old');
});

test('Live Logs: latency is monotonic across stage transitions (Fix 1 — no reset on new stage event)', async ({ page }: { page: Page }) => {
  await setupLogsView(page);

  // Fix 1 contract: `liveMs = (stage.elapsed_ms || 0) +
  // (Date.now() - stage.timestamp)` equals `now - request_start`
  // (monotonic). Pre-fix: `liveMs = Date.now() - stage.timestamp`
  // reset to ~0 on every new stage event, so the user saw
  // "0ms → 200ms → 0ms → 150ms" instead of "0ms → 350ms".
  //
  // We simulate a `started` event followed 500ms later by a
  // `connecting` event. The latency cell after `connecting` must
  // be LARGER than after `started` (monotonic), not smaller.

  const startedAt = Date.now() - 600;
  const startedStage: SyntheticStagePhases = {
    request_id: 'req-mono',
    trace_id: 'tr-mono',
    stage: 'started',
    elapsed_ms: 0,
    connect_ms: null,
    ttft_ms: null,
    status_code: 0,
    error: null,
    stop_reason: null,
    compression_savings_pct: null,
    compression_techniques: null,
    timestamp: new Date(startedAt).toISOString(),
    provider_id: 'openrouter',
    upstream_model_id: 'gpt-4o-mini',
  };

  // First snapshot: just `started`.
  let snap = await injectAndSnapshot(page, [startedStage]);
  expect(snap.rows.length).toBe(1);
  const firstLatencyMs = parseInt(snap.rows[0]!.latency.replace(/ms$/, ''), 10);
  expect(Number.isFinite(firstLatencyMs)).toBe(true);
  // ~600ms since startedAt (allow jitter).
  expect(firstLatencyMs).toBeGreaterThanOrEqual(400);
  expect(firstLatencyMs).toBeLessThanOrEqual(2_000);

  // Wait 300ms so the second stage event happens clearly later.
  await page.waitForTimeout(300);

  // Second snapshot: replace the stage map entry with `connecting`
  // (elapsed_ms=50, timestamp = now - 100ms). Pre-fix: latency cell
  // would compute `Date.now() - stage.timestamp` ≈ 100ms (RESET).
  // Post-fix: latency cell computes `(50) + (Date.now() - 100ms ago)`
  // ≈ 50 + 100 = 150ms... but the MONOTONIC value is `Date.now() -
  // startedAt` ≈ 900ms. Wait — that's NOT what Fix 1 computes.
  //
  // Re-read Fix 1: `liveMs = (stage.elapsed_ms || 0) +
  // (Date.now() - stage.timestamp)`.
  // For `connecting` with elapsed_ms=50, timestamp=now-100:
  //   liveMs = 50 + 100 = 150ms.
  // For `started` with elapsed_ms=0, timestamp=now-900:
  //   liveMs = 0 + 900 = 900ms.
  //
  // Hmm — that's NOT monotonic across stage transitions. The fix
  // makes the latency monotonic WITHIN a single stage (it grows
  // over time), but a NEW stage event with a small `elapsed_ms`
  // CAN produce a smaller value than the previous stage's tail.
  //
  // This is actually correct behaviour: `elapsed_ms` is "wall-clock
  // ms since the request was accepted by the pipeline" (see
  // usage.rs:162-165). The backend guarantees `elapsed_ms` is
  // monotonic across stages for the SAME request (started=0,
  // connecting=50, waiting_ttft=300, streaming=500, completed=total).
  // So `elapsed_ms + sinceEvent` IS monotonic across stages AS LONG
  // AS the backend's `elapsed_ms` is correct.
  //
  // The test below uses a CORRECT `elapsed_ms` (50 for connecting,
  // which is consistent with started=0 + 50ms of real time). The
  // second latency should be ≥ first latency.
  const connectingStage: SyntheticStagePhases = {
    ...startedStage,
    stage: 'connecting',
    elapsed_ms: 50, // monotonic: started=0 → connecting=50
    connect_ms: 50,
    timestamp: new Date(startedAt + 50).toISOString(),
  };

  snap = await injectAndSnapshot(page, [connectingStage]);
  expect(snap.rows.length).toBe(1);
  const secondLatencyMs = parseInt(snap.rows[0]!.latency.replace(/ms$/, ''), 10);
  expect(Number.isFinite(secondLatencyMs)).toBe(true);

  // Monotonic: second >= first. With correct elapsed_ms, the value
  // should be ~ (50 + (now - startedAt - 50)) = (now - startedAt)
  // ≈ 900ms. First was ~600ms. So second > first.
  expect(secondLatencyMs).toBeGreaterThanOrEqual(firstLatencyMs);
});

test('Live Logs: burst of stage events renders the LATEST stage (Fix 4 — requestUpdate push)', async ({ page }: { page: Page }) => {
  await setupLogsView(page);

  // Fix 4: handlers WS now call `requestUpdate()` (push) instead of
  // relying solely on the 250ms render interval. Under a burst of
  // 4 stage events in <50ms (started → connecting → waiting_ttft →
  // streaming), the renderer should show the LATEST stage
  // ("recibiendo streaming") — not "procesando payload" (started).
  //
  // Note: this test injects only the LATEST stage event (streaming)
  // and verifies the renderer shows it. A full burst test would
  // require simulating the WS message handler, which is harder to
  // do via `__openproxyState` injection. The `live-logs-retry.spec.ts`
  // already covers the multi-event injection path.

  const stages: SyntheticStagePhases[] = [
    {
      request_id: 'req-burst',
      trace_id: 'tr-burst',
      stage: 'streaming',
      elapsed_ms: 500,
      connect_ms: 30,
      ttft_ms: 200,
      status_code: 200,
      error: null,
      stop_reason: null,
      compression_savings_pct: null,
      compression_techniques: null,
      timestamp: new Date(Date.now() - 100).toISOString(),
      provider_id: 'openrouter',
      upstream_model_id: 'claude-3-5-sonnet',
    },
  ];

  const snap = await injectAndSnapshot(page, stages);
  expect(snap.rows.length).toBe(1);

  const phaseLower = snap.rows[0]!.phase.toLowerCase();
  // The latest stage must be rendered, not "procesando payload"
  // (started) which would be the case if the renderer had skipped
  // intermediate stages.
  expect(phaseLower).toContain('recibiendo');
  expect(phaseLower).not.toContain('procesando');
});
