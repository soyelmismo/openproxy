// lib/view-utils.ts — shared view lifecycle helpers.
//
// Two patterns:
// 1. GENERIC (Q15): createView<T>({ loader, render, … }) handles the full loading →
//    success/empty/error lifecycle with optional post-load hooks and cleanup. Suits one-shot
//    data-fetching views (combos, config, keys, proxies, …).
// 2. LEGACY: createView(renderFn, loadData, setError?) — positional-args form kept for
//    backward compatibility; keys, key-usage, proxy-sources, proxies, playground and config
//    still call it unchanged.
//
// Views with a complex lifecycle (polling loops, WebSocket subscriptions, chart instances,
// multi-phase background refresh) must NOT use createView — handle them manually. Future
// migration candidates, once their lifecycle is simplified:
//   - debug-logs.ts — chained-setTimeout polling with epoch-based cancellation; takes a
//     container param instead of #main
//   - notifications.ts — WS subscriptions, cursor pagination, DnD overlay lifecycle
//   - analytics.ts — multi-fetch, uPlot chart create/destroy cycle
//   - providers.ts — dual-mode (grid + detail), background refresh
//   - home.ts — live-store WebSocket, uPlot sparklines + charts
//   - logs.ts — WebSocket message handler, recording state toggle

import { html, type TemplateResult } from "lit-html";
import { mountView, requestUpdate } from "../state/reactive.js";

/**
 * Options for the generic `createView<T>` overload (Q15).
 *
 * Handles the full loading → success/empty/error lifecycle. Returns a cleanup function that
 * tears down the lit-html container and runs any custom cleanup.
 *
 * @template T — The data type returned by the loader.
 */
export interface ViewFactoryOptions<T> {
  /** Container to mount into. Defaults to `document.getElementById("main")`. */
  container?: HTMLElement | null;
  /** Async data loader, called after the loading skeleton is shown. */
  loader: () => Promise<T>;
  /** Render function called with the loaded data on success. */
  render: (data: T) => TemplateResult;
  /** Shown while the loader is in flight. Default: `<div class="loading">Loading...</div>`. */
  loading?: () => TemplateResult;
  /** Whether data should trigger the empty phase. Default: `Array.isArray(data) &&
   *  data.length === 0`. */
  empty?: (data: T) => boolean;
  /** Template for the empty state. */
  emptyMessage?: () => TemplateResult;
  /** Custom error rendering. Default: `<div class="banner banner-error">`. */
  error?: (err: unknown) => TemplateResult;
  /** Called after a successful load, before the first render with data. */
  onLoaded?: (data: T) => void;
  /** Called on cleanup/teardown (view unmount). */
  cleanup?: () => void;
}

const DEFAULT_LOADING = (): TemplateResult =>
  html`<div class="loading">Loading...</div>`;
const DEFAULT_EMPTY = (): TemplateResult =>
  html`<p class="empty">No data.</p>`;
const DEFAULT_ERROR = (err: unknown): TemplateResult =>
  html`<div class="banner banner-error">${err instanceof Error ? err.message : String(err)}</div>`;

/**
 * Generic view lifecycle factory (Q15).
 *
 * Mounts a loading skeleton immediately, runs the async loader, then transitions to the
 * success, empty or error phase. Returns a cleanup function that tears down the lit-html
 * container and runs any custom cleanup.
 */
export async function createView<T>(
  options: ViewFactoryOptions<T>,
): Promise<() => void>;

/**
 * Legacy positional-args overload (backward compatible).
 *
 * Mounts `renderFn` immediately (the user sees the loading skeleton), awaits `loadData()`
 * (which should populate module-local state), then calls `requestUpdate()` to re-render with
 * the fetched data. If `loadData()` throws, `setError(msg)` receives the error message so the
 * re-render shows the error state. Returns the cleanup function that tears down the container.
 */
export async function createView(
  renderFn: () => TemplateResult,
  loadData: () => Promise<void>,
  setError?: (msg: string) => void,
  container?: HTMLElement | null,
): Promise<(() => void) | void>;

export async function createView<T>(
  optionsOrRenderFn: ViewFactoryOptions<T> | (() => TemplateResult),
  loadData?: () => Promise<void>,
  setError?: (msg: string) => void,
  containerArg?: HTMLElement | null,
): Promise<(() => void) | void> {
  // ── Generic pattern (options object) ─────────────────────────────────────
  if (typeof optionsOrRenderFn !== "function") {
    const opts = optionsOrRenderFn;
    const container = opts.container ?? document.getElementById("main");
    if (!container || !container.isConnected) return () => {};

    let phase: "loading" | "success" | "error" | "empty" = "loading";
    let data!: T;
    let loadErr: unknown;

    const renderPhase = (): TemplateResult => {
      switch (phase) {
        case "loading":
          return (opts.loading ?? DEFAULT_LOADING)();
        case "error":
          return (opts.error ?? DEFAULT_ERROR)(loadErr);
        case "empty":
          return (opts.emptyMessage ?? DEFAULT_EMPTY)();
        case "success":
          return opts.render(data);
      }
    };

    const cleanupView = mountView(container, renderPhase);
    try {
      data = await opts.loader();
      if (!container.isConnected) {
        cleanupView();
        return () => { opts.cleanup?.(); };
      }
      const isEmpty =
        opts.empty !== undefined
          ? opts.empty(data)
          : Array.isArray(data) && data.length === 0;
      if (isEmpty) {
        phase = "empty";
      } else {
        opts.onLoaded?.(data);
        phase = "success";
      }
    } catch (e: unknown) {
      if (!container.isConnected) {
        cleanupView();
        return () => { opts.cleanup?.(); };
      }
      loadErr = e;
      phase = "error";
    }
    requestUpdate();
    return () => {
      opts.cleanup?.();
      cleanupView();
    };
  }

  // ── Legacy pattern (positional args) ─────────────────────────────────────

  const renderFn = optionsOrRenderFn;
  const container = containerArg !== undefined ? containerArg : document.getElementById("main");
  if (!container || !container.isConnected) return () => {};
  const cleanup = mountView(container, renderFn);
  try {
    await loadData!();
    if (!container.isConnected) {
      cleanup();
      return () => {};
    }
  } catch (e: unknown) {
    if (!container.isConnected) {
      cleanup();
      return () => {};
    }
    if (setError) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }
  requestUpdate();
  return cleanup;
}

export interface AccountTierInput {
  quota_plan_name?: string | null;
  provider_id?: string | null;
  auth_type?: string | null;
  oauth_provider_specific?: string | null;
  quota_weekly_used?: number | null;
  quota_session_limit?: number | null;
  quota_session_used?: number | null;
}

export interface ProviderTierInput {
  id?: string | null;
  name?: string | null;
}

const GENERIC_PLACEHOLDERS = new Set([
  "account",
  "default",
  "generic",
  "oauth",
  "api key",
  "apikey",
  "standard",
]);

const KNOWN_TIERS: Record<string, string> = {
  free: "Free",
  gratis: "Free",
  plus: "Plus",
  pro: "Pro",
  go: "Go",
  team: "Team",
  teams: "Teams",
  enterprise: "Enterprise",
  starter: "Starter",
  ultra: "Ultra",
  max: "Max",
  tier1: "Tier 1",
  tier2: "Tier 2",
  tier3: "Tier 3",
  tier4: "Tier 4",
  tier5: "Tier 5",
  business: "Business",
  unlimited: "Unlimited",
  "coding plan": "Coding Plan",
  coding_plan: "Coding Plan",
};

function isGenericProviderPlaceholder(s: string, providerId: string, providerName: string): boolean {
  const norm = s.toLowerCase().trim();
  if (!norm) return true;
  if (GENERIC_PLACEHOLDERS.has(norm)) return true;
  if (norm === "codex / chatgpt" || norm === "codex" || norm === "chatgpt") return true;
  if (providerId && norm === providerId) return true;
  if (providerName && norm === providerName) return true;
  if (providerId && norm === `${providerId} account`) return true;
  if (providerName && norm === `${providerName} account`) return true;
  return false;
}

function cleanTierString(s: string, providerId: string, providerName: string): string | null {
  const trimmed = s.trim();
  if (!trimmed) return null;
  const lower = trimmed.toLowerCase();

  if (isGenericProviderPlaceholder(trimmed, providerId, providerName)) {
    return null;
  }

  // Exact known tier match
  if (KNOWN_TIERS[lower]) {
    return KNOWN_TIERS[lower];
  }

  // Strip known provider prefix (e.g. "OpenCode Go" -> "Go", "CodeBuddy Enterprise" -> "Enterprise", "Devin Teams" -> "Teams")
  const words = trimmed.split(/\s+/);
  if (words.length > 1) {
    const lastWord = words[words.length - 1]!;
    const lastLower = lastWord.toLowerCase();
    if (KNOWN_TIERS[lastLower]) {
      return KNOWN_TIERS[lastLower];
    }
    const firstLower = words[0]!.toLowerCase();
    if (firstLower === providerId || (providerName && firstLower === providerName.split(" ")[0]!.toLowerCase())) {
      const rest = words.slice(1).join(" ").trim();
      if (!isGenericProviderPlaceholder(rest, providerId, providerName)) {
        return rest;
      }
      return null;
    }
  }

  return trimmed;
}

/**
 * Resolves the real subscription tier of an account (e.g. "Free", "Plus", "Pro", "Go", "Team", "Enterprise").
 * Strips provider branding, duplicate provider names, and generic placeholders.
 * Returns null if no genuine subscription tier can be resolved.
 */
export function resolveAccountTier(
  account: AccountTierInput,
  provider?: ProviderTierInput | null,
): string | null {
  const providerId = (account.provider_id || provider?.id || "").toLowerCase().trim();
  const providerName = (provider?.name || "").toLowerCase().trim();

  let meta: Record<string, unknown> | null = null;
  if (account.oauth_provider_specific) {
    try {
      meta = JSON.parse(account.oauth_provider_specific);
    } catch {
      // ignore
    }
  }

  // 1. If meta explicitly specifies a tier or subscription name
  if (meta) {
    for (const key of ["tier", "plan", "subscription_tier", "subscriptionTier", "plan_name", "planName"]) {
      const val = meta[key];
      if (typeof val === "string" && val.trim().length > 0) {
        const cleaned = cleanTierString(val, providerId, providerName);
        if (cleaned) return cleaned;
      }
    }
  }

  const raw = account.quota_plan_name?.trim();

  // 2. Provider-specific intelligent normalization
  if (providerId === "codex") {
    // If raw plan is an explicit non-generic tier (e.g. "Pro", "Plus", "Team"), use it
    if (raw && !isGenericProviderPlaceholder(raw, providerId, providerName)) {
      const cleaned = cleanTierString(raw, providerId, providerName);
      if (cleaned) return cleaned;
    }

    // A paid ChatGPT/Codex subscription (Plus/Pro) has banked reset credits or a weekly quota window.
    // Free accounts have reset_credits == 0 (or null) and NO weekly window.
    const hasPaidCredits = meta?.["reset_credits"] != null && Number(meta["reset_credits"]) > 0;
    const hasWeeklyQuota = account.quota_weekly_used != null;

    return (hasPaidCredits || hasWeeklyQuota) ? "Plus" : "Free";
  }

  if (!raw) return null;

  return cleanTierString(raw, providerId, providerName);
}
