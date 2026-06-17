// lib/dom.ts — DOM helpers. All pure functions over Document/Element.

export type Child = Node | string | number | boolean | null | undefined;
export type AttrValue = string | number | boolean | null | undefined | Partial<CSSStyleDeclaration> | ((e: Event) => void);
export type Attrs = Readonly<Record<string, AttrValue>>;
export type Children = readonly Child[];

export function h(tag: string, attrs: Attrs = {}, ...children: Children[]): HTMLElement {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (k === "class") {
      if (v != null && v !== false) el.className = String(v);
    } else if (k === "style" && typeof v === "object" && v !== null) {
      Object.assign(el.style, v as Partial<CSSStyleDeclaration>);
    } else if (k.startsWith("on") && typeof v === "function") {
      el.addEventListener(k.slice(2).toLowerCase(), v as EventListener);
    } else if (v === true) {
      el.setAttribute(k, "");
    } else if (v === false || v == null) {
      // skip
    } else {
      el.setAttribute(k, String(v));
    }
  }
  for (const child of children.flat()) {
    if (child == null || child === false) continue;
    if (typeof child === "string" || typeof child === "number") {
      el.appendChild(document.createTextNode(String(child)));
    } else if (child instanceof Node) {
      el.appendChild(child);
    } else {
      el.appendChild(document.createTextNode(String(child)));
    }
  }
  return el;
}

// Mount an HTML string into a target element. `mode` controls whether
// we replace the contents or append. Centralised so views never have
// to call `innerHTML` directly (and so we have one place to swap for
// tagged templates later if we want).
export function mount(target: HTMLElement, html: string, mode: "replace" | "append" = "replace"): void {
  if (mode === "replace") target.innerHTML = html;
  else if (mode === "append") target.insertAdjacentHTML("beforeend", html);
}

// Append a modal's HTML to the modal root (a child of <body>) instead
// of the view's #main container. This is critical: any code that
// replaces #main's innerHTML (e.g. a handler calling
// rerenderCurrentView() after a user action) would otherwise
// destroy a modal that lived inside it. Mounting at <body> level
// means the modal survives all such re-renders.
//
// Lazily creates #modal-root on first use so we don't have to
// thread the element through app.js. Returns the inserted element
// for callers that need a handle (most don't — they reach it back
// via getElementById()).
export function appendModal(html: string): Element | null {
  let root = document.getElementById("modal-root");
  if (!root) {
    root = document.createElement("div");
    root.id = "modal-root";
    // z-index 1000 puts modals above the page chrome without
    // needing !important hacks. The .modal-bg rule in CSS already
    // uses position: fixed; this just ensures stacking order.
    root.style.cssText = "position:relative;z-index:1000;";
    document.body.appendChild(root);
  }
  root.insertAdjacentHTML("beforeend", html);
  // The freshly-inserted node is the last child of #modal-root.
  return root.lastElementChild;
}

// Click-on-backdrop helper. Wires `onClose` to clicks whose target is
// the backdrop element itself (not a child).
export function backdropClose(el: HTMLElement, onClose: () => void): void {
  el.addEventListener("click", (e: Event) => { if (e.target === el) onClose(); });
}

// Find the closest ancestor (or self) matching a selector. Falls back
// to `null` (instead of throwing) if there's no match — most call
// sites want to early-return in that case.
export function closest<T extends Element = Element>(root: Element | null | undefined, selector: string): T | null {
  return root ? (root.closest(selector) as T | null) : null;
}
