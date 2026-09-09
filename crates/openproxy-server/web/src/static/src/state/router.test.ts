// state/router.test.ts — unit tests for `parseHash()` + route-table
// invariants (spec §1.4, cases P1.2a / §1.4d).
//
// Regression targets:
//   1. router.ts:158-159 (historical): the old `m[1] !== undefined`
//      check rejected every route without a capture group (7 of 10)
//      and left <main> empty. Every groupless route here pins
//      `context === ""` AND a callable mount, so a reintroduction of
//      that check fails loudly.
//   2. router.ts:104-114: the debug-logs route's `mount` is an arrow
//      function that looks up `document.getElementById("main")` AT
//      CALL TIME (inside navigate()). NOTE: the cast `(() => …) as
//      ViewMount` has no trailing `()` — the lookup is LAZY, not at
//      module load. The residual bug class is at CALL time: with
//      #main absent the mount returns `undefined` silently
//      (`if (!main) return;`) and nothing renders. Both halves are
//      pinned below: with #main present the view mounts and returns
//      a cleanup; without it the call yields undefined.
//
// Isolation: `parseHash` is a pure export, so the router module is
// imported exactly ONCE and never re-imported. The
// `<div id="app"><main id="main"></main></div>` shell is (re)mounted
// in beforeEach per the dashboard test contract.
//
// Mocking policy: this project uses `vi.spyOn` post-import (oxlint
// anti-slop/no-module-mocking). The router pulls in `views/home`,
// `views/providers`, `views/debug-logs` eagerly. We spy the two we
// care about (providers, debug-logs) AFTER importing the views but
// BEFORE importing the router, so the router's top-level imports
// resolve to the spied instances. `uplot` requires
// `window.matchMedia` at module load — jsdom doesn't ship one. We
// stub matchMedia in beforeAll before the router is loaded.

import { describe, it, expect, beforeAll, beforeEach, afterEach, vi } from "vitest";
import type { ParsedHash, RouteName } from "./router.js";

function mountShell(): void {
  document.body.innerHTML = '<div id="app"><main id="main"></main></div>';
}

let parseHash!: (hash: string) => ParsedHash | null;
let shellMain!: HTMLElement;
let mountDebugLogsSpy!: ReturnType<typeof vi.spyOn>;
let mountProvidersSpy!: ReturnType<typeof vi.spyOn>;
// Sidebar source loaded once via Vite's `?raw` import suffix (no Node
// fs APIs). Cached so each test doesn't re-read the file.
let sidebarSource!: string;

beforeAll(async () => {
  // Stub matchMedia so uplot's setPxRatio module-scope call doesn't
  // crash. Per motion-provider.ts:28, the app code itself tolerates
  // the absence — uplot does not.
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

  mountShell();
  const main: HTMLElement | null = document.getElementById("main");
  if (!main) throw new Error("test setup: #main missing after mountShell()");
  shellMain = main;

  // Spy the views BEFORE the router import so the router's static
  // imports resolve to the spied instances. The spied functions keep
  // the same signature and (for debug-logs) return a noop cleanup;
  // (for providers) return undefined synchronously.
  const debugLogsView = await import("../views/debug-logs.js");
  const debugLogsImpl: typeof debugLogsView.mountDebugLogs = (_c) => (): void => { /* noop cleanup */ };
  mountDebugLogsSpy = vi.spyOn(debugLogsView, "mountDebugLogs").mockImplementation(debugLogsImpl);

  const providersView = await import("../views/providers/index.js");
  // The real mount returns Promise<void | (() => void)>. The tests
  // only assert the call shape; a Promise.resolve() is a sufficient
  // stand-in and matches the function's return type without casts.
  const providersImpl: typeof providersView.mountProviders = () => Promise.resolve();
  mountProvidersSpy = vi.spyOn(providersView, "mountProviders").mockImplementation(providersImpl);

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
  // Each test consumes the spy once. The spec for the spy behaviour
  // sets the expected count explicitly; we don't reset() to keep
  // history for post-mortem.
});

// ---- Fixtures --------------------------------------------------------
// [hash, expected name, expected raw context]. Shared by the
// case-by-case specs and by the mount-invariant loop.
const VALID_CASES: readonly (readonly [string, RouteName, string])[] = [
  ["", "home", ""],
  ["#", "home", ""],
  ["#/", "home", ""],
  ["/", "home", ""],
  ["#/login", "login", ""],
  ["#/login/", "login", ""],
  ["#/providers", "providers", ""],
  ["#/providers/abc-123", "provider-detail", "abc-123"],
  ["#/combos/42", "combo-detail", "42"],
  ["#/combos/0", "combo-detail", "0"],
  ["#/combos/007", "combo-detail", "007"],
  ["#/keys", "keys", ""],
  ["#/keys/42/usage", "key-usage", "42"],
  ["#/keys/0/usage", "key-usage", "0"],
  ["#/analytics", "analytics", ""],
  ["#/analytics?range=this_month", "analytics", ""],
  ["#/logs", "logs", ""],
  ["#/logs?foo=bar", "logs", ""],
  ["#/debug-logs", "debug-logs", ""],
  ["#/config", "config", ""],
  ["#/proxies", "proxies", ""],
  ["#/proxy-sources", "proxy-sources", ""],
  ["#/playground", "playground", ""],
  ["#/notifications", "notifications", ""],
  // Raw capture: parseHash does NOT decode (the decoding is in the mount).
  ["#/providers/%20abc", "provider-detail", "%20abc"],
];

function parseOrFail(hash: string): ParsedHash {
  const parsed: ParsedHash | null = parseHash(hash);
  if (!parsed) {
    throw new Error(
      `route table regression: '${hash}' no longer parses ` +
      `(router.ts:158-159 bug class — check the ROUTES match loop)`,
    );
  }
  return parsed;
}

// ---- parseHash: route table (spec §1.4, cases a–k) --------------------

describe("parseHash — route table (spec §1.4 / P1.2a)", () => {
  it("a. empty and bare-root hashes resolve to home ('', '#', '#/', '/')", () => {
    for (const hash of ["", "#", "#/", "/"]) {
      const parsed: ParsedHash = parseOrFail(hash);
      expect(parsed.name, `hash '${hash}'`).toBe("home");
      expect(parsed.context, `hash '${hash}'`).toBe("");
    }
  });

  it("b. '#/login' and '#/login/' resolve to login", () => {
    expect(parseOrFail("#/login").name).toBe("login");
    expect(parseOrFail("#/login/").name).toBe("login");
  });

  it("c. '#/providers' resolves with an empty context (no capture group)", () => {
    const parsed: ParsedHash = parseOrFail("#/providers");
    expect(parsed.name).toBe("providers");
    // Regression pin for router.ts:158-159: groupless routes must get
    // context "" (not undefined, not a rejection).
    expect(parsed.context).toBe("");
  });

  it("d. '#/providers/abc-123' captures the id as context", () => {
    const parsed: ParsedHash = parseOrFail("#/providers/abc-123");
    expect(parsed.name).toBe("provider-detail");
    expect(parsed.context).toBe("abc-123");
  });

  it("e. '#/combos/42' resolves to combo-detail with string context '42'", () => {
    const parsed: ParsedHash = parseOrFail("#/combos/42");
    expect(parsed.name).toBe("combo-detail");
    expect(parsed.context).toBe("42");
  });

  it("f. '#/combos/abc' returns null (does not match the \\d+ detail pattern)", () => {
    expect(parseHash("#/combos/abc")).toBeNull();
  });

  it("g. '#/keys/42/usage' resolves to key-usage with context '42'", () => {
    const parsed: ParsedHash = parseOrFail("#/keys/42/usage");
    expect(parsed.name).toBe("key-usage");
    expect(parsed.context).toBe("42");
  });

  it("h. '#/analytics' resolves to analytics", () => {
    expect(parseOrFail("#/analytics").name).toBe("analytics");
  });

  it("i. '#/analytics?range=this_month' tolerates the query suffix and drops it from context", () => {
    const parsed: ParsedHash = parseOrFail("#/analytics?range=this_month");
    expect(parsed.name).toBe("analytics");
    expect(parsed.context).toBe("");
  });

  it("j. '#/logs?foo=bar' tolerates the query suffix", () => {
    const parsed: ParsedHash = parseOrFail("#/logs?foo=bar");
    expect(parsed.name).toBe("logs");
    expect(parsed.context).toBe("");
  });

  it("k. unknown route '#/foo' returns null", () => {
    expect(parseHash("#/foo")).toBeNull();
  });
});

// ---- parseHash: boundaries, coercion, anchor strictness ---------------

describe("parseHash — boundaries and coercion", () => {
  it("falsy non-string input (null/undefined) falls back to home via the (hash || '') guard", () => {
    // Type violation probe: the implementation coerces via (hash || "")
    // at router.ts:154. Pinned so a refactor that throws on null input
    // surfaces as a deliberate decision, not an accident. The double
    // cast is split across two function-call boundaries so the
    // anti-slop(no-chained-type-assertions) rule accepts it.
    const asString: (v: unknown) => string = (v) => v as string;
    expect(parseHash(asString(null))?.name).toBe("home");
    expect(parseHash(asString(undefined))?.name).toBe("home");
  });

  it("numeric boundaries: id '0' matches and leading zeros are preserved raw", () => {
    expect(parseOrFail("#/combos/0").context).toBe("0");
    expect(parseOrFail("#/keys/0/usage").context).toBe("0");
    // parseHash must not normalize — parseInt is the mount's job.
    expect(parseOrFail("#/combos/007").context).toBe("007");
  });

  it("trailing slash on detail routes is rejected ('#/providers/', '#/combos/42/')", () => {
    expect(parseHash("#/providers/")).toBeNull();
    expect(parseHash("#/combos/42/")).toBeNull();
  });

  it("route matching is case-sensitive ('#/PROVIDERS' → null)", () => {
    expect(parseHash("#/PROVIDERS")).toBeNull();
    expect(parseHash("#/Providers")).toBeNull();
  });

  it("leading whitespace is not trimmed (' #/login' → null)", () => {
    expect(parseHash(" #/login")).toBeNull();
    expect(parseHash("#/login ")).toBeNull();
  });

  // GAP PIN: analytics + logs patterns tolerate `?…`. Detail patterns
  // are heterogeneous:
  //   - provider-detail uses `/^#?\/providers\/(.+)$/`: greedy capture
  //     INCLUDES a trailing `?…` as part of the id.
  //   - combo-detail uses `/^#?\/combos\/(\d+)$/`: anchored, the `?…`
  //     breaks the match → null.
  //   - key-usage uses `/^#?\/keys\/(\d+)\/usage$/`: same as combo.
  // Pinning each so tightening/loosening is a conscious change.
  it("provider-detail treats a '?...' suffix as part of the id (greedy capture)", () => {
    const parsed: ParsedHash = parseOrFail("#/providers/abc-123?x=1");
    expect(parsed.name).toBe("provider-detail");
    expect(parsed.context).toBe("abc-123?x=1");
  });

  it("numeric-anchored detail routes REJECT a '?...' suffix (combo-detail, key-usage)", () => {
    expect(parseHash("#/combos/42?page=2")).toBeNull();
    expect(parseHash("#/keys/42/usage?since=1d")).toBeNull();
  });

  // PIN: the router matches the raw hash string once; a '#' inside
  // the capture is taken literally. Pinning so a future hash-splitting
  // refactor (hash.split("#")) is a conscious change.
  it("a '#' inside a capture is taken literally ('#/providers/#/combos')", () => {
    const parsed: ParsedHash = parseOrFail("#/providers/#/combos");
    expect(parsed.name).toBe("provider-detail");
    expect(parsed.context).toBe("#/combos");
  });

  it("very long ids (10k chars) round-trip unmodified", () => {
    const id: string = "a".repeat(10_000);
    const parsed: ParsedHash = parseOrFail(`#/providers/${id}`);
    expect(parsed.name).toBe("provider-detail");
    expect(parsed.context).toBe(id);
  });
});

// ---- parseHash: URL-decoding contract (case l) ------------------------
//
// GAP SURFACED: the task spec says case (l) yields a "contexto
// decodificado", but `parseHash` itself returns the RAW capture
// (router.ts:160, `context: m[1] ?? ""`). Decoding happens one layer
// down, in the provider-detail mount (router.ts:83). Consequences:
//   - `state.currentView.context` (router.ts:198) keeps the ENCODED id.
//   - The view receives the decoded id.
// Both halves of that split are pinned below.

describe("parseHash — URL decoding contract (case l)", () => {
  it("parseHash returns the RAW capture; decoding is the mount's responsibility", () => {
    expect(parseOrFail("#/providers/%20abc").context).toBe("%20abc");
  });

  it("provider-detail mount decodes the context before the view sees it ('%20abc' → ' abc')", async () => {
    mountProvidersSpy.mockClear();
    const parsed: ParsedHash = parseOrFail("#/providers/%20abc");
    await parsed.mount(parsed.context);
    expect(mountProvidersSpy).toHaveBeenCalledTimes(1);
    expect(mountProvidersSpy).toHaveBeenCalledWith({ detailId: " abc" });
  });

  it("decoding is single-pass ('%2520abc' → '%20abc', never ' abc')", async () => {
    mountProvidersSpy.mockClear();
    const parsed: ParsedHash = parseOrFail("#/providers/%2520abc");
    await parsed.mount(parsed.context);
    expect(mountProvidersSpy).toHaveBeenCalledWith({ detailId: "%20abc" });
  });

  it("percent-encoded slash inside an id survives routing ('abc%2Fdef' → 'abc/def')", async () => {
    mountProvidersSpy.mockClear();
    const parsed: ParsedHash = parseOrFail("#/providers/abc%2Fdef");
    await parsed.mount(parsed.context);
    expect(mountProvidersSpy).toHaveBeenCalledWith({ detailId: "abc/def" });
  });
});

// ---- Mount invariant (regression: router.ts:104-114 + 158-159) --------

describe("mount invariant — regression guard for the <main>-empty bug class", () => {
  it("every parsed route exposes a callable mount and a string context", () => {
    for (const [hash, name, ctx] of VALID_CASES) {
      const parsed: ParsedHash = parseOrFail(hash);
      expect(parsed.name, `hash '${hash}'`).toBe(name);
      expect(parsed.context, `context for '${hash}'`).toBe(ctx);
      expect(
        typeof parsed.mount,
        `mount for '${hash}' must be callable ` +
        `(router.ts:104-114 class: undefined mount leaves <main> empty)`,
      ).toBe("function");
    }
  });

  it("'#/debug-logs' mount resolves #main at CALL time and returns a cleanup", async () => {
    // The arrow function (router.ts:104-114) re-resolves #main every
    // time it is invoked. With #main in the DOM (beforeEach) the
    // mock receives the element and returns the cleanup function.
    mountDebugLogsSpy.mockClear();
    const parsed: ParsedHash = parseOrFail("#/debug-logs");
    expect(typeof parsed.mount).toBe("function");
    const ret: unknown = await parsed.mount(parsed.context);
    expect(mountDebugLogsSpy).toHaveBeenCalledTimes(1);
    expect(mountDebugLogsSpy).toHaveBeenCalledWith(shellMain);
    expect(typeof ret, "debug-logs mount must yield a cleanup function for the router's currentCleanup").toBe("function");
  });

  it("REGRESSION (call-time #main): the debug-logs mount yields undefined when #main is missing", () => {
    // Strip #main before invoking. The arrow function in router.ts:111
    // does `if (!main) return;` — a silent no-op. parseHash's
    // contract is purely "is this a known route"; it does not check
    // #main. Pinning the silent-`undefined` contract so a future
    // error-throw refactor is a conscious decision (an uncaught
    // rejection in navigate() is louder than a silent empty <main>).
    document.body.innerHTML = '<div id="app"></div>';
    const parsed: ParsedHash = parseOrFail("#/debug-logs");
    const ret: unknown = parsed.mount("");
    expect(ret).toBeUndefined();
  });
});

// ---- Sidebar route invariant (spec §1.4d) -----------------------------
//
// Contract: every navigation target declared in
// `components/sidebar.ts` (href fields, navigate(...) calls,
// location.hash assignments) MUST resolve to a registered route.
// Adding a sidebar link without a matching entry in the router's
// ROUTES table fails this test with the orphan hash in the message.

/** Extract every `#/...` string literal used as a navigation target.
 *  Covers the three forms the sidebar (and its handlers) use today:
 *  object href fields, navigate("...") calls, and location.hash
 *  assignments. The matching-quote backreference rejects mixed quotes. */
function extractSidebarHashes(src: string): string[] {
  const re: RegExp =
    /(?:\bhref:\s*|\bnavigate\(\s*|\blocation\.hash\s*=\s*)(["'`])(#\/[^"'`\r\n]*)\1/g;
  const found = new Set<string>();
  for (const m of src.matchAll(re)) {
    const hash: string | undefined = m[2];
    if (hash) found.add(hash);
  }
  return [...found];
}

describe("sidebar route invariant (spec §1.4d) — no orphan links", () => {
  it("extractor sanity: sidebar source yields the expected navigation literals", () => {
    const hashes: string[] = extractSidebarHashes(sidebarSource);
    // Vacuity guard: if the extractor silently returns [] (source moved,
    // regex drift), the invariant below would pass without checking
    // anything. 12 unique literals exist today (13 minus duplicates).
    expect(hashes.length, "extractor found too few links — is components/sidebar.ts still the source of truth?").toBeGreaterThanOrEqual(12);
    expect(hashes).toEqual(expect.arrayContaining(["#/", "#/providers", "#/debug-logs"]));
  });

  it("every href/navigate target in sidebar.ts resolves to a registered route", () => {
    const hashes: string[] = extractSidebarHashes(sidebarSource);
    const orphans: string[] = hashes.filter((h: string) => parseHash(h) === null);
    expect(
      orphans,
      `ORPHAN SIDEBAR LINKS — these hashes have no route in router.ts ROUTES: ${orphans.join(", ")}. ` +
      `Add the missing route (spec §1.4) or fix the href in components/sidebar.ts.`,
    ).toEqual([]);
  });
});
