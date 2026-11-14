// modals.test.ts — in-page modals of the public checkout (Terms of Service, FAQ).
//
// The CSP is `script-src 'self'`: inline scripts on the checkout page are
// blocked, so the modal wiring must ride in /checkout.js. What must hold:
//   * a [data-modal] anchor opens its modal instead of navigating;
//   * the terms iframe loads /terms lazily, once;
//   * Escape, the X and a backdrop click close — body scroll unlocks.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { initCheckoutModals } from "./modals.js";

function mount(): void {
  document.body.innerHTML = `
    <a href="/terms" data-modal="terms" id="trigger-terms">Terms of Service</a>
    <a href="#faq" data-modal="faq" id="trigger-faq">FAQ</a>
    <div class="modal-bg hidden" id="terms-modal">
      <div class="modal">
        <div class="modal-header"><h2>Terms of Service</h2>
          <button type="button" class="close-btn" data-close>&times;</button></div>
        <div class="modal-body">
          <!-- The server injects the terms body here; the modal never fetches. -->
          <div class="terms-body terms-modal-body"><h2>1. What you buy</h2><p>A plan grants a balance.</p></div>
        </div>
      </div>
    </div>
    <div class="modal-bg hidden" id="faq-modal">
      <div class="modal"><div class="modal-header"><h2>FAQ</h2></div></div>
    </div>`;
}

describe("checkout modals", () => {
  beforeEach(() => {
    mount();
    initCheckoutModals();
  });
  afterEach(() => {
    document.body.innerHTML = "";
    vi.restoreAllMocks();
  });

  it("a data-modal anchor opens its modal instead of navigating", () => {
    document.getElementById("trigger-terms")!.click();
    expect(document.getElementById("terms-modal")!.classList.contains("hidden")).toBe(false);
  });

  it("the injected terms body is already in the page: the modal never fetches", () => {
    // No network dependency: the server-rendered body must be there at load.
    expect(
      document.querySelector("#terms-modal .terms-modal-body h2")!.textContent,
    ).toBe("1. What you buy");
    expect(document.getElementById("terms-modal")!.querySelector("iframe")).toBeNull();
  });

  it("escape closes the open modal and unlocks the body scroll", () => {
    document.getElementById("trigger-faq")!.click();
    expect(document.getElementById("faq-modal")!.classList.contains("hidden")).toBe(false);
    expect(document.body.style.overflow).toBe("hidden");
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(document.getElementById("faq-modal")!.classList.contains("hidden")).toBe(true);
    expect(document.body.style.overflow).toBe("");
  });

  it("a backdrop click closes, a click inside the modal does not", () => {
    document.getElementById("trigger-terms")!.click();
    const bg = document.getElementById("terms-modal")!;
    (bg as HTMLElement).click();
    expect(bg.classList.contains("hidden")).toBe(true);
    document.getElementById("trigger-terms")!.click();
    (bg.querySelector(".modal") as HTMLElement).dispatchEvent(new MouseEvent("click", { bubbles: true }));
    expect(bg.classList.contains("hidden")).toBe(false);
  });

  it("the X button closes", () => {
    document.getElementById("trigger-terms")!.click();
    (document.querySelector("#terms-modal [data-close]") as HTMLElement).click();
    expect(document.getElementById("terms-modal")!.classList.contains("hidden")).toBe(true);
  });

  it("opening one modal closes the other: the page holds a single modal", () => {
    document.getElementById("trigger-terms")!.click();
    document.getElementById("trigger-faq")!.click();
    expect(document.getElementById("terms-modal")!.classList.contains("hidden")).toBe(true);
    expect(document.getElementById("faq-modal")!.classList.contains("hidden")).toBe(false);
  });
});
