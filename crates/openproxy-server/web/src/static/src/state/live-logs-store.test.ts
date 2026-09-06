import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  liveLogsStore,
  MAX_STORED_ROWS,
  type AttemptEventPayload,
  type AttemptState,
} from "./live-logs-store.js";
import * as apiModule from "../lib/api.js";
import type { RecentUsageRow, StageEvent } from "../lib/types/api.js";

const makeRow = (overrides: Partial<RecentUsageRow> = {}): RecentUsageRow => ({
  id: 1,
  request_id: "request-1",
  trace_id: "attempt-1",
  provider_id: "provider-1",
  upstream_model_id: "model-1",
  status_code: 200,
  total_ms: 100,
  prompt_tokens: null,
  completion_tokens: null,
  cached_tokens: null,
  cost_usd: null,
  connect_ms: null,
  ttft_ms: null,
  request_body_json: null,
  response_body_json: null,
  request_headers: null,
  response_headers: null,
  error_message: null,
  race_total: null,
  race_attempts: null,
  is_streaming: false,
  stream_complete: true,
  race_lost: false,
  stop_reason: null,
  compression_savings_pct: null,
  compression_techniques: null,
  client_response: true,
  prompt_tokens_estimated: false,
  completion_tokens_estimated: false,
  proxy_url: null,
  proxy_status: null,
  is_proxy_rotated: false,
  created_at: "1970-01-01T00:16:40.000Z",
  ...overrides,
});

beforeEach(() => {
  liveLogsStore.clearForTest();
});

describe("liveLogsStore detail management", () => {
  it("stores and selects detail correctly", () => {
    liveLogsStore.attemptsByKey.set("attempt-1", {
      attemptKey: "attempt-1",
      requestId: "req-1",
      traceId: "tr-1",
      providerId: "prov-1",
      upstreamModelId: "model-1",
      startedAtMs: 1000,
      updatedAtMs: 1000,
      stage: "completed",
      stageSeq: 1,
      stageRank: 1,
      elapsedMsAtEvent: 100,
      connectMs: 10,
      ttftMs: 20,
      statusCode: 200,
      terminal: true,
      terminalKind: "completed",
      error: null,
      rowId: 42,
      row: null,
      source: "live",
      endpointKind: null,
    });

    liveLogsStore.setDetail({ kind: "attempt", attemptKey: "attempt-1" }, { custom_field: "test" });
    const attempt = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "attempt-1" });
    expect(attempt?.detail).toEqual({ custom_field: "test" });
  });

  it("fetchLogDetail extracts query param and updates store", async () => {
    const apiSpy = vi.spyOn(apiModule, "api").mockResolvedValue({
      row: { id: 10, model_id: "gpt-4" },
    });

    const result = await liveLogsStore.fetchLogDetail("10", "trace-10", "attempt-10");
    expect(result).toBe(true);
    expect(apiSpy).toHaveBeenCalledWith("/usage/detail?id=10");
    apiSpy.mockRestore();
  });

  it("separates inflight and finished rows correctly", () => {
    liveLogsStore.clearForTest();

    liveLogsStore.dispatch({
      type: "attempt_event",
      cursor: 1,
      event: {
        attempt_key: "tr-inflight",
        request_id: "req-1",
        trace_id: "tr-inflight",
        stage: "streaming",
        event_time: 2000,
        started_at: 1000,
        stage_seq: 2,
        stage_rank: 3,
        terminal: false,
      },
    });

    liveLogsStore.dispatch({
      type: "attempt_event",
      cursor: 2,
      event: {
        attempt_key: "tr-finished",
        request_id: "req-2",
        trace_id: "tr-finished",
        stage: "completed",
        event_time: 3000,
        started_at: 1500,
        stage_seq: 9999,
        stage_rank: 4,
        terminal: true,
      },
    });

    const inflight = liveLogsStore.selectInflightRows();
    const finished = liveLogsStore.selectFinishedRows();

    expect(inflight.length).toBe(1);
    expect(inflight[0]?.attemptKey).toBe("tr-inflight");

    expect(finished.length).toBe(1);
    expect(finished[0]?.attemptKey).toBe("tr-finished");
  });

  it("cleans orphan unknownKey when trace_id event or row arrives", () => {
    liveLogsStore.clearForTest();

    liveLogsStore.dispatch({
      type: "attempt_event",
      cursor: 1,
      event: {
        attempt_key: "req-3:unknown",
        request_id: "req-3",
        stage: "started",
        event_time: 1000,
        started_at: 1000,
        stage_seq: 0,
        stage_rank: 0,
        terminal: false,
      },
    });

    expect(liveLogsStore.attemptsByKey.has("req-3:unknown")).toBe(true);

    liveLogsStore.dispatch({
      type: "attempt_event",
      cursor: 2,
      event: {
        attempt_key: "tr-3",
        request_id: "req-3",
        trace_id: "tr-3",
        stage: "completed",
        event_time: 2000,
        started_at: 1000,
        stage_seq: 9999,
        stage_rank: 4,
        terminal: true,
      },
    });

    expect(liveLogsStore.attemptsByKey.has("req-3:unknown")).toBe(false);
    expect(liveLogsStore.attemptsByKey.has("tr-3")).toBe(true);
  });

  it("marks attempt with status code >= 400 or error as terminal", () => {
    liveLogsStore.clearForTest();

    liveLogsStore.dispatch({
      type: "attempt_event",
      cursor: 1,
      event: {
        attempt_key: "tr-err",
        request_id: "req-err",
        trace_id: "tr-err",
        stage: "failed",
        status_code: 500,
        error: "Upstream failure",
        event_time: 2000,
        started_at: 1000,
        stage_seq: 3,
        stage_rank: 4,
        terminal: false,
      },
    });

    const inflight = liveLogsStore.selectInflightRows();
    const finished = liveLogsStore.selectFinishedRows();

    expect(inflight.length).toBe(0);
    expect(finished.length).toBe(1);
    expect(finished[0]?.terminal).toBe(true);
    expect(finished[0]?.terminalKind).toBe("failed");
  });

  it("auto-expires stale inflight requests older than 30m", () => {
    liveLogsStore.clearForTest();

    const staleTime = Date.now() - 1_900_000;
    liveLogsStore.dispatch({
      type: "attempt_event",
      cursor: 1,
      event: {
        attempt_key: "tr-stale",
        request_id: "req-stale",
        trace_id: "tr-stale",
        stage: "streaming",
        event_time: staleTime,
        started_at: staleTime,
        stage_seq: 1,
        stage_rank: 3,
        terminal: false,
      },
    });

    const inflight = liveLogsStore.selectInflightRows();
    const finished = liveLogsStore.selectFinishedRows();

    expect(inflight.length).toBe(0);
    expect(finished.length).toBe(1);
    expect(finished[0]?.terminal).toBe(true);
    expect(finished[0]?.terminalKind).toBe("failed");
  });

  it("enforces capacity by evicting oldest terminal entries", () => {
    liveLogsStore.clearForTest();

    for (let i = 1; i <= 5; i++) {
      liveLogsStore.dispatch({
        type: "usage_row",
        cursor: i,
        row: {
          id: i,
          request_id: `req-${i}`,
          trace_id: `tr-${i}`,
          provider_id: "prov-1",
          upstream_model_id: "mod-1",
          status_code: 200,
          total_ms: 100,
          connect_ms: 10,
          ttft_ms: 20,
          cost: 0,
          tokens_in: 10,
          tokens_out: 20,
          created_at: new Date(1000000 + i * 1000).toISOString(),
        },
      });
    }

    expect(liveLogsStore.selectFinishedRows().length).toBe(5);

    // Enforce max 3 items
    liveLogsStore.enforceCapacity(3);

    const remaining = liveLogsStore.selectFinishedRows();
    expect(remaining.length).toBe(3);
    // Oldest items (id 1 and 2) should be evicted
    expect(liveLogsStore.attemptsByKey.has("tr-1")).toBe(false);
    expect(liveLogsStore.rowsById.has(1)).toBe(false);
    expect(liveLogsStore.attemptsByKey.has("tr-2")).toBe(false);
    expect(liveLogsStore.rowsById.has(2)).toBe(false);
    expect(liveLogsStore.attemptsByKey.has("tr-3")).toBe(true);
    expect(liveLogsStore.rowsById.has(3)).toBe(true);
  });
});

// ===========================================================================
// Invariants: terminality, ordering, cursor, inflight_sync, eviction,
// retries, legacy normalization and malformed payloads.
//
// Everything below goes through the public API only: dispatch(),
// enforceCapacity(), select*() / selectDetail() and the public maps.
// Timestamps are literal epoch values; fake timers are used exclusively
// for the 30m inflight-expiration boundary.
// ===========================================================================

/** Resets the singleton: clearForTest() plus the public clock/status
 *  fields it does not cover (legacy normalization derives event_time
 *  from clockOffsetMs, so it must be zeroed for determinism). */
function resetStore() {
  liveLogsStore.clearForTest();
  liveLogsStore.clockOffsetMs = 0;
  liveLogsStore.lastServerNow = 0;
  liveLogsStore.connectionStatus = "disconnected";
}

function makeEvent(overrides: Partial<AttemptEventPayload> = {}): AttemptEventPayload {
  return {
    attempt_key: "attempt-1",
    request_id: "request-1",
    trace_id: "attempt-1",
    stage: "started",
    stage_seq: 0,
    stage_rank: 0,
    event_time: 1_000,
    started_at: 1_000,
    terminal: false,
    ...overrides,
  };
}

function dispatchAttemptEvent(cursor: number, event: AttemptEventPayload) {
  liveLogsStore.dispatch({ type: "attempt_event", cursor, event });
}

function dispatchUsageRow(cursor: number, row: RecentUsageRow) {
  liveLogsStore.dispatch({ type: "usage_row", cursor, row });
}

function makeStageEvent(overrides: Partial<StageEvent> = {}): StageEvent {
  return {
    request_id: "request-1",
    trace_id: "trace-1",
    provider_id: "provider-1",
    upstream_model_id: "model-1",
    stage: "started",
    elapsed_ms: 0,
    connect_ms: null,
    ttft_ms: null,
    status_code: 0,
    error: null,
    stop_reason: null,
    timestamp: "2026-01-01T00:00:00.000Z",
    compression_savings_pct: null,
    compression_techniques: null,
    ...overrides,
  };
}

/** Full AttemptState used as the camelCase payload of snapshot /
 *  inflight_sync envelopes (what the server broadcasts). */
function makeSnapshotAttempt(overrides: Partial<AttemptState> = {}): AttemptState {
  return {
    attemptKey: "attempt-1",
    requestId: "request-1",
    traceId: "attempt-1",
    providerId: "provider-1",
    upstreamModelId: "model-1",
    startedAtMs: 1_000,
    updatedAtMs: 1_000,
    stage: "streaming",
    stageSeq: 3,
    stageRank: 3,
    elapsedMsAtEvent: 0,
    connectMs: null,
    ttftMs: null,
    statusCode: null,
    terminal: false,
    terminalKind: null,
    error: null,
    rowId: null,
    row: null,
    source: "snapshot",
    endpointKind: null,
    ...overrides,
  };
}

function inflightKeys(): string[] {
  return liveLogsStore.selectInflightRows().map((a) => a.attemptKey);
}

function finishedKeys(): string[] {
  return liveLogsStore.selectFinishedRows().map((a) => a.attemptKey);
}

describe("liveLogsStore terminality invariants", () => {
  it("a terminal event never returns to inflight, even after a delayed non-terminal event", () => {
    resetStore();
    dispatchAttemptEvent(1, makeEvent({
      attempt_key: "tr-1", trace_id: "tr-1", request_id: "req-1",
      stage: "completed", stage_seq: 4, stage_rank: 4,
      event_time: 2_000, started_at: 1_000, terminal: true,
    }));
    expect(finishedKeys()).toEqual(["tr-1"]);
    expect(inflightKeys()).toEqual([]);

    dispatchAttemptEvent(2, makeEvent({
      attempt_key: "tr-1", trace_id: "tr-1", request_id: "req-1",
      stage: "streaming", stage_seq: 3, stage_rank: 3,
      event_time: 3_000, started_at: 1_000, terminal: false,
    }));

    expect(inflightKeys()).toEqual([]);
    expect(finishedKeys()).toEqual(["tr-1"]);
    const a = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-1" });
    expect(a?.stage).toBe("completed");
    expect(a?.updatedAtMs).toBe(2_000);
  });

  it("status_code >= 400 derives terminality even on a non-terminal stage", () => {
    resetStore();
    dispatchAttemptEvent(1, makeEvent({
      attempt_key: "tr-2", trace_id: "tr-2", request_id: "req-2",
      stage: "streaming", stage_seq: 3, stage_rank: 3,
      event_time: 2_000, started_at: 1_000, status_code: 502, terminal: false,
    }));

    expect(inflightKeys()).toEqual([]);
    expect(finishedKeys()).toEqual(["tr-2"]);
    const a = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-2" });
    expect(a?.terminal).toBe(true);
    expect(a?.terminalKind).toBe("failed");
    expect(a?.statusCode).toBe(502);

    dispatchAttemptEvent(2, makeEvent({
      attempt_key: "tr-2", trace_id: "tr-2", request_id: "req-2",
      stage: "streaming", stage_seq: 3, stage_rank: 3,
      event_time: 3_000, started_at: 1_000, terminal: false,
    }));
    expect(inflightKeys()).toEqual([]);
    expect(finishedKeys()).toEqual(["tr-2"]);
  });

  it("a non-empty error derives terminality and is preserved", () => {
    resetStore();
    dispatchAttemptEvent(1, makeEvent({
      attempt_key: "tr-3", trace_id: "tr-3", request_id: "req-3",
      stage: "streaming", stage_seq: 3, stage_rank: 3,
      event_time: 2_000, started_at: 1_000, error: "boom", terminal: false,
    }));

    expect(inflightKeys()).toEqual([]);
    expect(finishedKeys()).toEqual(["tr-3"]);
    const a = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-3" });
    expect(a?.terminal).toBe(true);
    expect(a?.terminalKind).toBe("failed");
    expect(a?.error).toBe("boom");
  });

  it("usage_row anchors the attempt and later attempt_events cannot mutate it", () => {
    resetStore();
    dispatchAttemptEvent(1, makeEvent({
      attempt_key: "tr-4", trace_id: "tr-4", request_id: "req-4",
      stage: "streaming", stage_seq: 3, stage_rank: 3,
      event_time: 1_200, started_at: 1_000,
    }));
    dispatchUsageRow(2, makeRow({ id: 5, trace_id: "tr-4", request_id: "req-4", status_code: 200, total_ms: 400 }));

    const a = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-4" });
    expect(a?.terminal).toBe(true);
    expect(a?.stage).toBe("completed");
    expect(a?.rowId).toBe(5);
    expect(a?.row?.id).toBe(5);
    expect(a?.updatedAtMs).toBe(1_400); // startedAtMs(1000) + total_ms(400)

    // Even a terminal-looking event must not overwrite the DB-anchored state.
    dispatchAttemptEvent(3, makeEvent({
      attempt_key: "tr-4", trace_id: "tr-4", request_id: "req-4",
      stage: "failed", stage_seq: 4, stage_rank: 4,
      event_time: 9_999, started_at: 1_000, status_code: 500, error: "late failure", terminal: true,
    }));
    expect(a?.stage).toBe("completed");
    expect(a?.rowId).toBe(5);
    expect(a?.error).toBeNull();
    expect(a?.statusCode).toBe(200);
  });

  it("cancelled and skipped signals map to their own terminal kinds", () => {
    resetStore();
    dispatchAttemptEvent(1, makeEvent({
      attempt_key: "tr-c", trace_id: "tr-c", request_id: "req-c",
      stage: "cancelled", stage_seq: 4, stage_rank: 4,
      event_time: 2_000, started_at: 1_000, terminal: true,
    }));
    const cancelled = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-c" });
    expect(cancelled?.terminalKind).toBe("cancelled");
    expect(inflightKeys()).toEqual([]);

    dispatchUsageRow(2, makeRow({ id: 7, trace_id: "tr-s", request_id: "req-s", status_code: 0 }));
    const skipped = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-s" });
    expect(skipped?.stage).toBe("predict_skipped");
    expect(skipped?.terminalKind).toBe("predict_skipped");
    expect(inflightKeys()).toEqual([]);
  });
});

describe("liveLogsStore phase progression", () => {
  it("a delayed non-terminal event with a lower stage_rank does not regress the phase", () => {
    resetStore();
    dispatchAttemptEvent(1, makeEvent({ attempt_key: "tr-p", stage: "started", stage_seq: 0, stage_rank: 0, event_time: 1_000, started_at: 1_000 }));
    dispatchAttemptEvent(2, makeEvent({ attempt_key: "tr-p", stage: "streaming", stage_seq: 3, stage_rank: 3, event_time: 3_000, started_at: 1_000, ttft_ms: 80 }));
    dispatchAttemptEvent(3, makeEvent({ attempt_key: "tr-p", stage: "connecting", stage_seq: 1, stage_rank: 1, event_time: 4_000, started_at: 1_000, connect_ms: 5 }));

    const a = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-p" });
    expect(a?.stage).toBe("streaming");
    expect(a?.stageRank).toBe(3);
    expect(a?.stageSeq).toBe(3);
    expect(a?.updatedAtMs).toBe(3_000);
    expect(a?.connectMs).toBeNull();
    expect(a?.ttftMs).toBe(80);
  });

  it("an event at the same stage_rank is still applied (only strictly lower ranks are rejected)", () => {
    resetStore();
    dispatchAttemptEvent(1, makeEvent({ attempt_key: "tr-q", stage: "streaming", stage_seq: 3, stage_rank: 3, event_time: 3_000, started_at: 1_000, ttft_ms: 80 }));
    dispatchAttemptEvent(2, makeEvent({ attempt_key: "tr-q", stage: "streaming", stage_seq: 3, stage_rank: 3, event_time: 3_500, started_at: 1_000, ttft_ms: 90 }));

    const a = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-q" });
    expect(a?.stage).toBe("streaming");
    expect(a?.updatedAtMs).toBe(3_500);
    expect(a?.ttftMs).toBe(90);
  });

  it("merging events keeps the original started_at timeline, so a retry's timestamps never mix in", () => {
    resetStore();
    dispatchAttemptEvent(1, makeEvent({ attempt_key: "tr-m", stage: "started", stage_seq: 0, stage_rank: 0, event_time: 1_100, started_at: 1_000 }));
    // Second event claims a foreign started_at (as if from another attempt).
    dispatchAttemptEvent(2, makeEvent({ attempt_key: "tr-m", stage: "streaming", stage_seq: 3, stage_rank: 3, event_time: 2_000, started_at: 9_000 }));

    const a = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-m" });
    expect(a?.startedAtMs).toBe(1_000);
    expect(a?.updatedAtMs).toBe(2_000);
    expect(a?.elapsedMsAtEvent).toBe(1_000); // 2000 - 1000, not 2000 - 9000
  });

  it("selectLogRows orders by startedAtMs desc with insertion order as tiebreaker", () => {
    resetStore();
    dispatchAttemptEvent(1, makeEvent({ attempt_key: "tr-early", started_at: 1_000, event_time: 1_100 }));
    dispatchAttemptEvent(2, makeEvent({ attempt_key: "tr-late", started_at: 5_000, event_time: 5_100 }));
    dispatchAttemptEvent(3, makeEvent({ attempt_key: "tr-tie-1", started_at: 3_000, event_time: 3_100 }));
    dispatchAttemptEvent(4, makeEvent({ attempt_key: "tr-tie-2", started_at: 3_000, event_time: 3_200 }));

    expect(liveLogsStore.selectLogRows().map((a) => a.attemptKey)).toEqual([
      "tr-late",
      "tr-tie-2",
      "tr-tie-1",
      "tr-early",
    ]);
  });
});

describe("liveLogsStore cursor deduplication", () => {
  it("a repeated cursor is ignored even when the payload differs", () => {
    resetStore();
    dispatchAttemptEvent(10, makeEvent({ attempt_key: "tr-a", stage: "streaming", stage_seq: 3, stage_rank: 3, event_time: 2_000, started_at: 1_000 }));
    // Same cursor, contradictory payload: must be dropped wholesale.
    dispatchAttemptEvent(10, makeEvent({ attempt_key: "tr-a", stage: "failed", stage_seq: 4, stage_rank: 4, event_time: 9_999, started_at: 1_000, status_code: 500, terminal: true }));

    const a = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-a" });
    expect(a?.stage).toBe("streaming");
    expect(a?.terminal).toBe(false);
    expect(liveLogsStore.lastAppliedCursor).toBe(10);
    expect(liveLogsStore.attemptsByKey.size).toBe(1);
  });

  it("a lower cursor is dropped before reaching the reducers", () => {
    resetStore();
    dispatchAttemptEvent(10, makeEvent({ attempt_key: "tr-a", stage: "streaming", stage_seq: 3, stage_rank: 3, event_time: 2_000, started_at: 1_000 }));
    dispatchAttemptEvent(9, makeEvent({ attempt_key: "tr-older", trace_id: "tr-older", request_id: "req-older", event_time: 1_500, started_at: 1_000 }));

    expect(liveLogsStore.attemptsByKey.has("tr-older")).toBe(false);
    expect(liveLogsStore.attemptsByKey.size).toBe(1);
    expect(liveLogsStore.lastAppliedCursor).toBe(10);
  });

  it("a repeated cursor on usage_row does not duplicate or overwrite the row", () => {
    resetStore();
    dispatchUsageRow(5, makeRow({ id: 1, total_ms: 100 }));
    dispatchUsageRow(5, makeRow({ id: 1, total_ms: 777, error_message: "mutated" }));

    expect(liveLogsStore.rowsById.size).toBe(1);
    expect(liveLogsStore.rowsById.get(1)?.total_ms).toBe(100);
    expect(liveLogsStore.rowsById.get(1)?.error_message).toBeNull();
    expect(liveLogsStore.lastAppliedCursor).toBe(5);
  });

  it("snapshots bypass the cursor guard and reset it (resync wins)", () => {
    resetStore();
    dispatchAttemptEvent(100, makeEvent({ attempt_key: "tr-a", stage: "streaming", stage_seq: 3, stage_rank: 3, event_time: 2_000, started_at: 1_000 }));
    liveLogsStore.dispatch({
      type: "snapshot",
      cursor: 7,
      server_now: 5_000,
      rows: [],
      attempts: [makeSnapshotAttempt({ attemptKey: "tr-b", stage: "waiting_ttft", stageSeq: 2, stageRank: 2 })],
    });

    expect(liveLogsStore.lastAppliedCursor).toBe(7);
    expect(liveLogsStore.connectionStatus).toBe("connected");
    expect(liveLogsStore.attemptsByKey.has("tr-a")).toBe(false);
    expect(inflightKeys()).toEqual(["tr-b"]);

    // Cursor baseline is now 7: 8 applies, 7 is ignored.
    dispatchAttemptEvent(8, makeEvent({ attempt_key: "tr-c", trace_id: "tr-c", request_id: "req-c", event_time: 6_000, started_at: 5_000 }));
    expect(liveLogsStore.attemptsByKey.has("tr-c")).toBe(true);
    dispatchAttemptEvent(7, makeEvent({ attempt_key: "tr-d", trace_id: "tr-d", request_id: "req-d", event_time: 5_500, started_at: 5_000 }));
    expect(liveLogsStore.attemptsByKey.has("tr-d")).toBe(false);
  });

  it("gap moves the store into recovering without touching cursor state", () => {
    resetStore();
    dispatchAttemptEvent(10, makeEvent({ attempt_key: "tr-a", event_time: 2_000, started_at: 1_000 }));
    liveLogsStore.dispatch({ type: "gap", from_cursor: 11, to_cursor: 20, reason: "lag" });

    expect(liveLogsStore.connectionStatus).toBe("recovering");
    expect(liveLogsStore.lastAppliedCursor).toBe(10);
    expect(liveLogsStore.attemptsByKey.size).toBe(1);
  });
});

describe("liveLogsStore inflight_sync", () => {
  it("replaces the inflight set with the server truth and removes only absent inflight attempts", () => {
    resetStore();
    dispatchAttemptEvent(1, makeEvent({ attempt_key: "tr-live", stage: "streaming", stage_seq: 3, stage_rank: 3, event_time: 2_000, started_at: 1_000 }));
    dispatchAttemptEvent(2, makeEvent({ attempt_key: "tr-gone", stage: "connecting", stage_seq: 1, stage_rank: 1, event_time: 1_500, started_at: 1_000 }));
    dispatchUsageRow(3, makeRow({ id: 1, trace_id: "tr-done", request_id: "req-done", status_code: 200 }));

    liveLogsStore.dispatch({
      type: "inflight_sync",
      server_now: 9_000,
      attempts: [makeSnapshotAttempt({ attemptKey: "tr-live", stage: "waiting_ttft", stageSeq: 2, stageRank: 2, startedAtMs: 1_000, updatedAtMs: 2_500 })],
    });

    expect(inflightKeys()).toEqual(["tr-live"]);
    expect(liveLogsStore.attemptsByKey.has("tr-gone")).toBe(false);
    const live = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-live" });
    expect(live?.stage).toBe("waiting_ttft");
    expect(live?.updatedAtMs).toBe(2_500);

    const done = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-done" });
    expect(done?.terminal).toBe(true);
    expect(done?.rowId).toBe(1);
    expect(liveLogsStore.rowsById.has(1)).toBe(true);
  });

  it("a finished attempt stays finished even when the sync still lists it (broadcast lag)", () => {
    resetStore();
    dispatchAttemptEvent(1, makeEvent({ attempt_key: "tr-x", stage: "streaming", stage_seq: 3, stage_rank: 3, event_time: 1_200, started_at: 1_000 }));
    dispatchUsageRow(2, makeRow({ id: 9, trace_id: "tr-x", request_id: "req-x", status_code: 200, total_ms: 300 }));

    // Server lag: its inflight snapshot still contains tr-x even though the
    // client already applied the authoritative usage row for it.
    liveLogsStore.dispatch({
      type: "inflight_sync",
      server_now: 9_000,
      attempts: [makeSnapshotAttempt({ attemptKey: "tr-x", stage: "streaming", stageSeq: 3, stageRank: 3, startedAtMs: 1_000, updatedAtMs: 1_200 })],
    });

    const a = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-x" });
    expect(a?.terminal).toBe(true);
    expect(a?.rowId).toBe(9);
    expect(a?.row?.id).toBe(9);
    expect(inflightKeys()).toEqual([]);
    expect(finishedKeys()).toEqual(["tr-x"]);
  });
});

describe("liveLogsStore eviction and capacity", () => {
  it("eviction keeps inflight attempts and removes the oldest terminal ones, keeping selectors coherent", () => {
    resetStore();
    dispatchAttemptEvent(1, makeEvent({ attempt_key: "tr-hot", stage: "streaming", stage_seq: 3, stage_rank: 3, event_time: 2_000, started_at: 1_000 }));

    const base = Date.parse("2026-01-01T00:00:00.000Z");
    for (let i = 1; i <= 4; i++) {
      dispatchUsageRow(i + 1, makeRow({
        id: i,
        trace_id: `tr-${i}`,
        request_id: `req-${i}`,
        created_at: new Date(base + i * 1_000).toISOString(),
      }));
    }

    liveLogsStore.enforceCapacity(2);

    expect(inflightKeys()).toEqual(["tr-hot"]);
    expect(finishedKeys().sort()).toEqual(["tr-3", "tr-4"]);
    expect(liveLogsStore.rowsById.has(1)).toBe(false);
    expect(liveLogsStore.rowsById.has(2)).toBe(false);
    expect(liveLogsStore.rowsById.has(3)).toBe(true);
    expect(liveLogsStore.rowsById.has(4)).toBe(true);
    expect(liveLogsStore.selectDetail({ kind: "row_id", id: 1 })).toBeNull();
    expect(liveLogsStore.selectDetail({ kind: "row_id", id: 2 })).toBeNull();
    expect(liveLogsStore.selectDetail({ kind: "row_id", id: 4 })?.attemptKey).toBe("tr-4");
    expect(liveLogsStore.requestGroups.has("req-1")).toBe(false);
    expect(liveLogsStore.requestGroups.has("req-2")).toBe(false);
    expect(liveLogsStore.requestGroups.has("req-4")).toBe(true);
  });

  it("ties on startedAtMs are broken by insertion order (oldest inserted evicted first)", () => {
    resetStore();
    const same = "2026-01-01T00:00:00.000Z";
    dispatchUsageRow(1, makeRow({ id: 1, trace_id: "tr-a", created_at: same }));
    dispatchUsageRow(2, makeRow({ id: 2, trace_id: "tr-b", created_at: same }));

    liveLogsStore.enforceCapacity(1);

    expect(finishedKeys()).toEqual(["tr-b"]);
    expect(liveLogsStore.rowsById.has(1)).toBe(false);
    expect(liveLogsStore.rowsById.has(2)).toBe(true);
  });

  it("enforceCapacity(0) evicts every terminal attempt but never the inflight ones", () => {
    resetStore();
    dispatchAttemptEvent(1, makeEvent({ attempt_key: "tr-hot", stage: "streaming", stage_seq: 3, stage_rank: 3, event_time: 2_000, started_at: 1_000 }));
    dispatchUsageRow(2, makeRow({ id: 1, trace_id: "tr-fin", status_code: 200 }));

    liveLogsStore.enforceCapacity(0);

    expect(finishedKeys()).toEqual([]);
    expect(inflightKeys()).toEqual(["tr-hot"]);
    expect(liveLogsStore.rowsById.size).toBe(0);
  });

  it("default capacity keeps MAX_STORED_ROWS terminal attempts", () => {
    resetStore();
    for (let i = 1; i <= MAX_STORED_ROWS + 1; i++) {
      const a = makeSnapshotAttempt({ attemptKey: `tr-bulk-${i}`, startedAtMs: i, updatedAtMs: i, terminal: true });
      liveLogsStore.attemptsByKey.set(a.attemptKey, a); // public map, same pattern as the existing suite
    }

    liveLogsStore.enforceCapacity();

    expect(liveLogsStore.attemptsByKey.size).toBe(MAX_STORED_ROWS);
    expect(liveLogsStore.attemptsByKey.has("tr-bulk-1")).toBe(false);
    expect(liveLogsStore.attemptsByKey.has(`tr-bulk-${MAX_STORED_ROWS + 1}`)).toBe(true);
  });
});

describe("liveLogsStore retries and usage rows", () => {
  it("retries of the same request stay separate attempts with their own rows", () => {
    resetStore();
    dispatchAttemptEvent(1, makeEvent({ attempt_key: "tr-r1", request_id: "req-r", trace_id: "tr-r1", stage: "streaming", stage_seq: 3, stage_rank: 3, event_time: 1_200, started_at: 1_000 }));
    dispatchUsageRow(2, makeRow({ id: 11, trace_id: "tr-r1", request_id: "req-r", status_code: 200 }));
    dispatchAttemptEvent(3, makeEvent({ attempt_key: "tr-r2", request_id: "req-r", trace_id: "tr-r2", stage: "streaming", stage_seq: 3, stage_rank: 3, event_time: 2_200, started_at: 2_000 }));
    dispatchUsageRow(4, makeRow({ id: 12, trace_id: "tr-r2", request_id: "req-r", status_code: 200 }));

    expect(liveLogsStore.requestGroups.get("req-r")).toEqual(new Set(["tr-r1", "tr-r2"]));
    const first = liveLogsStore.selectDetail({ kind: "row_id", id: 11 });
    const second = liveLogsStore.selectDetail({ kind: "row_id", id: 12 });
    expect(first?.attemptKey).toBe("tr-r1");
    expect(second?.attemptKey).toBe("tr-r2");
    expect(first?.row?.id).toBe(11);
    expect(second?.row?.id).toBe(12);
    expect(first?.row).not.toBe(second?.row);
    expect(finishedKeys().sort()).toEqual(["tr-r1", "tr-r2"]);
  });

  it("a usage row renames an unknown-key attempt and keeps its live phase", () => {
    resetStore();
    // Placeholder attempt without trace_id (no factory: trace_id must be absent).
    liveLogsStore.dispatch({
      type: "attempt_event",
      cursor: 1,
      event: {
        attempt_key: "req-u:unknown",
        request_id: "req-u",
        stage: "streaming",
        stage_seq: 3,
        stage_rank: 3,
        event_time: 1_200,
        started_at: 1_000,
        terminal: false,
      },
    });

    dispatchUsageRow(2, makeRow({ id: 3, trace_id: "tr-u", request_id: "req-u", total_ms: 250 }));

    expect(liveLogsStore.attemptsByKey.has("req-u:unknown")).toBe(false);
    const a = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-u" });
    expect(a?.attemptKey).toBe("tr-u");
    expect(a?.rowId).toBe(3);
    expect(a?.terminal).toBe(true);
    expect(a?.startedAtMs).toBe(1_000); // phase timeline preserved
    expect(a?.updatedAtMs).toBe(1_250); // 1000 + total_ms(250)
    expect(liveLogsStore.requestGroups.get("req-u")).toEqual(new Set(["tr-u"]));
  });

  it("a row whose attempt already exists anchors without restoring the unknown placeholder's phase", () => {
    resetStore();
    // First the trace-keyed attempt establishes its own phase (rank 2).
    dispatchAttemptEvent(1, makeEvent({ attempt_key: "tr-d", request_id: "req-d", trace_id: "tr-d", stage: "waiting_ttft", stage_seq: 2, stage_rank: 2, event_time: 1_500, started_at: 1_000 }));
    // Then a stray placeholder is created. The usage row redirects & anchors.
    liveLogsStore.dispatch({
      type: "attempt_event",
      cursor: 2,
      event: {
        attempt_key: "req-d:unknown",
        request_id: "req-d",
        stage: "started",
        stage_seq: 0,
        stage_rank: 0,
        event_time: 1_300,
        started_at: 1_000,
        terminal: false,
      },
    });

    const before = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-d" });
    expect(before?.stage).toBe("waiting_ttft");
    expect(before?.stageRank).toBe(2);

    dispatchUsageRow(3, makeRow({ id: 4, trace_id: "tr-d", request_id: "req-d", status_code: 200 }));

    expect(liveLogsStore.attemptsByKey.size).toBe(1);
    expect(liveLogsStore.attemptsByKey.has("req-d:unknown")).toBe(false);
    const after = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-d" });
    expect(after?.stage).toBe("completed");
    expect(after?.rowId).toBe(4);
    expect(after?.startedAtMs).toBe(1_000);
  });
});

describe("liveLogsStore legacy envelope normalization", () => {
  it("legacy stage events become attempt_events keyed by trace_id with derived timing", () => {
    resetStore();
    const t0 = Date.now();
    liveLogsStore.dispatch({
      type: "stage",
      data: makeStageEvent({ request_id: "req-l", trace_id: "tr-l", stage: "streaming", elapsed_ms: 250, connect_ms: 30, ttft_ms: 90 }),
    });
    const t1 = Date.now();

    const a = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-l" });
    expect(a?.attemptKey).toBe("tr-l");
    expect(a?.stage).toBe("streaming");
    expect(a?.stageRank).toBe(3);
    expect(a?.stageSeq).toBe(3); // legacy reuses rank as seq
    expect(a?.elapsedMsAtEvent).toBe(250);
    expect(a?.startedAtMs).toBe((a?.updatedAtMs ?? 0) - 250);
    expect(a?.connectMs).toBe(30);
    expect(a?.ttftMs).toBe(90);
    expect(a?.updatedAtMs).toBeGreaterThanOrEqual(t0);
    expect(a?.updatedAtMs).toBeLessThanOrEqual(t1);
    expect(liveLogsStore.lastAppliedCursor).toBe(0); // legacy envelopes carry no cursor
  });

  it("legacy stage events without trace_id use the unknown placeholder and redirect when the trace arrives", () => {
    resetStore();
    liveLogsStore.dispatch({
      type: "stage",
      data: makeStageEvent({ request_id: "req-r2", trace_id: "", stage: "started", elapsed_ms: 0 }),
    });
    expect(liveLogsStore.attemptsByKey.has("req-r2:unknown")).toBe(true);

    liveLogsStore.dispatch({
      type: "stage",
      data: makeStageEvent({ request_id: "req-r2", trace_id: "tr-r2", stage: "streaming", elapsed_ms: 300 }),
    });

    expect(liveLogsStore.attemptsByKey.size).toBe(1);
    expect(liveLogsStore.attemptsByKey.has("req-r2:unknown")).toBe(false);
    const a = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-r2" });
    expect(a?.stage).toBe("streaming");
    expect(a?.traceId).toBe("tr-r2");
  });

  it("legacy stage events derive terminality from stage, status and error", () => {
    resetStore();
    liveLogsStore.dispatch({
      type: "stage",
      data: makeStageEvent({ request_id: "req-e", trace_id: "tr-e", stage: "streaming", status_code: 500, elapsed_ms: 400 }),
    });

    expect(inflightKeys()).toEqual([]);
    expect(finishedKeys()).toEqual(["tr-e"]);
    const a = liveLogsStore.selectDetail({ kind: "attempt", attemptKey: "tr-e" });
    expect(a?.terminalKind).toBe("failed");
    expect(a?.statusCode).toBe(500);
  });

  it("legacy row and history envelopes hydrate the store like their V2 counterparts", () => {
    resetStore();
    const row1 = makeRow({ id: 1, trace_id: "tr-h1", status_code: 200 });
    const row2 = makeRow({ id: 2, trace_id: "tr-h2", status_code: 404, error_message: "nope" });
    liveLogsStore.dispatch({ type: "history", rows: [row1, row2] });

    expect(liveLogsStore.connectionStatus).toBe("connected");
    expect(liveLogsStore.rowsById.size).toBe(2);
    expect(finishedKeys().sort()).toEqual(["tr-h1", "tr-h2"]);
    const failed = liveLogsStore.selectDetail({ kind: "row_id", id: 2 });
    expect(failed?.terminalKind).toBe("failed");
    expect(failed?.error).toBe("nope");

    liveLogsStore.dispatch({ type: "row", data: makeRow({ id: 3, trace_id: "tr-h3", status_code: 200 }) });
    expect(liveLogsStore.rowsById.has(3)).toBe(true);
    expect(liveLogsStore.attemptKeyByRowId.get(3)).toBe("tr-h3");
  });

  it("pong accepts numeric and ISO string server_time and tolerates garbage without crashing", () => {
    resetStore();
    const t0 = Date.now();
    liveLogsStore.dispatch({ type: "pong", server_time: 5_000_000 });
    const t1 = Date.now();
    expect(liveLogsStore.lastServerNow).toBe(5_000_000);
    expect(liveLogsStore.clockOffsetMs).toBeGreaterThanOrEqual(t0 - 5_000_000);
    expect(liveLogsStore.clockOffsetMs).toBeLessThanOrEqual(t1 - 5_000_000);

    const iso = "2026-01-01T00:00:00.000Z";
    liveLogsStore.dispatch({ type: "pong", server_time: iso });
    expect(liveLogsStore.lastServerNow).toBe(Date.parse(iso));

    const g0 = Date.now();
    liveLogsStore.dispatch({ type: "pong", server_time: "garbage" }); // tolerated: falls back to now
    const g1 = Date.now();
    expect(liveLogsStore.lastServerNow).toBeGreaterThanOrEqual(g0);
    expect(liveLogsStore.lastServerNow).toBeLessThanOrEqual(g1);
  });
});

describe("liveLogsStore malformed payloads", () => {
  // Decision on malformed payloads: the store tolerates non-object, unknown
  // and log-only envelopes (no-ops) — covered below. It does NOT tolerate
  // structurally broken V2 payloads that violate LiveLogEnvelopeV2, e.g.
  // `{type:"usage_row", row:null}` (TypeError on row.id) or
  // `{type:"history", rows:null}` (TypeError on iteration). Those violate
  // the type contract, so per task rules they are documented here instead
  // of being pinned with speculative tests.

  it("ignores non-object, unknown and log-only envelopes without touching state", () => {
    resetStore();
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const garbage: unknown[] = [
      null,
      undefined,
      42,
      "envelope",
      [],
      {},
      { type: "mystery" },
      { type: "lag_warning" },
      { type: "resync" },
    ];

    for (const g of garbage) {
      expect(() => liveLogsStore.dispatch(g)).not.toThrow();
    }

    expect(liveLogsStore.attemptsByKey.size).toBe(0);
    expect(liveLogsStore.rowsById.size).toBe(0);
    expect(liveLogsStore.lastAppliedCursor).toBe(0);
    expect(liveLogsStore.connectionStatus).toBe("disconnected");
    warn.mockRestore();
  });

  it("error envelopes are logged and leave state untouched", () => {
    resetStore();
    const err = vi.spyOn(console, "error").mockImplementation(() => {});
    liveLogsStore.dispatch({ type: "error", message: "boom" });
    expect(liveLogsStore.attemptsByKey.size).toBe(0);
    expect(liveLogsStore.lastAppliedCursor).toBe(0);
    err.mockRestore();
  });
});

describe("liveLogsStore inflight expiry (fake timers)", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it("inflight attempts expire only after 30 minutes of silence", () => {
    vi.useFakeTimers({ now: 1_700_000_000_000, toFake: ["Date"] });
    resetStore();
    dispatchAttemptEvent(1, makeEvent({
      attempt_key: "tr-exp", trace_id: "tr-exp", request_id: "req-exp",
      stage: "streaming", stage_seq: 3, stage_rank: 3,
      event_time: 1_700_000_000_000, started_at: 1_700_000_000_000,
    }));

    // Exactly 30m of silence: still alive (guard is strict >).
    vi.setSystemTime(1_700_000_000_000 + 1_800_000);
    expect(inflightKeys()).toEqual(["tr-exp"]);

    // 1ms past the boundary: expired as a failed attempt.
    vi.setSystemTime(1_700_000_000_000 + 1_800_001);
    expect(inflightKeys()).toEqual([]);
    const finished = liveLogsStore.selectFinishedRows();
    expect(finished.length).toBe(1);
    expect(finished[0]?.attemptKey).toBe("tr-exp");
    expect(finished[0]?.terminalKind).toBe("failed");
    expect(finished[0]?.error).toBe("Inflight timeout (stale request)");
  });
});
