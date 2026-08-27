// views/proxy-sources.ts — Proxy sources management view.

import { html, type TemplateResult } from 'lit-html';
import { state } from "../state/index.js";
import { createView } from "../lib/view-utils.js";
import { reloadProxySources, moveProxySource } from "../handlers/proxy-source-handlers.js";
import type { ProxySource } from "../lib/types/api.js";

let loadError: string | null = null;

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
          + Add Source
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
          Add your first proxy source
        </button>
      </div>
    `;
  }

  return html`
    <div class="card table-card">
      <div class="table-wrap">
        <table class="data-table">
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
          ${sources.map(
            (s) => html`
              <tr
                draggable="true"
                data-drag-id=${s.id}
                @dragstart=${(e: DragEvent) => { e.dataTransfer?.setData("text/plain", s.id); (e.target as HTMLElement).classList.add("dragging"); }}
                @dragend=${(e: DragEvent) => { (e.target as HTMLElement).classList.remove("dragging"); }}
                @dragover=${(e: DragEvent) => { e.preventDefault(); (e.currentTarget as HTMLElement).classList.add("drag-over"); }}
                @dragleave=${(e: DragEvent) => { (e.currentTarget as HTMLElement).classList.remove("drag-over"); }}
                @drop=${(e: DragEvent) => { e.preventDefault(); (e.currentTarget as HTMLElement).classList.remove("drag-over"); const draggedId = e.dataTransfer?.getData("text/plain"); if (draggedId) { import("../handlers/proxy-source-handlers.js").then(m => m.reorderProxySources(draggedId, s.id)); } }}
              >
                <td class="drag-handle" style="white-space:nowrap;">
                  <span title="Drag to reorder" style="cursor:grab;">⠿</span>
                  <button type="button" class="small" style="padding:1px 4px;margin-left:2px;" title="Move up" @click=${() => void moveProxySource(s.id, -1)}>▲</button>
                  <button type="button" class="small" style="padding:1px 4px;" title="Move down" @click=${() => void moveProxySource(s.id, 1)}>▼</button>
                </td>
                <td><strong>${s.name}</strong></td>
                <td>
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
                <td><span class="badge">${s.priority}</span></td>
                <td>
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
                <td>
                  <span class="badge" title="Total Proxies">${s.proxies_total}</span>
                  <span class="badge success" title="Alive Proxies">${s.proxies_alive}</span>
                  <span class="badge danger" title="Dead Proxies">${s.proxies_dead}</span>
                </td>
                <td>${s.updated_at || s.created_at || "-"}</td>
                <td class="actions-cell">
                  <button
                    type="button"
                    class="secondary btn-sm"
                    data-action="testProxySource"
                    data-arg1=${s.id}
                    title="Test source by fetching URL and counting proxies"
                  >
                    Test Source
                  </button>
                  <button
                    type="button"
                    class="secondary btn-sm"
                    data-action="showEditProxySource"
                    data-arg1=${s.id}
                  >
                    Edit
                  </button>
                  ${!s.is_builtin
                    ? html`
                        <button
                          type="button"
                          class="danger btn-sm"
                          data-action="deleteProxySource"
                          data-arg1=${s.id}
                        >
                          Delete
                        </button>
                      `
                    : html`<span class="badge" style="margin-left: 0.5rem">Built-in</span>`}
                </td>
              </tr>
            `
          )}
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
