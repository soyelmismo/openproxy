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
});
