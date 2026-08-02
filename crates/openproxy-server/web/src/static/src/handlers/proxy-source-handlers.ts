// handlers/proxy-source-handlers.ts — proxy sources CRUD & test handlers.

import { html, render } from 'lit-html';
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { requestUpdate } from "../state/reactive.js";
import { showToast } from "../components/toast.js";
import { ensureModalRoot, showApiError } from "../lib/ui-utils.js";
import type { ProxySource } from "../lib/types/api.js";

export async function reloadProxySources(): Promise<void> {
  try {
    const sources = (await api("/proxy-sources")) as ProxySource[];
    state.proxySources = sources;
    requestUpdate();
  } catch (err: unknown) {
    console.error("reloadProxySources failed", err);
  }
}

export function showAddProxySource(): void {
  const wrapper = document.createElement("div");
  ensureModalRoot().appendChild(wrapper);
  render(
    html`
      <div
        class="modal-bg"
        id="add-proxy-source-modal"
        @click=${(e: Event) => {
          if (e.target === e.currentTarget) wrapper.remove();
        }}
      >
        <div class="modal">
          <div class="modal-header">
            <h2>Add Proxy Source</h2>
            <button
              type="button"
              class="close-btn"
              @click=${() => wrapper.remove()}
              aria-label="Close"
            >
              &times;
            </button>
          </div>
          <form
            @submit=${(e: Event) => {
              e.preventDefault();
              void createProxySource(e, wrapper);
            }}
          >
            <div class="modal-body">
              <div class="field">
                <label for="source-name">Source Name</label>
                <input
                  id="source-name"
                  name="name"
                  type="text"
                  placeholder="My Proxy List"
                  required
                />
              </div>
              <div class="field">
                <label for="source-url">URL</label>
                <input
                  id="source-url"
                  name="url"
                  type="url"
                  placeholder="https://example.com/proxies.txt"
                  required
                />
              </div>
              <div class="field">
                <label for="source-priority">Priority</label>
                <input
                  id="source-priority"
                  name="priority"
                  type="number"
                  value="0"
                />
              </div>
              <div class="field checkbox-field">
                <label>
                  <input type="checkbox" name="active" checked />
                  Active
                </label>
              </div>
            </div>
            <div class="modal-footer">
              <button type="button" @click=${() => wrapper.remove()}>
                Cancel
              </button>
              <button type="submit" class="primary">Add Source</button>
            </div>
          </form>
        </div>
      </div>
    `,
    wrapper
  );
}

export async function createProxySource(e: Event, wrapper: HTMLElement): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const f = new FormData(target);
  const name = (f.get("name") || "").toString().trim();
  const url = (f.get("url") || "").toString().trim();
  const priority = Number(f.get("priority") || 0);
  const active = f.get("active") === "on";

  try {
    await api("/proxy-sources", {
      method: "POST",
      body: JSON.stringify({ name, url, priority, active }),
    });
    showToast(`Proxy source '${name}' added`, "success");
    wrapper.remove();
    await reloadProxySources();
  } catch (err: unknown) {
    showApiError(err, "Failed to create proxy source");
  }
}

export function showEditProxySource(id: string): void {
  const src = state.proxySources.find((s) => s.id === id);
  if (!src) return;

  const wrapper = document.createElement("div");
  ensureModalRoot().appendChild(wrapper);
  render(
    html`
      <div
        class="modal-bg"
        id="edit-proxy-source-modal"
        @click=${(e: Event) => {
          if (e.target === e.currentTarget) wrapper.remove();
        }}
      >
        <div class="modal">
          <div class="modal-header">
            <h2>Edit Proxy Source</h2>
            <button
              type="button"
              class="close-btn"
              @click=${() => wrapper.remove()}
              aria-label="Close"
            >
              &times;
            </button>
          </div>
          <form
            @submit=${(e: Event) => {
              e.preventDefault();
              void updateProxySource(id, e, wrapper);
            }}
          >
            <div class="modal-body">
              <div class="field">
                <label for="edit-source-name">Source Name</label>
                <input
                  id="edit-source-name"
                  name="name"
                  type="text"
                  .value=${src.name}
                  required
                />
              </div>
              <div class="field">
                <label for="edit-source-url">URL</label>
                <input
                  id="edit-source-url"
                  name="url"
                  type="url"
                  .value=${src.url}
                  required
                />
              </div>
              <div class="field">
                <label for="edit-source-priority">Priority</label>
                <input
                  id="edit-source-priority"
                  name="priority"
                  type="number"
                  .value=${String(src.priority)}
                />
              </div>
              <div class="field checkbox-field">
                <label>
                  <input type="checkbox" name="active" .checked=${src.active} />
                  Active
                </label>
              </div>
            </div>
            <div class="modal-footer">
              <button type="button" @click=${() => wrapper.remove()}>
                Cancel
              </button>
              <button type="submit" class="primary">Save Changes</button>
            </div>
          </form>
        </div>
      </div>
    `,
    wrapper
  );
}

export async function updateProxySource(
  id: string,
  e: Event,
  wrapper: HTMLElement
): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const f = new FormData(target);
  const name = (f.get("name") || "").toString().trim();
  const url = (f.get("url") || "").toString().trim();
  const priority = Number(f.get("priority") || 0);
  const active = f.get("active") === "on";

  try {
    await api(`/proxy-sources/${id}`, {
      method: "PUT",
      body: JSON.stringify({ name, url, priority, active }),
    });
    showToast(`Proxy source '${name}' updated`, "success");
    wrapper.remove();
    await reloadProxySources();
  } catch (err: unknown) {
    showApiError(err, "Failed to update proxy source");
  }
}

export async function deleteProxySource(id: string): Promise<void> {
  const src = state.proxySources.find((s) => s.id === id);
  const name = src ? src.name : id;
  if (!confirm(`Are you sure you want to delete source '${name}'?`)) return;
  try {
    await api(`/proxy-sources/${id}`, { method: "DELETE" });
    showToast(`Proxy source '${name}' deleted`, "success");
    await reloadProxySources();
  } catch (err: unknown) {
    showApiError(err, "Failed to delete proxy source");
  }
}

export async function toggleProxySourceActive(id: string, e: Event): Promise<void> {
  const checkbox = e.target as HTMLInputElement;
  const newValue = checkbox.checked;
  const src = state.proxySources.find((s) => s.id === id);
  if (!src) return;

  try {
    const payload = {
      name: src.name,
      url: src.url,
      priority: src.priority,
      active: newValue,
    };
    await api(`/proxy-sources/${id}`, {
      method: "PUT",
      body: JSON.stringify(payload),
    });
    src.active = newValue;
    requestUpdate();
    showToast(`Proxy source ${newValue ? 'enabled' : 'disabled'}`, "success");
  } catch (err: unknown) {
    checkbox.checked = !newValue;
    showApiError(err, "Failed to toggle proxy source");
  }
}

export async function reorderProxySources(draggedId: string, targetId: string): Promise<void> {
  if (draggedId === targetId) return;
  const sources = [...state.proxySources];
  const fromIdx = sources.findIndex((s) => s.id === draggedId);
  const toIdx = sources.findIndex((s) => s.id === targetId);
  if (fromIdx < 0 || toIdx < 0) return;

  const moved = sources.splice(fromIdx, 1)[0];
  if (!moved) return;
  sources.splice(toIdx, 0, moved);

  // Optimistic update
  state.proxySources = sources;
  requestUpdate();

  const ids = sources.map((s) => s.id);
  try {
    await api("/proxy-sources/reorder", {
      method: "POST",
      body: JSON.stringify({ ids }),
    });
    await reloadProxySources();
  } catch (err: unknown) {
    showApiError(err, "Failed to reorder proxy sources");
    await reloadProxySources();
  }
}

export async function testProxySource(id: string): Promise<void> {
  const src = state.proxySources.find((s) => s.id === id);
  const name = src ? src.name : id;
  try {
    const res = await api(`/proxy-sources/${id}/test`, { method: "POST" });
    const count = (res as any).proxy_count || 0;
    showToast(`Source '${name}' tested: found ${count} proxies`, "success");
  } catch (err: unknown) {
    showApiError(err, `Failed to test proxy source '${name}'`);
  }
}
