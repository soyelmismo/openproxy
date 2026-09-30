import { describe, expect, it, vi } from "vitest";
import { render } from "lit-html";
import type { TemplateResult } from "lit-html";
import type { Provider } from "../../lib/types/api.js";

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
});
