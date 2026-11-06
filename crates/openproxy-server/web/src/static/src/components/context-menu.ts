// components/context-menu.ts — one global right-click menu.
//
// A single fixed-position element managed at module level. Everything that
// wants a context menu (grid rows, accounts, combos, later: logs) calls
// openContextMenu() and gets the same behaviour: closes on click outside,
// Escape, scroll or resize; clamps to the viewport; entries carry optional
// keyboard shortcuts. Plain DOM rather than lit-html because it must
// outlive any re-render and position itself against the viewport.

export interface ContextMenuItem {
  /** Stable action id, echoed back through `run`. */
  action?: string | undefined;
  label?: string | undefined;
  shortcut?: string | undefined;
  danger?: boolean | undefined;
  disabled?: boolean | undefined;
  /** Renders a full-width rule instead of an entry. */
  separator?: boolean;
  run?: () => void;
}

let root: HTMLDivElement | null = null;
let closeHandlers: Array<() => void> = [];

function ensureRoot(): HTMLDivElement {
  if (root) return root;
  root = document.createElement("div");
  root.className = "ctx-menu";
  root.setAttribute("role", "menu");
  root.hidden = true;
  document.body.appendChild(root);
  return root;
}

function detach(): void {
  for (const off of closeHandlers) off();
  closeHandlers = [];
}

export function closeContextMenu(): void {
  if (!root) return;
  root.hidden = true;
  root.innerHTML = "";
  detach();
}

export function isContextMenuOpen(): boolean {
  return !!root && !root.hidden;
}

export function openContextMenu(opts: { x: number; y: number; items: readonly (ContextMenuItem | "separator")[] }): void {
  const el = ensureRoot();
  el.innerHTML = "";
  const frag = document.createDocumentFragment();
  for (const raw of opts.items) {
    if (raw === "separator" || (raw as ContextMenuItem).separator) {
      frag.appendChild(document.createElement("hr"));
      continue;
    }
    const it = raw as ContextMenuItem;
    const btn = document.createElement("button");
    btn.type = "button";
    btn.setAttribute("role", "menuitem");
    btn.textContent = it.label ?? "";
    if (it.danger) btn.classList.add("danger");
    if (it.disabled) btn.disabled = true;
    if (it.shortcut) {
      const kbd = document.createElement("kbd");
      kbd.textContent = it.shortcut;
      btn.appendChild(kbd);
    }
    btn.addEventListener("click", () => {
      closeContextMenu();
      it.run?.();
    });
    frag.appendChild(btn);
  }
  el.appendChild(frag);
  el.hidden = false;

  // Clamp after layout so a menu near the bottom-right edge opens inward.
  const w = el.offsetWidth, h = el.offsetHeight;
  el.style.left = `${Math.max(8, Math.min(opts.x, window.innerWidth - w - 8))}px`;
  el.style.top = `${Math.max(8, Math.min(opts.y, window.innerHeight - h - 8))}px`;

  detach();
  const onDocClick = (e: Event): void => {
    if (!el.contains(e.target as Node)) closeContextMenu();
  };
  const onKey = (e: KeyboardEvent): void => {
    if (e.key === "Escape") { e.stopPropagation(); closeContextMenu(); }
  };
  const onScrollOrResize = (): void => closeContextMenu();
  // Listener is added after the current event finishes unwinding so the very
  // click that opened the menu (contextmenu) does not immediately close it.
  setTimeout(() => {
    document.addEventListener("click", onDocClick, true);
    document.addEventListener("contextmenu", onDocClick, true);
    document.addEventListener("keydown", onKey, true);
    window.addEventListener("resize", onScrollOrResize);
    window.addEventListener("scroll", onScrollOrResize, true);
    closeHandlers = [
      () => document.removeEventListener("click", onDocClick, true),
      () => document.removeEventListener("contextmenu", onDocClick, true),
      () => document.removeEventListener("keydown", onKey, true),
      () => window.removeEventListener("resize", onScrollOrResize),
      () => window.removeEventListener("scroll", onScrollOrResize, true),
    ];
  }, 0);
}
