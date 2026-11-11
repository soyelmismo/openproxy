// components/toast.ts — short-lived, non-blocking status messages. Toasts stack vertically
// in #toast-container with a per-toast dismiss control, hover/focus pause, and a cap on the
// visible stack so a burst of events can never blanket the controls underneath.
//
// Accessibility contract:
//   - The container is a labelled `region`; each toast carries its own live role (`status` for
//     info/success/warning, `alert` for errors) so screen readers announce the message once.
//   - The dismiss button is a real 24×24 target with a translated accessible name, and Escape
//     dismisses the focused toast.
//   - Timers pause while the pointer or keyboard focus is inside a toast, and while the tab is
//     hidden; resuming continues with the remaining time instead of restarting the full window.

import { html, render } from "lit-html";
import { icons } from "../lib/icons.js";
import { t } from "../i18n/index.js";

export type ToastType = "info" | "success" | "error" | "warning" | string;

type ToastTone = "info" | "success" | "error" | "warning";

/** Keep the visible stack short: three compact toasts fit above the fold on a phone and leave
 *  the controls underneath reachable. Older toasts are removed when a fourth arrives. */
const MAX_VISIBLE_TOASTS: number = 3;

/** Longer windows for messages the user must act on; short confirmations stay out of the way. */
const DEFAULT_DURATION_MS: Readonly<Record<ToastTone, number>> = {
  info: 4500,
  success: 3500,
  warning: 6000,
  error: 6500,
};

const KNOWN_TONES: ReadonlySet<string> = new Set<ToastTone>([
  "info",
  "success",
  "error",
  "warning",
]);

/** `warn` is an alias used by a few call sites; everything unknown falls back to `info` so the
 *  toast never renders without a tone class. */
function normalizeTone(type: ToastType): ToastTone {
  if (type === "warn") return "warning";
  return KNOWN_TONES.has(type) ? (type as ToastTone) : "info";
}

function toneIcon(tone: ToastTone) {
  switch (tone) {
    case "success":
      return icons.check();
    case "error":
      return icons.close();
    case "warning":
      return icons.warning();
    default:
      return icons.info();
  }
}

interface DismissibleToast extends HTMLDivElement {
  /** Set by `showToast`; lets the stack cap evict the oldest toast without leaking its timers. */
  __dismiss?: (immediate?: boolean) => void;
}

export function ensureToastContainer(): HTMLElement {
  let container = document.getElementById("toast-container");
  if (!container) {
    container = document.createElement("div");
    container.id = "toast-container";
    container.className = "toast-container";
    document.body.appendChild(container);
  }
  // Re-apply on every call: the container is removed when the last toast leaves, so a later
  // toast recreates it and must inherit the same semantics.
  container.setAttribute("role", "region");
  container.setAttribute("aria-label", t("notifications.title"));
  return container;
}

export function showToast(message: string, type: ToastType = "info", durationMs?: number): void {
  const container = ensureToastContainer();
  const tone: ToastTone = normalizeTone(type);
  const duration: number = durationMs ?? DEFAULT_DURATION_MS[tone];

  // Evict the oldest toast when the stack is full. This is the "does not block controls"
  // guarantee: the footprint is bounded even during a notification burst.
  while (container.children.length >= MAX_VISIBLE_TOASTS) {
    const oldest = container.firstElementChild as DismissibleToast | null;
    if (!oldest) break;
    oldest.__dismiss?.(true);
    if (container.firstElementChild === oldest) oldest.remove();
  }

  const toast: DismissibleToast = document.createElement("div");
  toast.className = `toast toast-${tone}`;
  toast.setAttribute("role", tone === "error" ? "alert" : "status");
  toast.setAttribute("aria-atomic", "true");

  let dismissed: boolean = false;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let remainingMs: number = Math.max(0, duration);
  let armedAt: number = 0;
  let paused: boolean = false;
  const pauseReasons: Set<string> = new Set();

  const clearTimer = (): void => {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
  };

  const remove = (): void => {
    clearTimer();
    document.removeEventListener("visibilitychange", onVisibilityChange);
    toast.remove();
    if (container.children.length === 0 && container.parentElement) {
      container.remove();
    }
  };

  const dismiss = (immediate: boolean = false): void => {
    if (dismissed) return;
    dismissed = true;
    clearTimer();
    document.removeEventListener("visibilitychange", onVisibilityChange);
    if (immediate) {
      remove();
      return;
    }
    toast.classList.remove("show");
    window.setTimeout(remove, 220);
  };
  toast.__dismiss = dismiss;

  const arm = (): void => {
    clearTimer();
    if (remainingMs <= 0) {
      dismiss();
      return;
    }
    armedAt = Date.now();
    timer = setTimeout(() => {
      timer = null;
      dismiss();
    }, remainingMs);
  };

  const pause = (): void => {
    if (paused) return;
    paused = true;
    if (timer !== null) {
      clearTimer();
      remainingMs = Math.max(0, remainingMs - (Date.now() - armedAt));
    }
  };

  const resume = (): void => {
    if (!paused) return;
    paused = false;
    arm();
  };

  const syncPause = (): void => {
    if (pauseReasons.size > 0) pause();
    else resume();
  };

  function onVisibilityChange(): void {
    if (document.hidden) pauseReasons.add("hidden");
    else pauseReasons.delete("hidden");
    syncPause();
  }

  render(
    html`
      <div class="toast-icon" aria-hidden="true">${toneIcon(tone)}</div>
      <div class="toast-content">${message}</div>
      <button
        type="button"
        class="toast-close"
        aria-label=${t("common.close")}
        title=${t("common.close")}
        @click=${() => dismiss()}
      >${icons.close()}</button>
    `,
    toast
  );

  container.appendChild(toast);

  requestAnimationFrame(() => {
    toast.classList.add("show");
  });

  toast.addEventListener("mouseenter", () => {
    pauseReasons.add("hover");
    syncPause();
  });
  toast.addEventListener("mouseleave", () => {
    pauseReasons.delete("hover");
    syncPause();
  });
  toast.addEventListener("focusin", () => {
    pauseReasons.add("focus");
    syncPause();
  });
  toast.addEventListener("focusout", (e: FocusEvent) => {
    // Moving focus between the toast's own controls must not resume the timer.
    if (e.relatedTarget instanceof Node && toast.contains(e.relatedTarget)) return;
    pauseReasons.delete("focus");
    syncPause();
  });
  toast.addEventListener("keydown", (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      dismiss();
    }
  });
  document.addEventListener("visibilitychange", onVisibilityChange);
  if (document.hidden) pauseReasons.add("hidden");

  arm();
}
