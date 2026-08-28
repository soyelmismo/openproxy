// views/proxy-sources.ts — Proxy sources management view.

import { html, type TemplateResult } from 'lit-html';
import { state } from "../state/index.js";
import { createView } from "../lib/view-utils.js";
import { icons } from "../lib/icons.js";
import { reloadProxySources, moveProxySource, reorderProxySources } from "../handlers/proxy-source-handlers.js";
import type { ProxySource } from "../lib/types/api.js";

let loadError: string | null = null;

let sourceTouchDragState: {
  draggedId: string;
  rowEl: HTMLElement;
  currentOverEl: HTMLElement | null;
} | null = null;

function onSourceTouchStart(sourceId: string, e: TouchEvent): void {
  const handle = e.currentTarget as HTMLElement;
  const row = handle.closest(".proxy-source-card-row") as HTMLElement | null;
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
  const overRow = el?.closest(".proxy-source-card-row") as HTMLElement | null;

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
    const dropTargetId = currentOverEl.getAttribute("data-drag-id");
    if (dropTargetId && dropTargetId !== draggedId) {
      await reorderProxySources(draggedId, dropTargetId);
    }
  }

  sourceTouchDragState = null;
}

function renderPageHeader(): TemplateResult {
  return html`
    <div class="page-header">
      <div>
        <h2>Proxy Sources</h2>
        <p class="subtitle">
          Manage dynamic URLs and endpoints to automatically scrape proxies from.
        </p>
      </div>
      <div class="actions">
        <button
          type="button"
          class="primary"
          data-action="showAddProxySource"
        >
          ${icons.plus()} Add Source
        </button>
      </div>
    </div>
  `;
}

function renderProxySourcesList(sources: ProxySource[]): TemplateResult {
  if (loadError) {
    return html`<div class="card error-card">${loadError}</div>`;
  }

  if (sources.length === 0) {
    return html`
      <div class="card empty-state">
        <p>No proxy sources configured yet.</p>
        <button
          type="button"
          class="primary"
          data-action="showAddProxySource"
          style="margin-top: 1rem;"
        >
          ${icons.plus()} Add your first proxy source
        </button>
      </div>
    `;
  }

  return html`
    <div class="card table-card">
      <div class="table-wrap">
        <table class="proxy-sources-table responsive-card-table">
        <thead>
          <tr>
            <th></th>
            <th>Name</th>
            <th>URL</th>
            <th>Priority</th>
            <th>Active</th>
            <th>Stats</th>
            <th>Updated</th>
            <th class="actions-col">Actions</th>
          </tr>
        </thead>
        <tbody>
          ${sources.map((s) => {
            const formattedDate = (s.updated_at || s.created_at || "").replace("T", " ").slice(0, 16);
            return html`
              <tr
                class="proxy-source-card-row ${s.active ? 'active' : 'inactive'}"
                draggable="true"
                data-drag-id=${s.id}
                @dragstart=${(e: DragEvent) => { e.dataTransfer?.setData("text/plain", s.id); (e.target as HTMLElement).classList.add("dragging"); }}
                @dragend=${(e: DragEvent) => { (e.target as HTMLElement).classList.remove("dragging"); }}
                @dragover=${(e: DragEvent) => { e.preventDefault(); (e.currentTarget as HTMLElement).classList.add("drag-over"); }}
                @dragleave=${(e: DragEvent) => { (e.currentTarget as HTMLElement).classList.remove("drag-over"); }}
                @drop=${(e: DragEvent) => { e.preventDefault(); (e.currentTarget as HTMLElement).classList.remove("drag-over"); const draggedId = e.dataTransfer?.getData("text/plain"); if (draggedId) { void reorderProxySources(draggedId, s.id); } }}
              >
                <!-- Desktop Table Cells -->
                <td class="drag-handle col-source-drag" style="white-space:nowrap;"
                    title="Drag to reorder"
                    @touchstart=${(e: TouchEvent) => onSourceTouchStart(s.id, e)}
                    @touchmove=${(e: TouchEvent) => onSourceTouchMove(e)}
                    @touchend=${() => void onSourceTouchEnd()}
                    @touchcancel=${() => void onSourceTouchEnd()}>
                  <span style="cursor:grab;">${icons.dragHandle()}</span>
                  <button type="button" class="small" style="padding:1px 4px;margin-left:2px;" title="Move up" @click=${() => void moveProxySource(s.id, -1)}>${icons.caretUp()}</button>
                  <button type="button" class="small" style="padding:1px 4px;" title="Move down" @click=${() => void moveProxySource(s.id, 1)}>${icons.caretDown()}</button>
                </td>
                <td class="col-source-name" data-label="Name"><strong>${s.name}</strong></td>
                <td class="col-source-url" data-label="URL">
                  <a
                    href=${s.url}
                    target="_blank"
                    rel="noopener noreferrer"
                    class="code-link"
                    style="word-break: break-all;"
                  >
                    ${s.url}
                  </a>
                </td>
                <td class="col-source-priority" data-label="Priority"><span class="badge">${s.priority}</span></td>
                <td class="col-source-active" data-label="Active">
                  <label class="switch" title="Toggle source active state">
                    <input
                      type="checkbox"
                      .checked=${s.active}
                      data-action="toggleProxySourceActive"
                      data-arg1=${s.id}
                    />
                    <span class="slider round"></span>
                  </label>
                </td>
                <td class="col-source-stats" data-label="Stats">
                  <span class="badge" title="Total Proxies">${s.proxies_total}</span>
                  <span class="badge success" title="Alive Proxies">${s.proxies_alive}</span>
                  <span class="badge danger" title="Dead Proxies">${s.proxies_dead}</span>
                </td>
                <td class="col-source-updated" data-label="Updated"><small class="muted">${s.updated_at || s.created_at || "-"}</small></td>
                <td class="actions-cell col-source-actions" data-label="Actions">
                  <div class="source-actions-wrap">
                    <button
                      type="button"
                      class="secondary btn-sm"
                      data-action="testProxySource"
                      data-arg1=${s.id}
                      title="Test source by fetching URL and counting proxies"
                    >
                      ${icons.flask()} Test
                    </button>
                    <button
                      type="button"
                      class="secondary btn-sm"
                      data-action="showEditProxySource"
                      data-arg1=${s.id}
                    >
                      ${icons.pencil()} Edit
                    </button>
                    ${!s.is_builtin
                      ? html`
                          <button
                            type="button"
                            class="danger btn-sm"
                            data-action="deleteProxySource"
                            data-arg1=${s.id}
                          >
                            ${icons.trash()} Delete
                          </button>
                        `
                      : html``}
                  </div>
                </td>

                <!-- Mobile Card Structure -->
                <td class="mobile-proxy-source-card-cell">
                  <!-- Línea 1: Drag + Nombre + Prioridad + Botones Reordenación + Switch -->
                  <div class="s-card-line-1">
                    <div class="s-card-title-group">
                      <span class="s-card-drag-icon" title="Drag to reorder"
                            @touchstart=${(e: TouchEvent) => onSourceTouchStart(s.id, e)}
                            @touchmove=${(e: TouchEvent) => onSourceTouchMove(e)}
                            @touchend=${() => void onSourceTouchEnd()}
                            @touchcancel=${() => void onSourceTouchEnd()}>⋮⋮</span>
                      <span class="s-card-name" title="${s.name}">${s.name}</span>
                      <span class="s-card-priority" title="Priority: ${s.priority}">P:${s.priority}</span>
                    </div>
                    <div class="s-card-controls">
                      <button type="button" class="s-card-reorder-btn" title="Move up" @click=${() => void moveProxySource(s.id, -1)}>▲</button>
                      <button type="button" class="s-card-reorder-btn" title="Move down" @click=${() => void moveProxySource(s.id, 1)}>▼</button>
                      <label class="switch" title="Toggle source active state">
                        <input 
                          type="checkbox" 
                          .checked=${s.active} 
                          data-action="toggleProxySourceActive"
                          data-arg1=${s.id}
                        />
                        <span class="slider round"></span>
                      </label>
                    </div>
                  </div>

                  <!-- Línea 2: URL de Descarga en 1 Sola Línea -->
                  <div class="s-card-line-2">
                    <a 
                      class="s-card-url" 
                      href="${s.url}" 
                      target="_blank" 
                      rel="noopener noreferrer"
                      title="Click to open or copy URL"
                    >
                      ${s.url}
                    </a>
                  </div>

                  <!-- Línea 3: Stats Badges y Fecha de Actualización -->
                  <div class="s-card-line-3">
                    <div class="s-card-stats-badges">
                      <span class="s-card-stat-pill">Total: ${s.proxies_total ?? 0}</span>
                      <span class="s-card-stat-pill success">🟢 ${s.proxies_alive ?? 0}</span>
                      <span class="s-card-stat-pill danger">🔴 ${s.proxies_dead ?? 0}</span>
                    </div>
                    <span class="s-card-updated">${formattedDate}</span>
                  </div>

                  <!-- Línea 4: Acciones -->
                  <div class="s-card-line-4">
                    <button type="button" class="secondary btn-sm" data-action="testProxySource" data-arg1=${s.id}>
                      🧪 Test
                    </button>
                    <button type="button" class="secondary btn-sm" data-action="showEditProxySource" data-arg1=${s.id}>
                      ✎ Edit
                    </button>
                    <button type="button" class="danger btn-sm" data-action="deleteProxySource" data-arg1=${s.id} title="Delete source">
                      ✕
                    </button>
                  </div>
                </td>
              </tr>
            `;
          })}
        </tbody>
      </table>
      </div>
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

export async function mountProxySources(): Promise<(() => void) | void> {
  loadError = null;
  return createView(
    renderProxySources,
    async () => {
      await reloadProxySources();
    },
    (msg) => {
      loadError = msg;
    }
  );
}
