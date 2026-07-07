// @see tsconfig.test.json for type settings.
//
// Phase-robustness e2e — §5.4 of
// `.hermes/phase-robustness-spec.md`.
//
// Regression test for the user-reported "el reason stop del upstream
// no se está tomando en cuenta" bug, where the live-logs dashboard
// kept showing a non-terminal phase (`waiting_ttft` / `streaming`)
// and the latency counter ran forever after a successful upstream
// request.
//
// The fix is split across two layers:
//
//   1. The backend (Rust) is the source of truth — it now publishes
//      a terminal `StageEvent` (`completed` / `failed`) before
//      `cost::record` returns, so a properly-running dashboard
//      freezes the counter within one tick.
//
//   2. The frontend (TypeScript) is defense-in-depth — §4.1 of
//      the spec. The ticker (a) freezes on a `streaming` event
//      older than 2 s with no follow-up, and (b) freezes on a
//      `RecentUsageRow` that the row envelope delivered for the
//      same `(request_id, trace_id)`. The §4.2 path additionally
//      synthesizes a terminal `StageEvent` from a finalized row
//      when the backend's terminal event was lost in transit.
//
// This spec exercises BOTH the §4.1 ticker freeze and the §4.2
// synthetic-event gate. We drive the dashboard via the
// `window.__openproxyState` and `window.__openproxyLogsGoPage`
// hooks declared in `app.ts` and used by the existing
// `live-logs-retry.spec.ts` for the same purpose.

import { test, expect, type Page } from '@playwright/test';
import type { StageEvent, RecentUsageRow } from '../../src/static/src/lib/types/api.js';

// NOTE: we deliberately do NOT redeclare `Window.__openproxyState`
// here. The `live-logs-retry.spec.ts` spec already declares it
// with a permissive shape (synthetic stage + inflight info) that
// the dashboard's actual `state.logs` type fits inside. The TS
// "subsequent property declaration must have the same type" rule
// means a second `declare global { interface Window { ... } }`
// in this file would conflict with the existing one. Instead we
// cast at the call site and re-use the imported `StageEvent` /
// `RecentUsageRow` types for the synthetic-event payloads we
// inject (the runtime is duck-typed; the type assertion only
// matters to the typechecker).

interface FreezeObservation {
  stateExposed: boolean;
  rowFound: boolean;
  firstLatency: string | null;
  secondLatency: string | null;
  sublabelText: string | null;
  phaseText: string | null;
  tickingClassPresent: boolean;
  stageInMap: string | null;
  totalMsInRow: number | null;
}

async function readFreezeObservation(
  page: Page,
  requestId: string,
  traceId: string,
  settleMs: number,
): Promise<FreezeObservation> {
  return page.evaluate(
    (args: { requestId: string; traceId: string; settleMs: number }): Promise<FreezeObservation> => {
      return new Promise((resolve) => {
        // Same local cast as the `page.evaluate` injects. The
        // `__openproxyState` window hook is declared in
        // `live-logs-retry.spec.ts` — we re-cast here so the
        // call site stays strict without redeclaring the
        // global Window type (which would conflict with the
        // declaration in the other spec file).
        interface SyntheticStage {
          request_id: string;
          trace_id: string;
          stage: string;
          elapsed_ms: number;
          connect_ms: number | null;
          ttft_ms: number | null;
          status_code: number;
          error: string | null;
          timestamp: string;
          provider_id: string;
          upstream_model_id: string;
        }
        const w = window as unknown as {
          __openproxyState: {
            logs: {
              stagesByTraceId?: Map<string, SyntheticStage>;
              stagesByRequestId: Map<string, SyntheticStage>;
              inflightByTraceId?: Map<string, RecentUsageRow>;
              inflightByRequestId: Map<string, RecentUsageRow>;
              rows: RecentUsageRow[];
              followTail: boolean;
              page: number;
              rowsPerPage: number;
              rowById: Map<number, RecentUsageRow>;
            };
          };
        };
        const logs = w.__openproxyState.logs;

        // First read of the latency text.
        const firstRead = (): { latency: string | null; sublabel: string | null; phase: string | null; tickingClass: boolean; rowFound: boolean } => {
          const rowEl = document.querySelector(
            `#logs .log-row[data-request-id="${args.requestId}"][data-trace-id="${args.traceId}"]`,
          ) as HTMLElement | null;
          if (!rowEl) {
            return { latency: null, sublabel: null, phase: null, tickingClass: false, rowFound: false };
          }
          const latencyEl = rowEl.querySelector('.log-latency');
          const subEl = rowEl.querySelector('.log-phase-sub');
          const phaseEl = rowEl.querySelector('.log-phase');
          return {
            latency: latencyEl?.textContent?.trim() ?? null,
            sublabel: subEl?.textContent?.trim() ?? null,
            phase: phaseEl?.textContent?.trim() ?? null,
            tickingClass: !!(subEl && subEl.classList.contains('log-phase-sub--ticking')),
            rowFound: true,
          };
        };

        const first = firstRead();

        // Wait `settleMs` (default 500) and re-read. If the ticker
        // is frozen, the two reads must be identical.
        setTimeout(() => {
          const second = firstRead();
          const stageMap: Map<string, SyntheticStage> | undefined = logs.stagesByTraceId;
          const stageReqMap: Map<string, SyntheticStage> = logs.stagesByRequestId;
          const stage: SyntheticStage | undefined = stageMap?.get(args.traceId)
            ?? stageReqMap.get(args.requestId);
          const row: RecentUsageRow | undefined = (logs.rows as RecentUsageRow[]).find(
            (r: RecentUsageRow) => r.request_id === args.requestId && r.trace_id === args.traceId,
          );
          resolve({
            stateExposed: true,
            rowFound: first.rowFound,
            firstLatency: first.latency,
            secondLatency: second.latency,
            sublabelText: second.sublabel,
            phaseText: second.phase,
            tickingClassPresent: second.tickingClass,
            stageInMap: stage?.stage ?? null,
            totalMsInRow: row?.total_ms ?? null,
          });
        }, args.settleMs);
      });
    },
    { requestId, traceId, settleMs },
  );
}

test.beforeEach(async ({ page }: { page: Page }) => {
  // Surface any page-side error in the test log so a 500/404 from
  // the dev server doesn't masquerade as a "ticker didn't freeze"
  // failure.
  page.on('pageerror', (e: Error) => {
    // eslint-disable-next-line no-console
    console.error('[phase-robustness] pageerror:', e.message);
  });
});

test('Live Logs: stale streaming stage freezes the latency ticker', async ({ page }: { page: Page }) => {
  await page.goto('http://localhost:8790/#/logs');
  await expect(page.locator('#logs')).toBeVisible();
  // Wait for the view to be fully mounted.
  await expect(page.locator('#logs >> text=Phase').first()).toBeVisible({ timeout: 5000 });

  const requestId = 'req-stale-test-1';
  const traceId = 'tr-stale-1';

  // 5 seconds ago — well past the 2 s stale cap from §4.1.
  const fiveSecondsAgo = new Date(Date.now() - 5_000).toISOString();
  const streamingEvent: StageEvent = {
    request_id: requestId,
    trace_id: traceId,
    stage: 'streaming',
    elapsed_ms: 5_000,
    connect_ms: 30,
    ttft_ms: 120,
    status_code: 200,
    error: null,
    timestamp: fiveSecondsAgo,
    provider_id: 'openrouter',
    upstream_model_id: 'gpt-4o-mini',
    stop_reason: null,
    compression_savings_pct: null,
    compression_techniques: null,
  };

  // Inject the streaming event and a matching row, then trigger
  // a re-render via the page-changer hook (same pattern as
  // `live-logs-retry.spec.ts`).
  await page.evaluate(
    (args: { event: StageEvent; requestId: string; traceId: string }) => {
      // The `__openproxyState` type is declared in
      // `live-logs-retry.spec.ts`. We re-cast locally with the
      // shape we actually need to keep the call site strict
      // (the imported `StageEvent` / `RecentUsageRow` types
      // are the source of truth for the dashboard's state).
      interface SyntheticStage {
        request_id: string;
        trace_id: string;
        stage: string;
        elapsed_ms: number;
        connect_ms: number | null;
        ttft_ms: number | null;
        status_code: number;
        error: string | null;
        timestamp: string;
        provider_id: string;
        upstream_model_id: string;
      }
      const w = window as unknown as {
        __openproxyState: {
          logs: {
            stagesByTraceId?: Map<string, SyntheticStage>;
            stagesByRequestId: Map<string, SyntheticStage>;
            inflightByTraceId?: Map<string, RecentUsageRow>;
            inflightByRequestId: Map<string, RecentUsageRow>;
            rows: RecentUsageRow[];
            followTail: boolean;
            page: number;
            rowsPerPage: number;
            rowById: Map<number, RecentUsageRow>;
          };
        };
        __openproxyLogsGoPage: (page: number) => void;
      };
      const logs = w.__openproxyState.logs;

      // Isolate the test: the live dashboard may have other
      // rows/stages streaming in from the WS feed.
      logs.rows = [];
      logs.stagesByTraceId?.clear();
      logs.stagesByRequestId.clear();
      logs.inflightByTraceId?.clear();
      logs.inflightByRequestId.clear();
      logs.page = 1;
      logs.rowsPerPage = 50;
      logs.followTail = false;

      // Seed an inflight placeholder so the row is rendered even
      // though no `row` envelope has arrived yet — the §4.1
      // "stale `streaming`" path operates on the in-flight row
      // without a finalized row.
      logs.inflightByTraceId?.set(args.traceId, {
        id: 0,
        request_id: args.requestId,
        trace_id: args.traceId,
        provider_id: 'openrouter',
        upstream_model_id: 'gpt-4o-mini',
        created_at: new Date().toISOString(),
        status_code: 0,
        total_ms: 0,
        prompt_tokens: null,
        completion_tokens: null,
        cost_usd: 0,
        is_streaming: true,
        stream_complete: false,
        race_lost: false,
        connect_ms: 30,
        ttft_ms: 120,
        request_body_json: null,
        response_body_json: null,
        request_headers: null,
        response_headers: null,
        race_total: null,
        race_attempts: null,
        error_message: null,
        stop_reason: null,
        compression_savings_pct: null,
        compression_techniques: null,
        client_response: false,
        prompt_tokens_estimated: false,
        completion_tokens_estimated: false,
      });
      logs.stagesByTraceId?.set(args.traceId, args.event);

      w.__openproxyLogsGoPage(1);
    },
    { event: streamingEvent, requestId, traceId },
  );

  // Give the renderer at least one microtask to settle.
  // (The 100ms latency ticker in `state/ticker.ts` is disabled —
  // see `views/logs.ts:1351` — so the only path that mutates the
  // latency cell is the `renderLogRowHtml` template, evaluated on
  // each `requestUpdate()` cycle.)
  await page.waitForTimeout(150);

  // Take a reading, wait 500 ms, take another. With Fix 1
  // (monotonic latency in `components/log-row.ts:78-85`), the
  // latency cell computes `live = stage.elapsed_ms + (now - stage.timestamp)`
  // which equals `now - request_start` (monotonically growing).
  // Previously the latency cell was frozen by §4.1's stale-cap
  // path; Fix 1 removed that cap from the latency cell (the cap
  // is now only applied to the `.log-phase-sub` sublabel, see
  // `renderLogPhaseHtml` line 34-35). The two reads must therefore
  // DIFFER — the second read must be ~500 ms higher than the first.
  const obs = await readFreezeObservation(page, requestId, traceId, 500);

  expect(obs.stateExposed).toBe(true);
  expect(obs.rowFound).toBe(true);
  // Sanity: the stage IS in the map (this is what feeds the
  // renderer) and the row is NOT finalized (no `total_ms`).
  expect(obs.stageInMap).toBe('streaming');
  expect(obs.totalMsInRow).toBeNull();
  // Fix 1 contract: the latency cell shows
  //   `(stage.elapsed_ms || 0) + (now - stage.timestamp)`
  // which is monotonic. Both reads must be non-null, the second
  // must be strictly greater than the first (the 500 ms wait
  // between reads guarantees ~500 ms of growth), and both must
  // be in the expected range for the synthetic event
  // (`elapsed_ms=5_000` + `now - 5s_ago` ≈ 10_000 ms).
  expect(obs.firstLatency).not.toBeNull();
  expect(obs.secondLatency).not.toBeNull();
  // Parse the integer ms value out of the `"NNNNms"` textContent.
  const firstMs: number = parseInt(obs.firstLatency!.replace(/ms$/, ''), 10);
  const secondMs: number = parseInt(obs.secondLatency!.replace(/ms$/, ''), 10);
  expect(Number.isFinite(firstMs)).toBe(true);
  expect(Number.isFinite(secondMs)).toBe(true);
  // Monotonic growth: second read > first read.
  expect(secondMs).toBeGreaterThan(firstMs);
  // The growth between reads (separated by 500 ms) should be at
  // least 100 ms (allows for scheduler jitter) and at most 2_000 ms
  // (sanity ceiling — anything bigger means the renderer is
  // double-counting elapsed time).
  const delta: number = secondMs - firstMs;
  expect(delta).toBeGreaterThanOrEqual(100);
  expect(delta).toBeLessThanOrEqual(2_000);
  // The absolute value of the first read must be at least
  // `stage.elapsed_ms` (5_000) — the formula is
  // `elapsed_ms + sinceEvent`, and `sinceEvent >= 0`, so the
  // rendered latency is always >= elapsed_ms.
  expect(firstMs).toBeGreaterThanOrEqual(5_000);
  // The sublabel class `log-phase-sub--ticking` should be
  // absent — the ticker (`state/ticker.ts`) is disabled in this
  // build, and the renderer (`renderLogPhaseHtml`) never adds
  // the class itself, so it can never be present.
  expect(obs.tickingClassPresent).toBe(false);
});

test('Live Logs: finalized row freezes ticker at the row total_ms', async ({ page }: { page: Page }) => {
  await page.goto('http://localhost:8790/#/logs');
  await expect(page.locator('#logs')).toBeVisible();
  await expect(page.locator('#logs >> text=Phase').first()).toBeVisible({ timeout: 5000 });

  const requestId = 'req-finalized-test-1';
  const traceId = 'tr-finalized-1';
  const totalMs = 4_231;

  // 6 seconds ago — past the 2 s stale cap.
  const sixSecondsAgo = new Date(Date.now() - 6_000).toISOString();
  const streamingEvent: StageEvent = {
    request_id: requestId,
    trace_id: traceId,
    stage: 'streaming',
    elapsed_ms: 6_000,
    connect_ms: 30,
    ttft_ms: 120,
    status_code: 200,
    error: null,
    timestamp: sixSecondsAgo,
    provider_id: 'openrouter',
    upstream_model_id: 'gpt-4o-mini',
    stop_reason: null,
    compression_savings_pct: null,
    compression_techniques: null,
  };
  // Finalized row: total_ms is set, status_code is 200, and the
  // row is NOT in-flight (is_streaming=false, stream_complete=true).
  const finalizedRow: RecentUsageRow = {
    id: 999999,
    request_id: requestId,
    trace_id: traceId,
    provider_id: 'openrouter',
    upstream_model_id: 'gpt-4o-mini',
    created_at: sixSecondsAgo,
    status_code: 200,
    total_ms: totalMs,
    prompt_tokens: 12,
    completion_tokens: 7,
    cost_usd: 0.0001,
    is_streaming: false,
    stream_complete: true,
    race_lost: false,
    connect_ms: 30,
    ttft_ms: 120,
    request_body_json: null,
    response_body_json: null,
    request_headers: null,
    response_headers: null,
    race_total: null,
    race_attempts: null,
    error_message: null,
    stop_reason: null,
    compression_savings_pct: null,
    compression_techniques: null,
    client_response: true,
    prompt_tokens_estimated: false,
    completion_tokens_estimated: false,
  };

  await page.evaluate(
    (args: { event: StageEvent; row: RecentUsageRow; requestId: string; traceId: string }) => {
      // Same local cast as the first test — see note above
      // about `live-logs-retry.spec.ts` declaring
      // `Window.__openproxyState` for the project.
      interface SyntheticStage {
        request_id: string;
        trace_id: string;
        stage: string;
        elapsed_ms: number;
        connect_ms: number | null;
        ttft_ms: number | null;
        status_code: number;
        error: string | null;
        timestamp: string;
        provider_id: string;
        upstream_model_id: string;
      }
      const w = window as unknown as {
        __openproxyState: {
          logs: {
            stagesByTraceId?: Map<string, SyntheticStage>;
            stagesByRequestId: Map<string, SyntheticStage>;
            inflightByTraceId?: Map<string, RecentUsageRow>;
            inflightByRequestId: Map<string, RecentUsageRow>;
            rows: RecentUsageRow[];
            followTail: boolean;
            page: number;
            rowsPerPage: number;
            rowById: Map<number, RecentUsageRow>;
          };
        };
        __openproxyLogsGoPage: (page: number) => void;
      };
      const logs = w.__openproxyState.logs;

      logs.rows = [];
      logs.stagesByTraceId?.clear();
      logs.stagesByRequestId.clear();
      logs.inflightByTraceId?.clear();
      logs.inflightByRequestId.clear();
      logs.page = 1;
      logs.rowsPerPage = 50;
      logs.followTail = false;

      // We deliberately do NOT push a terminal `stage` event
      // here. The §4.2 path is supposed to synthesise one from
      // the row envelope — that's the defense-in-depth we are
      // exercising. The row arrives via the `row` message path
      // in production, but in the e2e harness we mutate
      // `logs.rows` directly and let the next render pick it
      // up.
      logs.stagesByTraceId?.set(args.traceId, args.event);
      logs.rows.push(args.row);
      logs.rowById.set(args.row.id, args.row);

      w.__openproxyLogsGoPage(1);
    },
    { event: streamingEvent, row: finalizedRow, requestId, traceId },
  );

  await page.waitForTimeout(150);

  // After two reads separated by 500 ms, the ticker must be
  // frozen AT `totalMs` (not at `now - timestamp`, which would
  // be > 6 s and growing).
  const obs = await readFreezeObservation(page, requestId, traceId, 500);

  expect(obs.stateExposed).toBe(true);
  expect(obs.rowFound).toBe(true);
  // The latency column reads the row's `total_ms` directly per
  // `components/log-row.ts`. The ticker mutates it in place but
  // only when the stage is non-terminal. With the row finalized
  // and no terminal stage event, the ticker mutates it to
  // `finalizedRow.total_ms` via the row-finalized freeze branch
  // — so the value must be `4231ms` (or whatever `totalMs` is)
  // and must NOT change between the two reads.
  expect(obs.firstLatency).toBe(`${totalMs}ms`);
  expect(obs.secondLatency).toBe(`${totalMs}ms`);
  // The sublabel (rendered by `renderLogPhaseHtml` and mutated
  // by the ticker) reads `total ${total_ms}ms` when the stage
  // is terminal, or `ttft ${ttft_ms}ms` / `${totalMs}ms stale`
  // / `${live}ms` otherwise. After the §4.1 fix + the §4.2
  // synth-on-row path, the row's stage map is updated to a
  // terminal `completed` event, so the sublabel must read
  // `total 4231ms`. (The §4.1 row-finalized branch also forces
  // `total ${total_ms}ms` on the ticker side, so the assertion
  // is robust regardless of which path was taken.)
  expect(obs.sublabelText).toBe(`total ${totalMs}ms`);
  expect(obs.tickingClassPresent).toBe(false);
  // The stage map must hold a terminal `completed` event for
  // this attempt — that's the §4.2 synth-on-row contract.
  expect(obs.stageInMap).toBe('completed');
  // The phase label rendered in the DOM is the friendly
  // `completado` (the `STAGE_LABELS.completed` mapping in
  // `lib/constants.ts:22`).
  expect((obs.phaseText ?? '').toLowerCase()).toContain('completado');
});
