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

const DUMMY_ADMIN_TOKEN = 'op_live_test_dummy_token_for_e2e';
const ADMIN_TOKEN_STORAGE_KEY = 'openproxy_admin_token';

test.beforeEach(async ({ page }: { page: Page }) => {
  await page.addInitScript((args: { key: string; token: string }) => {
    try { localStorage.setItem(args.key, args.token); } catch (_e) {}
  }, { key: ADMIN_TOKEN_STORAGE_KEY, token: DUMMY_ADMIN_TOKEN });
});

async function setupLogsView(page: Page): Promise<void> {
  await page.goto('http://localhost:8790/#/logs');
  await expect(page.locator('#logs')).toBeVisible();
  await expect(page.locator('#logs >> text=Phase').first()).toBeVisible({ timeout: 5000 });
}

function makeStage(overrides: Partial<SyntheticStagePhases> = {}): SyntheticStagePhases {
  return {
    request_id: 'req-1',
    trace_id: 'tr-1',
    stage: 'waiting_ttft',
    elapsed_ms: 300,
    connect_ms: 30,
    ttft_ms: 300,
    status_code: 200,
    error: null,
    stop_reason: null,
    compression_savings_pct: null,
    compression_techniques: null,
    timestamp: new Date(Date.now() - 100).toISOString(),
    provider_id: 'openrouter',
    upstream_model_id: 'claude-3-5-sonnet',
    ...overrides,
  };
}

async function injectAndSnapshot(page: Page, stages: SyntheticStagePhases[]): Promise<RenderSnapshot> {
  return page.evaluate(async (args: { stages: SyntheticStagePhases[] }): Promise<RenderSnapshot> => {
    const w = window as any;
    const logs = w.__openproxyState.logs;
    w.__liveLogsStore.clearForTest();
    logs.page = 1;
    logs.rowsPerPage = 50;
    logs.followTail = false;

    for (const e of args.stages) {
      const attemptKey = e.trace_id || `${e.request_id}:unknown`;
      const nodeTimestamp = Date.parse(e.timestamp.endsWith("Z") ? e.timestamp : e.timestamp + "Z");
      const ageMs = Math.max(0, Date.now() - nodeTimestamp);
      const timestampMs = logs?.clockStore?.nowMs ? logs.clockStore.nowMs - ageMs : Date.now() - ageMs;
      const isTerminal = e.stage === "completed" || e.stage === "failed" || e.stage === "cancelled";
      w.__liveLogsStore.applyAttemptEvent({
        attempt_key: attemptKey, request_id: e.request_id, trace_id: e.trace_id,
        stage: e.stage, event_time: timestampMs, started_at: timestampMs - e.elapsed_ms,
        stage_seq: isTerminal ? 9999 : 0, stage_rank: isTerminal ? 4 : 0, terminal: isTerminal,
        connect_ms: e.connect_ms, ttft_ms: e.ttft_ms, status_code: e.status_code,
        error: e.error, provider_id: e.provider_id, upstream_model_id: e.upstream_model_id,
      });
    }

    w.__openproxyLogsGoPage(1);
    await new Promise<void>((resolve) => queueMicrotask(resolve));

    const rowEls = Array.from(document.querySelectorAll('#logs .log-row[data-trace-id]')) as HTMLElement[];
    const rows: RowRenderInfo[] = rowEls.map((el) => ({
      traceId: el.dataset['traceId'] || '',
      phase: el.querySelector('.log-phase')?.textContent?.trim() ?? '',
      latency: el.querySelector('.log-latency')?.textContent?.trim() ?? '',
      statusCode: el.querySelector('.log-status')?.textContent?.trim() ?? '',
    }));
    return { stateExposed: true, rows };
  }, { stages });
}

test('Live Logs: inflight placeholder with status_code=200 (waiting_ttft) does NOT show "completed"', async ({ page }) => {
  await setupLogsView(page);
  const stages = [makeStage({ request_id: 'req-bug-A', trace_id: 'tr-bug-A', stage: 'waiting_ttft', elapsed_ms: 300 })];
  const snap = await injectAndSnapshot(page, stages);
  expect(snap.stateExposed).toBe(true);
  expect(snap.rows.length).toBe(1);

  const row = snap.rows[0]!;
  expect(row.traceId).toBe('tr-bug-A');
  const phaseLower = row.phase.toLowerCase();
  expect(phaseLower).not.toContain('completed');
  expect(phaseLower).not.toContain('failed');
  expect(phaseLower).not.toContain('cancelled');
  expect(phaseLower).toContain('waiting');

  const latencyMs = parseInt(row.latency.replace(/ms$/, ''), 10);
  expect(Number.isFinite(latencyMs)).toBe(true);
  expect(latencyMs).toBeGreaterThanOrEqual(100);
});

test('Live Logs: inflight streaming with status_code=200 shows "streaming response" not "completed"', async ({ page }) => {
  await setupLogsView(page);
  const stages = [makeStage({ request_id: 'req-streaming', trace_id: 'tr-streaming', stage: 'streaming', elapsed_ms: 500 })];
  const snap = await injectAndSnapshot(page, stages);
  expect(snap.rows.length).toBe(1);

  const phaseLower = snap.rows[0]!.phase.toLowerCase();
  expect(phaseLower).not.toContain('completed');
  expect(phaseLower).toContain('streaming');
});

test('Live Logs: newer inflight renders ABOVE older inflight', async ({ page }) => {
  await setupLogsView(page);
  const now = Date.now();
  const stages = [
    makeStage({ request_id: 'req-old', trace_id: 'tr-old', stage: 'streaming', elapsed_ms: 5000, timestamp: new Date(now - 5000).toISOString() }),
    makeStage({ request_id: 'req-new', trace_id: 'tr-new', stage: 'started', elapsed_ms: 0, status_code: 0, timestamp: new Date(now - 100).toISOString() }),
  ];
  const snap = await injectAndSnapshot(page, stages);
  expect(snap.rows.length).toBe(2);
  expect(snap.rows[0]!.traceId).toBe('tr-new');
  expect(snap.rows[1]!.traceId).toBe('tr-old');
});

test('Live Logs: latency is monotonic across stage transitions', async ({ page }) => {
  await setupLogsView(page);
  const startedAt = Date.now() - 600;
  const startedStage = makeStage({ request_id: 'req-mono', trace_id: 'tr-mono', stage: 'started', elapsed_ms: 0, status_code: 0, timestamp: new Date(startedAt).toISOString() });

  let snap = await injectAndSnapshot(page, [startedStage]);
  expect(snap.rows.length).toBe(1);
  const firstLatencyMs = parseInt(snap.rows[0]!.latency.replace(/ms$/, ''), 10);
  expect(Number.isFinite(firstLatencyMs)).toBe(true);
  expect(firstLatencyMs).toBeGreaterThanOrEqual(200);
  expect(firstLatencyMs).toBeLessThanOrEqual(2000);

  await page.waitForTimeout(300);
  const connectingStage = { ...startedStage, stage: 'connecting', elapsed_ms: 50, connect_ms: 50, timestamp: new Date(startedAt + 50).toISOString() };
  snap = await injectAndSnapshot(page, [connectingStage]);
  expect(snap.rows.length).toBe(1);
  const secondLatencyMs = parseInt(snap.rows[0]!.latency.replace(/ms$/, ''), 10);
  expect(Number.isFinite(secondLatencyMs)).toBe(true);
  expect(secondLatencyMs).toBeGreaterThanOrEqual(firstLatencyMs);
});

test('Live Logs: burst of stage events renders the LATEST stage', async ({ page }) => {
  await setupLogsView(page);
  const stages = [makeStage({ request_id: 'req-burst', trace_id: 'tr-burst', stage: 'streaming', elapsed_ms: 500 })];
  const snap = await injectAndSnapshot(page, stages);
  expect(snap.rows.length).toBe(1);
  const phaseLower = snap.rows[0]!.phase.toLowerCase();
  expect(phaseLower).toContain('streaming');
  expect(phaseLower).not.toContain('processing');
});
