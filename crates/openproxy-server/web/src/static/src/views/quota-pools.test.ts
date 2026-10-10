import { describe, expect, it, beforeEach, afterEach, vi } from "vitest";
import { render } from "lit-html";
import type { TemplateResult } from "lit-html";
import { renderQuotaCell } from "./quota-cell.js";
import { renderQuotaPools, orderedPools, poolHeading, parsePoolDate } from "./quota-pools.js";
import type { Account, QuotaPool } from "../lib/types/api.js";

function toString(tpl: TemplateResult): string {
  const container = document.createElement("div");
  render(tpl, container);
  return container.innerHTML;
}

function toDom(tpl: TemplateResult): HTMLElement {
  const container = document.createElement("div");
  render(tpl, container);
  return container;
}

const NOW_MS = 1_800_000_000_000; // fixed epoch for deterministic assertions

function makeAccount(overrides: Partial<Account> = {}): Account {
  return {
    id: 1,
    provider_id: "zai",
    label: null,
    priority: 100,
    extra_config_json: null,
    health_status: "healthy",
    rate_limited_until: null,
    quota_session_used: null,
    quota_session_limit: null,
    quota_session_reset_at: null,
    quota_weekly_used: null,
    quota_weekly_limit: null,
    quota_weekly_reset_at: null,
    quota_plan_name: null,
    quota_last_fetched_at: null,
    quota_fetch_error: null,
    quota_model_details: null,
    auth_type: "oauth",
    email: null,
    oauth_scope: null,
    oauth_provider_specific: null,
    expires_at: null,
    created_at: "2026-01-01T00:00:00Z",
    ...overrides,
  };
}

function makePool(overrides: Partial<QuotaPool> = {}): QuotaPool {
  return {
    id: "pool-1",
    source: "zcode_starter",
    plan_name: null,
    status: "active",
    unit: "requests",
    used: null,
    limit: null,
    remaining: null,
    reset_at: null,
    expires_at: null,
    starts_at: null,
    model_ids: [],
    model_details: null,
    fetch_error: null,
    last_fetched_at: String(Math.floor(NOW_MS / 1000)),
    ...overrides,
  };
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date(NOW_MS));
});

afterEach(() => {
  vi.useRealTimers();
});

describe("renderQuotaPools — ordering", () => {
  it("always renders the Starter pool first, paid Coding Plan below", () => {
    const paid = makePool({ id: "p2", source: "coding_plan", plan_name: "Pro", unit: "tokens", used: 1000, limit: 5000, remaining: 4000 });
    const starter = makePool({ id: "p1", source: "zcode_starter", used: 5, limit: 50, remaining: 45 });
    const html = toString(renderQuotaPools([paid, starter])!);
    const starterIdx = html.indexOf("Starter · Free");
    const paidIdx = html.indexOf("Coding Plan · Pro");
    expect(starterIdx).toBeGreaterThanOrEqual(0);
    expect(paidIdx).toBeGreaterThan(starterIdx);
  });

  it("keeps the order even when Starter arrives last and is exhausted", () => {
    const paid = makePool({ id: "p2", source: "coding_plan", plan_name: "Lite", unit: "requests", used: 10, limit: 100, remaining: 90 });
    const starter = makePool({ id: "p1", source: "zcode_starter", used: 50, limit: 50, remaining: 0, status: "exhausted" });
    const html = toString(renderQuotaPools([paid, starter])!);
    expect(html.indexOf("Starter · Free")).toBeLessThan(html.indexOf("Coding Plan · Lite"));
  });

  it("returns null for empty and null pool arrays", () => {
    expect(renderQuotaPools([])).toBeNull();
    expect(renderQuotaPools(null)).toBeNull();
    expect(renderQuotaPools(undefined)).toBeNull();
  });

  it("orders via orderedPools with exhausted ranked after active", () => {
    const active = makePool({ id: "a", source: "coding_plan", plan_name: "A" });
    const exhausted = makePool({ id: "b", source: "coding_plan", plan_name: "B", status: "exhausted" });
    const [first, second] = orderedPools([exhausted, active]);
    expect(first?.id).toBe("a");
    expect(second?.id).toBe("b");
  });
});

describe("renderQuotaPools — pool independence", () => {
  it("shows both figures independently without summing unlike units", () => {
    const starter = makePool({ used: 5, limit: 50, remaining: 45, unit: "requests" });
    const paid = makePool({ id: "p2", source: "coding_plan", plan_name: "Pro", unit: "tokens", used: 250_000, limit: 1_000_000, remaining: 750_000 });
    const html = toString(renderQuotaPools([starter, paid])!);
    // Starter: 45/50 requests left
    expect(html).toContain("45 / 50 requests left");
    // Paid: 750,000/1,000,000 tokens left (locale-formatted, never summed with requests)
    expect(html).toContain("750,000 / 1,000,000 tokens left");
    expect(html).not.toContain("750,045");
  });

  it("an exhausted paid pool does not hide the healthy Starter pool", () => {
    const starter = makePool({ used: 5, limit: 50, remaining: 45 });
    const paid = makePool({ id: "p2", source: "coding_plan", status: "exhausted", used: 100, limit: 100, remaining: 0 });
    const html = toString(renderQuotaPools([starter, paid])!);
    expect(html).toContain("45 / 50 requests left");
    expect(html).toContain("Exhausted");
    expect(html).toContain("0 requests left");
  });

  it("an exhausted Starter pool does not hide the healthy paid pool", () => {
    const starter = makePool({ used: 50, limit: 50, remaining: 0, status: "exhausted" });
    const paid = makePool({ id: "p2", source: "coding_plan", plan_name: "Pro", unit: "tokens", used: 1, limit: 10, remaining: 9 });
    const html = toString(renderQuotaPools([starter, paid])!);
    expect(html).toContain("Exhausted");
    expect(html).toContain("9 / 10 tokens left");
  });
});

describe("renderQuotaPools — missing data is never a fake zero", () => {
  it("absent paid pool says No active Coding Plan without a progressbar", () => {
    const paid = makePool({ id: "p2", source: "coding_plan", status: "absent" });
    const dom = toDom(renderQuotaPools([paid])!);
    expect(dom.textContent).toContain("No active Coding Plan");
    expect(dom.querySelector('[role="progressbar"]')).toBeNull();
  });

  it("absent Starter does not borrow the Coding Plan wording", () => {
    const starter = makePool({ source: "zcode_starter", status: "absent" });
    const html = toString(renderQuotaPools([starter])!);
    expect(html).toContain("No Starter");
    expect(html).not.toContain("No active Coding Plan");
  });

  it("a paid pool absent while Starter is healthy keeps both visible", () => {
    const starter = makePool({ used: 5, limit: 50, remaining: 45 });
    const paid = makePool({ id: "p2", source: "coding_plan", status: "absent" });
    const dom = toDom(renderQuotaPools([starter, paid])!);
    expect(dom.textContent).toContain("45 / 50 requests left");
    expect(dom.textContent).toContain("No active Coding Plan");
    expect(dom.querySelectorAll(".quota-pool").length).toBe(2);
  });

  it("unknown figures render no progressbar and no invented 0", () => {
    const pool = makePool({ used: null, limit: null, remaining: null, status: "unavailable" });
    const dom = toDom(renderQuotaPools([pool])!);
    expect(dom.querySelector('[role="progressbar"]')).toBeNull();
    expect(dom.textContent).not.toContain("0 requests");
    expect(dom.textContent).toContain("no usage data");
  });

  it("a pool with only used and no limit shows used without a percentage", () => {
    const pool = makePool({ used: 1234, limit: null, remaining: null });
    const html = toString(renderQuotaPools([pool])!);
    expect(html).toContain("1,234 requests used");
    expect(html).not.toContain("left");
  });

  it("missing starter data shows clarity, not a false zero bar", () => {
    const starter = makePool({ status: "unavailable", fetch_error: null });
    const html = toString(renderQuotaPools([starter])!);
    expect(html).toContain("no usage data reported");
    expect(html).not.toContain("0 /");
  });
});

describe("renderQuotaPools — expiry semantics", () => {
  it("Starter uses expires wording, never resets soon, even one-time", () => {
    const future = NOW_MS + 30 * 24 * 3600 * 1000;
    const starter = makePool({ expires_at: String(Math.floor(future / 1000)), used: 5, limit: 50, remaining: 45 });
    const html = toString(renderQuotaPools([starter])!);
    expect(html).toContain("expires");
    expect(html).not.toContain("resets");
  });

  it("marks an expired Starter with the expiry date", () => {
    const past = NOW_MS - 24 * 3600 * 1000;
    const starter = makePool({ expires_at: String(Math.floor(past / 1000)), status: "expired" });
    const html = toString(renderQuotaPools([starter])!);
    expect(html).toContain("expired");
    expect(html).not.toContain("resets soon");
  });

  it("paid pool shows a rolling reset, not expires", () => {
    const in8h = NOW_MS + 8 * 3600 * 1000;
    const paid = makePool({ id: "p2", source: "coding_plan", plan_name: "Pro", reset_at: String(Math.floor(in8h / 1000)), used: 1, limit: 10, remaining: 9 });
    const html = toString(renderQuotaPools([paid])!);
    expect(html).toContain("resets in 8h 0m");
    expect(html).not.toContain("expires");
  });

  it("unavailable pool with a future starts_at announces when it starts", () => {
    const future = NOW_MS + 5 * 24 * 3600 * 1000;
    const pool = makePool({ status: "unavailable", starts_at: String(Math.floor(future / 1000)) });
    const html = toString(renderQuotaPools([pool])!);
    expect(html).toContain("starts");
  });
});

describe("renderQuotaPools — per-source errors", () => {
  it("a failing pool renders its error without hiding the healthy sibling", () => {
    const starter = makePool({ used: 5, limit: 50, remaining: 45 });
    const paid = makePool({ id: "p2", source: "coding_plan", fetch_error: "401 unauthorized" });
    const dom = toDom(renderQuotaPools([starter, paid])!);
    expect(dom.textContent).toContain("401 unauthorized");
    expect(dom.textContent).toContain("45 / 50 requests left");
    // Two independent pools rendered
    expect(dom.querySelectorAll(".quota-pool").length).toBe(2);
  });

  it("a pool error does not fabricate a zero balance for that pool", () => {
    const pool = makePool({ fetch_error: "boom" });
    const dom = toDom(renderQuotaPools([pool])!);
    expect(dom.textContent).toContain("boom");
    expect(dom.textContent).not.toContain("0 requests left");
  });
});

describe("renderQuotaPools — clamping and locale", () => {
  it("clamps out-of-range percentages into [0,100]", () => {
    const over = makePool({ used: -20, limit: 100, remaining: 120 });
    const dom = toDom(renderQuotaPools([over])!);
    const fill = dom.querySelector<HTMLElement>(".quota-bar-fill");
    expect(fill?.style.width).toBe("100%");
    const under = makePool({ used: 200, limit: 100, remaining: -50 });
    const dom2 = toDom(renderQuotaPools([under])!);
    const fill2 = dom2.querySelector<HTMLElement>(".quota-bar-fill");
    expect(fill2?.style.width).toBe("0%");
  });

  it("formats figures with locale separators", () => {
    const pool = makePool({ used: 1_500_000, limit: 2_000_000, remaining: 500_000, unit: "tokens" });
    const html = toString(renderQuotaPools([pool])!);
    expect(html).toContain("500,000 / 2,000,000 tokens left");
  });
});

describe("renderQuotaPools — accessibility", () => {
  it("exposes progressbar roles with values and titles per pool", () => {
    const starter = makePool({ used: 5, limit: 50, remaining: 45 });
    const paid = makePool({ id: "p2", source: "coding_plan", plan_name: "Pro", unit: "tokens", used: 1, limit: 4, remaining: 3 });
    const dom = toDom(renderQuotaPools([starter, paid])!);
    const bars = dom.querySelectorAll<HTMLElement>('[role="progressbar"]');
    expect(bars.length).toBe(2);
    const [starterBar, paidBar] = [...bars];
    expect(starterBar?.getAttribute("aria-label")).toBe("Starter · Free quota");
    expect(starterBar?.getAttribute("aria-valuenow")).toBe("90");
    expect(starterBar?.getAttribute("aria-valuemin")).toBe("0");
    expect(starterBar?.getAttribute("aria-valuemax")).toBe("100");
    expect(starterBar?.getAttribute("aria-valuetext")).toBe("45 / 50 requests left");
    expect(paidBar?.getAttribute("aria-valuenow")).toBe("75");
  });
});

describe("renderQuotaCell — Z.ai integration", () => {
  it("routes accounts with quota_pools to the specialized renderer", () => {
    const account = makeAccount({
      quota_pools: [
        makePool({ used: 5, limit: 50, remaining: 45 }),
        makePool({ id: "p2", source: "coding_plan", plan_name: "Pro", unit: "tokens", used: 1, limit: 4, remaining: 3 }),
      ],
    });
    const html = toString(renderQuotaCell(account));
    expect(html).toContain("quota-pools");
    expect(html).toContain("Starter · Free");
    expect(html).toContain("Coding Plan · Pro");
    // Legacy SessionWindow rendering must not leak into the pools path.
    expect(html).not.toContain("Session Window");
  });

  it("a top-level fetch_error does not hide healthy pools", () => {
    const account = makeAccount({
      quota_fetch_error: "partial refresh failure",
      quota_pools: [makePool({ used: 5, limit: 50, remaining: 45 })],
    });
    const html = toString(renderQuotaCell(account));
    expect(html).toContain("partial refresh failure");
    expect(html).toContain("45 / 50 requests left");
  });

  it("ignores legacy SessionWindow 0/— when pools are present", () => {
    const account = makeAccount({
      quota_session_used: 0,
      quota_session_limit: null,
      quota_pools: [makePool({ used: 10, limit: 100, remaining: 90 })],
    });
    const html = toString(renderQuotaCell(account));
    expect(html).toContain("90 / 100 requests left");
    expect(html).not.toContain("Session Window");
  });

  it("backward compatibility: accounts without pools keep the legacy render", () => {
    const account = makeAccount({
      provider_id: "claude",
      quota_session_used: 10,
      quota_session_limit: 100,
    });
    const html = toString(renderQuotaCell(account));
    expect(html).toContain("5h Window");
    expect(html).not.toContain("quota-pools");
  });

  it("backward compatibility: null and empty pools fall back to legacy", () => {
    const withNull = makeAccount({ quota_pools: null, quota_plan_name: "Free" });
    expect(toString(renderQuotaCell(withNull))).not.toContain("quota-pools");
    const withEmpty = makeAccount({ quota_pools: [] });
    expect(toString(renderQuotaCell(withEmpty))).not.toContain("quota-pools");
  });

  it("escapes provider-supplied plan names (XSS)", () => {
    const account = makeAccount({
      quota_pools: [
        makePool({
          source: "coding_plan",
          plan_name: '<img src=x onerror="alert(1)">',
          used: 1,
          limit: 2,
          remaining: 1,
        }),
      ],
    });
    const dom = toDom(renderQuotaCell(account));
    expect(dom.querySelector("img")).toBeNull();
    expect(dom.textContent).toContain("<img src=x onerror=\"alert(1)\">");
  });

  it("escapes provider-supplied fetch_error and model ids (XSS)", () => {
    const account = makeAccount({
      quota_pools: [
        makePool({
          fetch_error: '<script>alert("x")</script>',
          model_ids: ['<script>evil()</script>'],
          used: 1,
          limit: 2,
          remaining: 1,
        }),
      ],
    });
    const dom = toDom(renderQuotaCell(account));
    expect(dom.querySelector("script")).toBeNull();
    expect(dom.textContent).toContain("<script>alert(\"x\")</script>");
  });
});

describe("quota-pools helpers", () => {
  it("poolHeading labels Starter as free and paid with its plan", () => {
    expect(poolHeading(makePool())).toBe("Starter · Free");
    expect(poolHeading(makePool({ source: "coding_plan", plan_name: "Pro" }))).toBe("Coding Plan · Pro");
    expect(poolHeading(makePool({ source: "coding_plan", plan_name: null }))).toBe("Coding Plan");
  });

  it("parsePoolDate handles unix seconds, millis and ISO strings", () => {
    expect(parsePoolDate("1800000000")?.getTime()).toBe(NOW_MS);
    expect(parsePoolDate("1800000000000")?.getTime()).toBe(NOW_MS);
    expect(parsePoolDate("2027-01-15T10:00:00Z")?.getUTCFullYear()).toBe(2027);
    expect(parsePoolDate(null)).toBeNull();
    expect(parsePoolDate("")).toBeNull();
    expect(parsePoolDate("garbage")).toBeNull();
    expect(parsePoolDate("0")).toBeNull();
  });

  it("model rows render per-model figures with the pool unit", () => {
    const pool = makePool({
      used: 10, limit: 100, remaining: 90,
      model_details: [
        { model_id: "glm-4.6", session_used: 60, session_limit: 100, session_reset_at: null, remaining_fraction: 0.4 },
        { model_id: "glm-4.5-air", session_used: 0, session_limit: 100, session_reset_at: null, remaining_fraction: 1 },
      ],
    });
    const html = toString(renderQuotaPools([pool])!);
    expect(html).toContain("glm-4.6");
    expect(html).toContain("60 / 100 requests");
    expect(html).toContain("glm-4.5-air");
    expect(html).toContain("100 requests available");
  });
});
