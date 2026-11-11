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
export type ProviderDetailTab = 'accounts' | 'models' | 'config';

export interface ProviderDetailUiState {
  tab?: ProviderDetailTab;
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
    tab: raw?.tab === 'models' || raw?.tab === 'config' ? raw.tab : 'accounts',
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

export function getProviderCountryCode(p: Provider): string {
  const host = extractDomain(p.base_url);
  if (!host) return "—";
  const parts = host.toLowerCase().split(".");
  const tld = parts[parts.length - 1];
  if (tld && tld.length === 2 && /^[a-z]{2}$/.test(tld)) {
    // Standard ccTLDs (uk, de, fr, jp, cn, us...)
    if (tld === "uk") return "GB";
    return tld.toUpperCase();
  }
  // Generic / commercial TLDs mostly based in the US
  if (tld === "com" || tld === "ai" || tld === "io" || tld === "org" || tld === "net" || tld === "app" || tld === "dev") {
    return "US";
  }
  return tld ? tld.toUpperCase() : "—";
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
export function renderProviderIcon(p: Provider, size?: 'small' | 'large'): TemplateResult {
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

  const imgCls = size === 'small' ? 'provider-favicon provider-favicon-sm' : 'provider-favicon';

  return html`
    <img src=${src ?? ''} alt=${p.name} class=${imgCls} @error=${onImgError} loading="lazy" ${ref(onImgMounted)} />
    <span style="display: none;">${fallback}</span>
  `;
}

/**
 * Capability icons: shape + color + native title tooltip. Replaces the text
 * chips that ate the row width and clipped in narrow tiles (user comment on
 * the antigravity detail list). Paths are static lucide-style strokes.
 */
type CapIcon = { d: string; circles?: [number, number, number][]; label: string; cls: string };
const CAP_ICONS: Record<string, CapIcon> = {
  unary:     { d: "M7.5 7.5h9v9h-9z", label: "Unary (non-streaming)", cls: "cap-ic-unary" },
  stream:    { d: "M22 12h-4l-3 9L9 3l-3 9H2", label: "Streaming (SSE)", cls: "cap-ic-stream" },
  vision:    { d: "M2 12s3.5-6 10-6 10 6 10 6-3.5 6-10 6-10-6-10-6z", circles: [[12, 12, 2.5]], label: "Vision (images)", cls: "cap-ic-vision" },
  tools:     { d: "M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z", label: "Tool calling", cls: "cap-ic-tools" },
  reasoning: { d: "M9 18h6M10 22h4M15.09 14c.18-.98.65-1.74 1.41-2.5A4.65 4.65 0 0 0 18 8 6 6 0 0 0 6 8c0 1 .23 2.23 1.5 3A6.64 6.64 0 0 1 8.91 14", label: "Reasoning", cls: "cap-ic-reasoning" },
  thinking:  { d: "M12 3l1.8 5.2L19 10l-5.2 1.8L12 17l-1.8-5.2L5 10l5.2-1.8zM19 15l.9 2.1L22 18l-2.1.9L19 21l-.9-2.1L16 18l2.1-.9z", label: "Thinking (extended)", cls: "cap-ic-thinking" },
  json:      { d: "M16 18l6-6-6-6M8 6l-6 6 6 6", label: "Structured output (JSON)", cls: "cap-ic-json" },
  attach:    { d: "m21.44 11.05-9.19 9.19a6 6 0 0 1-8.49-8.49l8.57-8.57A4 4 0 1 1 18 8.84l-8.59 8.57a2 2 0 0 1-2.83-2.83l8.49-8.48", label: "Attachments", cls: "cap-ic-attach" },
  decisions: { d: "M6 3v9M18 9a9 9 0 0 1-9 9", circles: [[18, 6, 3], [6, 18, 3]], label: "Decisions", cls: "cap-ic-decisions" },
};

function capIcon(cap: CapIcon): TemplateResult {
  return html`<span class="cap-ic ${cap.cls}" title=${cap.label} aria-label=${cap.label}>
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d=${cap.d}/>
      ${cap.circles?.map(([cx, cy, r]) => html`<circle cx=${cx} cy=${cy} r=${r}/>`)}
    </svg>
  </span>`;
}

export function renderCapabilityBadges(
  json: string | null | undefined,
  modelType?: string | null,
): TemplateResult {
  const icons: TemplateResult[] = [];
  if (modelType && modelType !== 'chat') {
    icons.push(html`<span class="cap-badge">${modelType}</span>`);
  }
  if (json != null) {
    let caps: unknown;
    if (typeof json === 'string') {
      try {
        caps = JSON.parse(json) as unknown;
      } catch {
        // fall through with caps = undefined; handled below.
      }
    } else {
      caps = json;
    }
    if (caps && typeof caps === 'object') {
      const c = caps as Record<string, unknown>;
      if (c['streaming'] === false) icons.push(capIcon(CAP_ICONS['unary']!));
      if (c['streaming'] === true) icons.push(capIcon(CAP_ICONS['stream']!));
      if (c['vision']) icons.push(capIcon(CAP_ICONS['vision']!));
      if (c['tool_calling']) icons.push(capIcon(CAP_ICONS['tools']!));
      if (c['reasoning']) icons.push(capIcon(CAP_ICONS['reasoning']!));
      if (c['thinking']) icons.push(capIcon(CAP_ICONS['thinking']!));
      if (c['structured_output']) icons.push(capIcon(CAP_ICONS['json']!));
      if (c['attachment']) icons.push(capIcon(CAP_ICONS['attach']!));
      if (c['decisions']) icons.push(capIcon(CAP_ICONS['decisions']!));
    }
  }
  return icons.length > 0 ? html`<span class="caps-row">${icons}</span>` : html`<span class="muted">—</span>`;
}
