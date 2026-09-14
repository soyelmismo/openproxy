// views/providers/models.test.ts — W4 unit tests for the per-provider
// "notify on keyword matches only" toggle visibility.
//
// W3 added the toggle to the provider detail models section; W4 pins the
// two behaviors the spec calls out:
//   1. The toggle renders ONLY when `auto_activate_keyword` is set
//      (trimmed non-empty). Clearing the keyword hides it, so the UI can
//      never PATCH `notif_keyword_only` without a keyword.
//   2. The toggle reflects the three-state field: `true` -> on,
//      `false`/`undefined` -> off, and the help text switches with it.
//
// The template is rendered with lit-html into a jsdom container; no
// browser is needed because the assertion is on markup, not CSS.

import { describe, expect, it, vi, beforeEach } from "vitest";
import { render } from "lit-html";
import type { TemplateResult } from "lit-html";
import type { Provider } from "../../lib/types/api.js";

function toString(tpl: TemplateResult): string {
  const container = document.createElement("div");
  render(tpl, container);
  return container.innerHTML;
}

function makeProvider(overrides: Partial<Provider> = {}): Provider {
  return {
    id: "provider-test",
    name: "Test Provider",
    base_url: "https://example.com",
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

// `renderModelsSection` is the exported template entry point. It is
// pulled in lazily inside an `it` because the module imports the global
// `state` singleton (side-effectful) — importing it at file scope would
// run that for every test file in the same worker.
async function modelsSectionHtml(provider: Provider): Promise<string> {
  const mod = await import("./models.js");
  return toString(
    mod.renderModelsSection(provider, [], {
      filter: "all",
      search: "",
      sort: null,
      page: 1,
      pageSize: 50,
    }),
  );
}

beforeEach(() => {
  vi.resetModules();
});

describe("provider detail — notif_keyword_only toggle visibility", () => {
  it("hides the toggle when auto_activate_keyword is null", async () => {
    const html = await modelsSectionHtml(
      makeProvider({ auto_activate_keyword: null, notif_keyword_only: true }),
    );
    expect(html).not.toContain('role="switch"');
    expect(html).not.toContain("providers.detail.notif_keyword_only");
  });

  it("hides the toggle when auto_activate_keyword is empty or whitespace", async () => {
    for (const keyword of ["", "   ", "\t"]) {
      const html = await modelsSectionHtml(
        makeProvider({ auto_activate_keyword: keyword, notif_keyword_only: true }),
      );
      expect(html, `keyword=${JSON.stringify(keyword)}`).not.toContain('role="switch"');
    }
  });

  it("shows the toggle when a keyword is set", async () => {
    const html = await modelsSectionHtml(
      makeProvider({ auto_activate_keyword: "gpt-5", notif_keyword_only: false }),
    );
    expect(html).toContain('role="switch"');
    expect(html).toContain("providers.detail.notif_keyword_only");
  });

  it("reflects OFF state: aria-checked=false and the off help text", async () => {
    const html = await modelsSectionHtml(
      makeProvider({ auto_activate_keyword: "gpt-5", notif_keyword_only: false }),
    );
    expect(html).toContain('aria-checked="false"');
    expect(html).toContain("toggle-btn off");
    expect(html).toContain("providers.detail.notif_keyword_only_help_off");
  });

  it("reflects ON state: aria-checked=true and the on help text", async () => {
    const html = await modelsSectionHtml(
      makeProvider({ auto_activate_keyword: "gpt-5", notif_keyword_only: true }),
    );
    expect(html).toContain('aria-checked="true"');
    expect(html).toContain("toggle-btn on");
    expect(html).toContain("providers.detail.notif_keyword_only_help_on");
  });

  it("treats an absent field as OFF (three-state default)", async () => {
    const provider = makeProvider({ auto_activate_keyword: "gpt-5" });
    delete provider.notif_keyword_only;
    const html = await modelsSectionHtml(provider);
    expect(html).toContain('aria-checked="false"');
    expect(html).toContain("providers.detail.notif_keyword_only_help_off");
  });
});