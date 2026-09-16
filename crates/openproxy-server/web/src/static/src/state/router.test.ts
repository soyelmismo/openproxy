import { describe, it, expect, beforeAll, beforeEach, afterEach, vi } from "vitest";
import type { ParsedHash, RouteName } from "./router.js";

function mountShell(): void {
  document.body.innerHTML = '<div id="app"><main id="main"></main></div>';
}

let parseHash!: (hash: string) => ParsedHash | null;
let shellMain!: HTMLElement;
let mountDebugLogsSpy!: ReturnType<typeof vi.spyOn>;
let mountProvidersSpy!: ReturnType<typeof vi.spyOn>;
let sidebarSource!: string;

beforeAll(async () => {
  if (typeof window.matchMedia !== "function") {
    Object.defineProperty(window, "matchMedia", {
      configurable: true,
      writable: true,
      value: vi.fn().mockImplementation((q: string) => ({
        matches: false, media: q, onchange: null,
        addListener: (): void => {}, removeListener: (): void => {},
        addEventListener: (): void => {}, removeEventListener: (): void => {},
        dispatchEvent: (): boolean => false,
      })),
    });
  }

  mountShell();
  const main = document.getElementById("main");
  if (!main) throw new Error("test setup: #main missing after mountShell()");
  shellMain = main;

  const debugLogsView = await import("../views/debug-logs.js");
  mountDebugLogsSpy = vi.spyOn(debugLogsView, "mountDebugLogs").mockImplementation((_c) => (): void => {});

  const providersView = await import("../views/providers/index.js");
  mountProvidersSpy = vi.spyOn(providersView, "mountProviders").mockImplementation(() => Promise.resolve());

  const mod = await import("./router.js");
  parseHash = mod.parseHash;

  // @ts-expect-error vite ?raw import, untyped module
  const raw = await import("../components/sidebar.ts?raw");
  sidebarSource = raw.default as string;
});

beforeEach(mountShell);
afterEach(() => {
  document.body.innerHTML = "";
  window.location.hash = "";
});

const VALID_CASES: readonly (readonly [string, RouteName, string])[] = [
  ["", "home", ""], ["#", "home", ""], ["#/", "home", ""], ["/", "home", ""],
  ["#/login", "login", ""], ["#/login/", "login", ""],
  ["#/providers", "providers", ""], ["#/providers/abc-123", "provider-detail", "abc-123"],
  ["#/combos/42", "combo-detail", "42"], ["#/combos/0", "combo-detail", "0"], ["#/combos/007", "combo-detail", "007"],
  ["#/keys", "keys", ""], ["#/keys/42/usage", "key-usage", "42"], ["#/keys/0/usage", "key-usage", "0"],
  ["#/analytics", "analytics", ""], ["#/analytics?range=this_month", "analytics", ""],
  ["#/logs", "logs", ""], ["#/logs?foo=bar", "logs", ""],
  ["#/debug-logs", "debug-logs", ""], ["#/config", "config", ""],
  ["#/proxies", "proxies", ""], ["#/proxy-sources", "proxy-sources", ""],
  ["#/playground", "playground", ""], ["#/notifications", "notifications", ""],
  ["#/providers/%20abc", "provider-detail", "%20abc"],
];

function parseOrFail(hash: string): ParsedHash {
  const parsed = parseHash(hash);
  if (!parsed) throw new Error(`route table regression: '${hash}' no longer parses`);
  return parsed;
}

describe("parseHash — route table (spec §1.4 / P1.2a)", () => {
  it("resolves valid routes and context correctly", () => {
    for (const [hash, name, ctx] of VALID_CASES) {
      const parsed = parseOrFail(hash);
      expect(parsed.name, `hash '${hash}'`).toBe(name);
      expect(parsed.context, `context for '${hash}'`).toBe(ctx);
    }
  });

  it("returns null on invalid routes", () => {
    for (const h of ["#/combos/abc", "#/foo", "#/providers/", "#/combos/42/", "#/PROVIDERS", "#/Providers", " #/login", "#/login "]) {
      expect(parseHash(h), `hash '${h}'`).toBeNull();
    }
  });
});

describe("parseHash — boundaries and coercion", () => {
  it("falsy non-string input falls back to home", () => {
    const asString: (v: unknown) => string = (v) => v as string;
    expect(parseHash(asString(null))?.name).toBe("home");
    expect(parseHash(asString(undefined))?.name).toBe("home");
  });

  it("query parameter and capture quirks", () => {
    expect(parseOrFail("#/providers/abc-123?x=1").context).toBe("abc-123?x=1");
    expect(parseHash("#/combos/42?page=2")).toBeNull();
    expect(parseHash("#/keys/42/usage?since=1d")).toBeNull();
    expect(parseOrFail("#/providers/#/combos").context).toBe("#/combos");
    const longId = "a".repeat(10_000);
    expect(parseOrFail(`#/providers/${longId}`).context).toBe(longId);
  });
});

describe("parseHash — URL decoding contract (case l)", () => {
  it("parseHash returns raw capture; provider-detail mount decodes context", async () => {
    expect(parseOrFail("#/providers/%20abc").context).toBe("%20abc");
    for (const [raw, expected] of [["%20abc", " abc"], ["%2520abc", "%20abc"], ["abc%2Fdef", "abc/def"]]) {
      mountProvidersSpy.mockClear();
      const parsed = parseOrFail(`#/providers/${raw}`);
      await parsed.mount(parsed.context);
      expect(mountProvidersSpy).toHaveBeenCalledWith({ detailId: expected });
    }
  });
});

describe("mount invariant — regression guard for the <main>-empty bug class", () => {
  it("every parsed route exposes a callable mount", () => {
    for (const [hash] of VALID_CASES) {
      const parsed = parseOrFail(hash);
      expect(typeof parsed.mount, `mount for '${hash}'`).toBe("function");
    }
  });

  it("'#/debug-logs' mount resolves #main at CALL time and returns cleanup or undefined", async () => {
    mountDebugLogsSpy.mockClear();
    const parsed = parseOrFail("#/debug-logs");
    const ret = await parsed.mount(parsed.context);
    expect(mountDebugLogsSpy).toHaveBeenCalledWith(shellMain);
    expect(typeof ret).toBe("function");

    document.body.innerHTML = '<div id="app"></div>';
    expect(parseOrFail("#/debug-logs").mount("")).toBeUndefined();
  });
});

describe("sidebar route invariant (spec §1.4d) — no orphan links", () => {
  function extractSidebarHashes(src: string): string[] {
    const re = /(?:\bhref:\s*|\bnavigate\(\s*|\blocation\.hash\s*=\s*)(["'`])(#\/[^"'`\r\n]*)\1/g;
    const found = new Set<string>();
    for (const m of src.matchAll(re)) {
      if (m[2]) found.add(m[2]);
    }
    return [...found];
  }

  it("every href/navigate target in sidebar.ts resolves to a registered route", () => {
    const hashes = extractSidebarHashes(sidebarSource);
    expect(hashes.length).toBeGreaterThanOrEqual(12);
    expect(hashes).toEqual(expect.arrayContaining(["#/", "#/providers", "#/debug-logs"]));
    const orphans = hashes.filter((h: string) => parseHash(h) === null);
    expect(orphans).toEqual([]);
  });
});
