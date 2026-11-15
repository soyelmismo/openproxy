// checkout/model-estimate-popover.ts — the panel behind a model pill.
//
// The pill carries the estimate inline ("~136.8M tokens"); the panel explains
// where the number comes from (real per-1M rates + the modeled agent mix). It
// is a real DOM node — `role="tooltip"` in <body>, wired to the trigger with
// `aria-describedby`/`aria-expanded` — not a `title` attribute: a title is
// invisible to touch, unreachable by keyboard and unstyleable.
//
// Interaction contract:
//   * mouse: pointerenter opens, pointerleave closes, click pins it open;
//   * keyboard: focus opens, Escape closes and returns focus to the pill;
//   * touch: pointerenter is ignored (pointerType !== "mouse") and a tap
//     toggles — a tap can never both focus-open and click-close;
//   * a pointerdown outside, a scroll or a resize closes it.
//
// Lifecycle: the document/window listeners exist ONLY while a panel is open,
// one panel at a time, and closing removes the node. Nothing accumulates
// across card re-renders.

import type { EstimatePopover } from "./token-estimate.js";

type OpenReason = "hover" | "click" | "focus";

interface OpenState {
  trigger: HTMLElement;
  node: HTMLElement;
  close: (restoreFocus?: boolean) => void;
}

let active: OpenState | null = null;
let seq = 0;

/** True while a panel is on screen. */
export function modelEstimateOpen(): boolean {
  return active !== null;
}

/** Close the open panel (optionally returning focus to its pill). */
export function closeModelEstimate(restoreFocus = false): void {
  active?.close(restoreFocus);
}

function buildNode(content: EstimatePopover): HTMLElement {
  const node = document.createElement("div");
  node.className = "plan-est-pop";
  node.setAttribute("role", "tooltip");

  const title = document.createElement("div");
  title.className = "plan-est-title";
  title.textContent = content.title;
  node.appendChild(title);

  const headline = document.createElement("div");
  headline.className = "plan-est-headline";
  headline.textContent = content.headline;
  node.appendChild(headline);

  if (content.rows.length > 0) {
    const list = document.createElement("dl");
    list.className = "plan-est-rates";
    for (const row of content.rows) {
      const group = document.createElement("div");
      const term = document.createElement("dt");
      term.textContent = row.label;
      const value = document.createElement("dd");
      value.textContent = row.value;
      group.appendChild(term);
      group.appendChild(value);
      list.appendChild(group);
    }
    node.appendChild(list);
  }

  const note = document.createElement("p");
  note.className = "plan-est-note";
  note.textContent = content.note;
  node.appendChild(note);

  return node;
}

/** Fixed-position placement under the pill, flipped above when it would leave
 *  the viewport, clamped horizontally. */
function place(node: HTMLElement, trigger: HTMLElement): void {
  const margin = 8;
  const rect = trigger.getBoundingClientRect();
  const vw = window.innerWidth || 1024;
  const vh = window.innerHeight || 768;
  const width = node.offsetWidth || 280;
  const height = node.offsetHeight || 0;
  const left = Math.max(margin, Math.min(rect.left, vw - width - margin));
  let top = rect.bottom + 6;
  if (top + height > vh - margin && rect.top - height - 6 >= margin) {
    top = rect.top - height - 6;
  }
  node.style.left = Math.round(left) + "px";
  node.style.top = Math.round(top) + "px";
}

/**
 * Wire one pill. Listeners are attached once per pill at card-build time; the
 * panel node itself only exists while open.
 */
export function attachModelEstimate(trigger: HTMLElement, content: EstimatePopover): void {
  let state: OpenState | null = null;
  let reason: OpenReason | null = null;
  // A pointer interaction already owns this pill: the `focus` it causes must
  // not pre-open the panel, or the following `click` would close it again.
  let pointerActive = false;
  // `close(true)` focuses the pill; that focus must not re-open the panel.
  let restoring = false;

  function onKeyDown(e: KeyboardEvent): void {
    if (e.key !== "Escape" || !state) return;
    e.preventDefault();
    e.stopPropagation();
    close(true);
  }

  function onDocPointerDown(e: Event): void {
    const target = e.target;
    if (target instanceof Node && trigger.contains(target)) return;
    close();
  }

  function onViewportChange(): void {
    close();
  }

  function close(restoreFocus = false): void {
    if (!state) return;
    const { node } = state;
    document.removeEventListener("keydown", onKeyDown, true);
    document.removeEventListener("pointerdown", onDocPointerDown, true);
    window.removeEventListener("resize", onViewportChange);
    window.removeEventListener("scroll", onViewportChange, true);
    node.remove();
    if (trigger.getAttribute("aria-describedby") === node.id) {
      trigger.removeAttribute("aria-describedby");
    }
    trigger.setAttribute("aria-expanded", "false");
    state = null;
    reason = null;
    if (active && active.trigger === trigger) active = null;
    if (restoreFocus && trigger.isConnected) {
      restoring = true;
      trigger.focus();
      restoring = false;
    }
  }

  function open(why: OpenReason): void {
    if (state) {
      reason = why;
      return;
    }
    closeModelEstimate(); // one panel at a time
    const node = buildNode(content);
    node.id = "plan-est-" + ++seq;
    document.body.appendChild(node);
    place(node, trigger);
    trigger.setAttribute("aria-describedby", node.id);
    trigger.setAttribute("aria-expanded", "true");
    document.addEventListener("keydown", onKeyDown, true);
    document.addEventListener("pointerdown", onDocPointerDown, true);
    window.addEventListener("resize", onViewportChange);
    window.addEventListener("scroll", onViewportChange, true);
    reason = why;
    state = { trigger, node, close };
    active = state;
  }

  trigger.setAttribute("aria-expanded", "false");

  trigger.addEventListener("pointerenter", (e) => {
    const type = (e as PointerEvent).pointerType;
    if (type && type !== "mouse") return; // touch opens on tap, not on hover
    if (!state) open("hover");
  });

  trigger.addEventListener("pointerleave", (e) => {
    const type = (e as PointerEvent).pointerType;
    if (type && type !== "mouse") return;
    if (reason === "hover") close();
  });

  trigger.addEventListener("pointerdown", () => {
    pointerActive = true;
  });

  trigger.addEventListener("pointerup", () => {
    pointerActive = false;
  });

  trigger.addEventListener("focus", () => {
    if (pointerActive || restoring || state) return;
    open("focus");
  });

  trigger.addEventListener("blur", () => {
    close();
  });

  trigger.addEventListener("click", (e) => {
    // The pill lives inside a plan card; a tap must never bubble into a
    // selection or a form submit.
    e.preventDefault();
    e.stopPropagation();
    pointerActive = false;
    if (!state) {
      open("click");
      return;
    }
    if (reason === "hover" || reason === "focus") {
      reason = "click"; // pin what the pointer/focus already opened
      return;
    }
    close();
  });
}
