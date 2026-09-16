import { test, expect, type Page } from '@playwright/test';
import type { StageEvent, RecentUsageRow } from '../../src/static/src/lib/types/api.js';

const ADMIN_TOKEN_STORAGE_KEY = 'openproxy_admin_token';
const DUMMY_ADMIN_TOKEN = 'test_token_123';

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

async function readFreezeObservation(page: Page, requestId: string, traceId: string, settleMs: number): Promise<FreezeObservation> {
  return page.evaluate((args: { requestId: string; traceId: string; settleMs: number }): Promise<FreezeObservation> => {
    return new Promise((resolve) => {
      const w = window as any;
      const firstRead = () => {
        const rowEl = document.querySelector(`#logs .log-row[data-request-id="${args.requestId}"][data-trace-id="${args.traceId}"]`) as HTMLElement | null;
        if (!rowEl) return { latency: null, sublabel: null, phase: null, tickingClass: false, rowFound: false };
        const subEl = rowEl.querySelector('.log-phase-sub');
        return {
          latency: rowEl.querySelector('.log-latency')?.textContent?.trim() ?? null,
          sublabel: subEl?.textContent?.trim() ?? null,
          phase: rowEl.querySelector('.log-phase')?.textContent?.trim() ?? null,
          tickingClass: !!(subEl && subEl.classList.contains('log-phase-sub--ticking')),
          rowFound: true,
        };
      };

      const first = firstRead();
      setTimeout(() => {
        const second = firstRead();
        const store = w.__liveLogsStore;
        const attemptKey = args.traceId && store?.requestGroups?.get(args.requestId)
          ? Array.from(store.requestGroups.get(args.requestId) as Set<string>).find((k: string) => store.attemptsByKey.get(k)?.traceId === args.traceId)
          : undefined;
        const attempt = attemptKey ? store.attemptsByKey.get(attemptKey) : undefined;
        const row = Array.from(store?.rowsById?.values() || []).find((r: any) => r.request_id === args.requestId && r.trace_id === args.traceId) as RecentUsageRow | undefined;
        resolve({
          stateExposed: true,
          rowFound: first.rowFound,
          firstLatency: first.latency,
          secondLatency: second.latency,
          sublabelText: second.sublabel,
          phaseText: second.phase,
          tickingClassPresent: second.tickingClass,
          stageInMap: attempt ? attempt.stage : null,
          totalMsInRow: row?.total_ms ?? null,
        });
      }, args.settleMs);
    });
  }, { requestId, traceId, settleMs });
}

test.describe('Phase robustness', () => {
  test.beforeEach(async ({ page }: { page: Page }) => {
    page.on('pageerror', (e: Error) => console.error('[phase-robustness] pageerror:', e.message));
    await page.addInitScript((args: { key: string; token: string }) => {
      try { localStorage.setItem(args.key, args.token); } catch (_e) {}
    }, { key: ADMIN_TOKEN_STORAGE_KEY, token: DUMMY_ADMIN_TOKEN });
  });

  test('Live Logs: stale streaming stage freezes the latency ticker', async ({ page }: { page: Page }) => {
    await page.goto('http://localhost:8790/#/logs');
    await expect(page.locator('#logs')).toBeVisible();
    await expect(page.locator('#logs >> text=Phase').first()).toBeVisible({ timeout: 5000 });

    const requestId = 'req-stale-test-1';
    const traceId = 'tr-stale-1';
    const now = Date.now();
    const streamingEvent = {
      attempt_key: traceId, request_id: requestId, trace_id: traceId, stage: 'streaming',
      started_at: now - 6000, event_time: now, terminal: false, stage_seq: 1, stage_rank: 1,
      connect_ms: 30, ttft_ms: 120, error: null, provider_id: 'openrouter', upstream_model_id: 'gpt-4o-mini',
    };

    await page.evaluate((args: { event: any; requestId: string; traceId: string }) => {
      const w = window as any;
      const store = w.__liveLogsStore;
      store.rowsById.clear();
      store.attemptsByKey.clear();
      store.requestGroups.clear();
      store.attemptKeyByRowId.clear();
      if (store.clockOffsetMs) {
        args.event.started_at -= store.clockOffsetMs;
        args.event.event_time -= store.clockOffsetMs;
      }
      store.dispatch({ type: 'attempt_event', cursor: 0, event: args.event });
      w.__openproxyLogsGoPage(1);
    }, { event: streamingEvent, requestId, traceId });

    await page.waitForTimeout(150);
    const obs = await readFreezeObservation(page, requestId, traceId, 500);
    expect(obs.stateExposed).toBe(true);
    expect(obs.rowFound).toBe(true);
    expect(obs.stageInMap).toBe('streaming');
    expect(obs.totalMsInRow).toBeNull();
    expect(obs.firstLatency).not.toBeNull();
    expect(obs.secondLatency).not.toBeNull();

    const firstMs = parseInt(obs.firstLatency!.replace(/ms$/, ''), 10);
    const secondMs = parseInt(obs.secondLatency!.replace(/ms$/, ''), 10);
    expect(Number.isFinite(firstMs)).toBe(true);
    expect(Number.isFinite(secondMs)).toBe(true);
    expect(secondMs).toBeGreaterThan(firstMs);
    expect(secondMs - firstMs).toBeGreaterThanOrEqual(100);
    expect(secondMs - firstMs).toBeLessThanOrEqual(2000);
    expect(firstMs).toBeGreaterThanOrEqual(5000);
    expect(obs.tickingClassPresent).toBe(false);
  });

  test('Live Logs: finalized row freezes ticker at the row total_ms', async ({ page }: { page: Page }) => {
    await page.goto('http://localhost:8790/#/logs');
    await expect(page.locator('#logs')).toBeVisible();
    await expect(page.locator('#logs >> text=Phase').first()).toBeVisible({ timeout: 5000 });

    const requestId = 'req-finalized-test-1';
    const traceId = 'tr-finalized-1';
    const totalMs = 4231;
    const sixSecondsAgo = new Date(Date.now() - 6000).toISOString();

    const streamingEvent: StageEvent = {
      request_id: requestId, trace_id: traceId, stage: 'streaming', elapsed_ms: 6000,
      connect_ms: 30, ttft_ms: 120, status_code: 200, error: null, timestamp: sixSecondsAgo,
      provider_id: 'openrouter', upstream_model_id: 'gpt-4o-mini', stop_reason: null,
      compression_savings_pct: null, compression_techniques: null,
    };

    const finalizedRow: RecentUsageRow = {
      id: 999999, request_id: requestId, trace_id: traceId, provider_id: 'openrouter',
      upstream_model_id: 'gpt-4o-mini', created_at: sixSecondsAgo, status_code: 200, total_ms: totalMs,
      prompt_tokens: 12, completion_tokens: 7, cost_usd: 0.0001, cached_tokens: null, is_streaming: false,
      stream_complete: true, race_lost: false, connect_ms: 30, ttft_ms: 120, request_body_json: null,
      response_body_json: null, request_headers: null, response_headers: null, race_total: null,
      race_attempts: null, error_message: null, stop_reason: null, compression_savings_pct: null,
      compression_techniques: null, proxy_url: null, proxy_status: null, is_proxy_rotated: false,
      client_response: true, prompt_tokens_estimated: false, completion_tokens_estimated: false,
    };

    await page.evaluate((args: { event: StageEvent; row: RecentUsageRow; requestId: string; traceId: string }) => {
      const w = window as any;
      const logs = w.__openproxyState.logs;
      const store = w.__liveLogsStore;
      store.rowsById.clear();
      store.attemptsByKey.clear();
      store.requestGroups.clear();
      store.attemptKeyByRowId.clear();
      logs.page = 1;
      logs.rowsPerPage = 50;
      logs.followTail = false;
      store.dispatch({ type: 'stage', data: args.event });
      store.applyUsageRow(args.row);
      w.__openproxyLogsGoPage(1);
    }, { event: streamingEvent, row: finalizedRow, requestId, traceId });

    await page.waitForTimeout(150);
    const obs = await readFreezeObservation(page, requestId, traceId, 500);
    expect(obs.stateExposed).toBe(true);
    expect(obs.rowFound).toBe(true);
    expect(obs.firstLatency).toBe(`${totalMs}ms`);
    expect(obs.secondLatency).toBe(`${totalMs}ms`);
    expect(obs.sublabelText).toBe(`total ${totalMs}ms`);
    expect(obs.tickingClassPresent).toBe(false);
    expect(obs.stageInMap).toBe('completed');
    expect((obs.phaseText ?? '').toLowerCase()).toContain('completed');
  });
});
