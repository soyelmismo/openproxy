// checkout/dom.ts — tiny DOM helpers of the public checkout page.
// No framework here: the checkout is a hand-written page (CSP forbids
// inline scripts and the bundle stays deliberately dependency-free).

/** Lookup by id with an inferred element type. Throws when missing —
 *  a typo in a checkout id must fail loudly, not silently. */
export const $ = <T extends HTMLElement = HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error("checkout: missing element #" + id);
  return el as T;
};

export function show(id: string): void {
  $(id).classList.remove("hidden");
}

export function hide(id: string): void {
  $(id).classList.add("hidden");
}

export function err(id: string, message: string): void {
  const el = $(id);
  el.textContent = message;
  el.classList.remove("hidden");
}
