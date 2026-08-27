import { describe, it, expect, vi } from "vitest";
import { liveLogsStore } from "./live-logs-store.js";
import * as apiModule from "../lib/api.js";

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
