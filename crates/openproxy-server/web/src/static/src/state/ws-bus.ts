// state/ws-bus.ts — minimal typed pub/sub for WebSocket messages, by type.
//
// Background: the live-logs view (`views/logs.ts`) historically owned the
// single WS connection and routed every message through `handleLogsMessage`.
// That handler is tailored to logs (`history`, `row`, `stage`, `lag_warning`,
// `resync`, `pong`, `error`). New views that need real-time WS data (the
// notifications tray F4, the live-store F5) had no way to listen without
// re-routing through the logs view.
//
// F2 introduces this tiny bus so any module can subscribe to a specific
// `WsEnvelope.type` and receive the envelope. The existing logs handler
// continues to run unchanged — `dispatchWs(msg)` is called from
// `state/ws.ts` immediately after `messageHandler(event)` returns. The
// logs handler sees the message first (preserving the existing semantics
// — e.g. the logs view updates `lastSeenId` from each `row` before any
// subscriber sees it), then the bus fans the same message out to all
// registered listeners for that `type`.
//
// Thread-safety: this runs in the browser, so single-threaded. The
// `Map<string, Set<WsHandler>>` is module-local state. Subscribers MUST
// be resilient to being called multiple times for the same message (the
// logs handler and the bus both receive every message — subscribers
// should not assume exclusivity).
//
// Lifecycle: subscriptions are returned as an unsubscribe function. There
// is no built-in "dispose all" — the bus is process-global. Subscribers
// are responsible for unsubscribing when their view unmounts to avoid
// leaks (the closure would otherwise keep the view's state alive via the
// handler's captured variables).

import type { WsEnvelope } from "../views/logs.js";

/** A subscriber callback. Receives the full envelope; the subscriber is
 *  responsible for narrowing `data` based on `type` (or just reading the
 *  top-level fields like `skipped`, `since_id`, `channel`). */
export type WsHandler = (msg: WsEnvelope) => void;

// Map of `type` → set of handlers. We use `Set` (not `Array`) so
// unsubscribing is O(1) and double-subscribing the same function is a
// no-op (idempotent). The `!` on the `set!.delete(...)` return is safe
// because we just looked up the set in the map.
const handlers = new Map<string, Set<WsHandler>>();

/** Subscribe to WS envelopes of a specific `type`. Returns an unsubscribe
 *  function — call it when the view unmounts to avoid leaks.
 *
 *  Example:
 *  ```ts
 *  const off = subscribeWs("notification", (msg) => {
 *    const evt = msg.data as NotificationEvent;
 *    state.notifications.tray.unshift(evt);
 *  });
 *  // later, on unmount:
 *  off();
 *  ```
 *
 *  The `type` argument is `string` (not the `WsEnvelope["type"]` union)
 *  so subscribers can listen for arbitrary future types without a type-
 *  assertion. The bus does not validate that the type is a known one —
 *  unknown types simply never fire. */
export function subscribeWs(type: string, fn: WsHandler): () => void {
  let set = handlers.get(type);
  if (!set) {
    set = new Set<WsHandler>();
    handlers.set(type, set);
  }
  set.add(fn);
  return () => {
    const s = handlers.get(type);
    if (s) {
      s.delete(fn);
      // Don't delete the empty set from the map — a future subscribe
      // for the same type would re-create it anyway, and leaving the
      // empty set avoids a map mutation on every unsubscribe (which
      // matters if a view toggles mount/unmount frequently).
    }
  };
}

/** Dispatch a parsed WS envelope to all subscribers registered for
 *  `msg.type`. Called from `state/ws.ts` after the logs handler has run.
 *  Failures in one subscriber do not block other subscribers — each is
 *  wrapped in try/catch and logged to the console. */
export function dispatchWs(msg: WsEnvelope): void {
  const set = handlers.get(msg.type);
  if (!set) return;
  for (const fn of set) {
    try {
      fn(msg);
    } catch (err) {
      // Same defensive pattern as `state/ws.ts`'s messageHandler
      // wrapper: a single broken subscriber must not break the bus for
      // the rest. The error is logged so the operator notices.
      console.error(
        "[openproxy] ws-bus subscriber for type",
        msg.type,
        "threw:",
        err,
      );
    }
  }
}

/** Test-only helper: clears all subscribers. Used in unit tests to
 *  isolate test cases. No-op in production. */
export function _clearWsBusForTests(): void {
  handlers.clear();
}
