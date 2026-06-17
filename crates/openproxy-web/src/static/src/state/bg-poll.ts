// state/bg-poll.ts — background poll. After the user feedback that
// the 3s data poll was "destroying the UX" (re-paints under inputs,
// lost focus, etc.) we removed the data poll entirely. Each view is
// now responsible for fetching what it needs at mount time (see
// views/home.js, views/providers.js, etc.) and for re-fetching after
// any user mutation (see handlers/*-handlers.js, which call
// rerenderCurrentView()).
//
// What remains is a single lightweight health poll, ~1 endpoint
// every 3s, to keep the sidebar health pill live. The user
// explicitly asked for this: "no, solo dejamos polling a ese
// endpoint health que es liviano, para indicar en tiempo real el
// estado del backend". /web/api/health returns a tiny JSON object
// (`{status, message}`) so the cost is negligible.
//
// If a view needs live updates in the future (e.g. a "live quota"
// badge on a row), it should subscribe to a specific state slice
// in its own mount and patch that single node — not poll. Live
// logs are handled separately by the WebSocket in state/ws.ts.
//
// The pattern below is setTimeout-recursive (not setInterval), so
// the next tick is scheduled inside the previous tick's `finally`
// AFTER the await resolved. That keeps a single in-flight call at
// a time and avoids the classic setInterval(asyncFn) re-entrancy.

import { state, setPollHandle } from "./index.js";
import { api } from "./api.js";

const POLL_MS = 3000;

/** Health payload as returned by /web/api/health. Kept narrow
 *  because the pill only reads `.status`. The `message` field is
 *  informational and shown in tooltips. */
interface HealthPayload {
  status: string;
  message?: string;
}

async function healthTick(): Promise<void> {
  try {
    // `api()` returns `unknown` (Bun-style). We narrow to
    // HealthPayload | null with a defensive guard so the pill
    // stays a no-op if the server ever changes the shape.
    const raw: unknown = await api("/health");
    const health: HealthPayload | null = isHealthPayload(raw) ? raw : null;
    if (health) state.health = health;
    const pill: HTMLElement | null = document.getElementById("health-status");
    if (pill) {
      if (!health) { pill.className = "loading"; pill.textContent = "—"; }
      else if (health.status === "ok" || health.status === "healthy") {
        pill.className = "ok"; pill.textContent = "🟢 healthy";
      } else {
        pill.className = "error"; pill.textContent = "🔴 " + (health.status || "down");
      }
    }
  } catch (_e: unknown) { /* swallow — next tick will try again */ }
  finally {
    if (state && state.__healthPollActive) {
      if (state.__healthPollHandle != null) clearTimeout(state.__healthPollHandle);
      state.__healthPollHandle = setTimeout(healthTick, POLL_MS);
    }
  }
}

/** Narrow an `unknown` into the HealthPayload shape we expect
 *  from /web/api/health. The bg-poll never crashes on a bad
 *  payload — the pill just shows "—" until the next tick. */
function isHealthPayload(x: unknown): x is HealthPayload {
  if (typeof x !== "object" || x === null) return false;
  const o: Record<string, unknown> = x as Record<string, unknown>;
  if (typeof o["status"] !== "string") return false;
  if (o["message"] !== undefined && typeof o["message"] !== "string") return false;
  return true;
}

export function startBgPoll(): void {
  state.__healthPollActive = true;
  if (state.__healthPollHandle != null) {
    clearTimeout(state.__healthPollHandle);
    state.__healthPollHandle = null;
  }
  if (!state.__healthPollRunning) {
    state.__healthPollHandle = setTimeout(healthTick, 0);
  }
  setPollHandle(state.__healthPollHandle);
}

export function stopBgPoll(): void {
  state.__healthPollActive = false;
  if (state.__healthPollHandle != null) {
    clearTimeout(state.__healthPollHandle);
    state.__healthPollHandle = null;
  }
}
