/**
 * Models catalog view — the client-facing table of every model the caller's
 * API key can see (same filtering as `GET /v1/models`), enriched with
 * per-1M pricing from the billing lookup. Sortable columns, free-text filter
 * and three density presets (normal / compact / ultra). Full mobile support:
 * under 760px the table reflows into labelled cards.
 *
 * Peak/off-peak schedules: when the catalog entry carries a
 * `pricing_schedule`, the price cells show the band active right now and the
 * Band column counts down to the next real transition. A single one-second
 * timer updates those cells in place (one timer for the whole table, no
 * per-row fetch, no full re-render every second); the table is only rebuilt
 * when a band actually flips while sorting by a price/band column. A present
 * but malformed schedule renders as "unavailable" — never as the flat price.
 */
import { api } from '../state/api.js';
import { escapeHtml } from '../lib/escape.js';
import { createVisibilityAwareInterval, type VisibilityAwareHandle } from '../lib/visibility-aware-interval.js';
import {
  parsePricingSchedule,
  resolvePricing,
  type PricingBand,
  type PricingSchedule,
} from '../lib/pricing-schedule.js';

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
  /** Present when the entry has a peak/off-peak schedule. */
  pricing_schedule?: unknown;
  /** Server epoch seconds the pricing_* fields were computed at. */
  pricing_as_of?: number | null;
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
  | 'price_out'
  | 'band';

type Density = 'normal' | 'compact' | 'ultra';

interface RowPricing {
  band: PricingBand | null;
  input: number | null;
  output: number | null;
  cached: number | null;
  /** Absolute epoch ms in the schedule's clock, null when no transition. */
  nextTransitionMs: number | null;
  nextBand: PricingBand | null;
  /** Schedule present but malformed: show unavailable, not the flat price. */
  unavailable: boolean;
  /** Clock used to compute nextTransitionMs (server-corrected). */
  nowMs: number;
}

interface Row {
  entry: CatalogEntry;
  idx: number;
  pricing: RowPricing;
}

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
  { key: 'band', label: 'Band', dataL: 'Band' },
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
let schedules: (PricingSchedule | null)[] = [];
let clockOffsets: number[] = [];
let unavailable: boolean[] = [];
let lastBands: (PricingBand | null)[] = [];
let ticker: VisibilityAwareHandle | null = null;
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

function fmtCountdown(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const days = Math.floor(total / 86_400);
  const hours = Math.floor((total % 86_400) / 3_600);
  const minutes = Math.floor((total % 3_600) / 60);
  const seconds = total % 60;
  const pad = (n: number): string => String(n).padStart(2, '0');
  if (days > 0) return `${days}d ${pad(hours)}:${pad(minutes)}`;
  return `${pad(hours)}:${pad(minutes)}:${pad(seconds)}`;
}

function providerOf(e: CatalogEntry): string {
  if (e.owned_by) return e.owned_by;
  const slash = e.id.indexOf('/');
  return slash > 0 ? e.id.slice(0, slash) : '';
}

function isPriceSort(key: SortKey): boolean {
  return key === 'price_in' || key === 'price_cached' || key === 'price_out';
}

/** Pricing for one entry at `nowMs`, honouring its schedule when present. */
function pricingFor(entry: CatalogEntry, idx: number, nowMs: number): RowPricing {
  const schedule = schedules[idx] ?? null;
  if (schedule) {
    const clock = nowMs + (clockOffsets[idx] ?? 0);
    const resolved = resolvePricing(schedule, clock);
    return {
      band: resolved.band,
      input: resolved.rates.input_per_1m,
      output: resolved.rates.output_per_1m,
      cached: resolved.rates.cached_per_1m,
      nextTransitionMs: resolved.nextTransitionMs,
      nextBand: resolved.nextBand,
      unavailable: false,
      nowMs: clock,
    };
  }
  return {
    band: null,
    input: entry.pricing_input_per_1m ?? null,
    output: entry.pricing_output_per_1m ?? null,
    cached: entry.pricing_cached_per_1m ?? null,
    nextTransitionMs: null,
    nextBand: null,
    unavailable: unavailable[idx] === true,
    nowMs,
  };
}

function sortValue(row: Row, key: SortKey): string | number {
  const e = row.entry;
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
      return row.pricing.input ?? -1;
    case 'price_cached':
      return row.pricing.cached ?? -1;
    case 'price_out':
      return row.pricing.output ?? -1;
    case 'band':
      return row.pricing.unavailable ? 'unavailable' : (row.pricing.band ?? '');
  }
}

function compareRows(a: Row, b: Row): number {
  const av = sortValue(a, sortKey);
  const bv = sortValue(b, sortKey);
  let cmp: number;
  if (typeof av === 'number' && typeof bv === 'number') cmp = av - bv;
  else cmp = String(av).localeCompare(String(bv));
  if (cmp === 0) cmp = a.entry.id.localeCompare(b.entry.id);
  return cmp * sortDir;
}

function visibleRows(): Row[] {
  const q = query.trim().toLowerCase();
  const now = Date.now();
  const rows: Row[] = [];
  entries.forEach((entry, idx) => {
    const matches =
      q === '' ||
      entry.id.toLowerCase().includes(q) ||
      (entry.display_name || '').toLowerCase().includes(q) ||
      (entry.family || '').toLowerCase().includes(q) ||
      providerOf(entry).toLowerCase().includes(q);
    if (matches) rows.push({ entry, idx, pricing: pricingFor(entry, idx, now) });
  });
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

function bandCell(p: RowPricing): string {
  if (p.unavailable) {
    return '<span class="mc-band is-unavailable" data-band>unavailable</span>';
  }
  if (p.band == null) return '<span class="mc-muted">—</span>';
  const isPeak = p.band === 'peak';
  const label = isPeak ? 'peak' : 'off-peak';
  const remaining = p.nextTransitionMs != null ? fmtCountdown(p.nextTransitionMs - p.nowMs) : null;
  const next = p.nextBand != null ? (p.nextBand === 'peak' ? 'peak' : 'off-peak') : null;
  const countdown = remaining != null
    ? `<span class="mc-countdown" data-countdown title=${escapeHtml(next ? `Next: ${next} at ${remaining}` : remaining)}>${remaining}</span>`
    : '';
  const nextTag = next != null ? `<span class="mc-band-next" data-next>→ ${next}</span>` : '';
  return `<span class="mc-band ${isPeak ? 'is-peak' : 'is-offpeak'}" data-band>${label}</span>${countdown}${nextTag}`;
}

function rowHtml(row: Row): string {
  const e = row.entry;
  const p = row.pricing;
  // Combos are listed under their bare routing name (no `combo:` prefix), so
  // the marker is the owner/family the catalog stamps on every combo entry.
  const isCombo = (e.type || '').toLowerCase() === 'combo' ||
    e.owned_by === 'combo' || e.family === 'combo' || e.id.startsWith('combo:');
  const sub = e.display_name && e.display_name !== e.id
    ? `<span class="mc-sub">${escapeHtml(e.display_name)}</span>`
    : '';
  const typeLabel = escapeHtml(e.type || 'chat');
  return `<tr class="mc-row${isCombo ? ' is-combo' : ''}" data-mc-idx="${row.idx}">
    <td data-l="Model" class="mc-model"><span class="mc-id">${escapeHtml(e.id)}</span>${sub}</td>
    <td data-l="Provider">${escapeHtml(providerOf(e))}</td>
    <td data-l="Type"><span class="mc-type${isCombo ? ' is-combo' : ''}">${typeLabel}</span></td>
    <td data-l="Context" class="num">${fmtTokens(e.context_length)}</td>
    <td data-l="Max out" class="num">${fmtTokens(e.max_output_tokens)}</td>
    <td data-l="$ in /M" class="num" data-price="in">${p.unavailable ? '—' : fmtPrice(p.input)}</td>
    <td data-l="$ cached /M" class="num" data-price="cached">${p.unavailable ? '—' : fmtPrice(p.cached)}</td>
    <td data-l="$ out /M" class="num" data-price="out">${p.unavailable ? '—' : fmtPrice(p.output)}</td>
    <td data-l="Band" class="mc-band-cell">${bandCell(p)}</td>
    <td data-l="Capabilities"><div class="mc-badges">${badgesHtml(e)}</div></td>
  </tr>`;
}

function tableHtml(rows: Row[]): string {
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

function syncLastBands(rows: Row[]): void {
  lastBands = schedules.map(() => null);
  for (const row of rows) {
    if (schedules[row.idx]) lastBands[row.idx] = row.pricing.band;
  }
}

function renderBody(): void {
  if (!mounted) return;
  const body = document.getElementById('mc-body');
  const countEl = document.getElementById('mc-count');
  const rows = loading || errorMsg ? [] : visibleRows();
  if (countEl) {
    countEl.textContent = loading
      ? ''
      : query.trim()
        ? `${rows.length} of ${entries.length} models`
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
  body.innerHTML = tableHtml(rows);
  syncLastBands(rows);
}

function setCellText(rowEl: HTMLElement, key: string, text: string): void {
  const el = rowEl.querySelector<HTMLElement>(`[data-price="${key}"]`);
  if (el) el.textContent = text;
}

/** One tick for every scheduled row: update cells in place. A band flip on a
 *  price/band sort rebuilds the table once so the order stays truthful. */
function tick(): void {
  if (!mounted || loading || errorMsg) return;
  const now = Date.now();
  let bandChanged = false;
  for (let idx = 0; idx < entries.length; idx++) {
    const schedule = schedules[idx];
    if (!schedule) continue;
    const rowEl = document.querySelector<HTMLElement>(`tr[data-mc-idx="${idx}"]`);
    if (!rowEl) continue;
    const clock = now + (clockOffsets[idx] ?? 0);
    const resolved = resolvePricing(schedule, clock);
    if ((lastBands[idx] ?? null) !== resolved.band) {
      lastBands[idx] = resolved.band;
      bandChanged = true;
    }
    setCellText(rowEl, 'in', fmtPrice(resolved.rates.input_per_1m));
    setCellText(rowEl, 'out', fmtPrice(resolved.rates.output_per_1m));
    setCellText(rowEl, 'cached', fmtPrice(resolved.rates.cached_per_1m));

    const bandEl = rowEl.querySelector<HTMLElement>('[data-band]');
    if (bandEl) {
      const isPeak = resolved.band === 'peak';
      bandEl.textContent = isPeak ? 'peak' : 'off-peak';
      bandEl.classList.toggle('is-peak', isPeak);
      bandEl.classList.toggle('is-offpeak', !isPeak);
    }
    const remaining = resolved.nextTransitionMs != null ? resolved.nextTransitionMs - clock : null;
    const countdownEl = rowEl.querySelector<HTMLElement>('[data-countdown]');
    if (countdownEl) countdownEl.textContent = remaining != null ? fmtCountdown(remaining) : '—';
    const nextEl = rowEl.querySelector<HTMLElement>('[data-next]');
    if (nextEl) {
      nextEl.textContent = resolved.nextBand
        ? `→ ${resolved.nextBand === 'peak' ? 'peak' : 'off-peak'}`
        : '';
    }
  }
  if (bandChanged && (sortKey === 'band' || isPriceSort(sortKey))) renderBody();
}

function stopTicker(): void {
  ticker?.stop();
  ticker = null;
}

function startTicker(): void {
  stopTicker();
  if (!mounted || !schedules.some((s) => s !== null)) return;
  ticker = createVisibilityAwareInterval(() => tick(), 1_000, { suppressMissedTicks: true });
}

/** Index the freshly loaded entries: parsed schedule, server clock offset,
 *  malformed flag. `pricing_as_of` (epoch seconds) is optional and sampled
 *  once per load — never on every tick. */
function buildPricingIndex(): void {
  schedules = [];
  clockOffsets = [];
  unavailable = [];
  lastBands = [];
  const now = Date.now();
  for (const entry of entries) {
    const parsed = parsePricingSchedule(entry.pricing_schedule);
    schedules.push(parsed.schedule);
    unavailable.push(parsed.status === 'invalid');
    const asOf = entry.pricing_as_of;
    clockOffsets.push(
      typeof asOf === 'number' && Number.isFinite(asOf) ? asOf * 1_000 - now : 0,
    );
  }
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
    buildPricingIndex();
    renderBody();
    startTicker();
  }
}

export async function mountModelsCatalog(container?: HTMLElement | string): Promise<(() => void) | undefined> {
  const main = typeof container === "object" && container !== null
    ? container
    : document.getElementById('main');
  if (!main) return;
  stopTicker();
  mounted = true;
  entries = [];
  schedules = [];
  clockOffsets = [];
  unavailable = [];
  lastBands = [];
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
    stopTicker();
  };
}
