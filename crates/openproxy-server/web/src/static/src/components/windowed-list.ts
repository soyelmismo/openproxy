// components/windowed-list.ts — THE list engine for large client-side catalogs.
//
// Every list that paints a big dataset (model pickers, bulk tables, editor
// pickers) renders through this: it owns the DOM budget (windowing), the
// per-group "show more" chunks, and append-only extension so existing
// element state (checked checkboxes, focus) never gets thrown away.
//
// WindowModel below is the single source of the windowing POLICY (initial
// budget, page size, pager label); WindowedList and data-grid both consume it.
//
// Why central: rendering a 5k-item catalog as DOM freezes mobile at 0fps.
// The engine caps the initial DOM to `initialBudget` items, exposes the rest
// through page-size chunks, and consumers never re-render the whole list —
// they call reset() only when the DATA SET changes (search, filter).

export interface WindowedListGroup<T> {
  id: string;
  label?: string;
  items: T[];
}

export interface WindowedListOptions<T> {
  container: HTMLElement;
  /** Flat item list, in render order (already filtered by the caller). */
  items: T[];
  /** Optional grouping: items carry a group id; groups render in first-seen order. */
  groupIdOf?: (item: T) => string;
  /** Optional group header node (label defaults to the group id). */
  renderGroupHeader?: (groupId: string) => HTMLElement;
  /** Builds one item row. MUST be cheap: called per rendered item. */
  renderItem: (item: T) => HTMLElement;
  /** Items rendered up front, across all groups (default 240). */
  initialBudget?: number;
  /** Items added per "show more" tap (default 60). */
  pageSize?: number;
  /** Class for the engine's root wrapper (default "windowed-list"). */
  className?: string;
  ariaLabel?: string;
}

const DEFAULT_BUDGET = 240;
const DEFAULT_PAGE = 60;

/** Default first paint for grid-style tables (data-grid). */
export const LIST_WINDOW = 150;
/** Rows added per "show more" tap in grid-style tables. */
export const LIST_PAGE = 100;

export interface WindowModelOptions {
  initialBudget?: number;
  pageSize?: number;
  /** Resume at a previously chosen window size (state kept by the caller). */
  visible?: number;
}

/**
 * THE windowing policy, shared by every large-list renderer: how many items to
 * paint first, how each "show more" grows the window, and the label both the
 * list engine and the data-grid print. Renderers decide only HOW to paint;
 * they never re-implement the arithmetic.
 */
export class WindowModel {
  readonly total: number;
  private readonly budget: number;
  private readonly page: number;
  private visibleCount: number;

  constructor(total: number, options: WindowModelOptions = {}) {
    this.total = Math.max(0, total);
    this.budget = Math.max(1, options.initialBudget ?? LIST_WINDOW);
    this.page = Math.max(1, options.pageSize ?? LIST_PAGE);
    this.visibleCount =
      options.visible != null
        ? Math.min(this.total, Math.max(0, options.visible))
        : Math.min(this.total, this.budget);
  }

  /** Items to paint right now. */
  get visible(): number {
    return this.visibleCount;
  }

  /** Items still hidden behind "show more". */
  get pending(): number {
    return this.total - this.visibleCount;
  }

  get hasPending(): boolean {
    return this.pending > 0;
  }

  /** Grow the window by one page; returns the new visible count. */
  showMore(): number {
    this.visibleCount = Math.min(this.total, this.visibleCount + this.page);
    return this.visibleCount;
  }

  /** Items the next "show more" tap will reveal. */
  get nextChunk(): number {
    return Math.min(this.page, this.pending);
  }

  /** The shared pager label. */
  moreLabel(): string {
    return (
      `Show ${this.nextChunk.toLocaleString("en-US")} more` +
      ` (${this.visibleCount.toLocaleString("en-US")} of ${this.total.toLocaleString("en-US")} shown)`
    );
  }
}

interface GroupRuntime<T> {
  id: string;
  items: T[];
  /** Item index of the next not-yet-rendered item (per group). */
  next: number;
  itemsEl: HTMLElement;
  tailEl: HTMLElement;
}

export class WindowedList<T> {
  private readonly opts: Required<Pick<WindowedListOptions<T>, "container" | "items" | "renderItem" | "initialBudget" | "pageSize">> & WindowedListOptions<T>;
  private readonly root: HTMLElement;
  private groups: Array<GroupRuntime<T>> = [];
  private total = 0;

  constructor(options: WindowedListOptions<T>) {
    this.opts = {
      initialBudget: DEFAULT_BUDGET,
      pageSize: DEFAULT_PAGE,
      ...options,
    };
    this.root = document.createElement("div");
    this.root.className = this.opts.className || "windowed-list";
    this.opts.container.appendChild(this.root);
    this.renderAll();
  }

  /** Total items in the data set (not just rendered). */
  get totalCount(): number {
    return this.total;
  }

  /** Items currently present in the DOM. */
  get renderedCount(): number {
    return this.groups.reduce((sum, g) => sum + g.next, 0);
  }

  /** Number of items still hidden behind "show more". */
  get pendingCount(): number {
    return this.total - this.renderedCount;
  }

  /** Replace the whole data set (search/filter change). Rebuilds from scratch. */
  reset(items: T[]): void {
    this.opts.items = items;
    this.groups = [];
    this.root.innerHTML = "";
    this.renderAll();
  }

  /** Iterate the item rows currently in the DOM (in render order). */
  forEachRendered(fn: (el: HTMLElement, item: T) => void): void {
    for (const g of this.groups) {
      const rows = g.itemsEl.querySelectorAll<HTMLElement>(":scope > *");
      const upto = Math.min(g.next, g.items.length);
      rows.forEach((row, i) => {
        if (i < upto) fn(row, g.items[i]!);
      });
    }
  }

  /** Render everything (used by "expand all" affordances). */
  renderRemaining(): void {
    for (const g of this.groups) this.extendGroup(g, g.items.length - g.next);
  }

  private renderAll(): void {
    const items = this.opts.items;
    this.total = items.length;

    if (this.opts.groupIdOf) {
      // First-seen group order over the flat list.
      const byId = new Map<string, T[]>();
      for (const item of items) {
        const gid = this.opts.groupIdOf(item);
        if (!byId.has(gid)) byId.set(gid, []);
        byId.get(gid)!.push(item);
      }
      // Budget: fill groups in order until exhausted.
      let budget = this.opts.initialBudget;
      for (const [gid, gitems] of byId) {
        const take = Math.min(gitems.length, this.opts.pageSize, Math.max(budget, 0));
        budget -= take;
        this.groups.push(this.buildGroup(gid, gitems, take));
      }
    } else {
      const win = new WindowModel(items.length, {
        initialBudget: this.opts.initialBudget,
        pageSize: this.opts.pageSize,
      });
      this.groups.push(this.buildGroup("", items, win.visible));
    }
  }

  private buildGroup(gid: string, gitems: T[], take: number): GroupRuntime<T> {
    const section = document.createElement("section");
    section.className = "windowed-group";
    section.dataset["groupId"] = gid;

    if (gid && this.opts.renderGroupHeader) {
      section.appendChild(this.opts.renderGroupHeader(gid));
    } else if (gid) {
      const h = document.createElement("div");
      h.className = "windowed-group-header";
      h.textContent = gid;
      section.appendChild(h);
    }

    const itemsEl = document.createElement("div");
    itemsEl.className = "windowed-group-items";
    section.appendChild(itemsEl);

    const tailEl = document.createElement("div");
    tailEl.className = "windowed-group-tail";
    section.appendChild(tailEl);

    this.root.appendChild(section);
    const g: GroupRuntime<T> = { id: gid, items: gitems, next: 0, itemsEl, tailEl };
    this.extendGroup(g, take);
    return g;
  }

  /** Append-only: render `count` more items of `g`, then refresh its tail. */
  private extendGroup(g: GroupRuntime<T>, count: number): void {
    const start = g.next;
    const end = Math.min(start + count, g.items.length);
    const frag = document.createDocumentFragment();
    for (let i = start; i < end; i++) {
      frag.appendChild(this.opts.renderItem(g.items[i]!));
    }
    g.next = end;
    g.itemsEl.appendChild(frag);
    this.refreshTail(g);
  }

  private refreshTail(g: GroupRuntime<T>): void {
    const pending = g.items.length - g.next;
    g.tailEl.innerHTML = "";
    if (pending <= 0) return;
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "link windowed-more";
    btn.textContent =
      `Show ${Math.min(this.opts.pageSize, pending).toLocaleString("en-US")} more` +
      (pending > this.opts.pageSize ? ` of ${pending.toLocaleString("en-US")}` : "");
    btn.addEventListener("click", () => {
      this.extendGroup(g, this.opts.pageSize);
    });
    g.tailEl.appendChild(btn);
  }
}
