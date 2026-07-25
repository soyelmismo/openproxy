import type { RecentUsageRow, StageEvent } from "../lib/types/api.js";
import { api } from "../lib/api.js";

// ----------------------------------------------------------------------------
// Types
// ----------------------------------------------------------------------------

export type StageName =
  | "started"
  | "connecting"
  | "waiting_ttft"
  | "streaming"
  | "completed"
  | "failed"
  | "cancelled";

export interface AttemptState {
  attemptKey: string;
  requestId: string;
  traceId: string;
  providerId: string;
  upstreamModelId: string;
  startedAtMs: number;
  updatedAtMs: number;
  stage: StageName;
  stageSeq: number;
  stageRank: number;
  elapsedMsAtEvent: number;
  connectMs: number | null;
  ttftMs: number | null;
  statusCode: number | null;
  terminal: boolean;
  terminalKind: "completed" | "failed" | "cancelled" | null;
  error: string | null;
  rowId: number | null;
  row: RecentUsageRow | null;
  detail?: Record<string, unknown> | null;
  source: "live" | "snapshot" | "db";
}

export type LiveLogEnvelopeV2 =
  | { type: "snapshot"; cursor: number; server_now: number; rows: RecentUsageRow[]; attempts: AttemptState[] }
  | { type: "attempt_event"; cursor: number; event: AttemptEventPayload }
  | { type: "usage_row"; cursor: number; row: RecentUsageRow }
  | { type: "gap"; from_cursor: number; to_cursor: number; reason: string }
  | { type: "pong"; server_time: number }
  | { type: "error"; message: string };

export interface AttemptEventPayload {
  attempt_key: string;
  request_id: string;
  trace_id?: string;
  stage_seq: number;
  stage_rank: number;
  event_time: number;
  started_at: number;
  terminal: boolean;
  stage: string;
  connect_ms?: number;
  ttft_ms?: number;
  error?: string;
  status_code?: number;
  provider_id?: string;
  upstream_model_id?: string;
}

// ----------------------------------------------------------------------------
// Store State
// ----------------------------------------------------------------------------

class LiveLogsStore {
  public attemptsByKey = new Map<string, AttemptState>();
  public rowsById = new Map<number, RecentUsageRow>();
  public attemptKeyByRowId = new Map<number, string>();
  public requestGroups = new Map<string, Set<string>>(); // request_id -> Set<attempt_key>
  public attemptKeyRedirects = new Map<string, string>();
  
  public lastAppliedCursor = 0;
  public connectionStatus: "connecting" | "connected" | "recovering" | "recovering_failed" | "disconnected" = "disconnected";
  public lastServerNow = 0;
  public clockOffsetMs = 0;

  // --------------------------------------------------------------------------
  // Actions
  // --------------------------------------------------------------------------

  public dispatch(envelope: unknown) {
    const v2Envelope = this.normalizeWsEnvelope(envelope);
    if (!v2Envelope) return;

    if ("cursor" in v2Envelope && v2Envelope.cursor > 0) {
      if (v2Envelope.cursor <= this.lastAppliedCursor && v2Envelope.type !== "snapshot") {
        return; // Ignore old events unless it's a snapshot overriding state
      }
    }

    switch (v2Envelope.type) {
      case "snapshot":
        this.applySnapshot(v2Envelope);
        break;
      case "attempt_event":
        this.applyAttemptEvent(v2Envelope.event);
        if (v2Envelope.cursor) this.lastAppliedCursor = v2Envelope.cursor;
        break;
      case "usage_row":
        this.applyUsageRow(v2Envelope.row);
        if (v2Envelope.cursor) this.lastAppliedCursor = v2Envelope.cursor;
        break;
      case "gap":
        this.connectionStatus = "recovering";
        // Here we would trigger a fetch for a snapshot, handled by ws.ts usually
        break;
      case "pong":
        this.lastServerNow = v2Envelope.server_time;
        this.clockOffsetMs = Date.now() - v2Envelope.server_time;
        break;
      case "error":
        console.error("LiveLogsStore WS Error:", v2Envelope.message);
        break;
    }
  }

  // --------------------------------------------------------------------------
  // Reducers
  // --------------------------------------------------------------------------

  private applySnapshot(snapshot: Extract<LiveLogEnvelopeV2, { type: "snapshot" }>) {
    this.lastAppliedCursor = snapshot.cursor;
    this.lastServerNow = snapshot.server_now;
    this.clockOffsetMs = Date.now() - snapshot.server_now;
    this.connectionStatus = "connected";

    // Rebuild state atomically
    this.attemptsByKey.clear();
    this.rowsById.clear();
    this.attemptKeyByRowId.clear();
    this.requestGroups.clear();
    this.attemptKeyRedirects.clear();

    for (const attempt of snapshot.attempts) {
      this.attemptsByKey.set(attempt.attemptKey, attempt);
      this.trackRequestGroup(attempt.requestId, attempt.attemptKey);
    }

    for (const row of snapshot.rows) {
      this.applyUsageRow(row);
    }
  }

  private applyAttemptEvent(event: AttemptEventPayload) {
    if (event.trace_id) {
      const unknownKey = `${event.request_id}:unknown`;
      if (this.attemptsByKey.has(unknownKey) && unknownKey !== event.attempt_key) {
        if (!this.attemptsByKey.has(event.attempt_key)) {
          const oldAttempt = this.attemptsByKey.get(unknownKey)!;
          this.attemptsByKey.delete(unknownKey);
          const group = this.requestGroups.get(event.request_id);
          if (group) group.delete(unknownKey);
          
          oldAttempt.attemptKey = event.attempt_key;
          oldAttempt.traceId = event.trace_id;
          
          this.attemptsByKey.set(event.attempt_key, oldAttempt);
          this.trackRequestGroup(event.request_id, event.attempt_key);
        }
      }
    }

    const existing = this.attemptsByKey.get(event.attempt_key);

    // If there's already a terminal row for this attempt, ignore phase updates
    if (existing && existing.rowId) {
      return;
    }

    // If existing is terminal, ignore non-terminal updates
    if (existing && existing.terminal && !event.terminal) {
      return;
    }

    // Sequence check for out of order non-terminal events
    if (existing && !event.terminal) {
      if (existing.stageSeq > event.stage_seq) {
        return;
      }
      if (existing.stageSeq === event.stage_seq && existing.stageRank >= event.stage_rank) {
        return;
      }
    }

    const newState: AttemptState = existing ? { ...existing } : {
      attemptKey: event.attempt_key,
      requestId: event.request_id,
      traceId: event.trace_id || "",
      providerId: event.provider_id || "",
      upstreamModelId: event.upstream_model_id || "",
      startedAtMs: event.started_at,
      updatedAtMs: event.event_time,
      stage: event.stage as StageName,
      stageSeq: event.stage_seq,
      stageRank: event.stage_rank,
      elapsedMsAtEvent: event.event_time - event.started_at,
      connectMs: event.connect_ms || null,
      ttftMs: event.ttft_ms || null,
      statusCode: event.status_code || null,
      terminal: event.terminal,
      terminalKind: event.terminal ? (event.stage as any) : null,
      error: event.error || null,
      rowId: null,
      row: null,
      source: "live"
    };

    // Update fields if merging
    if (existing) {
      newState.updatedAtMs = event.event_time;
      newState.stage = event.stage as StageName;
      newState.stageSeq = event.stage_seq;
      newState.stageRank = event.stage_rank;
      newState.elapsedMsAtEvent = event.event_time - newState.startedAtMs;
      newState.terminal = event.terminal;
      if (event.terminal) newState.terminalKind = event.stage as any;
      if (event.connect_ms != null) newState.connectMs = event.connect_ms;
      if (event.ttft_ms != null) newState.ttftMs = event.ttft_ms;
      if (event.status_code != null) newState.statusCode = event.status_code;
      if (event.error != null) newState.error = event.error;
    }

    this.attemptsByKey.set(event.attempt_key, newState);
    this.trackRequestGroup(newState.requestId, newState.attemptKey);
  }

  private applyUsageRow(row: RecentUsageRow) {
    this.rowsById.set(row.id, row);
    
    // Attempt key fallback if trace_id is empty
    const attemptKey = row.trace_id || `${row.request_id}:unknown`;
    this.attemptKeyByRowId.set(row.id, attemptKey);

    if (row.trace_id) {
      const unknownKey = `${row.request_id}:unknown`;
      if (this.attemptsByKey.has(unknownKey) && unknownKey !== attemptKey) {
        if (!this.attemptsByKey.has(attemptKey)) {
          const oldAttempt = this.attemptsByKey.get(unknownKey)!;
          this.attemptsByKey.delete(unknownKey);
          const group = this.requestGroups.get(row.request_id);
          if (group) group.delete(unknownKey);
          
          oldAttempt.attemptKey = attemptKey;
          oldAttempt.traceId = row.trace_id;
          
          this.attemptsByKey.set(attemptKey, oldAttempt);
          this.trackRequestGroup(row.request_id, attemptKey);
        }
      }
    }

    // A row from history or the usage feed always represents a finished
    // (or aborted) request that has been saved to the DB. Even if
    // `stream_complete` is false (e.g. client disconnected mid-stream),
    // the request is terminal.
    const isTerminal = true;

    let attempt = this.attemptsByKey.get(attemptKey);
    if (!attempt) {
      const startedAt = Date.parse(row.created_at.endsWith("Z") ? row.created_at : row.created_at + "Z");
      attempt = {
        attemptKey,
        requestId: row.request_id,
        traceId: row.trace_id,
        providerId: row.provider_id,
        upstreamModelId: row.upstream_model_id,
        startedAtMs: startedAt,
        updatedAtMs: startedAt + row.total_ms,
        stage: isTerminal ? (row.status_code >= 400 ? "failed" : "completed") as StageName : "started" as StageName,
        stageSeq: isTerminal ? 9999 : 0,
        stageRank: isTerminal ? 4 : 0,
        elapsedMsAtEvent: row.total_ms,
        connectMs: row.connect_ms,
        ttftMs: row.ttft_ms,
        statusCode: row.status_code,
        terminal: isTerminal,
        terminalKind: isTerminal ? (row.status_code >= 400 ? "failed" : "completed") : null,
        error: row.error_message,
        rowId: row.id,
        row: row,
        source: "db"
      };
    } else {
      attempt.rowId = row.id;
      attempt.row = row;
      if (isTerminal) {
        attempt.terminal = true;
        attempt.terminalKind = row.status_code >= 400 ? "failed" : "completed";
        attempt.stage = (row.status_code >= 400 ? "failed" : "completed") as StageName;
        attempt.stageSeq = 9999;
        attempt.stageRank = 4;
      }
      attempt.updatedAtMs = attempt.startedAtMs + row.total_ms;
      attempt.elapsedMsAtEvent = row.total_ms;
      attempt.source = "db";
      if (row.upstream_model_id !== undefined) attempt.upstreamModelId = row.upstream_model_id || "";
      if (row.provider_id !== undefined) attempt.providerId = row.provider_id || "";
      if (row.trace_id !== undefined) attempt.traceId = row.trace_id || "";
      if (row.connect_ms !== undefined) attempt.connectMs = row.connect_ms;
      if (row.ttft_ms !== undefined) attempt.ttftMs = row.ttft_ms;
      if (row.status_code !== undefined) attempt.statusCode = row.status_code;
      if (row.error_message !== undefined) attempt.error = row.error_message;
    }

    this.attemptsByKey.set(attemptKey, attempt);
    this.trackRequestGroup(row.request_id, attemptKey);
  }

  private trackRequestGroup(requestId: string, attemptKey: string) {
    if (!this.requestGroups.has(requestId)) {
      this.requestGroups.set(requestId, new Set());
    }
    this.requestGroups.get(requestId)!.add(attemptKey);
  }

  // --------------------------------------------------------------------------
  // Compatibility / Normalization
  // --------------------------------------------------------------------------

  private normalizeWsEnvelope(env: unknown): LiveLogEnvelopeV2 | null {
    if (typeof env !== "object" || env === null) return null;
    const typedEnv = env as Record<string, unknown>;

    if (typedEnv["type"] === "snapshot" || typedEnv["type"] === "attempt_event" || typedEnv["type"] === "usage_row" || typedEnv["type"] === "gap") {
      return typedEnv as unknown as LiveLogEnvelopeV2; // Already V2
    }

    // Legacy conversions
    const now = Date.now() - this.clockOffsetMs;
    if (typedEnv["type"] === "stage" && typedEnv["data"]) {
      const data = typedEnv["data"] as StageEvent;
      const attempt_key = data.trace_id || `${data.request_id}:unknown`;
      const stage_rank = this.rankStage(data.stage);
      const isTerminal = data.stage === "completed" || data.stage === "failed" || data.stage === "cancelled";
      return {
        type: "attempt_event",
        cursor: 0,
        event: {
          attempt_key,
          request_id: data.request_id,
          ...(data.trace_id ? { trace_id: data.trace_id } : {}),
          stage_seq: Math.floor(data.elapsed_ms), // fallback sequence
          stage_rank,
          event_time: now,
          started_at: now - data.elapsed_ms,
          terminal: isTerminal,
          stage: data.stage,
          ...(data.connect_ms != null ? { connect_ms: data.connect_ms } : {}),
          ...(data.ttft_ms != null ? { ttft_ms: data.ttft_ms } : {}),
          ...(data.error ? { error: data.error } : {}),
          ...(data.status_code != null ? { status_code: data.status_code } : {}),
          ...(data.provider_id ? { provider_id: data.provider_id } : {}),
          ...(data.upstream_model_id ? { upstream_model_id: data.upstream_model_id } : {})
        }
      };
    }

    if (typedEnv["type"] === "row" && typedEnv["data"]) {
      return {
        type: "usage_row",
        cursor: 0,
        row: typedEnv["data"] as RecentUsageRow
      };
    }

    if (typedEnv["type"] === "history" && typedEnv["rows"]) {
      // Treat history as a mini snapshot of rows
      return {
        type: "snapshot",
        cursor: 0,
        server_now: now,
        rows: typedEnv["rows"] as RecentUsageRow[],
        attempts: []
      };
    }

    if (typedEnv["type"] === "pong") {
      const server_time = typedEnv["server_time"];
      const st = typeof server_time === "string" ? Date.parse(server_time) : (Number(server_time) || Date.now());
      return { type: "pong", server_time: st } as unknown as LiveLogEnvelopeV2;
    }

    if (typedEnv["type"] === "error" && typedEnv["message"]) {
      return { type: "error", message: String(typedEnv["message"]) } as unknown as LiveLogEnvelopeV2;
    }

    return null;
  }

  private rankStage(stage: string): number {
    switch (stage) {
      case "started": return 0;
      case "connecting": return 1;
      case "waiting_ttft": return 2;
      case "streaming": return 3;
      case "completed":
      case "failed":
      case "cancelled": return 4;
      default: return -1;
    }
  }

  // --------------------------------------------------------------------------
  // Selectors
  // --------------------------------------------------------------------------

  public selectLogRows(): AttemptState[] {
    const arr = Array.from(this.attemptsByKey.values());
    arr.sort((a, b) => b.startedAtMs - a.startedAtMs);
    return arr;
  }

  public selectDetail(identity: { kind: "row_id", id: number } | { kind: "attempt", attemptKey: string }) {
    if (identity.kind === "row_id") {
      const attemptKey = this.attemptKeyByRowId.get(identity.id);
      if (attemptKey) return this.attemptsByKey.get(attemptKey);
    } else {
      return this.attemptsByKey.get(identity.attemptKey);
    }
    return null;
  }

  public setDetail(identity: { kind: "row_id", id: number } | { kind: "attempt", attemptKey: string }, detail: Record<string, unknown>) {
    const attempt = this.selectDetail(identity);
    if (attempt) {
      attempt.detail = detail;
    }
  }

  public async fetchLogDetail(id: string, traceId: string, fallbackAttemptKey: string): Promise<boolean> {
    const hasValidId = Boolean(id && id !== "0");
    const queryParam = hasValidId ? `id=${encodeURIComponent(id)}` : (traceId ? `trace_id=${encodeURIComponent(traceId)}` : "");
    if (!queryParam) return false;
    try {
      const payload = await api(`/usage/detail?${queryParam}`) as any;
      if (payload && payload.row) {
        this.setDetail(
          hasValidId ? { kind: "row_id", id: Number(id) } : { kind: "attempt", attemptKey: fallbackAttemptKey },
          payload.row
        );
        return true;
      }
    } catch {
      // Ignored
    }
    return false;
  }

  public clearForTest() {
    this.attemptsByKey.clear();
    this.rowsById.clear();
    this.attemptKeyByRowId.clear();
    this.requestGroups.clear();
    this.attemptKeyRedirects.clear();
  }
}

export const liveLogsStore = new LiveLogsStore();
