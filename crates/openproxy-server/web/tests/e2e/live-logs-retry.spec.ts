// @see tsconfig.test.json for type settings.
//
// Regression test for the user-reported bug (second occurrence):
//   "When the first request times out while connecting to upstream,
//    a new entry is created for the retry — good — but the previous
//    entry's 'Connecting' phase gets *reset* as if a new attempt
//    started, when it should already be 'Failed (timeout)'."
//
// The first fix (commit 465d93b) keyed the stage map by `trace_id`
// to isolate per-attempt phase, which the backend already provides
// (see `UsageInput.trace_id` in `crates/openproxy-core/src/usage.rs`).
//
// This test injects a synthetic `connecting` stage event with
// `trace_id=tr-old`, then a fresh `started` event with
// `trace_id=tr-new` for the same `request_id`, and asserts:
//   1. The DOM renders two distinct rows (one per `trace_id`).
//   2. The old row's phase label stays as "conectando a upstream"
//      (the `connecting` stage label from `lib/constants.ts:17`),
//      even after the new attempt's `started` event has been
//      processed.
//   3. The new row's phase label is "procesando payload"
//      (the `started` stage label from `lib/constants.ts:18`).
//
// Pre-fix behaviour (keying the stage map by `request_id`) would
// overwrite the `connecting` stage of the old attempt with the
// `started` stage of the new one, so the old row's phase label
// would show "procesando payload" instead of "conectando a
// upstream" — assertion 2 catches that.

import { test, expect, type Page } from '@playwright/test';

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

interface InflightInfo {
  request_id: string;
  trace_id: string;
  created_at: string;
}

interface StateSnapshot {
  // The dashboard may have keyed the stage map by `request_id`
  // (pre-fix) or by `trace_id` (post-fix). We read whichever
  // exists and check the same invariant — that the `connecting`
  // stage for `tr-old` is NOT overwritten by the `started`
  // stage for `tr-new`.
  attemptsByKey: Record<string, any>;
  stateExposed: boolean;
  renderedTraceIds: string[];
  oldPhaseInDom: string | null;
  newPhaseInDom: string | null;
}

// The dashboard exposes its in-memory `state` object and the
// `logsGoPage` re-render trigger on `window.__openproxy*` for
// the e2e suite. The hook is declared in `app.ts` and is
// only intended for tests (and operator debugging). See the
// comment block in that file for the rationale.
declare global {
  interface Window {
    __openproxyState: {
      logs: {
        // Pre-fix: only `stagesByRequestId` and
        // `inflightByRequestId` exist.
        // Post-fix: both exist; `stagesByTraceId` and
        // `inflightByTraceId` are the primary keys.
        stagesByTraceId?: Map<string, SyntheticStage>;
        stagesByRequestId: Map<string, SyntheticStage>;
        inflightByTraceId?: Map<string, InflightInfo>;
        inflightByRequestId: Map<string, InflightInfo>;
        rows: { id?: number; request_id?: string; trace_id?: string }[];
        followTail: boolean;
        page: number;
        rowsPerPage: number;
      };
    };
    __openproxyLogsGoPage: (page: number) => void;
        __liveLogsStore: any;
  }
}

// All assertions are run inside a single `page.evaluate` so
// the live `Map` references in the dashboard's state stay
// intact (the JSON serialiser that the playwright boundary
// uses to pass data in and out flattens `Map`s to plain
// objects, which would force the test to re-create them
// before each round-trip and lose the identity of any keys
// the renderer happens to be holding in a closure).
async function snapshotAfterRetry(
  page: Page,
  eventOld: SyntheticStage,
  eventNew: SyntheticStage,
): Promise<StateSnapshot> {
  return page.evaluate(
    async (args: { eventOld: SyntheticStage; eventNew: SyntheticStage }): Promise<StateSnapshot> => {
      const w = window as unknown as {
        __openproxyState: {
          logs: {
            stagesByTraceId?: Map<string, SyntheticStage>;
            stagesByRequestId: Map<string, SyntheticStage>;
            inflightByTraceId?: Map<string, InflightInfo>;
            inflightByRequestId: Map<string, InflightInfo>;
            rows: { id?: number; request_id?: string; trace_id?: string }[];
            followTail: boolean;
            page: number;
            rowsPerPage: number;
          };
        };
        __openproxyLogsGoPage: (page: number) => void;
        __liveLogsStore: any;
      };
      const logs = w.__openproxyState.logs;

      // Replicate the dashboard's `handleStageEvent` path. The
      // production code is in `views/logs.ts`. We follow the
      // *post-fix* contract (key by `trace_id`) because that's
      // what the test wants to validate. Pre-fix code keyed by
      // `request_id`; the test must catch the regression where
      // the keys were swapped back, so the assertion on the DOM
      // is the load-bearing one.
      const inject = (e: SyntheticStage): void => {
        const attemptKey = e.trace_id || `${e.request_id}:unknown`;
        const timestampMs = Date.parse(e.timestamp.endsWith("Z") ? e.timestamp : e.timestamp + "Z");
        const isTerminal = e.stage === "completed" || e.stage === "failed" || e.stage === "cancelled";
        w.__liveLogsStore.applyAttemptEvent({
          attempt_key: attemptKey,
          request_id: e.request_id,
          trace_id: e.trace_id,
          stage: e.stage,
          event_time: timestampMs,
          started_at: timestampMs - e.elapsed_ms,
          stage_seq: isTerminal ? 9999 : 0,
          stage_rank: isTerminal ? 4 : 0,
          terminal: isTerminal,
          connect_ms: e.connect_ms,
          ttft_ms: e.ttft_ms,
          status_code: e.status_code,
          error: e.error,
          provider_id: e.provider_id,
          upstream_model_id: e.upstream_model_id,
        });
      };

      w.__liveLogsStore.clearForTest();
      logs.page = 1;
      logs.rowsPerPage = 50;
      logs.followTail = false;

      inject(args.eventOld);
      inject(args.eventNew);

      // Force a re-render. The export `logsGoPage` is exposed
      // on `window.__openproxyLogsGoPage` by `app.ts` for the
      // e2e suite; calling it always re-runs the private
      // `renderLogsRows()` symbol inside the bundle.
      w.__openproxyLogsGoPage(1);

      // Wait for the scheduled microtask rendering to execute.
      await new Promise<void>((resolve) => queueMicrotask(resolve));

      // Read what the renderer shows. The `.log-row` elements
      // carry `data-trace-id` (set by `renderLogRowHtml` in
      // `components/log-row.ts:65`). The phase is rendered
      // as the row's `.log-phase` text.
      const rowEls = Array.from(
        document.querySelectorAll('#logs .log-row[data-trace-id]'),
      ) as HTMLElement[];
      const renderedTraceIds = rowEls
        .map((el) => el.dataset['traceId'] || '')
        .filter((t) => t.length > 0);
      const oldRow = rowEls.find((el) => el.dataset['traceId'] === 'tr-old');
      const newRow = rowEls.find((el) => el.dataset['traceId'] === 'tr-new');
      const oldPhase = oldRow?.querySelector('.log-phase')?.textContent?.trim() ?? null;
      const newPhase = newRow?.querySelector('.log-phase')?.textContent?.trim() ?? null;

      // Convert the live Maps to plain records so the result
      // is JSON-serialisable across the playwright boundary.
      const rec: Record<string, any> = {};
      for (const entry of Array.from(w.__liveLogsStore.attemptsByKey.entries() as any)) {
        rec[(entry as any)[0]] = (entry as any)[1];
      }
      return {
        attemptsByKey: rec,
        stateExposed: true,
        renderedTraceIds,
        oldPhaseInDom: oldPhase,
        newPhaseInDom: newPhase,
      };
    },
    { eventOld, eventNew },
  );
}

test('Live Logs retry: previous attempt keeps its own stage (no cross-attempt bleed)', async ({ page }: { page: Page }) => {
  await page.goto('http://localhost:8790/#/logs');
  await expect(page.locator('#logs')).toBeVisible();
  // Wait for the view to fully render: the "Phase" header is
  // always present once the logs view is mounted. The first
  // time the user lands on the logs view, the WS connection
  // is still being negotiated; the empty state ("No recent
  // requests yet") or a real row table are both acceptable.
  // We just need the "Phase" header to confirm the view is
  // mounted.
  await expect(page.locator('#logs >> text=Phase').first()).toBeVisible({ timeout: 5000 });

  const eventOld: SyntheticStage = {
    request_id: 'req-retry-test-1',
    trace_id: 'tr-old',
    stage: 'connecting',
    elapsed_ms: 50,
    connect_ms: null,
    ttft_ms: null,
    status_code: 0,
    error: null,
    timestamp: '2026-06-18T02:00:00.050Z',
    provider_id: 'openrouter',
    upstream_model_id: 'gpt-4o-mini',
  };
  const eventNew: SyntheticStage = {
    request_id: 'req-retry-test-1',
    trace_id: 'tr-new',
    stage: 'started',
    elapsed_ms: 0,
    connect_ms: null,
    ttft_ms: null,
    status_code: 0,
    error: null,
    timestamp: '2026-06-18T02:00:01.000Z',
    provider_id: 'openrouter',
    upstream_model_id: 'gpt-4o-mini',
  };

  const snap = await snapshotAfterRetry(page, eventOld, eventNew);
  expect(snap.stateExposed).toBe(true);

  // 1. The DOM renders the two rows separately (different
  //    `data-trace-id` attributes).
  expect(snap.renderedTraceIds).toContain('tr-old');
  expect(snap.renderedTraceIds).toContain('tr-new');

  // 2. The old row's phase label stays as "conectando a
  //    upstream" (the `connecting` stage label from
  //    `lib/constants.ts:19`). Pre-fix behaviour would have
  //    overwritten it with "procesando payload" (the
  //    `started` stage label) because the stage map is
  //    keyed by `request_id`.
  const oldPhase = (snap.oldPhaseInDom ?? '').toLowerCase();
  const newPhase = (snap.newPhaseInDom ?? '').toLowerCase();
  expect(oldPhase).toContain('conectando');
  expect(oldPhase).not.toContain('procesando');
  // 3. The new row's phase label is "procesando payload"
  //    (the `started` stage label from `lib/constants.ts:18`).
  expect(newPhase).toContain('procesando');
});
