// The only background poll left is the health tick below, one lightweight
// request every 3s to keep the sidebar health pill live. Views fetch their own
// data at mount and after every user mutation (see handlers/*-handlers.ts,
// which call `rerenderCurrentView()`), because a global data poll re-painted
// the DOM under inputs and stole focus. A view that needs live numbers
// subscribes to a state slice in its own mount and patches that node. Live
// logs go through the WebSocket in state/ws.ts.
//
// The tick is setTimeout-recursive, not setInterval: the next tick is
// scheduled in `finally`, after the await settles, so a single call is ever
// in flight.

import { state, setPollHandle } from "./index.js";
import { renderSidebar } from "../components/sidebar.js";
import { triggerServerReconnect } from "./server-reconnect.js";

const POLL_MS = 3000;

/** `/admin/health` payload. `message` is informational, shown in tooltips. */
interface HealthPayload {
  status: string;
  message?: string;
}

async function healthTick(): Promise<void> {
  try {
    // The public endpoint, not /admin/api/health: it is unauthenticated (the
    // LB liveness probe), so the pill works on the login page too.
    const res: Response = await fetch("/admin/health");
    if (!res.ok) {
      if (res.status === 502 || res.status === 503 || res.status === 504) {
        triggerServerReconnect();
      }
      throw new Error(`${res.status}`);
    }
    const raw: unknown = await res.json();
    const health: HealthPayload | null = isHealthPayload(raw) ? raw : null;
    if (health) {
      state.health = health;
    } else {
      state.health = { status: "unknown" };
    }
    // Route the update through lit-html. Writing `#health-status` directly
    // lost to the next `renderSidebar()` from the router or the
    // notifications store, which re-rendered the stale `state.health`.
    renderSidebar();
  } catch (err: unknown) {
    // If fetch failed completely (network down / server stopped)
    if (err instanceof TypeError) {
      triggerServerReconnect();
    }
    /* swallow — next tick will try again */
  }
  finally {
    if (state && state.__healthPollActive) {
      if (state.__healthPollHandle != null) clearTimeout(state.__healthPollHandle);
      state.__healthPollHandle = setTimeout(healthTick, POLL_MS);
    }
  }
}

/** Narrow an `unknown` to HealthPayload. A bad payload leaves the pill on its
 *  placeholder until the next tick. */
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
