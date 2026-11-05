import { describe, expect, it, vi, afterEach } from "vitest";
import { render } from "lit-html";
import type { TemplateResult } from "lit-html";
import type { Account, Provider } from "../../lib/types/api.js";

function makeTestAccount(overrides: Partial<Account> = {}): Account {
  return {
    id: 1,
    provider_id: "minimax",
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
    active: true,
    ...overrides,
  };
}

function setupMatchMedia(): void {
  if (typeof window.matchMedia !== "function") {
    Object.defineProperty(window, "matchMedia", {
      configurable: true,
      writable: true,
      value: vi.fn().mockImplementation((q: string) => ({
        matches: false,
        media: q,
        onchange: null,
        addListener: (): void => {},
        removeListener: (): void => {},
        addEventListener: (): void => {},
        removeEventListener: (): void => {},
        dispatchEvent: (): boolean => false,
      })),
    });
  }
}

setupMatchMedia();

afterEach(async () => {
  const { state } = await import('../../state/index.js');
  const { setDetailProviderId } = await import('./shared.js');
  setDetailProviderId(null);
  state.providers = [];
  state.accounts = [];
  state.models = [];
  state.providerDetail = {};
});

describe('provider detail tabs', () => {
  it('separates accounts/models/config and preserves model search across switches', async () => {
    const { state } = await import('../../state/index.js');
    const { setDetailProviderId, getProviderUi, setProviderUi } = await import('./shared.js');
    const { renderProviderDetail } = await import('./detail.js');
    state.providers = [makeProvider()];
    setDetailProviderId('minimax');
    const container = document.createElement('div');
    const paint = (): void => { render(renderProviderDetail(), container); };
    paint();
    const tabs = (): HTMLButtonElement[] => Array.from(container.querySelectorAll('[role="tab"]'));
    expect(tabs()).toHaveLength(3);
    expect(tabs()[0]!.getAttribute('aria-selected')).toBe('true');
    expect(container.querySelectorAll('[role="tabpanel"]:not([hidden])')).toHaveLength(1);
    tabs()[1]!.click();
    paint();
    expect(getProviderUi('minimax').tab).toBe('models');
    expect(container.querySelector('[role="tabpanel"]:not([hidden]) .models-list')).not.toBeNull();
    expect(container.querySelector('[role="tabpanel"]:not([hidden]) .auto-activate-bar')).toBeNull();
    setProviderUi('minimax', { ...getProviderUi('minimax'), search: 'preserved', filter: 'active' });
    tabs()[2]!.click();
    paint();
    expect(container.querySelector('[role="tabpanel"]:not([hidden]) .provider-config')).not.toBeNull();
    expect(container.querySelector('[role="tabpanel"]:not([hidden]) .auto-activate-bar')).not.toBeNull();
    tabs()[1]!.click();
    paint();
    expect(getProviderUi('minimax')).toMatchObject({ tab: 'models', search: 'preserved', filter: 'active' });
    tabs()[1]!.dispatchEvent(new KeyboardEvent('keydown', { key: 'End', bubbles: true }));
    paint();
    expect(getProviderUi('minimax').tab).toBe('config');
    expect(tabs()[2]!.getAttribute('tabindex')).toBe('0');
  });

  it('keeps tab choice per provider', async () => {
    const { getProviderUi, setProviderUi } = await import('./shared.js');
    setProviderUi('minimax', { ...getProviderUi('minimax'), tab: 'config' });
    expect(getProviderUi('another').tab).toBe('accounts');
    expect(getProviderUi('minimax').tab).toBe('config');
  });
});

function toString(tpl: TemplateResult): string {
  const container = document.createElement("div");
  render(tpl, container);
  return container.innerHTML;
}

function makeProvider(overrides: Partial<Provider> = {}): Provider {
  return {
    id: "minimax",
    name: "MiniMax",
    base_url: "https://api.minimax.chat/v1",
    auth_type: "bearer",
    format: "openai",
    extra_headers_json: null,
    auto_activate_keyword: null,
    active: true,
    created_at: "2026-01-01T00:00:00Z",
    use_proxies: false,
    current_proxy_id: null,
    proxy_rotation_errors: "0",
    proxy_rotation_mode: "global",
    ...overrides,
  };
}

describe("provider detail header extra_headers handling", () => {
  it("safely handles ***redacted*** sentinel without throwing SyntaxError", async () => {
    setupMatchMedia();
    const { getHeadersChipLabel, getHeadersChipTitle, renderDetailHeader } =
      await import("./detail.js");

    expect(getHeadersChipLabel("***redacted***")).toBe("headers (configured)");
    expect(getHeadersChipTitle("***redacted***")).toBe(
      "Extra headers configured (redacted for security)"
    );

    const provider = makeProvider({
      extra_headers_json: "***redacted***",
    });

    expect(() => renderDetailHeader(provider)).not.toThrow();
    const html = toString(renderDetailHeader(provider));
    expect(html).toContain("headers (configured)");
    expect(html).toContain("Extra headers configured (redacted for security)");
  });

  it("parses valid JSON extra_headers and counts keys", async () => {
    setupMatchMedia();
    const { getHeadersChipLabel, getHeadersChipTitle, renderDetailHeader } =
      await import("./detail.js");

    const raw = JSON.stringify({
      Authorization: "Bearer secret",
      "User-Agent": "CustomAgent/1.0",
    });
    expect(getHeadersChipLabel(raw)).toBe("headers (2)");
    expect(getHeadersChipTitle(raw)).toBe(raw);

    const provider = makeProvider({ extra_headers_json: raw });
    const html = toString(renderDetailHeader(provider));
    expect(html).toContain("headers (2)");
  });

  it("handles malformed JSON gracefully", async () => {
    setupMatchMedia();
    const { getHeadersChipLabel, getHeadersChipTitle, renderDetailHeader } =
      await import("./detail.js");

    expect(getHeadersChipLabel("not-valid-json")).toBe("headers (configured)");
    expect(getHeadersChipTitle("not-valid-json")).toBe("not-valid-json");

    const provider = makeProvider({ extra_headers_json: "not-valid-json" });
    expect(() => renderDetailHeader(provider)).not.toThrow();
    const html = toString(renderDetailHeader(provider));
    expect(html).toContain("headers (configured)");
  });

  it("omits chip when extra_headers_json is null or empty", async () => {
    setupMatchMedia();
    const { getHeadersChipLabel, renderDetailHeader } =
      await import("./detail.js");

    expect(getHeadersChipLabel(null)).toBe("");
    expect(getHeadersChipLabel("")).toBe("");
    expect(getHeadersChipLabel("   ")).toBe("");

    const provider = makeProvider({ extra_headers_json: null });
    const html = toString(renderDetailHeader(provider));
    expect(html).not.toContain("headers-chip");
  });

  it("omits Copy button for OAuth accounts and includes it for API key accounts", async () => {
    setupMatchMedia();
    const { renderConnectionsSection } = await import("./oauth.js");
    const container = document.createElement("div");

    const oauthProvider = makeProvider({ id: "codex", auth_type: "oauth" });
    const oauthAccount = makeTestAccount({
      id: 1,
      provider_id: "codex",
      auth_type: "oauth",
      label: "My OAuth",
      active: true,
    });

    render(renderConnectionsSection(oauthProvider, [oauthAccount]), container);
    const oauthButtons = Array.from(container.querySelectorAll("button.small")).map(b => b.textContent?.trim());
    expect(oauthButtons).toContain("Reauth");
    expect(oauthButtons).not.toContain("Copy");

    const apiKeyProvider = makeProvider({ id: "minimax", auth_type: "bearer" });
    const apiKeyAccount = makeTestAccount({
      id: 2,
      provider_id: "minimax",
      auth_type: "api_key",
      label: "My API Key",
      active: true,
    });

    render(renderConnectionsSection(apiKeyProvider, [apiKeyAccount]), container);
    const apiKeyButtons = Array.from(container.querySelectorAll("button.small")).map(b => b.textContent?.trim());
    expect(apiKeyButtons).toContain("Copy");
    expect(apiKeyButtons).toContain("Key");
    expect(apiKeyButtons).not.toContain("Reauth");
  });

  it("renders subscription tier centrally without duplication or generic provider names", async () => {
    setupMatchMedia();
    const { renderConnectionsSection } = await import("./oauth.js");
    const container = document.createElement("div");

    // Minimax with Free tier
    const minimaxProvider = makeProvider({ id: "minimax", auth_type: "oauth" });
    const minimaxAccount = makeTestAccount({
      id: 10,
      provider_id: "minimax",
      auth_type: "oauth",
      quota_plan_name: "Free",
      label: "Minimax User",
      active: true,
    });

    render(renderConnectionsSection(minimaxProvider, [minimaxAccount]), container);
    const planTags = Array.from(container.querySelectorAll(".account-plan")).map(el => el.textContent?.trim());
    expect(planTags).toEqual(["Free"]);
    // Quota cell must not contain a second duplicate .quota-plan
    expect(container.querySelectorAll(".quota-plan").length).toBe(0);

    // Codex with reset credits resolves to "Plus", never "Codex / ChatGPT"
    const codexProvider = makeProvider({ id: "codex", auth_type: "oauth" });
    const codexAccount = makeTestAccount({
      id: 11,
      provider_id: "codex",
      auth_type: "oauth",
      quota_plan_name: "Codex / ChatGPT",
      oauth_provider_specific: JSON.stringify({ reset_credits: 3 }),
      label: "Codex User",
      active: true,
    });

    render(renderConnectionsSection(codexProvider, [codexAccount]), container);
    const codexPlanTags = Array.from(container.querySelectorAll(".account-plan")).map(el => el.textContent?.trim());
    expect(codexPlanTags).toEqual(["Plus"]);
    expect(container.textContent).not.toContain("Codex / ChatGPT");
  });
});
