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
  endpointKind: string | null;
}

export type LiveLogEnvelopeV2 =
  | { type: "snapshot"; cursor: number; server_now: number; rows: RecentUsageRow[]; attempts: AttemptState[] }
  | { type: "inflight_sync"; server_now: number; attempts: AttemptState[] }
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
  endpoint_kind?: string;
}

// ----------------------------------------------------------------------------
// Helpers
// ----------------------------------------------------------------------------

const STAGE_RANK = {
  started: 0,
  connecting: 1,
  waiting_ttft: 2,
  streaming: 3,
  completed: 4,
  failed: 4,
  cancelled: 4,
} satisfies Record<string, number>;

function rankStage(stage: string): number {
  return (stage in STAGE_RANK) ? STAGE_RANK[stage as keyof typeof STAGE_RANK] : -1;
}

function isTerminalStage(stage: string): boolean {
  return stage === "completed" || stage === "failed" || stage === "cancelled";
}

function deriveTerminal(stage: string, statusCode: number | null | undefined, error: string | null | undefined): boolean {
  return isTerminalStage(stage)
    || (statusCode != null && statusCode >= 400)
    || (!!error && error.length > 0);
}

function deriveTerminalKind(stage: string, statusCode: number | null | undefined): "completed" | "failed" | "cancelled" | null {
  if (stage === "cancelled") return "cancelled";
  if (stage === "failed" || (statusCode != null && statusCode >= 400)) return "failed";
  if (stage === "completed") return "completed";
  return "failed"; // fallback for terminal with unknown stage
}

/** Monotonic counter for insertion order — used as a stable tiebreaker
 *  when multiple attempts share the same startedAtMs. */
let insertionCounter = 0;

// Extend AttemptState at runtime with a hidden ordering field.
// Not in the interface because it's store-internal.
const insertionOrder = new WeakMap<AttemptState, number>();

function getInsertionOrder(a: AttemptState): number {
  return insertionOrder.get(a) ?? 0;
}

export const MAX_STORED_ROWS = 2000;

// ----------------------------------------------------------------------------
// Store
// ----------------------------------------------------------------------------

class LiveLogsStore {
  public attemptsByKey = new Map<string, AttemptState>();
  public rowsById = new Map<number, RecentUsageRow>();
  public attemptKeyByRowId = new Map<number, string>();
  public requestGroups = new Map<string, Set<string>>();
  public attemptKeyRedirects = new Map<string, string>();

  public lastAppliedCursor = 0;
  public connectionStatus: "connecting" | "connected" | "recovering" | "recovering_failed" | "disconnected" = "disconnected";
  public lastServerNow = 0;
  public clockOffsetMs = 0;

  private evictAttempt(a: AttemptState) {
    this.attemptsByKey.delete(a.attemptKey);
    if (a.rowId != null) {
      this.rowsById.delete(a.rowId);
      this.attemptKeyByRowId.delete(a.rowId);
    }
    if (a.row && a.row.id != null) {
      this.rowsById.delete(a.row.id);
      this.attemptKeyByRowId.delete(a.row.id);
    }
    const group = this.requestGroups.get(a.requestId);
    if (group) {
      group.delete(a.attemptKey);
      if (group.size === 0) {
        this.requestGroups.delete(a.requestId);
      }
    }
    this.attemptKeyRedirects.delete(a.attemptKey);
  }

  public enforceCapacity(maxRows = MAX_STORED_ROWS) {
    const finished: AttemptState[] = [];
    for (const a of this.attemptsByKey.values()) {
      if (a.terminal) {
        finished.push(a);
      }
    }

    if (finished.length <= maxRows) return;

    finished.sort((a, b) => this.stableSort(b, a));

    const toEvict = finished.length - maxRows;
    for (let i = 0; i < toEvict; i++) {
      const a = finished[i];
      if (a) {
        this.evictAttempt(a);
      }
    }
  }

  // --------------------------------------------------------------------------
  // Actions
  // --------------------------------------------------------------------------

  public dispatch(envelope: unknown) {
    const v2 = this.normalizeWsEnvelope(envelope);
    if (!v2) return;

    if ("cursor" in v2 && v2.cursor > 0) {
      if (v2.cursor <= this.lastAppliedCursor && v2.type !== "snapshot") {
        return;
      }
    }

    switch (v2.type) {
      case "snapshot":
        this.applySnapshot(v2);
        break;
      case "inflight_sync":
        this.applyInflightSync(v2);
        break;
      case "attempt_event":
        this.applyAttemptEvent(v2.event);
        if (v2.cursor) this.lastAppliedCursor = v2.cursor;
        break;
      case "usage_row":
        this.applyUsageRow(v2.row);
        if (v2.cursor) this.lastAppliedCursor = v2.cursor;
        break;
      case "gap":
        this.connectionStatus = "recovering";
        break;
      case "pong":
        this.lastServerNow = v2.server_time;
        this.clockOffsetMs = Date.now() - v2.server_time;
        break;
      case "error":
        console.error("LiveLogsStore WS Error:", v2.message);
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

    this.attemptsByKey.clear();
    this.rowsById.clear();
    this.attemptKeyByRowId.clear();
    this.requestGroups.clear();
    this.attemptKeyRedirects.clear();

    // Hydrate server-side inflight attempts into proper AttemptState objects.
    // The server sends InflightAttempt with camelCase keys that mostly match,
    // but `row` is missing and must be set to null.
    for (const raw of snapshot.attempts) {
      const a: AttemptState = {
        attemptKey: raw.attemptKey || "",
        requestId: raw.requestId || "",
        traceId: raw.traceId || "",
        providerId: raw.providerId || "",
        upstreamModelId: raw.upstreamModelId || "",
        startedAtMs: raw.startedAtMs || 0,
        updatedAtMs: raw.updatedAtMs || 0,
        stage: (raw.stage || "started") as StageName,
        stageSeq: raw.stageSeq || 0,
        stageRank: raw.stageRank || 0,
        elapsedMsAtEvent: raw.elapsedMsAtEvent || 0,
        connectMs: raw.connectMs ?? null,
        ttftMs: raw.ttftMs ?? null,
        statusCode: raw.statusCode ?? null,
        terminal: raw.terminal || false,
        terminalKind: (raw.terminalKind as AttemptState["terminalKind"]) || null,
        error: raw.error ?? null,
        rowId: raw.rowId ?? null,
        row: null,  // Server never sends row data in snapshot attempts
        source: "snapshot",
        endpointKind: raw.endpointKind ?? (("endpoint_kind" in raw ? (raw as Record<string, unknown>)["endpoint_kind"] as string : null)) ?? null,
      };
      insertionOrder.set(a, ++insertionCounter);
      this.attemptsByKey.set(a.attemptKey, a);
      this.trackRequestGroup(a.requestId, a.attemptKey);
    }

    for (const row of snapshot.rows) {
      this.applyUsageRow(row);
    }
    this.enforceCapacity();
  }

  /** Merge authoritative inflight state from server (sent on broadcast lag).
   *  Unlike applySnapshot, this does NOT clear finished rows — it only
   *  replaces the inflight set with the server's truth. */
  private applyInflightSync(sync: Extract<LiveLogEnvelopeV2, { type: "inflight_sync" }>) {
    this.clockOffsetMs = Date.now() - sync.server_now;

    // Collect current inflight keys
    const oldInflight = new Set<string>();
    for (const [key, a] of this.attemptsByKey) {
      if (!a.terminal) oldInflight.add(key);
    }

    // Add/update server-authoritative inflight attempts
    const serverKeys = new Set<string>();
    for (const raw of sync.attempts) {
      const key = raw.attemptKey || "";
      serverKeys.add(key);

      const a: AttemptState = {
        attemptKey: key,
        requestId: raw.requestId || "",
        traceId: raw.traceId || "",
        providerId: raw.providerId || "",
        upstreamModelId: raw.upstreamModelId || "",
        startedAtMs: raw.startedAtMs || 0,
        updatedAtMs: raw.updatedAtMs || 0,
        stage: (raw.stage || "started") as StageName,
        stageSeq: raw.stageSeq || 0,
        stageRank: raw.stageRank || 0,
        elapsedMsAtEvent: raw.elapsedMsAtEvent || 0,
        connectMs: raw.connectMs ?? null,
        ttftMs: raw.ttftMs ?? null,
        statusCode: raw.statusCode ?? null,
        terminal: false,
        terminalKind: null,
        error: raw.error ?? null,
        rowId: null,
        row: null,
        source: "snapshot",
        endpointKind: raw.endpointKind ?? (("endpoint_kind" in raw ? (raw as Record<string, unknown>)["endpoint_kind"] as string : null)) ?? null,
      };
      insertionOrder.set(a, ++insertionCounter);
      this.attemptsByKey.set(key, a);
      this.trackRequestGroup(a.requestId, key);
    }

    // Remove stale inflight entries that the server no longer has
    for (const key of oldInflight) {
      if (!serverKeys.has(key)) {
        const a = this.attemptsByKey.get(key);
        if (a) this.evictAttempt(a);
      }
    }
    this.enforceCapacity();
  }

  private applyAttemptEvent(event: AttemptEventPayload) {
    // Redirect unknown-keyed attempts when trace_id arrives
    if (event.trace_id) {
      const unknownKey = `${event.request_id}:unknown`;
      if (this.attemptsByKey.has(unknownKey) && unknownKey !== event.attempt_key) {
        if (!this.attemptsByKey.has(event.attempt_key)) {
          const old = this.attemptsByKey.get(unknownKey)!;
          this.attemptsByKey.delete(unknownKey);
          const group = this.requestGroups.get(event.request_id);
          if (group) group.delete(unknownKey);

          old.attemptKey = event.attempt_key;
          old.traceId = event.trace_id;

          this.attemptsByKey.set(event.attempt_key, old);
          this.trackRequestGroup(event.request_id, event.attempt_key);
        } else {
          this.attemptsByKey.delete(unknownKey);
          const group = this.requestGroups.get(event.request_id);
          if (group) group.delete(unknownKey);
        }
      }
    }

    const existing = this.attemptsByKey.get(event.attempt_key);

    // If we already have a DB row for this attempt, skip phase updates
    if (existing && existing.rowId) return;

    // Terminal attempts stay terminal
    if (existing && existing.terminal && !event.terminal) return;

    // Out-of-order guard: only accept forward progression
    const eventRank = event.stage_rank;
    if (existing && !event.terminal) {
      if (existing.stageRank > eventRank) return;
    }

    const terminal = deriveTerminal(event.stage, event.status_code, event.error);

    const a: AttemptState = existing ? { ...existing } : {
      attemptKey: event.attempt_key,
      requestId: event.request_id,
      traceId: event.trace_id || "",
      providerId: event.provider_id || "",
      upstreamModelId: event.upstream_model_id || "",
      startedAtMs: event.started_at,
      updatedAtMs: event.event_time,
      stage: event.stage as StageName,
      stageSeq: event.stage_seq,
      stageRank: eventRank,
      elapsedMsAtEvent: event.event_time - event.started_at,
      connectMs: event.connect_ms ?? null,
      ttftMs: event.ttft_ms ?? null,
      statusCode: event.status_code ?? null,
      terminal,
      terminalKind: terminal ? deriveTerminalKind(event.stage, event.status_code) : null,
      error: event.error || null,
      rowId: null,
      row: null,
      source: "live",
      endpointKind: event.endpoint_kind ?? null,
    };

    // Merge into existing
    if (existing) {
      a.updatedAtMs = event.event_time;
      a.stage = event.stage as StageName;
      a.stageSeq = event.stage_seq;
      a.stageRank = eventRank;
      a.elapsedMsAtEvent = event.event_time - a.startedAtMs;
      a.terminal = terminal;
      if (terminal) a.terminalKind = deriveTerminalKind(event.stage, event.status_code);
      if (event.connect_ms != null) a.connectMs = event.connect_ms;
      if (event.ttft_ms != null) a.ttftMs = event.ttft_ms;
      if (event.status_code != null) a.statusCode = event.status_code;
      if (event.error != null) a.error = event.error;
      if (event.provider_id) a.providerId = event.provider_id;
      if (event.upstream_model_id) a.upstreamModelId = event.upstream_model_id;
      if (event.endpoint_kind) {
        a.endpointKind = event.endpoint_kind;
      }
    }

    if (!insertionOrder.has(a)) {
      insertionOrder.set(a, ++insertionCounter);
    }

    this.attemptsByKey.set(event.attempt_key, a);
    this.trackRequestGroup(a.requestId, a.attemptKey);
    if (terminal) {
      this.enforceCapacity();
    }
  }

  private applyUsageRow(row: RecentUsageRow) {
    this.rowsById.set(row.id, row);

    const attemptKey = row.trace_id || `${row.request_id}:unknown`;
    this.attemptKeyByRowId.set(row.id, attemptKey);

    // Redirect unknown-keyed attempts
    if (row.trace_id) {
      const unknownKey = `${row.request_id}:unknown`;
      if (this.attemptsByKey.has(unknownKey) && unknownKey !== attemptKey) {
        if (!this.attemptsByKey.has(attemptKey)) {
          const old = this.attemptsByKey.get(unknownKey)!;
          this.attemptsByKey.delete(unknownKey);
          const group = this.requestGroups.get(row.request_id);
          if (group) group.delete(unknownKey);

          old.attemptKey = attemptKey;
          old.traceId = row.trace_id;

          this.attemptsByKey.set(attemptKey, old);
          this.trackRequestGroup(row.request_id, attemptKey);
        } else {
          this.attemptsByKey.delete(unknownKey);
          const group = this.requestGroups.get(row.request_id);
          if (group) group.delete(unknownKey);
        }
      }
    }

    let a = this.attemptsByKey.get(attemptKey);
    if (!a) {
      const startedAt = Date.parse(row.created_at.endsWith("Z") ? row.created_at : row.created_at + "Z");
      a = {
        attemptKey,
        requestId: row.request_id,
        traceId: row.trace_id,
        providerId: row.provider_id,
        upstreamModelId: row.upstream_model_id,
        startedAtMs: startedAt,
        updatedAtMs: startedAt + row.total_ms,
        stage: (row.status_code >= 400 ? "failed" : "completed") as StageName,
        stageSeq: 9999,
        stageRank: 4,
        elapsedMsAtEvent: row.total_ms,
        connectMs: row.connect_ms,
        ttftMs: row.ttft_ms,
        statusCode: row.status_code,
        terminal: true,
        terminalKind: row.status_code >= 400 ? "failed" : "completed",
        error: row.error_message,
        rowId: row.id,
        row,
        source: "db",
        endpointKind: row.endpoint_kind || null,
      };
    } else {
      a.rowId = row.id;
      a.row = row;
      a.terminal = true;
      a.terminalKind = row.status_code >= 400 ? "failed" : "completed";
      a.stage = (row.status_code >= 400 ? "failed" : "completed") as StageName;
      a.stageSeq = 9999;
      a.stageRank = 4;
      a.updatedAtMs = a.startedAtMs + row.total_ms;
      a.elapsedMsAtEvent = row.total_ms;
      a.source = "db";
      if (row.endpoint_kind) a.endpointKind = row.endpoint_kind;
      if (row.upstream_model_id !== undefined) a.upstreamModelId = row.upstream_model_id || "";
      if (row.provider_id !== undefined) a.providerId = row.provider_id || "";
      if (row.trace_id !== undefined) a.traceId = row.trace_id || "";
      if (row.connect_ms !== undefined) a.connectMs = row.connect_ms;
      if (row.ttft_ms !== undefined) a.ttftMs = row.ttft_ms;
      if (row.status_code !== undefined) a.statusCode = row.status_code;
      if (row.error_message !== undefined) a.error = row.error_message;
    }

    if (!insertionOrder.has(a)) {
      insertionOrder.set(a, ++insertionCounter);
    }

    this.attemptsByKey.set(attemptKey, a);
    this.trackRequestGroup(row.request_id, attemptKey);
    this.enforceCapacity();
  }

  private trackRequestGroup(requestId: string, attemptKey: string) {
    if (!this.requestGroups.has(requestId)) {
      this.requestGroups.set(requestId, new Set());
    }
    this.requestGroups.get(requestId)!.add(attemptKey);
  }

  // --------------------------------------------------------------------------
  // Normalization (legacy WS envelopes → V2)
  // --------------------------------------------------------------------------

  private normalizeWsEnvelope(env: unknown): LiveLogEnvelopeV2 | null {
    if (typeof env !== "object" || env === null) return null;
    const e = env as Record<string, unknown>;

    // Already V2
    if (e["type"] === "snapshot" || e["type"] === "inflight_sync" || e["type"] === "attempt_event" || e["type"] === "usage_row" || e["type"] === "gap") {
      return env as LiveLogEnvelopeV2;
    }

    const now = Date.now() - this.clockOffsetMs;

    // Legacy stage event → attempt_event
    if (e["type"] === "stage" && e["data"]) {
      const d = e["data"] as StageEvent;
      const key = d.trace_id || `${d.request_id}:unknown`;
      const rank = rankStage(d.stage);
      const terminal = deriveTerminal(d.stage, d.status_code, d.error);
      const event: AttemptEventPayload = {
        attempt_key: key,
        request_id: d.request_id,
        stage_seq: rank, // use rank as seq for monotonic ordering
        stage_rank: rank,
        event_time: now,
        started_at: now - d.elapsed_ms,
        terminal,
        stage: d.stage,
      };
      if (d.trace_id) event.trace_id = d.trace_id;
      if (d.connect_ms != null) event.connect_ms = d.connect_ms;
      if (d.ttft_ms != null) event.ttft_ms = d.ttft_ms;
      if (d.error) event.error = d.error;
      if (d.status_code != null) event.status_code = d.status_code;
      if (d.provider_id) event.provider_id = d.provider_id;
      if (d.upstream_model_id) event.upstream_model_id = d.upstream_model_id;

      return {
        type: "attempt_event",
        cursor: 0,
        event,
      };
    }

    // Legacy row event → usage_row
    if (e["type"] === "row" && e["data"]) {
      return { type: "usage_row", cursor: 0, row: e["data"] as RecentUsageRow };
    }

    // Legacy history → snapshot (no inflight attempts)
    if (e["type"] === "history" && e["rows"]) {
      return {
        type: "snapshot",
        cursor: 0,
        server_now: now,
        rows: e["rows"] as RecentUsageRow[],
        attempts: [],
      };
    }

    if (e["type"] === "pong") {
      const st = e["server_time"];
      const t = typeof st === "string" ? Date.parse(st) : (Number(st) || Date.now());
      return { type: "pong", server_time: t };
    }

    if (e["type"] === "error" && e["message"]) {
      return { type: "error", message: String(e["message"]) };
    }

    // Legacy lag/resync — log but don't crash
    if (e["type"] === "lag_warning" || e["type"] === "resync") {
      console.warn("[openproxy] WS lag/resync:", e);
      return null;
    }

    return null;
  }

  // --------------------------------------------------------------------------
  // Selectors
  // --------------------------------------------------------------------------

  /** Stable sort comparator: startedAtMs desc, then insertion order desc */
  private stableSort(a: AttemptState, b: AttemptState): number {
    const dt = b.startedAtMs - a.startedAtMs;
    if (dt !== 0) return dt;
    return getInsertionOrder(b) - getInsertionOrder(a);
  }

  public selectLogRows(): AttemptState[] {
    const arr = Array.from(this.attemptsByKey.values());
    arr.sort((a, b) => this.stableSort(a, b));
    return arr;
  }

  public selectInflightRows(): AttemptState[] {
    const nowMs = Date.now() - this.clockOffsetMs;
    const arr: AttemptState[] = [];

    for (const a of this.attemptsByKey.values()) {
      if (a.terminal) continue;
      // Safety net: auto-expire stale inflight after 30m (1800s)
      if (a.updatedAtMs > 1_000_000_000_000 && (nowMs - a.updatedAtMs > 1_800_000)) {
        a.terminal = true;
        a.terminalKind = "failed";
        a.stage = "failed";
        a.error = a.error || "Inflight timeout (stale request)";
        continue;
      }
      arr.push(a);
    }

    arr.sort((a, b) => this.stableSort(a, b));
    return arr;
  }

  public selectFinishedRows(): AttemptState[] {
    const arr: AttemptState[] = [];
    for (const a of this.attemptsByKey.values()) {
      if (a.terminal) arr.push(a);
    }
    arr.sort((a, b) => this.stableSort(a, b));
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
      const payload = await api(`/usage/detail?${queryParam}`) as { row?: Record<string, unknown> };
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
    this.lastAppliedCursor = 0;
    insertionCounter = 0;
  }
}

export const liveLogsStore = new LiveLogsStore();
