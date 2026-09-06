// views/providers/shared.ts — shared types, module-local state, and
// render helpers used by BOTH the provider grid (list.ts) and the
// provider detail (detail.ts).
//
// Public surface kept narrow on purpose — anything that only one of
// the siblings uses lives next to that sibling, not here. The goal is
// "shared" in the literal sense (both consumers reference it), not
// "kitchen sink".

import { html, type TemplateResult } from 'lit-html';
import type { ModelSort } from '../../components/model-table.js';
import type { Provider } from '../../lib/types/api.js';

// ---- Types shared by list + detail ----

/** Per-detail-view UI state. Stored on `state.providerDetail[providerId]`
 *  so the user's filter / search / sort / page choice survives
 *  navigation away and back. The grid view does not consume this
 *  type, but the type still lives in shared.ts because detail.ts and
 *  index.ts both reach into it (index.ts seeds the defaults on first
 *  mount, detail.ts reads/writes it on every filter click). */
export interface ProviderDetailUiState {
  filter: 'all' | 'active' | 'inactive';
  search: string;
  sort: ModelSort | null;
  page: number;
  pageSize: number;
  // The original interface allowed an open index signature; preserved
  // here for source-compatibility with any caller that reached into
  // arbitrary keys (none in this codebase, but the spec said "no
  // behavior changes").
  [key: string]: unknown;
}

// ---- Module-local state shared by index + detail ----
//
// `detailProviderId` is the active detail context (set by mountProviders
// when `detailId` is provided). `loadError` is set by the fetch path in
// index.ts and read by both list.ts and detail.ts to render an error
// banner. Keeping them here lets index.ts stay a thin orchestrator
// and avoids duplicating the assignments in both view files.

export let detailProviderId: string | null = null;
export function setDetailProviderId(id: string | null): void {
  detailProviderId = id;
}

export let loadError: string | null = null;
export function setLoadError(err: string | null): void {
  loadError = err;
}

// ---- Render helpers used by both list and detail ----

/** Extract the hostname (with no scheme, no path) from a base_url.
 *  Tolerant of inputs missing the protocol — providers in the DB
 *  sometimes store bare hosts. Returns null on total garbage. */
export function extractDomain(urlStr: string | null | undefined): string | null {
  if (!urlStr || typeof urlStr !== 'string') return null;
  try {
    const u = new URL(urlStr.startsWith('http') ? urlStr : `https://${urlStr}`);
    return u.hostname;
  } catch {
    const stripped = urlStr
      .trim()
      .replace(/^https?:\/\//i, '')
      .split('/')[0]
      ?.split(':')[0]
      ?.trim();
    return stripped || null;
  }
}

/** Reduce a hostname to its apex (registered) domain. Naive but
 *  good-enough heuristic for favicon lookups — handles common
 *  compound TLDs (`.co.uk`, `.com.au`, ...) so we don't accidentally
 *  hit a public-suffix redirect. */
export function extractApexDomain(host: string): string {
  const parts = host.split('.');
  if (parts.length <= 2) return host;
  const secondToLast = parts[parts.length - 2] || '';
  const last = parts[parts.length - 1] || '';
  const isCompoundTld =
    ['co', 'com', 'org', 'net', 'gov', 'edu'].includes(secondToLast) && last.length === 2;
  if (isCompoundTld && parts.length >= 3) {
    return parts.slice(-3).join('.');
  }
  return parts.slice(-2).join('.');
}

/** Render the provider favicon with a graceful fallback chain:
 *  1. provider's own `favicon_base64` (if set),
 *  2. Google's `s2/favicons` service keyed on the host,
 *  3. retry against the apex domain (handles `cdn.provider.com`),
 *  4. retry against DuckDuckGo's IP3 icon service,
 *  5. finally hide the <img> and show the letter fallback.
 *
 *  Used by both `renderProviderCard` (grid) and `renderDetailHeader`
 *  (detail) — that's why it lives in shared.ts. */
export function renderProviderIcon(p: Provider): TemplateResult {
  const fallback = (p.id[0] || '?').toUpperCase();
  const host = extractDomain(p.base_url);
  const apex = host ? extractApexDomain(host) : null;
  const src =
    p.favicon_base64 ||
    (host ? `https://www.google.com/s2/favicons?domain=${encodeURIComponent(host)}&sz=64` : null);

  if (!src) {
    return html`<span>${fallback}</span>`;
  }

  let attempt = 0;
  const onImgError = (e: Event): void => {
    const img = e.currentTarget as HTMLImageElement;
    attempt++;
    if (attempt === 1 && apex && apex !== host) {
      img.src = `https://www.google.com/s2/favicons?domain=${encodeURIComponent(apex)}&sz=64`;
      return;
    }
    if (attempt <= 2 && apex) {
      img.src = `https://icons.duckduckgo.com/ip3/${encodeURIComponent(apex)}.ico`;
      return;
    }
    img.style.display = 'none';
    if (img.nextElementSibling) {
      (img.nextElementSibling as HTMLElement).style.display = '';
    }
  };

  return html`
    <img src=${src} alt=${p.name} class="provider-favicon" @error=${onImgError} loading="lazy" />
    <span style="display: none;">${fallback}</span>
  `;
}