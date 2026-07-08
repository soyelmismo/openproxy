import type { RecentUsageRow, StageEvent } from "../lib/types/api.js";

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
  
  public lastAppliedCursor = 0;
  public connectionStatus: "connecting" | "connected" | "recovering" | "recovering_failed" | "disconnected" = "disconnected";
  public lastServerNow = 0;
  public clockOffsetMs = 0;

  // --------------------------------------------------------------------------
  // Actions
  // --------------------------------------------------------------------------

  public dispatch(envelope: LiveLogEnvelopeV2 | any) {
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

    for (const attempt of snapshot.attempts) {
      this.attemptsByKey.set(attempt.attemptKey, attempt);
      this.trackRequestGroup(attempt.requestId, attempt.attemptKey);
    }

    for (const row of snapshot.rows) {
      this.applyUsageRow(row);
    }
  }

  private applyAttemptEvent(event: AttemptEventPayload) {
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
    if (existing && existing.stageSeq >= event.stage_seq && !event.terminal) {
      return;
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
        stage: (row.status_code >= 400 ? "failed" : "completed") as StageName,
        stageSeq: 9999, // Terminal row wins over phases
        stageRank: 4,
        elapsedMsAtEvent: row.total_ms,
        connectMs: row.connect_ms,
        ttftMs: row.ttft_ms,
        statusCode: row.status_code,
        terminal: true,
        terminalKind: row.status_code >= 400 ? "failed" : "completed",
        error: row.error_message,
        rowId: row.id,
        row: row,
        source: "db"
      };
    } else {
      attempt.rowId = row.id;
      attempt.row = row;
      attempt.terminal = true;
      attempt.terminalKind = row.status_code >= 400 ? "failed" : "completed";
      attempt.updatedAtMs = attempt.startedAtMs + row.total_ms;
      attempt.elapsedMsAtEvent = row.total_ms;
      attempt.source = "db";
      if (row.connect_ms != null) attempt.connectMs = row.connect_ms;
      if (row.ttft_ms != null) attempt.ttftMs = row.ttft_ms;
      if (row.status_code != null) attempt.statusCode = row.status_code;
      if (row.error_message != null) attempt.error = row.error_message;
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

  private normalizeWsEnvelope(env: any): LiveLogEnvelopeV2 | null {
    if (env.type === "snapshot" || env.type === "attempt_event" || env.type === "usage_row" || env.type === "gap") {
      return env as LiveLogEnvelopeV2; // Already V2
    }

    // Legacy conversions
    const now = Date.now() - this.clockOffsetMs;
    if (env.type === "stage" && env.data) {
      const s = env.data as StageEvent;
      const attempt_key = s.trace_id || `${s.request_id}:unknown`;
      const stage_rank = this.rankStage(s.stage);
      const isTerminal = s.stage === "completed" || s.stage === "failed" || s.stage === "cancelled";
      return {
        type: "attempt_event",
        cursor: 0,
        event: {
          attempt_key,
          request_id: s.request_id,
          ...(s.trace_id ? { trace_id: s.trace_id } : {}),
          stage_seq: Math.floor(s.elapsed_ms), // fallback sequence
          stage_rank,
          event_time: now,
          started_at: now - s.elapsed_ms,
          terminal: isTerminal,
          stage: s.stage,
          ...(s.connect_ms != null ? { connect_ms: s.connect_ms } : {}),
          ...(s.ttft_ms != null ? { ttft_ms: s.ttft_ms } : {}),
          ...(s.error ? { error: s.error } : {}),
          ...(s.status_code != null ? { status_code: s.status_code } : {}),
          ...(s.provider_id ? { provider_id: s.provider_id } : {}),
          ...(s.upstream_model_id ? { upstream_model_id: s.upstream_model_id } : {})
        }
      };
    }

    if (env.type === "row" && env.data) {
      return {
        type: "usage_row",
        cursor: 0,
        row: env.data as RecentUsageRow
      };
    }

    if (env.type === "history" && env.rows) {
      // Treat history as a mini snapshot of rows
      return {
        type: "snapshot",
        cursor: 0,
        server_now: now,
        rows: env.rows,
        attempts: []
      };
    }

    if (env.type === "pong") {
      const st = typeof env.server_time === "string" ? Date.parse(env.server_time) : (env.server_time || Date.now());
      return { type: "pong", server_time: st };
    }

    if (env.type === "error" && env.message) {
      return { type: "error", message: env.message };
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
}

export const liveLogsStore = new LiveLogsStore();
