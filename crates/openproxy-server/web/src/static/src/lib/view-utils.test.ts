// lib/view-utils.test.ts — unit tests for the generic createView<T>() overload (Q15 pattern).
// Covers the full lifecycle (loading → success/empty/error) plus the cleanup function, using the
// real mountView/requestUpdate from state/reactive.ts (they work in jsdom) and a real container
// element.

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { html } from "lit-html";
import { createView, resolveAccountTier } from "./view-utils.js";

let container: HTMLElement;

beforeEach(() => {
  container = document.createElement("div");
  container.id = "test-container";
  document.body.appendChild(container);
});

afterEach(() => {
  document.body.innerHTML = "";
  vi.restoreAllMocks();
});

function flushMicrotasks(): Promise<void> {
  return new Promise((r) => setTimeout(r, 0));
}

describe("createView (generic overload)", () => {
  it("executes the loader and renders the data", async () => {
    const loader = vi.fn().mockResolvedValue(["item1", "item2"]);
    const renderFn = vi.fn().mockReturnValue(html`<ul>${["item1", "item2"].map((i) => html`<li>${i}</li>`)}</ul>`);

    const cleanup = await createView({ container, loader, render: renderFn });
    // requestUpdate fires on a microtask.
    await flushMicrotasks();

    expect(loader).toHaveBeenCalledOnce();
    expect(renderFn).toHaveBeenCalledOnce();
    expect(container.textContent).toContain("item1");
    expect(container.textContent).toContain("item2");

    cleanup();
  });

  it("shows the loading state while the loader is in flight", async () => {
    let resolveLoader!: (v: string[]) => void;
    const loader = vi.fn().mockImplementation(
      () => new Promise<string[]>((resolve) => { resolveLoader = resolve; }),
    );
    const renderFn = vi.fn().mockReturnValue(html`<span>data</span>`);

    createView({ container, loader, render: renderFn });
    // Let the loading template render (mountView renders synchronously).
    await flushMicrotasks();

    expect(container.textContent).toContain("Loading...");
    expect(renderFn).not.toHaveBeenCalled();

    resolveLoader(["a"]);
    await flushMicrotasks();

    expect(container.textContent).toContain("a");
    expect(container.textContent).not.toContain("Loading...");
  });

  it("shows the error state when the loader rejects", async () => {
    const loader = vi.fn().mockRejectedValue(new Error("network timeout"));
    const renderFn = vi.fn().mockReturnValue(html`<span>ok</span>`);

    await createView({ container, loader, render: renderFn });
    await flushMicrotasks();

    expect(container.textContent).toContain("network timeout");
    expect(renderFn).not.toHaveBeenCalled();
  });

  it("shows the default error template when error option is not provided", async () => {
    const loader = vi.fn().mockRejectedValue(new Error("boom"));
    const renderFn = vi.fn().mockReturnValue(html`<span>data</span>`);

    await createView({ container, loader, render: renderFn });
    await flushMicrotasks();

    expect(container.querySelector(".banner-error")).not.toBeNull();
    expect(container.textContent).toContain("boom");
  });

  it("uses custom error rendering when error option is provided", async () => {
    const loader = vi.fn().mockRejectedValue(new Error("custom err"));
    const renderFn = vi.fn().mockReturnValue(html`<span>data</span>`);
    const errorFn = vi.fn().mockReturnValue(html`<div class="my-error">Custom</div>`);

    await createView({ container, loader, render: renderFn, error: errorFn });
    await flushMicrotasks();

    expect(errorFn).toHaveBeenCalledOnce();
    expect(container.textContent).toContain("Custom");
  });

  it("shows empty state when data is an empty array (default empty check)", async () => {
    const loader = vi.fn().mockResolvedValue([]);
    const renderFn = vi.fn().mockReturnValue(html`<span>should not render</span>`);

    await createView({ container, loader, render: renderFn });
    await flushMicrotasks();

    expect(container.textContent).toContain("No data.");
    expect(renderFn).not.toHaveBeenCalled();
  });

  it("uses custom empty check and message when provided", async () => {
    const loader = vi.fn().mockResolvedValue({ items: [] });
    const renderFn = vi.fn().mockReturnValue(html`<span>data</span>`);
    const emptyFn = vi.fn().mockReturnValue(true);
    const emptyMsgFn = vi.fn().mockReturnValue(html`<p>Nothing here</p>`);

    await createView({
      container,
      loader,
      render: renderFn,
      empty: emptyFn,
      emptyMessage: emptyMsgFn,
    });
    await flushMicrotasks();

    expect(emptyFn).toHaveBeenCalledOnce();
    expect(emptyMsgFn).toHaveBeenCalledOnce();
    expect(container.textContent).toContain("Nothing here");
    expect(renderFn).not.toHaveBeenCalled();
  });

  it("calls onLoaded after a successful load", async () => {
    const onLoaded = vi.fn();
    const loader = vi.fn().mockResolvedValue(["data"]);

    await createView({
      container,
      loader,
      render: () => html`<span>ok</span>`,
      onLoaded,
    });
    await flushMicrotasks();

    expect(onLoaded).toHaveBeenCalledOnce();
    expect(onLoaded).toHaveBeenCalledWith(["data"]);
  });

  it("returns a cleanup function that tears down the container", async () => {
    const cleanupFn = vi.fn();
    const loader = vi.fn().mockResolvedValue(["x"]);

    const cleanup = await createView({
      container,
      loader,
      render: () => html`<span>x</span>`,
      cleanup: cleanupFn,
    });
    await flushMicrotasks();

    expect(typeof cleanup).toBe("function");
    cleanup!();
    expect(cleanupFn).toHaveBeenCalledOnce();
  });

  it("returns empty cleanup when container is null", async () => {
    const loader = vi.fn().mockResolvedValue(["x"]);

    const cleanup = await createView({
      container: null,
      loader,
      render: () => html`<span>x</span>`,
    });

    // Should be a no-op function, not undefined.
    expect(typeof cleanup).toBe("function");
    expect(() => cleanup!()).not.toThrow();
  });

  it("uses loading skeleton from options when provided", async () => {
    let resolveLoader!: (v: string[]) => void;
    const loader = vi.fn().mockImplementation(
      () => new Promise<string[]>((resolve) => { resolveLoader = resolve; }),
    );
    const customLoading = vi.fn().mockReturnValue(html`<div class="spinner">Please wait...</div>`);

    createView({ container, loader, render: () => html`<span>x</span>`, loading: customLoading });
    await flushMicrotasks();

    expect(customLoading).toHaveBeenCalled();
    expect(container.textContent).toContain("Please wait...");

    resolveLoader(["done"]);
    await flushMicrotasks();
  });

  it("renders non-array data that passes the default empty check", async () => {
    const loader = vi.fn().mockResolvedValue({ name: "test" });
    const renderFn = vi.fn().mockReturnValue(html`<span>rendered</span>`);

    await createView({ container, loader, render: renderFn });
    await flushMicrotasks();

    // Non-array data is not empty by default (the Array.isArray check fails).
    expect(renderFn).toHaveBeenCalledOnce();
    expect(container.textContent).toContain("rendered");
  });

  it("legacy positional overload accepts an explicit container and renders into it", async () => {
    let loaded = false;
    const cleanup = await createView(
      () => html`<span>${loaded ? "legacy-loaded" : "legacy-loading"}</span>`,
      async () => {
        loaded = true;
      },
      undefined,
      container,
    );
    await flushMicrotasks();

    expect(container.textContent).toContain("legacy-loaded");
    cleanup?.();
  });

  it("legacy positional overload skips execution if container is already disconnected", async () => {
    container.remove();
    const loadFn = vi.fn().mockResolvedValue(undefined);
    const cleanup = await createView(
      () => html`<span>test</span>`,
      loadFn,
      undefined,
      container,
    );
    await flushMicrotasks();

    expect(loadFn).not.toHaveBeenCalled();
    expect(typeof cleanup).toBe("function");
    cleanup?.();
  });
});

describe("resolveAccountTier (centralized, deduplicated, global)", () => {
  it("resolves clean tiers for Minimax (Free, Pro)", () => {
    expect(
      resolveAccountTier(
        { provider_id: "minimax", quota_plan_name: "Free" },
        { id: "minimax", name: "MiniMax" },
      ),
    ).toBe("Free");

    expect(
      resolveAccountTier(
        { provider_id: "minimax", quota_plan_name: "Pro" },
        { id: "minimax", name: "MiniMax" },
      ),
    ).toBe("Pro");
  });

  it("resolves Plus or Free for Codex, discarding generic 'Codex / ChatGPT'", () => {
    // Codex with reset credits -> Plus
    expect(
      resolveAccountTier(
        {
          provider_id: "codex",
          quota_plan_name: "Codex / ChatGPT",
          oauth_provider_specific: JSON.stringify({ reset_credits: 5 }),
        },
        { id: "codex", name: "OpenAI Codex" },
      ),
    ).toBe("Plus");

    // Codex with weekly limit -> Plus
    expect(
      resolveAccountTier(
        {
          provider_id: "codex",
          quota_plan_name: "Codex / ChatGPT",
          quota_weekly_used: 15,
        },
        { id: "codex", name: "OpenAI Codex" },
      ),
    ).toBe("Plus");

    // Codex with session limit and reset_credits 0 (bigdataligma@gmail.com real case) -> Free
    expect(
      resolveAccountTier(
        {
          provider_id: "codex",
          quota_plan_name: "Codex / ChatGPT",
          oauth_provider_specific: JSON.stringify({ reset_credits: 0 }),
          quota_session_used: 10,
          quota_session_limit: 100,
        },
        { id: "codex", name: "OpenAI Codex" },
      ),
    ).toBe("Free");

    // Codex without quotas or credits -> Free
    expect(
      resolveAccountTier(
        {
          provider_id: "codex",
          quota_plan_name: "Codex / ChatGPT",
        },
        { id: "codex", name: "OpenAI Codex" },
      ),
    ).toBe("Free");

    // Codex with null quota_plan_name -> defaults intelligently
    expect(
      resolveAccountTier(
        {
          provider_id: "codex",
          quota_plan_name: null,
          oauth_provider_specific: JSON.stringify({ reset_credits: 2 }),
        },
        { id: "codex", name: "OpenAI Codex" },
      ),
    ).toBe("Plus");
  });

  it("strips provider placeholders and extracts real tiers", () => {
    // Antigravity placeholder matching provider name -> null
    expect(
      resolveAccountTier(
        { provider_id: "antigravity", quota_plan_name: "Antigravity" },
        { id: "antigravity", name: "Antigravity" },
      ),
    ).toBeNull();

    // Antigravity Pro -> Pro
    expect(
      resolveAccountTier(
        { provider_id: "antigravity", quota_plan_name: "Pro" },
        { id: "antigravity", name: "Antigravity" },
      ),
    ).toBe("Pro");

    // OpenCode Go -> Go
    expect(
      resolveAccountTier(
        { provider_id: "opencode", quota_plan_name: "OpenCode Go" },
        { id: "opencode", name: "OpenCode" },
      ),
    ).toBe("Go");

    // Devin Teams -> Teams
    expect(
      resolveAccountTier(
        { provider_id: "devin", quota_plan_name: "Devin Teams" },
        { id: "devin", name: "Devin" },
      ),
    ).toBe("Teams");

    // CodeBuddy Enterprise -> Enterprise
    expect(
      resolveAccountTier(
        { provider_id: "codebuddy", quota_plan_name: "CodeBuddy Enterprise" },
        { id: "codebuddy", name: "CodeBuddy" },
      ),
    ).toBe("Enterprise");

    // CodeBuddy Account placeholder -> null
    expect(
      resolveAccountTier(
        { provider_id: "codebuddy", quota_plan_name: "CodeBuddy Account" },
        { id: "codebuddy", name: "CodeBuddy" },
      ),
    ).toBeNull();

    // Z.ai coding_plan -> Coding Plan
    expect(
      resolveAccountTier(
        { provider_id: "zai", quota_plan_name: "coding_plan" },
        { id: "zai", name: "Z.ai" },
      ),
    ).toBe("Coding Plan");
  });

  it("reads explicit tier from oauth_provider_specific meta", () => {
    expect(
      resolveAccountTier({
        provider_id: "custom",
        quota_plan_name: null,
        oauth_provider_specific: JSON.stringify({ tier: "Enterprise" }),
      }),
    ).toBe("Enterprise");
  });
});
