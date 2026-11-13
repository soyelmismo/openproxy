// __test-utils__/index.ts — shared helpers for the dashboard unit tests, centralising patterns
// that would otherwise be copy-pasted across the state/*.test.ts files:
//   - `setupTimers()` — fake timers + module reset per case
//   - `setupAuthMock()` — spy on auth.getToken / isLoggedIn
//   - `spyModule()` — generic single-method spy
//
// `vi.mock` is avoided (oxlint anti-slop/no-module-mocking) in favour of `vi.spyOn` over
// `await import(...)` after `vi.resetModules()`, so every test starts from a clean module graph.

import { vi, beforeEach, afterEach } from "vitest";

/**
 * Configure fake timers and reset the module graph per case.
 *
 * Call once at the top of a `describe(...)` block. Pairs:
 *   - `beforeEach`: enable fake timers + reset modules
 *   - `afterEach`:  restore real timers, unstub globals, restore mocks
 *
 * Tests that import the module graph themselves (the standard pattern here) then re-`await
 * import(...)` after the reset.
 */
export function setupTimers(): void {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.resetModules();
  });
  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });
}

/**
 * Import `state/auth.js` and spy on `getToken` + `isLoggedIn`.
 *
 * Returns the live module handle plus the two spies so individual tests can override the return
 * value via `getToken.mockReturnValueOnce(...)` or assert on call counts.
 *
 * The default mock returns a valid token with `isLoggedIn === true` — the logged-in baseline
 * every test in this suite assumes unless it explicitly opts out via `opts`.
 */
export async function setupAuthMock(
  opts: { token?: string | null; loggedIn?: boolean; fullAdmin?: boolean } = {},
): Promise<{
  auth: typeof import("../state/auth.js");
  getToken: ReturnType<typeof vi.spyOn>;
  isLoggedIn: ReturnType<typeof vi.spyOn>;
  isFullAdmin: ReturnType<typeof vi.spyOn>;
}> {
  const auth = await import("../state/auth.js");
  // Use `in` to distinguish "token not provided" (use the default) from "token explicitly set
  // to null" (no auth). `??` treats null as nullish and would fall back to the default.
  const token: string | null = "token" in opts ? opts.token! : "mock-token-123";
  const getToken = vi
    .spyOn(auth, "getToken")
    .mockReturnValue(token);
  const isLoggedIn = vi
    .spyOn(auth, "isLoggedIn")
    .mockReturnValue(opts.loggedIn ?? true);
  // `isFullAdmin()` is fail-closed on an unknown profile, so the logged-in
  // operator baseline these suites assume needs the manage profile stated.
  const isFullAdmin = vi
    .spyOn(auth, "isFullAdmin")
    .mockReturnValue(opts.fullAdmin ?? true);
  return { auth, getToken, isLoggedIn, isFullAdmin };
}

/**
 * Import a module and spy on a single method with a given implementation. The module is loaded as
 * a record keyed by string so `vi.spyOn` can resolve a method-shaped key without the caller
 * spelling out the full module type. Callers can re-cast the returned `mod` to a more specific
 * shape if needed.
 */
export async function spyModule(
  path: string,
  method: string,
  impl: (...args: unknown[]) => unknown,
): Promise<{ mod: Record<string, unknown>; spy: ReturnType<typeof vi.spyOn> }> {
  const mod = (await import(path)) as Record<string, unknown>;
  // The local interface widens the property to a function so the vi.spyOn generic resolves to
  // Mock<Procedure> rather than `never`.
  const target = mod as Record<string, (...args: unknown[]) => unknown>;
  const spy = vi.spyOn(target, method as keyof typeof target);
  spy.mockImplementation(impl);
  return { mod, spy };
}
