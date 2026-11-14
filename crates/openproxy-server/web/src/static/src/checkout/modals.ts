// checkout/modals.ts — in-page modals (Terms of Service, FAQ).
//
// The CSP is `script-src 'self'`: inline scripts on the checkout page are
// blocked, so this must live in the bundle served at /checkout.js. Triggers
// are `[data-modal="terms|faq"]` anchors — clicking one opens the matching
// `#terms-modal` / `#faq-modal` instead of navigating; Escape, the X and a
// backdrop click close, and the body scroll locks while a modal is open.
//
// The terms content is not fetched and not framed: the server injects it
// into the page itself (checkout_html replaces <!--TERMS_BODY-->), so the
// modal has zero network dependencies.

function close(openId: string | null): void {
  if (!openId) return;
  document.getElementById(openId)?.classList.add("hidden");
  document.body.style.overflow = "";
}

function open(name: string): void {
  close(current);
  const modal = document.getElementById(`${name}-modal`);
  if (!modal) return;
  modal.classList.remove("hidden");
  document.body.style.overflow = "hidden";
  current = modal.id;
  modal.querySelector<HTMLElement>(".close-btn")?.focus();
}

let current: string | null = null;

export function initCheckoutModals(): void {
  document.addEventListener("click", (e: Event) => {
    const target = e.target as Element | null;
    if (!target || typeof (target as Element).closest !== "function") return;
    const trigger = target.closest<HTMLElement>("[data-modal]");
    if (trigger) {
      e.preventDefault();
      open(trigger.dataset["modal"] ?? "");
      return;
    }
    if (target.closest("[data-close]")) {
      e.preventDefault();
      close(current);
      return;
    }
    // Only a click on the backdrop itself closes (not inside .modal).
    if (target.classList?.contains("modal-bg")) close(current);
  });
  document.addEventListener("keydown", (e: KeyboardEvent) => {
    if (e.key === "Escape" && current) {
      e.stopPropagation();
      close(current);
    }
  });
}
