import { describe, expect, it, beforeEach, vi } from "vitest";
import type { TemplateResult } from "lit-html";
import type { Provider } from "../../lib/types/api.js";

if (typeof window !== "undefined") {
  (window as unknown as { matchMedia: unknown }).matchMedia = vi.fn().mockImplementation((q: string) => ({
    matches: false,
    media: q,
    onchange: null,
    addListener: (): void => {},
    removeListener: (): void => {},
    addEventListener: (): void => {},
    removeEventListener: (): void => {},
    dispatchEvent: (): boolean => false,
  }));
}

import { render } from "lit-html";
import { state } from "../../state/index.js";
import {
  getProviderListFilter,
  setProviderListFilter,
  renderProvidersGrid,
} from "./list.js";

function toString(tpl: TemplateResult): string {
  const container = document.createElement("div");
  render(tpl, container);
  return container.innerHTML;
}

function makeProvider(id: string, active: boolean): Provider {
  return {
    id,
    name: `Provider ${id}`,
    base_url: `https://${id}.example.com`,
    auth_type: "bearer",
    format: "openai",
    extra_headers_json: null,
    auto_activate_keyword: null,
    active,
    created_at: "2026-01-01T00:00:00Z",
    use_proxies: false,
    current_proxy_id: null,
    proxy_rotation_errors: "0",
    proxy_rotation_mode: "global",
    active_models: 5,
    total_models: 10,
  };
}

describe("providers list filter (All / Active / Inactive) and persistence", () => {
  beforeEach(() => {
    localStorage.clear();
    state.providers = [
      makeProvider("prov-active-1", true),
      makeProvider("prov-active-2", true),
      makeProvider("prov-inactive-1", false),
    ];
    state.accounts = [];
  });

  it("defaults to 'all' and persists choice to localStorage", () => {
    expect(getProviderListFilter()).toBe("all");

    setProviderListFilter("active");
    expect(getProviderListFilter()).toBe("active");
    expect(localStorage.getItem("openproxy.providers.filter")).toBe("active");

    setProviderListFilter("inactive");
    expect(getProviderListFilter()).toBe("inactive");
    expect(localStorage.getItem("openproxy.providers.filter")).toBe("inactive");

    setProviderListFilter("all");
    expect(getProviderListFilter()).toBe("all");
    expect(localStorage.getItem("openproxy.providers.filter")).toBe("all");
  });

  it("renders all providers when filter is 'all'", () => {
    setProviderListFilter("all");
    const html = toString(renderProvidersGrid());
    expect(html).toContain("prov-active-1");
    expect(html).toContain("prov-active-2");
    expect(html).toContain("prov-inactive-1");
    expect(html).toMatch(/providers\.list\.filter\.all[^(]*\(\s*<!--\?lit\$[0-9]+\$-->3\s*\)/);
    expect(html).toMatch(/providers\.list\.filter\.active[^(]*\(\s*<!--\?lit\$[0-9]+\$-->2\s*\)/);
    expect(html).toMatch(/providers\.list\.filter\.inactive[^(]*\(\s*<!--\?lit\$[0-9]+\$-->1\s*\)/);
  });

  it("renders only active providers when filter is 'active'", () => {
    setProviderListFilter("active");
    const html = toString(renderProvidersGrid());
    expect(html).toContain("prov-active-1");
    expect(html).toContain("prov-active-2");
    expect(html).not.toContain("prov-inactive-1");
  });

  it("renders only inactive providers when filter is 'inactive'", () => {
    setProviderListFilter("inactive");
    const html = toString(renderProvidersGrid());
    expect(html).not.toContain("prov-active-1");
    expect(html).not.toContain("prov-active-2");
    expect(html).toContain("prov-inactive-1");
  });

  it("shows filtered empty state when no providers match", () => {
    state.providers = [makeProvider("prov-active-1", true)];
    setProviderListFilter("inactive");
    const html = toString(renderProvidersGrid());
    expect(html).toContain("providers.list.empty.filtered");
    expect(html).not.toContain("prov-active-1");
  });
});
