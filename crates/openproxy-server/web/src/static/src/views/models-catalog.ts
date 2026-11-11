/**
 * Models catalog view — the client-facing table of every model the caller's
 * API key can see (same filtering as `GET /v1/models`), enriched with
 * per-1M pricing from the billing lookup. Sortable columns, free-text filter
 * and three density presets (normal / compact / ultra). Full mobile support:
 * under 760px the table reflows into labelled cards.
 */
import { api } from '../state/api.js';
import { escapeHtml } from '../lib/escape.js';

interface CatalogEntry {
  id: string;
  display_name?: string | null;
  owned_by?: string | null;
  type?: string | null;
  family?: string | null;
  context_length?: number | null;
  max_output_tokens?: number | null;
  capabilities?: Record<string, boolean> | null;
  pricing_input_per_1m?: number | null;
  pricing_output_per_1m?: number | null;
  pricing_cached_per_1m?: number | null;
}

interface CatalogResponse {
  count: number;
  data: CatalogEntry[];
}

type SortKey =
  | 'id'
  | 'provider'
  | 'type'
  | 'context'
  | 'max_out'
  | 'price_in'
  | 'price_cached'
  | 'price_out';

type Density = 'normal' | 'compact' | 'ultra';

const LS_KEY = 'openproxy.modelsCatalogDensity';

const COLUMNS: readonly { key: SortKey | null; label: string; dataL: string; num?: boolean }[] = [
  { key: 'id', label: 'Model', dataL: 'Model' },
  { key: 'provider', label: 'Provider', dataL: 'Provider' },
  { key: 'type', label: 'Type', dataL: 'Type' },
  { key: 'context', label: 'Context', dataL: 'Context', num: true },
  { key: 'max_out', label: 'Max out', dataL: 'Max out', num: true },
  { key: 'price_in', label: '$ in /M', dataL: '$ in /M', num: true },
  { key: 'price_cached', label: '$ cached /M', dataL: '$ cached /M', num: true },
  { key: 'price_out', label: '$ out /M', dataL: '$ out /M', num: true },
  { key: null, label: 'Capabilities', dataL: 'Capabilities' },
];

const CAP_BADGES: readonly [string, string][] = [
  ['tools', 'tools'],
  ['vision', 'vision'],
  ['reasoning', 'reason'],
  ['thinking', 'think'],
  ['attachment', 'files'],
  ['structured_output', 'json'],
  ['audio_input', 'audio-in'],
  ['audio_output', 'audio-out'],
];

let entries: CatalogEntry[] = [];
let loading = true;
let errorMsg: string | null = null;
let query = '';
let sortKey: SortKey = 'id';
let sortDir: 1 | -1 = 1;
let density: Density = 'normal';
let mounted = false;

function loadDensity(): Density {
  try {
    const v = localStorage.getItem(LS_KEY);
    if (v === 'compact' || v === 'ultra' || v === 'normal') return v;
  } catch {
    /* private mode */
  }
  return 'normal';
}

function saveDensity(d: Density): void {
  try {
    localStorage.setItem(LS_KEY, d);
  } catch {
    /* private mode */
  }
}

function fmtTokens(n?: number | null): string {
  if (n == null || n <= 0) return '—';
  if (n >= 1_000_000) {
    const v = n / 1_000_000;
    return `${v >= 10 ? Math.round(v) : Math.round(v * 10) / 10}M`;
  }
  if (n >= 1_000) return `${Math.round(n / 1_000)}k`;
  return String(n);
}

function fmtPrice(n?: number | null): string {
  if (n == null) return '—';
  if (n === 0) return 'free';
  if (n < 0.01) return `$${n.toFixed(4)}`;
  if (n < 1) return `$${n.toFixed(3)}`;
  return `$${n.toFixed(2)}`;
}

function providerOf(e: CatalogEntry): string {
  if (e.owned_by) return e.owned_by;
  const slash = e.id.indexOf('/');
  return slash > 0 ? e.id.slice(0, slash) : '';
}

function sortValue(e: CatalogEntry, key: SortKey): string | number {
  switch (key) {
    case 'id':
      return e.id.toLowerCase();
    case 'provider':
      return providerOf(e).toLowerCase();
    case 'type':
      return (e.type || '').toLowerCase();
    case 'context':
      return e.context_length ?? -1;
    case 'max_out':
      return e.max_output_tokens ?? -1;
    case 'price_in':
      return e.pricing_input_per_1m ?? -1;
    case 'price_cached':
      return e.pricing_cached_per_1m ?? -1;
    case 'price_out':
      return e.pricing_output_per_1m ?? -1;
  }
}

function compareRows(a: CatalogEntry, b: CatalogEntry): number {
  const av = sortValue(a, sortKey);
  const bv = sortValue(b, sortKey);
  let cmp: number;
  if (typeof av === 'number' && typeof bv === 'number') cmp = av - bv;
  else cmp = String(av).localeCompare(String(bv));
  if (cmp === 0) cmp = a.id.localeCompare(b.id);
  return cmp * sortDir;
}

function visibleEntries(): CatalogEntry[] {
  const q = query.trim().toLowerCase();
  const rows = q
    ? entries.filter(
        (e) =>
          e.id.toLowerCase().includes(q) ||
          (e.display_name || '').toLowerCase().includes(q) ||
          (e.family || '').toLowerCase().includes(q) ||
          providerOf(e).toLowerCase().includes(q),
      )
    : entries.slice();
  rows.sort(compareRows);
  return rows;
}

function badgesHtml(e: CatalogEntry): string {
  const caps = e.capabilities || {};
  const parts: string[] = [];
  for (const [flag, label] of CAP_BADGES) {
    if (caps[flag]) parts.push(`<span class="mc-badge">${label}</span>`);
  }
  return parts.length > 0 ? parts.join('') : '<span class="mc-muted">—</span>';
}

function rowHtml(e: CatalogEntry): string {
  const isCombo = (e.type || '').toLowerCase() === 'combo' || e.id.startsWith('combo:');
  const sub = e.display_name && e.display_name !== e.id
    ? `<span class="mc-sub">${escapeHtml(e.display_name)}</span>`
    : '';
  const typeLabel = escapeHtml(e.type || 'chat');
  return `<tr class="mc-row${isCombo ? ' is-combo' : ''}">
    <td data-l="Model" class="mc-model"><span class="mc-id">${escapeHtml(e.id)}</span>${sub}</td>
    <td data-l="Provider">${escapeHtml(providerOf(e))}</td>
    <td data-l="Type"><span class="mc-type${isCombo ? ' is-combo' : ''}">${typeLabel}</span></td>
    <td data-l="Context" class="num">${fmtTokens(e.context_length)}</td>
    <td data-l="Max out" class="num">${fmtTokens(e.max_output_tokens)}</td>
    <td data-l="$ in /M" class="num">${fmtPrice(e.pricing_input_per_1m)}</td>
    <td data-l="$ cached /M" class="num">${fmtPrice(e.pricing_cached_per_1m)}</td>
    <td data-l="$ out /M" class="num">${fmtPrice(e.pricing_output_per_1m)}</td>
    <td data-l="Capabilities"><div class="mc-badges">${badgesHtml(e)}</div></td>
  </tr>`;
}

function tableHtml(): string {
  const rows = visibleEntries();
  const head = COLUMNS.map((c) => {
    if (!c.key) {
      return `<th class="${c.num ? 'num' : ''}">${c.label}</th>`;
    }
    const active = sortKey === c.key;
    const arrow = active ? (sortDir === 1 ? '▲' : '▼') : '';
    const aria = active ? (sortDir === 1 ? 'ascending' : 'descending') : 'none';
    return `<th class="sortable${c.num ? ' num' : ''}${active ? ' active' : ''}" data-key="${c.key}" aria-sort="${aria}">
      ${c.label}<span class="mc-arrow">${arrow}</span>
    </th>`;
  }).join('');
  const body = rows.length > 0
    ? rows.map(rowHtml).join('')
    : `<tr><td class="mc-empty" colspan="${COLUMNS.length}">No models match “${escapeHtml(query)}”.</td></tr>`;
  return `<div class="mc-tablewrap"><table class="mc-table"><thead><tr>${head}</tr></thead><tbody>${body}</tbody></table></div>`;
}

function renderBody(): void {
  if (!mounted) return;
  const body = document.getElementById('mc-body');
  const countEl = document.getElementById('mc-count');
  if (countEl) {
    const shown = visibleEntries().length;
    countEl.textContent = loading
      ? ''
      : query.trim()
        ? `${shown} of ${entries.length} models`
        : `${entries.length} models`;
  }
  if (!body) return;
  if (loading) {
    body.innerHTML = '<div class="mc-state">Loading models…</div>';
    return;
  }
  if (errorMsg) {
    body.innerHTML = `<div class="mc-error">Could not load the model catalog: ${escapeHtml(errorMsg)}</div>`;
    return;
  }
  body.innerHTML = tableHtml();
}

function applyDensity(): void {
  const card = document.querySelector('.mc-card');
  if (!card) return;
  card.classList.remove('density-normal', 'density-compact', 'density-ultra');
  card.classList.add(`density-${density}`);
  document.querySelectorAll<HTMLButtonElement>('.mc-density button').forEach((b) => {
    b.classList.toggle('active', b.dataset['density'] === density);
  });
}

async function refresh(): Promise<void> {
  loading = true;
  errorMsg = null;
  renderBody();
  try {
    const resp = (await api('/models/catalog')) as CatalogResponse;
    entries = Array.isArray(resp?.data) ? resp.data : [];
  } catch (err) {
    entries = [];
    errorMsg = err instanceof Error ? err.message : String(err);
  } finally {
    loading = false;
    renderBody();
  }
}

export async function mountModelsCatalog(container?: HTMLElement | string): Promise<(() => void) | undefined> {
  const main = typeof container === "object" && container !== null
    ? container
    : document.getElementById('main');
  if (!main) return;
  mounted = true;
  entries = [];
  loading = true;
  errorMsg = null;
  query = '';
  sortKey = 'id';
  sortDir = 1;
  density = loadDensity();

  main.innerHTML = `
    <div class="card mc-card density-${density}">
      <div class="mc-toolbar">
        <div class="mc-heading">
          <h1 class="mc-title">Models</h1>
          <span class="mc-count" id="mc-count"></span>
        </div>
        <div class="mc-tools">
          <input id="mc-search" class="mc-search" type="search" autocomplete="off"
                 placeholder="Filter by model, provider or family…" aria-label="Filter models" />
          <div class="mc-density" role="group" aria-label="Density">
            <button type="button" data-density="normal" title="Normal density">Normal</button>
            <button type="button" data-density="compact" title="Compact density">Compact</button>
            <button type="button" data-density="ultra" title="Ultra-compact density">Ultra</button>
          </div>
        </div>
      </div>
      <div id="mc-body" class="mc-body"></div>
    </div>`;

  const search = document.getElementById('mc-search') as HTMLInputElement | null;
  search?.addEventListener('input', () => {
    query = search.value;
    renderBody();
  });

  document.querySelectorAll<HTMLButtonElement>('.mc-density button').forEach((b) => {
    b.addEventListener('click', () => {
      const d = b.dataset['density'] as Density;
      if (d !== 'normal' && d !== 'compact' && d !== 'ultra') return;
      density = d;
      saveDensity(d);
      applyDensity();
    });
  });

  const bodyHost = document.getElementById('mc-body');
  bodyHost?.addEventListener('click', (ev: Event) => {
    const th = (ev.target as HTMLElement).closest('th[data-key]') as HTMLElement | null;
    if (!th) return;
    const key = th.dataset['key'] as SortKey;
    if (sortKey === key) sortDir = sortDir === 1 ? -1 : 1;
    else {
      sortKey = key;
      sortDir = 1;
    }
    renderBody();
  });

  applyDensity();
  await refresh();

  return () => {
    mounted = false;
  };
}
