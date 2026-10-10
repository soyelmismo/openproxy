// views/quota-pools.ts — Specialized renderer for Z.ai dual-quota accounts.
//
// Z.ai accounts expose two independent quota pools (Starter free grant and the
// paid Coding Plan) via `Account.quota_pools`. Each pool has its own unit,
// figures, lifecycle and reset/expiry semantics, so the legacy single
// SessionWindow rendering is unusable: it would either hide one pool behind
// the other or sum unlike units.
//
// Invariants (see account-handler contract in AGENTS.md):
// - Starter (`zcode_starter`) is ALWAYS rendered first, the paid pool below it.
// - Pools are never summed: figures are per-pool and units may differ.
// - "No active Coding Plan" is only shown for a paid pool that is `absent`.
// - Starter grants are one-time: expiry wording, never "resets soon".
// - Paid plans roll: reset wording, never "expired".
// - A pool `fetch_error` fails that pool only; healthy siblings still render.
// - `used` is only shown when `remaining`/`limit` make it coherent (a token
//   pool with tokens spent shows remaining, not "max calls").
// - Provider-supplied strings are never trusted as HTML (XSS).

import { html, type TemplateResult } from "lit-html";
import type { ModelQuotaDetail, QuotaPool, QuotaPoolSource } from "../lib/types/api.js";

/** Per-pool numeric snapshot used by the helpers below. */
interface PoolFigures {
  /** Used portion percentage, or null when it cannot be computed. */
  usedPct: number | null;
  /** Remaining portion percentage, or null when it cannot be computed. */
  remainingPct: number | null;
  /** Whether `remaining` and `limit` agree well enough to trust `remaining`. */
  coherent: boolean;
}

/** Presentational colour classes shared with the legacy quota bars. */
const COLOR_OK = "ok";
const COLOR_WARN = "warn";
const COLOR_DANGER = "danger";
const COLOR_UNKNOWN = "unknown";

function finiteNumber(v: number | null | undefined): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

/** Clamp into [0,100] and round; non-finite input collapses to 0. */
function clampPct(v: number | null | undefined): number {
  const n = finiteNumber(v);
  if (n == null) return 0;
  return Math.min(100, Math.max(0, Math.round(n)));
}

function localeNum(v: number): string {
  return v.toLocaleString();
}

/** Bar colour for a "remaining" reading: low remaining is bad. */
function remainingColor(remPct: number | null): string {
  if (remPct == null) return COLOR_UNKNOWN;
  if (remPct <= 20) return COLOR_DANGER;
  if (remPct <= 50) return COLOR_WARN;
  return COLOR_OK;
}

/** Bar colour for a "used" reading: high usage is bad. */
function usedColor(usedPct: number | null): string {
  if (usedPct == null) return COLOR_UNKNOWN;
  if (usedPct > 80) return COLOR_DANGER;
  if (usedPct > 50) return COLOR_WARN;
  return COLOR_OK;
}

/**
 * Derive the trustworthy percentages for one pool.
 *
 * `remaining`/`limit` is preferred when both are finite, consistent with each
 * other and with `used`; otherwise `used`/`limit` is used. `used` alone is
 * never shown as a full bar percentage, because a pool reporting only `used`
 * (e.g. a tokens pool whose `limit` is unknown) would otherwise render 100%
 * consumed when nothing is known.
 */
function poolFigures(p: QuotaPool): PoolFigures {
  const used = finiteNumber(p.used);
  const limit = finiteNumber(p.limit);
  const remaining = finiteNumber(p.remaining);

  if (limit != null && limit > 0) {
    // Compute from any non-negative reading and let clampPct() enforce [0,100]
    // at render time. Rejecting out-of-range values here would instead render
    // them as 0%, which misreports an over-quota payload as "nothing used".
    const fromRem = remaining != null && remaining >= 0
      ? (remaining / limit) * 100
      : null;
    const fromUsed = used != null && used >= 0
      ? (1 - used / limit) * 100
      : null;

    // Prefer `remaining`, fall back to `used`. Treat as coherent when both
    // agree within a rounding step (guards against stale/partial payloads).
    const remPct = fromRem ?? fromUsed;
    const usedPct = fromUsed ?? (fromRem != null ? 100 - fromRem : null);
    const coherent = fromRem != null && fromUsed != null
      ? Math.abs(fromRem - fromUsed) <= 1
      : fromRem != null || fromUsed != null;
    return { usedPct, remainingPct: remPct, coherent };
  }

  return { usedPct: null, remainingPct: null, coherent: false };
}

/** Human label for the pool's unit, e.g. `tokens`, `requests`. */
function unitLabel(p: QuotaPool): string {
  const u = (p.unit ?? "").trim();
  return u.length > 0 ? u : "requests";
}

/** True when the pool's figures are already a normalized 0-100 percentage
 *  (paid Coding Plan). Counts must never be labelled with this unit. */
function isPercentageUnit(p: QuotaPool): boolean {
  return (p.unit ?? "").trim().toLowerCase() === "percentage";
}

/** Suffix for an absolute figure: `%` for normalized percentages, otherwise
 *  the pool's own measurement unit. */
function figureSuffix(p: QuotaPool): string {
  return isPercentageUnit(p) ? "%" : ` ${unitLabel(p)}`;
}

/** Source heading. Both pools surface the backend plan name when present. */
export function poolHeading(p: QuotaPool): string {
  const plan = (p.plan_name ?? "").trim();
  if (p.source === "zcode_starter") {
    return plan.length > 0 ? `Starter · Free · ${plan}` : "Starter · Free";
  }
  return plan.length > 0 ? `Coding Plan · ${plan}` : "Coding Plan";
}

/** Sub-line naming the plan/model coverage, e.g. "glm-4.6, glm-4.5". */
function coverageLine(p: QuotaPool): string {
  const models = p.model_ids.filter((m) => typeof m === "string" && m.trim().length > 0);
  if (models.length === 0) {
    // An empty list means "the whole catalog" only for the account-wide paid
    // plan; a Starter bucket authorizes nothing until its capabilities list
    // models, so saying "all models" would grant it coverage it never had.
    return p.source === "zcode_starter" ? "no models listed" : "all models";
  }
  return models.join(", ");
}

/**
 * Absolute-date hint. Accepts unix seconds or millis (seconds are assumed when
 * < 1e12), matching the backend `normalize_unix_secs` behaviour, or an ISO
 * string. Returns null when unparseable.
 */
export function parsePoolDate(ts: string | null | undefined): Date | null {
  if (ts == null) return null;
  const raw = String(ts).trim();
  if (raw.length === 0) return null;
  const num = Number(raw);
  if (!Number.isNaN(num)) {
    // Pure-numeric timestamps only; epoch 0 means "unset", not year 1970.
    if (num <= 0) return null;
    const ms = num > 1e11 ? num : num * 1000;
    const d = new Date(ms);
    return Number.isNaN(d.getTime()) ? null : d;
  }
  // Non-numeric input must look like a real date, not a bare token such as
  // "0" or "garbage" that `new Date()` would coerce into a phantom year.
  const d = new Date(raw);
  return Number.isNaN(d.getTime()) ? null : d;
}

/** Short `en`-locale absolute date, e.g. `12 mar 2026`. */
function absoluteDate(d: Date): string {
  return d.toLocaleDateString("en-GB", { day: "numeric", month: "short", year: "numeric" });
}

/**
 * Lifecycle suffix for a pool.
 *
 * Starter pools only get expiry wording (the grant is one-time: there is no
 * rolling reset to announce). Paid pools only get reset wording. `expired` and
 * `unavailable` status are reflected even when the timestamp is already past.
 */
function lifecycleSuffix(p: QuotaPool): string {
  const isStarter = p.source === "zcode_starter";
  const expiryTs = isStarter ? p.expires_at : null;
  const resetTs = isStarter ? null : p.reset_at;

  if (expiryTs) {
    const d = parsePoolDate(expiryTs);
    if (!d) return "";
    if (p.status === "expired") return ` · expired ${absoluteDate(d)}`;
    return ` · expires ${absoluteDate(d)}`;
  }

  if (resetTs) {
    const d = parsePoolDate(resetTs);
    if (!d) return "";
    if (p.status === "exhausted" && d.getTime() <= Date.now()) {
      return " · resets soon";
    }
    if (p.status === "expired") return ` · expired ${absoluteDate(d)}`;
    return ` · resets in ${relativeFrom(d)}`;
  }

  // Not yet started: a grant that activates in the future is neither active nor
  // expired, so say when it starts instead of implying it is unusable now.
  if (p.status === "unavailable" && p.starts_at) {
    const d = parsePoolDate(p.starts_at);
    if (d && d.getTime() > Date.now()) return ` · starts ${absoluteDate(d)}`;
  }

  if (p.status === "expired") return " · expired";
  if (p.status === "exhausted") return "";
  return "";
}

/** Compact future duration, e.g. `3d 4h`, `5h 12m`. */
function relativeFrom(d: Date): string {
  const diffMs = d.getTime() - Date.now();
  if (diffMs <= 0) return "now";
  const diffHrs = Math.floor(diffMs / (1000 * 60 * 60));
  const diffMins = Math.floor((diffMs % (1000 * 60 * 60)) / (1000 * 60));
  if (diffHrs >= 24) {
    const days = Math.floor(diffHrs / 24);
    const hrs = diffHrs % 24;
    return `${days}d ${hrs}h`;
  }
  return `${diffHrs}h ${diffMins}m`;
}

/** Right-hand figure for a pool bar. Never fabricates a zero. */
function poolValueText(p: QuotaPool): string {
  const used = finiteNumber(p.used);
  const limit = finiteNumber(p.limit);
  const remaining = finiteNumber(p.remaining);

  if (p.status === "exhausted") {
    // An exhausted pool genuinely consumed its allowance; the 0 remaining is
    // real information, not a missing reading.
    return `0${figureSuffix(p)} left`;
  }

  if (remaining != null && limit != null && limit > 0 && remaining >= 0 && remaining <= limit) {
    return isPercentageUnit(p) ? `${localeNum(remaining)}% left` : `${localeNum(remaining)} / ${localeNum(limit)} ${unitLabel(p)} left`;
  }

  if (used != null && limit != null && limit > 0) {
    return isPercentageUnit(p) ? `${localeNum(used)}% used` : `${localeNum(used)} / ${localeNum(limit)} ${unitLabel(p)} used`;
  }

  if (remaining != null && limit == null) {
    return `${localeNum(remaining)}${figureSuffix(p)} left`;
  }

  if (used != null && limit == null) {
    return `${localeNum(used)}${figureSuffix(p)} used`;
  }

  return "no usage data";
}

/** Models sub-list for a pool, mirroring the legacy `quota-model-row` markup. */
function renderPoolModels(p: QuotaPool): TemplateResult | null {
  const details = p.model_details;
  if (!details || details.length === 0) return null;
  const unit = unitLabel(p);
  return html`<div class="quota-model-list">
    ${details.map((d) => {
      const limit = finiteNumber(d.session_limit);
      const used = finiteNumber(d.session_used);
      const pct = limit != null && limit > 0 && used != null
        ? clampPct((used / limit) * 100)
        : null;
      const text = limit != null && limit > 0 && used != null
        ? (used > 0
            ? `${localeNum(used)} / ${localeNum(limit)} ${unit}`
            : `${localeNum(limit)} ${unit} available`)
        : "no usage data";
      const title = d.model_id;
      return html`<div class="quota-model-row">
        <div class="quota-model-header">
          <span class="quota-model-name" title="${title}">${title}</span>
          <span class="quota-model-text">${text}${lifecycleSuffix(p)}</span>
        </div>
        <div class="quota-bar mini ${usedColor(pct)}">
          <div class="quota-bar-track">
            <div class="quota-bar-fill" style="width: ${pct == null ? 0 : pct}%"></div>
          </div>
        </div>
      </div>`;
    })}
  </div>`;
}

/**
 * One pool row. Ordering and independence are the caller's job; this function
 * renders exactly one pool, with its own status, error and figures.
 */
function renderPool(p: QuotaPool): TemplateResult {
  const figs = poolFigures(p);
  const pct = clampPct(figs.remainingPct);
  const color = remainingColor(figs.remainingPct);
  const statusClass = `quota-pool status-${p.status}`;
  const barStatusClass = `quota-pool-bar status-${p.status}`;
  const title = poolHeading(p);
  const valueText = poolValueText(p);

  const badge = poolStatusBadge(p);
  const err = p.fetch_error
    ? html`<div class="quota-pool-error" role="status"><small>✗ ${p.fetch_error}</small></div>`
    : null;

  // A bar (and its progressbar role) is only emitted when the pool actually
  // reports figures, or is exhausted (where a 0 remaining is real data). An
  // "absent" or unresolvable pool gets an explicit note instead of a fake bar
  // that would read as 0/0 = 0%.
  const showBar = p.status !== "absent" && (finiteNumber(p.remaining) != null
    || finiteNumber(p.used) != null
    || p.status === "exhausted");

  const body = showBar
    ? html`<div class="quota-bar ${barStatusClass} ${color}">
        <div class="quota-bar-header">
          <span class="quota-pool-source" title="Quota source: ${p.source}">${coverageLine(p)}</span>
          <span class="quota-bar-label-right">${valueText}${lifecycleSuffix(p)}</span>
        </div>
        <div
          class="quota-bar-track"
          role="progressbar"
          tabindex="0"
          aria-label="${title} quota"
          aria-valuemin="0"
          aria-valuemax="100"
          aria-valuenow="${pct}"
          aria-valuetext="${valueText}"
        >
          <div class="quota-bar-fill" style="width: ${pct}%"></div>
        </div>
      </div>`
    : html`<div class="quota-pool-muted"><small>${poolMissingNote(p)}</small></div>`;

  return html`<div class="${statusClass}">
    <div class="quota-pool-heading">
      <span class="quota-pool-title">${title}</span>
      ${badge}
    </div>
    ${body}
    ${err}
    ${renderPoolModels(p)}
  </div>`;
}

/** Explains why a pool shows no bar, without inventing a 0 figure. */
function poolMissingNote(p: QuotaPool): string {
  if (p.status === "absent") {
    // "No active Coding Plan" is paid-only wording; a missing free Starter
    // grant must not be described as a missing subscription.
    return p.source === "zcode_starter" ? "No Starter grant" : "No active Coding Plan";
  }
  if (p.fetch_error) return "usage data unavailable for this source";
  // Surface a future activation window even though there are no figures yet.
  if (p.status === "unavailable" && p.starts_at) {
    const d = parsePoolDate(p.starts_at);
    if (d && d.getTime() > Date.now()) return `no usage data yet · starts ${absoluteDate(d)}`;
  }
  return "no usage data reported";
}

/** Status pill for a pool (active / exhausted / expired / absent / unavailable). */
function poolStatusBadge(p: QuotaPool): TemplateResult | null {
  switch (p.status) {
    case "absent":
      // "No active plan" wording is for the paid pool; Starter absence reads
      // differently so the user knows which source is missing.
      return p.source === "zcode_starter"
        ? html`<span class="status-pill warn" title="No Starter free grant on this account">No Starter</span>`
        : html`<span class="status-pill warn" title="No active Coding Plan">No active plan</span>`;
    case "exhausted":
      return html`<span class="status-pill err" title="This quota is used up">Exhausted</span>`;
    case "expired":
      return html`<span class="status-pill lost" title="This quota has expired">Expired</span>`;
    case "unavailable":
      return html`<span class="status-pill warn" title="Usage figures unavailable for this pool">Unavailable</span>`;
    case "active":
    default:
      return null;
  }
}

/**
 * Render all Z.ai quota pools: Starter first, paid plan below.
 *
 * A pool that failed to fetch still occupies its slot so the ordering is
 * stable and the user can see which source is broken; only the healthy pools
 * render figures.
 */
export function renderQuotaPools(pools: QuotaPool[] | null | undefined): TemplateResult | null {
  if (!pools || pools.length === 0) return null;

  const starter = pools.filter((p) => p.source === "zcode_starter");
  const paid = pools.filter((p) => p.source === "coding_plan");
  const others = pools.filter((p) => p.source !== "zcode_starter" && p.source !== "coding_plan");

  const ordered: QuotaPool[] = [
    // Starter first, paid below, then any future/unknown source defensively.
    ...sortPools(starter),
    ...sortPools(paid),
    ...others,
  ];

  if (ordered.length === 0) return null;

  return html`<div class="quota-pools">
    ${ordered.map((p) => renderPool(p))}
  </div>`;
}

/**
 * Stable order inside a source group: exhausted/absent last, then by plan
 * name so multiple paid buckets (e.g. Pro Lite and Pro) keep a fixed order.
 */
function sortPools(list: QuotaPool[]): QuotaPool[] {
  const rank = (p: QuotaPool): number => {
    switch (p.status) {
      case "active": return 0;
      case "expired": return 1;
      case "unavailable": return 2;
      case "exhausted": return 3;
      case "absent": return 4;
      default: return 5;
    }
  };
  return [...list].sort((a, b) => {
    const r = rank(a) - rank(b);
    if (r !== 0) return r;
    const an = (a.plan_name ?? "").toLowerCase();
    const bn = (b.plan_name ?? "").toLowerCase();
    return an < bn ? -1 : an > bn ? 1 : 0;
  });
}

/** Convenience for tests and callers that need the ordered pool list. */
export function orderedPools(pools: QuotaPool[] | null | undefined): QuotaPool[] {
  if (!pools || pools.length === 0) return [];
  const starter = pools.filter((p) => p.source === "zcode_starter");
  const paid = pools.filter((p) => p.source === "coding_plan");
  const others = pools.filter((p) => p.source !== "zcode_starter" && p.source !== "coding_plan");
  return [...sortPools(starter), ...sortPools(paid), ...others];
}

/** True when a pool can show a numeric bar (has figures or is exhausted). */
export function poolHasNumeric(p: QuotaPool): boolean {
  return finiteNumber(p.remaining) != null || finiteNumber(p.used) != null || p.status === "exhausted";
}

export type { QuotaPool, QuotaPoolSource, ModelQuotaDetail };
