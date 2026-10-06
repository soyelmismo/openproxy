// Shared types, module-local state, and render helpers used by both the
// provider grid (list.ts) and the provider detail (detail.ts). Anything only
// one sibling uses lives next to that sibling.

import { html, type TemplateResult } from 'lit-html';
import { ref } from 'lit-html/directives/ref.js';
import { state } from '../../state/index.js';
import { getToken } from '../../state/auth.js';
import type { ModelSort } from '../../components/model-table.js';
import type { Provider } from '../../lib/types/api.js';


/** Per-detail-view UI state, stored on `state.providerDetail[providerId]`
 *  so the filter / search / sort / page choice survives navigation.
 *  index.ts seeds the defaults, detail.ts reads and writes them. */
export interface ProviderDetailUiState {
  filter: 'all' | 'active' | 'inactive';
  search: string;
  sort: ModelSort | null;
  page: number;
  pageSize: number;
  // Index signature kept for source-compatibility with callers reaching
  // into arbitrary keys (none in this codebase).
  [key: string]: unknown;
}

//
// The models section reads/writes these on every filter/sort/page
// interaction, so they live next to the `ProviderDetailUiState` type.

export function getProviderUi(providerId: string): ProviderDetailUiState {
  const raw = state.providerDetail[providerId] as
    | Partial<ProviderDetailUiState>
    | undefined;
  return {
    filter: raw?.filter ?? 'all',
    search: raw?.search ?? '',
    sort: raw?.sort ?? null,
    page: raw?.page ?? 1,
    pageSize: raw?.pageSize ?? 50,
  };
}

export function setProviderUi(
  providerId: string,
  ui: ProviderDetailUiState,
): void {
  state.providerDetail[providerId] = ui;
}

//
// `detailProviderId` is the active detail context (set by mountProviders
// when `detailId` is provided). `loadError` is set by the fetch path in
// index.ts and read by list.ts and detail.ts to render an error banner.

export let detailProviderId: string | null = null;
export function setDetailProviderId(id: string | null): void {
  detailProviderId = id;
}

export let loadError: string | null = null;
export function setLoadError(err: string | null): void {
  loadError = err;
}


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

//
// `GET /admin/api/providers/:id/icon` sits behind the admin auth
// middleware like every other `/admin/api/*` route. A bare `<img src>`
// cannot carry the Bearer header, so we fetch the blob ourselves and hand
// the `<img>` an object URL, cached per provider id for the page lifetime.

const iconObjectUrls = new Map<string, string>();
const iconInflight = new Map<string, Promise<string | null>>();

/** Fetch the persisted favicon with the admin Bearer token and return a
 *  `blob:` URL, or null if the server has none / the request failed. */
export function loadProviderIconUrl(providerId: string): Promise<string | null> {
  const cached = iconObjectUrls.get(providerId);
  if (cached) return Promise.resolve(cached);
  const inflight = iconInflight.get(providerId);
  if (inflight) return inflight;
  const token = getToken();
  const headers: HeadersInit = token ? { Authorization: `Bearer ${token}` } : {};
  const promise: Promise<string | null> = fetch(
    `/admin/api/providers/${encodeURIComponent(providerId)}/icon`,
    { headers },
  )
    .then(async (r: Response) => {
      if (!r.ok) return null;
      const url = URL.createObjectURL(await r.blob());
      iconObjectUrls.set(providerId, url);
      return url;
    })
    .catch(() => null)
    .finally(() => {
      iconInflight.delete(providerId);
    });
  iconInflight.set(providerId, promise);
  return promise;
}

/** Drop a cached icon (e.g. after the provider's favicon was refreshed). */
export function invalidateProviderIcon(providerId: string): void {
  const url = iconObjectUrls.get(providerId);
  if (url) {
    URL.revokeObjectURL(url);
    iconObjectUrls.delete(providerId);
  }
}

/** Render the provider favicon with a graceful fallback chain:
 *  1. persisted favicon via the authenticated `/admin/api/providers/:id/icon`
 *     endpoint (if `has_favicon`), served to the `<img>` as a `blob:` URL,
 *  2. legacy `favicon_base64`,
 *  3. Google's `s2/favicons` service keyed on the host,
 *  4. retry against the apex domain (handles `cdn.provider.com`),
 *  5. retry against DuckDuckGo's IP3 icon service,
 *  6. finally hide the <img> and show the letter fallback.
 *
 *  Used by both `renderProviderCard` (grid) and `renderDetailHeader`
 *  (detail). */
export function renderProviderIcon(p: Provider): TemplateResult {
  const fallback = (p.id[0] || '?').toUpperCase();
  const host = extractDomain(p.base_url);
  const apex = host ? extractApexDomain(host) : null;
  const externalSrc: string | null =
    p.favicon_base64 ||
    (host ? `https://www.google.com/s2/favicons?domain=${encodeURIComponent(host)}&sz=64` : null);
  const cachedIcon: string | undefined = p.has_favicon ? iconObjectUrls.get(p.id) : undefined;
  const src: string | null = cachedIcon ?? externalSrc;

  if (!src && !p.has_favicon) {
    return html`<span>${fallback}</span>`;
  }

  // Until the authenticated fetch lands, the external fallback (if any)
  // is shown so the card never renders an empty box.
  const onImgMounted = (el: Element | undefined): void => {
    if (!(el instanceof HTMLImageElement) || !p.has_favicon || cachedIcon) return;
    void loadProviderIconUrl(p.id).then((url: string | null) => {
      if (!url || !el.isConnected) return;
      el.src = url;
      // Undo a fallback-chain "hide" that may have run while we waited.
      el.style.display = '';
      if (el.nextElementSibling) (el.nextElementSibling as HTMLElement).style.display = 'none';
    });
  };

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
    <img src=${src ?? ''} alt=${p.name} class="provider-favicon" @error=${onImgError} loading="lazy" ${ref(onImgMounted)} />
    <span style="display: none;">${fallback}</span>
  `;
}

//
// Renders the vision/tools/reasoning/… badges parsed from a model's
// `capabilities_json` (raw JSON string or pre-parsed object; bad input
// renders as an em-dash instead of throwing). Unlike the simpler variant in
// components/model-table.ts, this one also surfaces the non-chat
// `modelType` badge.

export function renderCapabilityBadges(
  json: string | null | undefined,
  modelType?: string | null,
): TemplateResult {
  const badges: TemplateResult[] = [];
  if (modelType && modelType !== 'chat') {
    badges.push(html`<span class="cap-badge">${modelType}</span>`);
  }
  if (json != null) {
    let caps: unknown;
    if (typeof json === 'string') {
      try {
        caps = JSON.parse(json) as unknown;
      } catch {
        // fall through with caps = undefined; renderCapabilityBadges handles
        // both null and undefined below.
      }
    } else {
      caps = json;
    }
    if (caps && typeof caps === 'object') {
      const c = caps as Record<string, unknown>;
      if (c['streaming'] === false) badges.push(html`<span class="cap-badge warn" title="Forced unary (non-streaming)">unary</span>`);
      if (c['streaming'] === true) badges.push(html`<span class="cap-badge" title="Forced streaming (SSE)">stream</span>`);
      if (c['vision']) badges.push(html`<span class="cap-badge">vision</span>`);
      if (c['tool_calling']) badges.push(html`<span class="cap-badge">tools</span>`);
      if (c['reasoning']) badges.push(html`<span class="cap-badge">reasoning</span>`);
      if (c['thinking']) badges.push(html`<span class="cap-badge">thinking</span>`);
      if (c['structured_output']) badges.push(html`<span class="cap-badge">json</span>`);
      if (c['attachment']) badges.push(html`<span class="cap-badge">attach</span>`);
      if (c['decisions']) badges.push(html`<span class="cap-badge">decisions</span>`);
    }
  }
  return badges.length > 0 ? html`${badges}` : html`<span class="muted">—</span>`;
}