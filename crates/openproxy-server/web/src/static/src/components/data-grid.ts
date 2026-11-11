// components/data-grid.ts — dense listing primitive for the workspace.
//
// One line per row, always. Columns are sized by the browser from the actual
// content: every cell is measured at its natural width and the spare space is
// distributed proportionally to content size. There are no width breakpoints
// anywhere in this file — a wider tile gives the same rows more room, a
// narrower one keeps every column legible and scrolls only if the content
// genuinely needs more space than exists.
//
// Interaction contract (shared by every listing that adopts this primitive):
//   right-click / menu key  → context menu with the full action set
//   double-click / F2       → inline rename (when the caller allows it)
//   hover                   → reveals 2-3 ghost quick actions, nothing permanent
//   click / ctrl / shift    → selection, with a floating bulk bar
//   ↑↓ space F2 Supr Ctrl+A Esc → keyboard-first operation

import { html, nothing, type TemplateResult } from "lit-html";
import { openContextMenu, closeContextMenu, type ContextMenuItem } from "./context-menu.js";
import { computeGridTracks } from "./data-grid-layout.js";

export interface GridQuickAction<T> {
  action: string;
  title: string;
  icon?: TemplateResult | string;
  danger?: boolean;
  /** Hidden unless the predicate holds (e.g. revoke only when active). */
  when?: (row: T) => boolean;
}

export interface GridMenuItem {
  action?: string | undefined;
  label?: string | undefined;
  shortcut?: string | undefined;
  danger?: boolean | undefined;
  separator?: boolean | undefined;
  disabled?: boolean | undefined;
}

export interface GridBulkAction<T> {
  key: string;
  label: string;
  danger?: boolean;
  run: (rows: T[]) => void;
}

export interface GridColumn<T> {
  key: string;
  label: string;
  render?: (row: T) => TemplateResult | string;
  /** Minimum px this column keeps. For the elastic column this is the floor
   *  it compresses to; ignored for the others, which are always their natural
   *  width. Default 0. */
  min?: number;
  /** Mark ONE column as elastic: it absorbs all spare space and compresses,
   *  with ellipsis, down to its min. Every other column renders at its
   *  natural width or folds — nothing is ever squeezed into illegibility. */
  elastic?: boolean;
  /** Upper bound in px for this column's track: content beyond it
   *  ellipsizes instead of demanding more room. A property of the data
   *  type (a model-chip cluster never deserves 400px), not a breakpoint. */
  max?: number;
  /** Fold order when space runs out: 0 or undefined = essential (never
   *  folds), 1 folds first, 2 next, ... Purely space-driven — no pixel
   *  breakpoint is involved; a column folds the moment the tile is too
   *  narrow to keep the rest above their floors. */
  fold?: number;
}

export interface DataGridProps<T> {
  /** Stable id across renders — selection/editing state hangs off it. */
  id: string;
  columns: GridColumn<T>[];
  rows: readonly T[];
  rowKey: (row: T) => string | number;
  className?: string;
  rowClass?: (row: T) => string;
  emptyMessage?: string;
  /** Ghost buttons shown on hover / focus / selection. Nothing is permanent. */
  quickActions?: (row: T) => GridQuickAction<T>[];
  /** Right-click menu, as plain data. */
  menuItems?: (row: T) => GridMenuItem[];
  /** Every quick action, menu entry and Delete key lands here. */
  onAction: (row: T, action: string, rowEl: HTMLElement) => void;
  /** When it returns a string, that row renames inline (dblclick / F2). */
  renameable?: (row: T) => string | null;
  onRename?: (row: T, value: string) => void;
  selectable?: boolean;
  /** Expandable detail for a row (config drawer). One row open at a time;
   *  a plain click toggles it, Escape closes. Clicks inside the drawer do
   *  not re-toggle the row. */
  expandedRow?: (row: T) => TemplateResult;
  bulkActions?: GridBulkAction<T>[];
  /** Label for the bulk bar's counter; defaults to "N selected". */
  bulkCountLabel?: (n: number) => string;
  /** The caller re-renders; the grid pokes this whenever its state changes. */
  __refresh?: () => void;
}

interface GridState {
  selected: Set<string>;
  editing: string | null;
  focused: string | null;
  expanded: string | null;
  /** Viewport point of the click that produced the current selection: the
   *  bulk bar anchors there (fixed, so it survives scrolling). */
  bulkAt: { x: number; y: number } | null;
}

const states = new Map<string, GridState>();
/** Last post-render refresh per grid root: rows reference + box. A render
 *  that did not replace the rows array (most live-update re-renders) cannot
 *  have changed any cell width, so the refresh is skipped entirely. */
const lastRefresh = new WeakMap<HTMLElement, { rows: unknown; w: number; h: number }>();
/** Last per-column natural widths. Tile resizes reuse them (see scheduleMeasure/flushMeasures). */
const naturalsCache = new WeakMap<HTMLElement, number[]>();
/** Last applied fold state per column - lets the apply diff instead of
 *  rewriting every cell's visibility on each resize frame. */
const lastActiveCols = new WeakMap<HTMLElement, boolean[]>();
/** Last written cell-padding step (see flushMeasures). */
const lastPadCache = new WeakMap<HTMLElement, string>();
const observed = new WeakMap<HTMLElement, ResizeObserver>();

/** Re-run layout whenever the grid's box changes — tile resizes land here. */
function observeGrid(root: HTMLElement): void {
  if (observed.has(root)) return;
  const ro = new ResizeObserver(() => scheduleMeasure(root, { reuseNaturals: true }));
  ro.observe(root);
  observed.set(root, ro);
}

function stateFor(id: string): GridState {
  let s = states.get(id);
  if (!s) {
    s = { selected: new Set(), editing: null, focused: null, expanded: null, bulkAt: null };
    states.set(id, s);
  }
  return s;
}

function cellContent<T>(col: GridColumn<T>, row: T): TemplateResult {
  if (col.render) {
    const v = col.render(row);
    return typeof v === "string" ? html`${v}` : v;
  }
  const value: unknown = (row as Record<string, unknown>)[col.key];
  return html`${value == null ? "" : String(value)}`;
}

/* ---------------------------------------------------------------- layout */

/** Lay the grid out with the browser's own track sizing and fold columns
 *  only when the row truly overflows.
 *
 *  The column template is native CSS grid sizing, no measured pixels:
 *    - the one elastic column is minmax(min, 1fr): it absorbs every spare
 *      pixel and compresses, with ellipsis, down to its floor;
 *    - every other column is a max-content track: exactly as wide as its
 *      widest cell, header included - it is never squeezed into
 *      illegibility, it either fits at full width or folds away whole;
 *    - the quick-actions column is max-content too, so the hover buttons
 *      always have their space.
 *
 *  Folding is driven by a single real number - does the wrap overflow after
 *  this layout - never by hand-tuned widths or breakpoints: fold the next
 *  column by the caller's priority and re-check. Folded cells are hidden
 *  with display:none; because their siblings carry explicit grid-column
 *  placements, the survivors never slide into the wrong track.
 *
 *  The pass is idempotent: every run rebuilds the active set from scratch,
 *  unfolding as the tile grows and refolding as it narrows. */
/**
 * Read-only column sizing.
 *
 * The engine never invalidates layout to measure: scrollWidth of a
 * nowrap, overflow-hidden cell is the FULL content width even when the
 * cell is visually clipped by its track or folded into a 0px column, so
 * every width comes from one batched read pass over the cells that are
 * already on screen. Folding is then pure arithmetic over those numbers,
 * and the resulting pixel template is written once, and only when it
 * actually differs from the one applied.
 *
 * (The previous two-pass design laid out an all-max-content template to
 * measure, then re-laid-out per fold step: the forward feedback with the
 * ResizeObserver on top of analytics' periodic re-renders spent 4.1
 * seconds of a 5-second idle window in this function on a single grid.)
 */

interface MeasureEntry {
  root: HTMLElement;
  reuseNaturals: boolean;
  headEl: HTMLElement;
  rowEls: HTMLElement[];
  n: number;
  mins: number[];
  folds: number[];
  elastic: number;
  slides: HTMLElement[][];
  natural: number[] | undefined;
  avail: number;
}

/** All pending grid relayouts. Every layout READ for every grid happens in
 *  one batch, then every WRITE: interleaving reads and writes across N grids
 *  (each grid's RO callback reading after another's write) forced 3-4 full
 *  workspace relayouts per drag frame. */
const measureQueue = new Map<HTMLElement, MeasureEntry>();
let measureFlushRaf = 0;

function scheduleMeasure(root: HTMLElement, opts: { reuseNaturals?: boolean } = {}): void {
  const headEl = root.querySelector<HTMLElement>(".dg-head");
  const rowEls = Array.from(root.querySelectorAll<HTMLElement>(".dg-row"));
  if (!headEl || !rowEls.length) return;
  const n = headEl.querySelectorAll(":scope > .dg-col").length;
  const conf = (root as HTMLElement & { __dgConf?: { mins: number[]; folds: number[]; elastic: number } }).__dgConf;
  measureQueue.set(root, {
    root,
    reuseNaturals: opts.reuseNaturals === true,
    headEl,
    rowEls,
    n,
    mins: conf?.mins ?? [],
    folds: conf?.folds ?? [],
    elastic: typeof conf?.elastic === "number" ? conf.elastic : -1,
    slides: [headEl, ...rowEls].map((scope) =>
      Array.from(scope.querySelectorAll<HTMLElement>(":scope > .dg-col"))
    ),
    natural: undefined,
    avail: 0,
  });
  if (!measureFlushRaf) {
    measureFlushRaf = requestAnimationFrame(flushMeasures);
  }
}

function flushMeasures(): void {
  measureFlushRaf = 0;
  const items = Array.from(measureQueue.values());
  measureQueue.clear();

  // READ phase: naturals (only when not reusable) + available width, with
  // no writes in between, so a single relayout serves the whole batch.
  for (const it of items) {
    let natural = it.reuseNaturals ? naturalsCache.get(it.root) : undefined;
    if (!natural || natural.length !== it.n) {
      // The natural width of every column is the widest cell content. Read
      // from the .dg-inner box, never the cell: the cell fills its track, so
      // ITS scrollWidth would report the track width and tracks would never
      // shrink; the inner shrink-to-fit box reports the true content width
      // regardless of the track it sits in.
      natural = new Array(it.n).fill(0);
      for (const cells of it.slides) {
        for (let i = 0; i < it.n && i < cells.length; i++) {
          const inner = (cells[i]!.firstElementChild as HTMLElement) ?? cells[i]!;
          const w = inner.scrollWidth;
          if (w > natural[i]!) natural[i] = w;
        }
      }
      naturalsCache.set(it.root, natural);
    }
    it.natural = natural;
    const rowStyle = getComputedStyle(it.headEl);
    it.avail = Math.max(0, it.headEl.clientWidth
      - parseFloat(rowStyle.paddingLeft || "0")
      - parseFloat(rowStyle.paddingRight || "0"));
  }

  // WRITE phase: templates + visibility diffs, no reads.
  for (const it of items) {
    const natural = it.natural!;
    const n = it.n;
    const actions = it.root.querySelector(".dg-row .dg-actions") ? n - 1 : -1;
    const { template: tplStr, active } = computeGridTracks(
      natural, it.mins, it.folds, it.elastic, it.avail, actions,
    );
    // Cell padding, quantized to steps so a pure width change does not
    // restyle every .dg-inner of a large grid on every drag frame.
    const pad = it.avail > 1000 ? "9px" : it.avail > 700 ? "7px" : it.avail > 450 ? "5px" : it.avail > 280 ? "3px" : "2px";
    const lastPad = lastPadCache.get(it.root);
    if (lastPad !== pad) {
      it.root.style.setProperty("--dg-pad", pad);
      lastPadCache.set(it.root, pad);
    }
    if (it.root.style.getPropertyValue("--dg-cols") === tplStr) continue;

    it.root.style.setProperty("--dg-cols", tplStr);
    // Only cells whose column actually folded or unfolded get a write.
    const prev = lastActiveCols.get(it.root);
    for (let i = 0; i < n; i++) {
      const on = active[i]!;
      if (prev && prev[i] === on) continue;
      for (const cells of it.slides) {
        if (!cells[i]) continue;
        cells[i]!.style.visibility = on ? "" : "hidden";
      }
    }
    lastActiveCols.set(it.root, active);
  }
}

/* ------------------------------------------------------------------ editing */

function startEdit<T>(props: DataGridProps<T>, row: T): void {
  const s = stateFor(props.id);
  if (!props.renameable || props.renameable(row) == null) return;
  s.editing = String(props.rowKey(row));
  props.__refresh?.();
}

function commitEdit<T>(props: DataGridProps<T>, input: HTMLInputElement, save: boolean): void {
  const s = stateFor(props.id);
  const key = s.editing;
  s.editing = null;
  props.__refresh?.();
  if (!save || !key || !props.onRename) return;
  const row = props.rows.find((r) => String(props.rowKey(r)) === key);
  if (!row) return;
  const original = props.renameable?.(row) ?? "";
  const value = input.value.trim();
  if (!value || value === original) return;
  props.onRename(row, value);
}

/* -------------------------------------------------------------------- state */

function prune<T>(props: DataGridProps<T>): void {
  const s = stateFor(props.id);
  const keys = new Set(props.rows.map((r) => String(props.rowKey(r))));
  for (const k of Array.from(s.selected)) if (!keys.has(k)) s.selected.delete(k);
  if (s.editing && !keys.has(s.editing)) s.editing = null;
  if (s.focused && !keys.has(s.focused)) s.focused = null;
  if (s.expanded && !keys.has(s.expanded)) s.expanded = null;
}

function selectRange<T>(props: DataGridProps<T>, fromIdx: number, toIdx: number): void {
  const s = stateFor(props.id);
  s.selected.clear();
  const [a, b] = fromIdx <= toIdx ? [fromIdx, toIdx] : [toIdx, fromIdx];
  for (let i = a; i <= b; i++) s.selected.add(String(props.rowKey(props.rows[i]!)));
  props.__refresh?.();
}

/* ------------------------------------------------------------------ render */

export function renderDataGrid<T>(props: DataGridProps<T>): TemplateResult {
  const s = stateFor(props.id);
  prune(props);
  const { rows } = props;
  const empty: string = props.emptyMessage ?? "No items.";
  const selectable = props.selectable !== false;
  const cls = `dg ${props.className ?? ""}`.trim();
  const findRow = (key: string): T | undefined => rows.find((r) => String(props.rowKey(r)) === key);
  // The template function runs before lit commits the DOM; the microtask
  // render lands first, so a zero-delay timeout measures the committed tree.
  {
    const rowsRef: unknown = props.rows;
    setTimeout(() => {
      const root = document.querySelector<HTMLElement>(`[data-grid-id="${props.id}"]`);
      if (!root || !root.isConnected) return;
      observeGrid(root);
      const w = root.clientWidth, h = root.clientHeight;
      const last = lastRefresh.get(root);
      if (last && last.rows === rowsRef && last.w === w && last.h === h) return;
      lastRefresh.set(root, { rows: rowsRef, w, h });
      scheduleMeasure(root);
    }, 0);
  }

  const rowsTpl = rows.map((row, idx) => {
    const key = String(props.rowKey(row));
    const selected = s.selected.has(key);
    const editing = s.editing === key;
    const renameText = props.renameable?.(row) ?? null;
    const rowCls = `dg-row${selected ? " sel" : ""}${editing ? " editing" : ""}${props.rowClass ? " " + props.rowClass(row) : ""}`;
    return html`
      <div
        class=${rowCls}
        data-row-key=${key}
        data-i=${idx}
        tabindex="-1"
        @focus=${() => { s.focused = key; }}
        @keydown=${(e: KeyboardEvent) => onRowKeydown(e, props, row, rowElOf(e))}
      >
        ${props.columns.map((col, ci) => {
          const isName = editing && renameText != null && ci === 0;
          return html`
            <div class="dg-col" style=${col.max != null ? `max-width:${col.max}px` : nothing}>
              <div class="dg-inner">
                ${isName
                  ? html`<input
                      class="dg-rename"
                      .value=${renameText}
                      @keydown=${(e: KeyboardEvent) => {
                        e.stopPropagation();
                        if (e.key === "Enter") commitEdit(props, e.target as HTMLInputElement, true);
                        if (e.key === "Escape") commitEdit(props, e.target as HTMLInputElement, false);
                      }}
                      @blur=${(e: Event) => commitEdit(props, e.target as HTMLInputElement, true)}
                      @click=${(e: Event) => e.stopPropagation()}
                      @dblclick=${(e: Event) => e.stopPropagation()}
                    />`
                  : cellContent(col, row)}
              </div>
            </div>
          `;
        })}
        ${props.quickActions
          ? html`<div class="dg-col dg-actions">
              <div class="dg-inner">
                ${props.quickActions(row).filter((a) => !a.when || a.when(row)).map((a) => html`
                  <button
                    type="button"
                    class="dg-qa${a.danger ? " danger" : ""}"
                    title=${a.title}
                    @click=${(e: Event) => {
                      e.stopPropagation();
                      props.onAction(row, a.action, (e.currentTarget as HTMLElement).closest(".dg-row") as HTMLElement);
                    }}
                  >${a.icon ?? "⋯"}</button>
                `)}
              </div>
            </div>`
          : nothing}
      </div>
      ${s.expanded === key && props.expandedRow
        ? html`<div class="dg-expand" role="region" data-expand-for=${key}>${props.expandedRow(row)}</div>`
        : nothing}
    `;
  });

  // Anchor the bulk bar where the selection happened; fixed positioning keeps
  // it on screen while the page scrolls. Clamped so it never leaves the viewport.
  const at = s.bulkAt;
  const bulkPos = at
    ? `left:${Math.min(Math.max(at.x, 8), window.innerWidth - 408)}px;top:${Math.min(Math.max(at.y + 14, 8), window.innerHeight - 46)}px`
    : undefined;
  const bulk = s.selected.size > 0 && (props.bulkActions?.length ?? 0) > 0
    ? html`<div class="dg-bulk" role="toolbar" style=${bulkPos}>
        <span class="dg-bulk-n">${props.bulkCountLabel ? props.bulkCountLabel(s.selected.size) : `${s.selected.size} selected`}</span>
        ${props.bulkActions!.map((b) => html`
          <button type="button" class=${b.danger ? "danger" : ""} @click=${() => {
            const picked = rows.filter((r) => s.selected.has(String(props.rowKey(r))));
            b.run(picked);
            s.selected.clear();
            props.__refresh?.();
          }}>${b.label}</button>
        `)}
        <button type="button" class="ghost" @click=${() => { s.selected.clear(); props.__refresh?.(); }}>esc</button>
      </div>`
    : nothing;

  return html`
    <div
      class=${cls}
      data-grid-id=${props.id}
      tabindex="0"
      .__dgConf=${{ mins: props.columns.map((c) => c.min ?? 0), folds: props.columns.map((c) => c.fold ?? 0), elastic: props.columns.findIndex((c) => c.elastic) }}
      @click=${(e: MouseEvent) => {
        const rowEl = (e.target as HTMLElement).closest(".dg-row") as HTMLElement | null;
        if (!rowEl || (e.target as HTMLElement).closest(".dg-actions")) return;
        rowEl.focus();
        const key = rowEl.dataset["rowKey"]!;
        const idx = Number(rowEl.dataset["i"]);
        if (selectable && e.shiftKey && s.focused && s.focused !== key) {
          const from = rows.findIndex((r) => String(props.rowKey(r)) === s.focused);
          selectRange(props, from === -1 ? idx : from, idx);
          if (s.selected.size > 0) s.bulkAt = { x: e.clientX, y: e.clientY };
        } else if (selectable && (e.ctrlKey || e.metaKey)) {
          s.selected.has(key) ? s.selected.delete(key) : s.selected.add(key);
          s.focused = key;
          if (s.selected.size > 0) s.bulkAt = { x: e.clientX, y: e.clientY };
          props.__refresh?.();
        } else if (selectable) {
          if (s.selected.size === 1 && s.selected.has(key)) { s.selected.clear(); props.__refresh?.(); }
          else { s.selected.clear(); s.selected.add(key); s.focused = key; s.bulkAt = { x: e.clientX, y: e.clientY }; props.__refresh?.(); }
          if (props.expandedRow) {
            s.expanded = s.expanded === key ? null : key;
            props.__refresh?.();
          }
        }
      }}
      @dblclick=${(e: MouseEvent) => {
        const rowEl = (e.target as HTMLElement).closest(".dg-row") as HTMLElement | null;
        if (!rowEl) return;
        const row = findRow(rowEl.dataset["rowKey"]!);
        if (row) startEdit(props, row);
      }}
      @contextmenu=${(e: MouseEvent) => {
        const rowEl = (e.target as HTMLElement).closest(".dg-row") as HTMLElement | null;
        if (!rowEl) return;
        e.preventDefault();
        const row = findRow(rowEl.dataset["rowKey"]!);
        if (row) openRowMenuFor(props, row, e.clientX, e.clientY, rowEl);
      }}
      @keydown=${(e: KeyboardEvent) => onGridKeydown(e, props)}
    >
      <div class="dg-wrap">
        <div class="dg-head" aria-hidden="true">
          ${props.columns.map((c) => html`<div class="dg-col" style=${c.max != null ? `max-width:${c.max}px` : nothing}><div class="dg-inner">${c.label}</div></div>`)}
          ${props.quickActions ? html`<div class="dg-col dg-actions"><div class="dg-inner"></div></div>` : nothing}
        </div>
        ${rows.length === 0 ? html`<div class="dg-empty">${empty}</div>` : rowsTpl}
        ${bulk}
      </div>
    </div>
  `;
}

function rowElOf(e: Event): HTMLElement {
  return (e.currentTarget as HTMLElement);
}

/** Toggle a row's expanded drawer from outside the grid (context menu, view
 *  button). The caller owns the re-render: follow with requestUpdate(). */
export function toggleGridRowExpansion(gridId: string, rowKey: string | number): void {
  const s = states.get(gridId);
  if (!s) return;
  s.expanded = s.expanded === String(rowKey) ? null : String(rowKey);
}

/** Post-render hook: sizes the grid from measured content. Call it inside a
 *  requestAnimationFrame after any render that changed rows or columns. */
export function refreshDataGridLayout(gridId: string): void {
  const root = document.querySelector<HTMLElement>(`[data-grid-id="${gridId}"]`);
  if (!root) return;
  observeGrid(root);
  scheduleMeasure(root);
}

function onGridKeydown<T>(e: KeyboardEvent, props: DataGridProps<T>): void {
  const root = e.currentTarget as HTMLElement;
  const s = stateFor(props.id);
  if (e.key === "Escape") {
    closeContextMenu();
    s.selected.clear();
    s.expanded = null;
    props.__refresh?.();
    return;
  }
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "a" && props.selectable !== false) {
    e.preventDefault();
    props.rows.forEach((r) => s.selected.add(String(props.rowKey(r))));
    props.__refresh?.();
    return;
  }
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    const rowEls = Array.from(root.querySelectorAll<HTMLElement>(".dg-row"));
    if (!rowEls.length) return;
    const cur = rowEls.findIndex((r) => r.dataset["rowKey"] === s.focused);
    const next = e.key === "ArrowDown" ? Math.min(rowEls.length - 1, cur + 1) : Math.max(0, cur - 1);
    const target = rowEls[cur === -1 ? 0 : next]!;
    target.focus();
    s.focused = target.dataset["rowKey"]!;
  }
}

function onRowKeydown<T>(e: KeyboardEvent, props: DataGridProps<T>, row: T, rowEl: HTMLElement): void {
  const s = stateFor(props.id);
  if (e.key === " " && props.selectable !== false) {
    e.preventDefault();
    const key = String(props.rowKey(row));
    s.selected.has(key) ? s.selected.delete(key) : s.selected.add(key);
    props.__refresh?.();
    return;
  }
  if (e.key === "F2") { e.preventDefault(); startEdit(props, row); return; }
  if (e.key === "Delete" || e.key === "Backspace") { e.preventDefault(); props.onAction(row, "delete", rowEl); return; }
  if (e.key === "ContextMenu") {
    e.preventDefault();
    const r = rowEl.getBoundingClientRect();
    openRowMenuFor(props, row, r.left + 8, r.bottom + 4, rowEl);
  }
}

function openRowMenuFor<T>(props: DataGridProps<T>, row: T, x: number, y: number, rowEl: HTMLElement): void {
  const s = stateFor(props.id);
  const key = String(props.rowKey(row));
  if (!s.selected.has(key)) { s.selected.clear(); s.selected.add(key); props.__refresh?.(); }
  if (!props.menuItems) return;
  const items: (ContextMenuItem | "separator")[] = props.menuItems(row).map((it: GridMenuItem): ContextMenuItem | "separator" => {
    if (it.separator) return "separator" as const;
    return {
      label: it.label,
      shortcut: it.shortcut,
      danger: it.danger,
      disabled: it.disabled,
      run: () => {
        // "rename" is the grid's own reserved action: it drives inline
        // editing, not a view-side operation.
        if (it.action === "rename") { startEdit(props, row); return; }
        if (it.action) props.onAction(row, it.action, rowEl);
      },
    };
  });
  openContextMenu({ x, y, items });
}

export { nothing };
