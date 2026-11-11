// views/proxy-sources.ts — Proxy sources management view.
// Ported to the dense data-grid primitive with full actions, inline rename,
// reorder controls, toggle switch, context menu, and bulk deletion.

import { html, type TemplateResult } from 'lit-html';
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { createView } from "../lib/view-utils.js";
import { icons } from "../lib/icons.js";
import {
  reloadProxySources,
  moveProxySource,
  reorderProxySources,
  toggleProxySourceActive,
  showAddProxySource,
  showEditProxySource,
  deleteProxySource,
  testProxySource,
} from "../handlers/proxy-source-handlers.js";
import { showToast } from "../components/toast.js";
import { showConfirm } from "../lib/show-confirm.js";
import { renderDataGrid, type GridColumn } from "../components/data-grid.js";
import { requestUpdate } from "../state/reactive.js";
import { t } from "../i18n/index.js";
import type { ProxySource } from "../lib/types/api.js";

let loadError: string | null = null;

let sourceTouchDragState: {
  draggedId: string;
  rowEl: HTMLElement;
  currentOverEl: HTMLElement | null;
} | null = null;

function onSourceTouchStart(sourceId: string, e: TouchEvent): void {
  const handle = e.currentTarget as HTMLElement;
  const row = handle.closest(".dg-row") as HTMLElement | null;
  if (!row) return;

  sourceTouchDragState = {
    draggedId: sourceId,
    rowEl: row,
    currentOverEl: null,
  };

  row.classList.add("touch-dragging");
  if (navigator.vibrate) {
    try { navigator.vibrate(15); } catch { /* ignore */ }
  }
}

function onSourceTouchMove(e: TouchEvent): void {
  if (!sourceTouchDragState) return;
  const touch = e.touches[0];
  if (!touch) return;

  if (e.cancelable) e.preventDefault();

  const el = document.elementFromPoint(touch.clientX, touch.clientY);
  const overRow = el?.closest(".dg-row") as HTMLElement | null;

  if (sourceTouchDragState.currentOverEl && sourceTouchDragState.currentOverEl !== overRow) {
    sourceTouchDragState.currentOverEl.classList.remove("drag-over");
    sourceTouchDragState.currentOverEl = null;
  }

  if (overRow && overRow !== sourceTouchDragState.rowEl) {
    overRow.classList.add("drag-over");
    sourceTouchDragState.currentOverEl = overRow;
  }
}

async function onSourceTouchEnd(): Promise<void> {
  if (!sourceTouchDragState) return;
  const { draggedId, rowEl, currentOverEl } = sourceTouchDragState;

  rowEl.classList.remove("touch-dragging");
  if (currentOverEl) {
    currentOverEl.classList.remove("drag-over");
    const dropTargetId = currentOverEl.getAttribute("data-row-key");
    if (dropTargetId && dropTargetId !== draggedId) {
      await reorderProxySources(draggedId, dropTargetId);
    }
  }

  sourceTouchDragState = null;
}

async function onRenameSource(s: ProxySource, newName: string): Promise<void> {
  const trimmed = newName.trim();
  if (!trimmed || trimmed === s.name) return;
  try {
    const payload = {
      name: trimmed,
      url: s.url,
      priority: s.priority,
      active: s.active,
    };
    await api(`/proxy-sources/${s.id}`, {
      method: "PUT",
      body: JSON.stringify(payload),
    });
    s.name = trimmed;
    showToast(t("grid.toast.copied") ? `Renamed to ${trimmed}` : `Renamed to ${trimmed}`, "success");
    requestUpdate();
  } catch (err: unknown) {
    const msg = err instanceof Error ? err.message : String(err);
    showToast(`Failed to rename: ${msg}`, "error");
  }
}

async function onBulkDeleteSources(picked: ProxySource[]): Promise<void> {
  const deletable = picked.filter((s) => !s.is_builtin);
  if (deletable.length === 0) {
    showToast("Built-in sources cannot be deleted", "error");
    return;
  }
  if (!(await showConfirm({
    title: `Delete ${deletable.length} proxy source(s)?`,
    message: `This permanently deletes: ${deletable.map((s) => s.name || s.id).join(", ")}`,
    danger: true,
    confirmLabel: t("common.delete") || "Delete",
  }))) return;

  const failed: string[] = [];
  for (const s of deletable) {
    try {
      await api(`/proxy-sources/${s.id}`, { method: "DELETE" });
    } catch {
      failed.push(s.name || s.id);
    }
  }
  await reloadProxySources();
  if (failed.length > 0) {
    showToast(`Failed to delete: ${failed.join(", ")}`, "error");
  } else {
    showToast(`Deleted ${deletable.length} source(s)`, "success");
  }
}

function renderPageHeader(): TemplateResult {
  return html`
    <div
      class="cx-toolbar cx-sources-toolbar"
      title="Manage dynamic URLs and endpoints to automatically scrape proxies from."
      aria-description="Manage dynamic URLs and endpoints to automatically scrape proxies from."
    >
      <h2 class="view-identity">Proxy Sources</h2>
      <div class="cx-toolbar-actions">
        <button
          type="button"
          class="primary"
          @click=${() => showAddProxySource()}
        >
          ${icons.plus()} Add Source
        </button>
      </div>
    </div>
  `;
}

function renderProxySourcesList(sources: ProxySource[]): TemplateResult {
  if (loadError) {
    return html`<div class="banner banner-error">${loadError}</div>`;
  }

  if (sources.length === 0) {
    return html`
      <div class="card empty-state">
        <p>No proxy sources configured yet.</p>
        <button
          type="button"
          class="primary"
          style="margin-top: 1rem;"
          @click=${() => showAddProxySource()}
        >
          ${icons.plus()} Add your first proxy source
        </button>
      </div>
    `;
  }

  const columns: GridColumn<ProxySource>[] = [
    {
      key: "reorder",
      label: "",
      max: 65,
      render: (s) => html`
        <span
          class="drag-handle"
          title="Drag to reorder"
          draggable="true"
          @dragstart=${(e: DragEvent) => {
            e.dataTransfer?.setData("text/plain", s.id);
            const row = (e.target as HTMLElement).closest(".dg-row");
            row?.classList.add("dragging");
          }}
          @dragend=${(e: DragEvent) => {
            const row = (e.target as HTMLElement).closest(".dg-row");
            row?.classList.remove("dragging");
          }}
          @touchstart=${(e: TouchEvent) => onSourceTouchStart(s.id, e)}
          @touchmove=${(e: TouchEvent) => onSourceTouchMove(e)}
          @touchend=${() => void onSourceTouchEnd()}
          @touchcancel=${() => void onSourceTouchEnd()}
        >${icons.dragHandle()}</span>
        <button
          type="button"
          class="source-reorder-btn"
          title="Move up"
          @click=${(e: Event) => { e.stopPropagation(); void moveProxySource(s.id, -1); }}
        >${icons.caretUp()}</button>
        <button
          type="button"
          class="source-reorder-btn"
          title="Move down"
          @click=${(e: Event) => { e.stopPropagation(); void moveProxySource(s.id, 1); }}
        >${icons.caretDown()}</button>
      `,
    },
    {
      key: "name",
      label: "Name",
      min: 120,
      elastic: true,
      render: (s) => html`<strong class="source-name-text" title=${s.name}>${s.name}</strong>`,
    },
    {
      key: "url",
      label: "URL",
      max: 280,
      fold: 2,
      render: (s) => html`
        <a
          href=${s.url}
          target="_blank"
          rel="noopener noreferrer"
          class="source-url-text"
          title=${s.url}
          @click=${(e: Event) => e.stopPropagation()}
        >${s.url}</a>
      `,
    },
    {
      key: "priority",
      label: "Priority",
      max: 75,
      fold: 4,
      render: (s) => html`<span class="badge source-priority-badge">${s.priority}</span>`,
    },
    {
      key: "active",
      label: "Active",
      max: 60,
      render: (s) => html`
        <button
          type="button"
          role="switch"
          aria-checked=${s.active ? "true" : "false"}
          class="toggle-btn ${s.active ? "on" : "off"}"
          title="${s.active ? "Disable" : "Enable"} proxy source"
          @click=${(e: Event) => {
            e.stopPropagation();
            void toggleProxySourceActive(s.id);
          }}
        >
          <span class="toggle-thumb"></span>
        </button>
      `,
    },
    {
      key: "stats",
      label: "Stats",
      max: 160,
      fold: 1,
      render: (s) => html`
        <span class="source-pill" title="Total Proxies">${s.proxies_total ?? 0}</span>
        <span class="source-pill success" title="Alive Proxies">🟢 ${s.proxies_alive ?? 0}</span>
        <span class="source-pill danger" title="Dead Proxies">🔴 ${s.proxies_dead ?? 0}</span>
      `,
    },
    {
      key: "updated",
      label: "Updated",
      max: 130,
      fold: 3,
      render: (s) => {
        const d = (s.updated_at || s.created_at || "—").replace("T", " ").slice(0, 16);
        return html`<span class="dg-dim">${d}</span>`;
      },
    },
  ];

  return html`
    <div
      class="proxy-sources-grid-wrap"
      @dragover=${(e: DragEvent) => {
        e.preventDefault();
        const row = (e.target as HTMLElement).closest(".dg-row") as HTMLElement | null;
        if (row) row.classList.add("drag-over");
      }}
      @dragleave=${(e: DragEvent) => {
        const row = (e.target as HTMLElement).closest(".dg-row") as HTMLElement | null;
        if (row) row.classList.remove("drag-over");
      }}
      @drop=${(e: DragEvent) => {
        e.preventDefault();
        const row = (e.target as HTMLElement).closest(".dg-row") as HTMLElement | null;
        if (row) {
          row.classList.remove("drag-over");
          const targetId = row.getAttribute("data-row-key");
          const draggedId = e.dataTransfer?.getData("text/plain");
          if (draggedId && targetId && draggedId !== targetId) {
            void reorderProxySources(draggedId, targetId);
          }
        }
      }}
    >
      ${renderDataGrid<ProxySource>({
        id: "proxy-sources-grid",
        className: "proxy-sources-table",
        columns,
        rows: sources,
        rowKey: (s) => s.id,
        rowClass: (s) => s.active ? "active" : "inactive",
        emptyMessage: "No proxy sources configured yet.",
        renameable: (s) => s.name,
        onRename: (s, value) => { void onRenameSource(s, value); },
        quickActions: (s) => [
          { action: "test", title: "Test source", icon: icons.flask() },
          { action: "edit", title: "Edit source", icon: icons.pencil() },
          ...(!s.is_builtin ? [{ action: "delete", title: t("common.delete") || "Delete", icon: icons.trash(), danger: true }] : []),
        ],
        menuItems: (s) => [
          { action: "test", label: "Test source", shortcut: "T" },
          { action: "edit", label: "Edit source", shortcut: "E" },
          { action: "rename", label: t("grid.menu.rename"), shortcut: "F2" },
          { action: "toggle", label: s.active ? "Disable" : "Enable" },
          { separator: true },
          { action: "copy_url", label: "Copy URL", shortcut: "C" },
          ...(!s.is_builtin ? [
            { separator: true },
            { action: "delete", label: t("common.delete") || "Delete", shortcut: "Supr", danger: true },
          ] : []),
        ],
        onAction: (s, action) => {
          switch (action) {
            case "test":
              return void testProxySource(s.id);
            case "edit":
              return void showEditProxySource(s.id);
            case "rename":
              return; // Handled inline by renderDataGrid
            case "toggle":
              return void toggleProxySourceActive(s.id);
            case "copy_url":
              void navigator.clipboard?.writeText(s.url);
              showToast(t("grid.toast.copied"));
              return;
            case "delete":
              return void deleteProxySource(s.id);
          }
        },
        bulkActions: [
          {
            key: "delete",
            label: t("common.delete") || "Delete",
            danger: true,
            run: (picked) => void onBulkDeleteSources(picked),
          },
        ],
        bulkCountLabel: (n) => t("grid.bulk.selected", { n }),
        __refresh: () => requestUpdate(),
      })}
    </div>
  `;
}

function renderProxySources(): TemplateResult {
  const sources = state.proxySources || [];
  return html`
    ${renderPageHeader()}
    ${renderProxySourcesList(sources)}
  `;
}

export async function mountProxySources(
  target?: HTMLElement | null | string,
): Promise<(() => void) | void> {
  const container = (target instanceof HTMLElement) ? target : document.getElementById("main");
  loadError = null;
  return createView(
    renderProxySources,
    async () => {
      await reloadProxySources();
    },
    (msg) => {
      loadError = msg;
    },
    container,
  );
}
