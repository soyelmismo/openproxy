// The quota panel contract: a plan without window caps shows its BALANCE bar
// with the plan expiry, and no uncapped window ever renders a phantom
// "resets in ...". The monthly window no longer exists (its bound is the
// plan balance).

import { describe, expect, it } from "vitest";
import { render } from "lit-html";
import { quotaWindowList } from "./index.js";
import type { KeyQuotaStatus } from "../../lib/types/api.js";

const window0 = { cap_usd: null, spent_usd: 0, resets_at: null, oldest_spend_at: null };

function status(overrides: Partial<KeyQuotaStatus> = {}): KeyQuotaStatus {
  return {
    enabled: false,
    week: { ...window0 },
    five_hours: { ...window0 },
    plan: null,
    ...overrides,
  };
}

function renderList(s: KeyQuotaStatus): HTMLElement {
  const host = document.createElement("div");
  document.body.appendChild(host);
  render(quotaWindowList(s), host);
  return host;
}

describe("quotaWindowList", () => {
  it("renders only the plan balance bar when no window has a cap", () => {
    const expires = new Date(Date.now() + 7 * 86400_000).toISOString();
    const host = renderList(
      status({
        plan: { name: "Lite", credit_usd: 3, balance_usd: 2.25, expires_at: expires },
      }),
    );
    const titles = [...host.querySelectorAll(".quota-window-title")].map((e) => e.textContent);
    expect(titles).toEqual(["Lite balance"]);
    // 0.75 of 3 spent -> 25%.
    expect(host.querySelector(".quota-window-numbers")?.textContent).toBe("25%");
    expect(host.querySelector(".quota-window-hint")?.textContent).toContain("expires in");
    document.body.innerHTML = "";
  });

  it("never renders the monthly window", () => {
    const host = renderList(
      status({
        enabled: true,
        week: { ...window0, cap_usd: 5, spent_usd: 1, resets_at: "2030-01-07T00:00:00Z" },
        five_hours: { ...window0, cap_usd: 1, spent_usd: 0.2 },
      }),
    );
    const titles = [...host.querySelectorAll(".quota-window-title")].map((e) => e.textContent);
    expect(titles).toEqual(["5h Window", "Weekly Window"]);
    expect(host.textContent).not.toContain("Monthly");
    document.body.innerHTML = "";
  });

  it("orders plan first, then the capped windows", () => {
    const host = renderList(
      status({
        enabled: true,
        plan: { name: "Pro", credit_usd: 100, balance_usd: 90, expires_at: null },
        week: { ...window0, cap_usd: 20, spent_usd: 2, resets_at: "2030-01-07T00:00:00Z" },
      }),
    );
    const titles = [...host.querySelectorAll(".quota-window-title")].map((e) => e.textContent);
    expect(titles).toEqual(["Pro balance", "Weekly Window"]);
    // No expiry -> honest hint, not a countdown.
    const hints = [...host.querySelectorAll(".quota-window-hint")].map((e) => e.textContent);
    expect(hints[0]).toBe("no expiry");
    document.body.innerHTML = "";
  });

  it("renders nothing for a key with neither plan nor caps", () => {
    const host = renderList(status());
    expect(host.querySelectorAll(".quota-window")).toHaveLength(0);
    document.body.innerHTML = "";
  });
});
