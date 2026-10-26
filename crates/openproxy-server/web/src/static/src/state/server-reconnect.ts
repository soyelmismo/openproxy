// state/server-reconnect.ts — detects 502/server restarts, displays a friendly
// non-intrusive reconnecting banner/overlay, polls the server liveness probe,
// and auto-reloads or recovers the dashboard as soon as openproxy comes back up.

let isReconnecting = false;
let pollTimer: ReturnType<typeof setTimeout> | null = null;
let overlayEl: HTMLElement | null = null;
let consecutiveSuccesses = 0;

const LIVENESS_URL = "/admin/health";
const POLL_INTERVAL_MS = 1500;

function createOverlay(): HTMLElement {
  const el = document.createElement("div");
  el.id = "server-reconnect-overlay";
  el.className = "server-reconnect-overlay";
  el.innerHTML = `
    <div class="server-reconnect-card">
      <div class="server-reconnect-spinner"></div>
      <div class="server-reconnect-text">
        <h3>Server is restarting...</h3>
        <p>Reconnecting to OpenProxy. The page will reload automatically once online.</p>
      </div>
      <button type="button" class="server-reconnect-btn" id="server-reconnect-now-btn">Retry now</button>
    </div>
  `;
  const btn = el.querySelector("#server-reconnect-now-btn");
  if (btn) {
    btn.addEventListener("click", () => {
      void checkServerLiveness();
    });
  }
  return el;
}

function showOverlay(): void {
  if (!overlayEl) {
    overlayEl = createOverlay();
    document.body.appendChild(overlayEl);
  }
  overlayEl.classList.add("visible");
}

export function hideOverlay(): void {
  if (overlayEl) {
    overlayEl.classList.remove("visible");
    setTimeout(() => {
      if (overlayEl && !isReconnecting) {
        overlayEl.remove();
        overlayEl = null;
      }
    }, 300);
  }
}

async function checkServerLiveness(): Promise<void> {
  try {
    const res = await fetch(LIVENESS_URL, {
      method: "GET",
      cache: "no-store",
      headers: { "Cache-Control": "no-cache" },
    });

    if (res.ok) {
      consecutiveSuccesses++;
      if (consecutiveSuccesses >= 1) {
        // Server is back! Auto reload the page cleanly
        isReconnecting = false;
        if (pollTimer) {
          clearTimeout(pollTimer);
          pollTimer = null;
        }
        const text = overlayEl?.querySelector(".server-reconnect-text p");
        if (text) text.textContent = "Connected! Reloading...";
        setTimeout(() => {
          window.location.reload();
        }, 300);
        return;
      }
    } else {
      consecutiveSuccesses = 0;
    }
  } catch {
    consecutiveSuccesses = 0;
  }

  if (isReconnecting) {
    pollTimer = setTimeout(() => {
      void checkServerLiveness();
    }, POLL_INTERVAL_MS);
  }
}

/** Trigger the auto-reconnecting workflow when a 502/503/504 or network outage is detected. */
export function triggerServerReconnect(): void {
  if (isReconnecting) return;
  isReconnecting = true;
  consecutiveSuccesses = 0;
  showOverlay();
  if (pollTimer) clearTimeout(pollTimer);
  pollTimer = setTimeout(() => {
    void checkServerLiveness();
  }, 1000);
}

/** Returns true if a server reconnect is currently underway. */
export function isServerReconnecting(): boolean {
  return isReconnecting;
}

/** Test hook to reset state */
export function _resetServerReconnectForTest(): void {
  isReconnecting = false;
  if (pollTimer) clearTimeout(pollTimer);
  pollTimer = null;
  consecutiveSuccesses = 0;
  if (overlayEl) {
    overlayEl.remove();
    overlayEl = null;
  }
}
