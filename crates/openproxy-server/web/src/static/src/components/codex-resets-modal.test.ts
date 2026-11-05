import { describe, it, expect, afterEach, vi } from "vitest";
import * as apiModule from "../state/api.js";
import { showCodexResetsModal } from "./codex-resets-modal.js";

afterEach(() => {
  document.body.innerHTML = "";
  vi.restoreAllMocks();
});

describe("showCodexResetsModal", () => {
  it("renders credits list with SVG icons and without emojis", async () => {
    vi.spyOn(apiModule, "api").mockResolvedValueOnce({
      available_count: 2,
      credits: [
        {
          id: "cred-1111222233334444",
          title: "Banked Reset #1",
          expires_at: new Date(Date.now() + 86400000 * 5).toISOString(),
          description: "5h rolling and weekly window restoration",
        },
        {
          id: "cred-5555666677778888",
          title: "Banked Reset #2",
          expires_at: new Date(Date.now() + 3600000 * 2).toISOString(),
        },
      ],
    });

    showCodexResetsModal(42, "Production Codex");

    // Allow loadCredits() async promise tick to complete
    await new Promise((r) => setTimeout(r, 20));

    const modalRoot = document.getElementById("modal-root");
    expect(modalRoot).not.toBeNull();

    // Verify loading block is completely destroyed once resolved
    expect(modalRoot?.querySelector(".codex-modal-loading")).toBeNull();

    const title = modalRoot?.querySelector("#op-modal-title");
    expect(title?.textContent).toBe("Codex Reset Credits — Production Codex");

    // Toolbar available badge
    const availTag = modalRoot?.querySelector(".quota-tag.credits");
    expect(availTag).not.toBeNull();
    expect(availTag?.textContent).toContain("2 Available");
    expect(availTag?.textContent).not.toContain("⚡");
    expect(availTag?.querySelector("svg")).not.toBeNull();

    // Credit items
    const items = modalRoot?.querySelectorAll(".codex-credit-item");
    expect(items?.length).toBe(2);
    expect(items?.[0]?.getAttribute("role")).toBe("radio");
    expect(items?.[0]?.getAttribute("aria-checked")).toBe("true");

    // IDs format: unique suffix
    const idEl = items?.[0]?.querySelector(".codex-credit-id");
    expect(idEl?.textContent?.trim()).toBe("…33334444");

    // Expiration rows have SVG clock and NO emoji
    const expRow = items?.[0]?.querySelector(".codex-credit-exp");
    expect(expRow).not.toBeNull();
    expect(expRow?.textContent).toContain("Expiration:");
    expect(expRow?.textContent).not.toContain("🕒");
    expect(expRow?.querySelector("svg")).not.toBeNull();

    // Hint has SVG info and NO emoji
    const hint = modalRoot?.querySelector(".codex-resets-hint");
    expect(hint).not.toBeNull();
    expect(hint?.textContent).toContain("How it works:");
    expect(hint?.textContent).not.toContain("💡");
    expect(hint?.querySelector("svg")).not.toBeNull();

    // Footer buttons: exactly Cancel and Redeem Selected Credit, NO Close button
    const footerButtons = Array.from(modalRoot?.querySelectorAll(".modal-footer button") || []).map(b => b.textContent?.trim());
    expect(footerButtons).toEqual(["Cancel", "Redeem Selected Credit"]);

    // Redeem button has SVG lightning and NO emoji
    const redeemBtn = modalRoot?.querySelector(".modal-footer button.primary");
    expect(redeemBtn).not.toBeNull();
    expect(redeemBtn?.textContent).toContain("Redeem Selected Credit");
    expect(redeemBtn?.textContent).not.toContain("⚡");
    expect(redeemBtn?.querySelector("svg")).not.toBeNull();
  });

  it("renders empty state with shield SVG when no credits are available", async () => {
    vi.spyOn(apiModule, "api").mockResolvedValueOnce({
      available_count: 0,
      credits: [],
    });

    showCodexResetsModal(99);

    await new Promise((r) => setTimeout(r, 20));

    const modalRoot = document.getElementById("modal-root");
    expect(modalRoot).not.toBeNull();

    const empty = modalRoot?.querySelector(".codex-modal-empty");
    expect(empty).not.toBeNull();
    expect(empty?.textContent).toContain("No banked reset credits available");
    expect(empty?.querySelector(".codex-empty-icon svg")).not.toBeNull();

    // No redeem button in empty state
    const primaryBtn = modalRoot?.querySelector(".modal-footer button.primary");
    expect(primaryBtn).toBeNull();
  });
});
