import { describe, it, expect, beforeEach } from "vitest";
import { state } from "../../state/index.js";
import { getAccountDisplay, getComboDisplay, renderLogDetailModal } from "./modal.js";
import type { Account } from "../../lib/types/accounts.js";
import type { Combo } from "../../lib/types/combos.js";
import type { LogDetailLog } from "./state.js";

describe("getAccountDisplay in log detail modal", () => {
  beforeEach(() => {
    state.accounts = [
      {
        id: 205 as any,
        provider_id: "minimax" as any,
        label: "Production Minimax",
        priority: 1,
        health_status: "healthy",
        extra_config_json: null,
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
        auth_type: "api_key",
        email: null,
        oauth_scope: null,
        expires_at: null,
        created_at: "2026-01-01",
      } as Account,
      {
        id: 300 as any,
        provider_id: "openai" as any,
        label: null,
        priority: 1,
        health_status: "healthy",
        extra_config_json: null,
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
        auth_type: "api_key",
        email: null,
        oauth_scope: null,
        expires_at: null,
        created_at: "2026-01-01",
      } as Account,
    ];
  });

  it("displays direct account_label when provided by detail endpoint", () => {
    const res = getAccountDisplay(205, "Direct Label From DB");
    expect(res.text).toBe("Direct Label From DB");
    expect(res.title).toBe("Account #205");
  });

  it("displays label from state.accounts matching accountId when direct label is omitted", () => {
    const res = getAccountDisplay(205, null);
    expect(res.text).toBe("Production Minimax");
    expect(res.title).toBe("Account #205");
  });

  it("displays account ID when account exists in state but has no label", () => {
    const res = getAccountDisplay(300, null);
    expect(res.text).toBe("300");
    expect(res.title).toBeUndefined();
  });

  it("displays account ID when account does not exist in state and has no label", () => {
    const res = getAccountDisplay(404, null);
    expect(res.text).toBe("404");
    expect(res.title).toBeUndefined();
  });

  it("displays dash when accountId is null or empty", () => {
    expect(getAccountDisplay(null, null).text).toBe("—");
    expect(getAccountDisplay("", null).text).toBe("—");
    expect(getAccountDisplay("—", null).text).toBe("—");
  });
});

describe("getComboDisplay in log detail modal", () => {
  beforeEach(() => {
    state.combos = [
      {
        id: 193 as any,
        name: "minis",
        strategy: "priority",
        race_size: 1,
        preventive_rate_limit: true,
        created_at: "2026-07-02",
      } as Combo,
    ];
  });

  it("displays direct combo_name when provided by detail endpoint", () => {
    const res = getComboDisplay(193, "minis");
    expect(res.text).toBe("minis (#193)");
    expect(res.title).toBe("Combo #193");
  });

  it("resolves combo name from state.combos when direct combo_name is missing", () => {
    const res = getComboDisplay(193, null);
    expect(res.text).toBe("minis (#193)");
    expect(res.title).toBe("Combo #193");
  });

  it("falls back to combo ID when not found in state and no direct name", () => {
    const res = getComboDisplay(999, null);
    expect(res.text).toBe("999");
  });

  it("returns dash when combo ID is null or missing", () => {
    expect(getComboDisplay(null, null).text).toBe("—");
  });
});

describe("renderLogDetailModal regression: no synchronous infinite loop", () => {
  it("renders synchronously without invoking ensureCombosLoaded or triggering infinite re-render loop", () => {
    state.combos = [];
    const log: LogDetailLog = {
      id: 849102,
      request_id: "req-1",
      trace_id: "trace-1",
      combo_id: 193,
      status_code: 200,
      detail: {},
    };

    // Before fix, renderLogDetailModal invoked ensureCombosLoaded -> renderModal synchronously in a promise loop,
    // freezing the browser thread when combo_name was not yet resolved.
    const tpl = renderLogDetailModal(log);
    expect(tpl).toBeDefined();
    expect(tpl.strings).toBeDefined();
  });
});
