// state/bg-poll.js — background poll. After the user feedback that
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
// logs are handled separately by the WebSocket in state/ws.js.

import { state, setPollHandle } from "./index.js";
import { api } from "./api.js";

const POLL_MS = 3000;

async function healthTick() {
  try {
    const health = await api("/health").catch(() => null);
    if (health) state.health = health;
    const pill = document.getElementById("health-status");
    if (pill) {
      if (!health) { pill.className = "loading"; pill.textContent = "—"; }
      else if (health.status === "ok" || health.status === "healthy") {
        pill.className = "ok"; pill.textContent = "🟢 healthy";
      } else {
        pill.className = "error"; pill.textContent = "🔴 " + (health.status || "down");
      }
    }
  } catch (_) { /* swallow — next tick will try again */ }
  finally {
    if (state && state.__healthPollActive) {
      if (state.__healthPollHandle != null) clearTimeout(state.__healthPollHandle);
      state.__healthPollHandle = setTimeout(healthTick, POLL_MS);
    }
  }
}

export function startBgPoll() {
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

export function stopBgPoll() {
  state.__healthPollActive = false;
  if (state.__healthPollHandle != null) {
    clearTimeout(state.__healthPollHandle);
    state.__healthPollHandle = null;
  }
}
