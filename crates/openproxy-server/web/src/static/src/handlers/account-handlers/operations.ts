import { html } from "lit-html";
import { state } from "../../state/index.js";
import { api } from "../../state/api.js";
import { showToast } from "../../components/toast.js";
import { copyToClipboard } from "../../lib/clipboard.js";
import { showApiError } from "../../lib/ui-utils.js";
import { showConfirm, showPrompt } from "../../lib/show-confirm.js";
import { mutateAndRefresh } from "../../lib/mutate.js";
import { renderOpModal } from "../../components/render-op-modal.js";
import { pollForDiscoveredModels } from "../../state/models-sync.js";
import { requestUpdate } from "../../state/reactive.js";

export function closeCreateAccount(): void {
  const root = document.getElementById("modal-root");
  if (!root) return;
  const last = root.lastElementChild;
  if (!last) return;
  const closeBtn = last.querySelector<HTMLButtonElement>(".close-btn");
  closeBtn?.click();
}

export async function deleteAccount(id: number): Promise<void> {
  if (
    !(await showConfirm({
      title: "Delete account",
      message: "Delete account #" + id + "?",
      danger: true,
      confirmLabel: "Delete",
    }))
  )
    return;
  await mutateAndRefresh({
    apiCall: async () => {
      await api("/accounts/" + id, { method: "DELETE" });
      state.accounts = (await api("/accounts")) as typeof state.accounts;
    },
  });
}

export function showUpdateAccountKey(id: number): void {
  const handle = renderOpModal({
    title: `Update API key for account #${id}`,
    body: html`
      <div class="field">
        <label for="account-key">New API key</label>
        <input id="account-key" name="api_key" type="password" placeholder="paste the new API key here">
      </div>
      <p><small>Leave empty and submit to <strong>clear</strong> the key (OAuth-only account).</small></p>
    `,
    actions: html`
      <button type="button" @click=${() => handle.close()}>Cancel</button>
      <button type="submit" class="primary"
        @click=${(e: Event) => { e.preventDefault(); void updateAccountKey(id, e, handle.close); }}>
        Save key
      </button>
    `,
  });
}

export function closeUpdateAccountKey(): void {
  const root = document.getElementById("modal-root");
  if (!root) return;
  const last = root.lastElementChild;
  if (!last) return;
  const closeBtn = last.querySelector<HTMLButtonElement>(".close-btn");
  closeBtn?.click();
}

export async function updateAccountKey(id: number, e: Event, close?: () => void): Promise<void> {
  const target = e.target;
  let apiKey: string | null = null;
  if (target instanceof HTMLFormElement) {
    const f = new FormData(target);
    apiKey = f.get("api_key")?.toString().trim() || null;
  } else {
    const root = document.getElementById("modal-root");
    const modal = root?.lastElementChild;
    const input = modal?.querySelector<HTMLInputElement>("#account-key");
    apiKey = input?.value.trim() || null;
  }
  try {
    await api("/accounts/" + id + "/api-key", {
      method: "PUT",
      body: JSON.stringify({ api_key: apiKey }),
    });
    const acc = (state.accounts || []).find((a) => a.id === id);
    const pid = acc?.provider_id;
    state.accounts = (await api("/accounts")) as typeof state.accounts;
    if (close) close();
    else closeUpdateAccountKey();
    if (pid) {
      pollForDiscoveredModels(pid);
    }
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}

export async function updateAccountLabel(id: number, currentLabel: string): Promise<void> {
  const newLabel = await showPrompt("Rename account label", "New label:", currentLabel || "");
  if (newLabel == null) return;
  const trimmed = newLabel.trim();
  if (trimmed === currentLabel) return;

  await mutateAndRefresh({
    apiCall: async () => {
      await api("/accounts/" + id + "/label", {
        method: "PATCH",
        body: JSON.stringify({ label: trimmed || null }),
      });
      state.accounts = (await api("/accounts")) as typeof state.accounts;
    },
    errorMessage: "Error updating label",
  });
}

export async function copyAccountApiKey(id: number): Promise<void> {
  try {
    const res = (await api("/accounts/" + id + "/api-key", { method: "GET" })) as {
      api_key?: string;
    };
    if (res && res.api_key) {
      try {
        await copyToClipboard(res.api_key);
        showToast("API key copied to clipboard", "success");
      } catch (_e: unknown) {
        showApiKeyDisplay(res.api_key);
      }
    } else {
      showToast("No API key returned", "error");
    }
  } catch (err: unknown) {
    showApiError(err, "Error copying API key");
  }
}

function showApiKeyDisplay(apiKey: string): void {
  const handle = renderOpModal({
    title: "API Key Created",
    body: html`
      <p style="margin: 0 0 var(--space-3); color: var(--color-text-muted); font-size: var(--fs-sm);">
        Copy this key. It will not be shown again.
      </p>
      <input
        type="text"
        readonly
        .value=${apiKey}
        style="width: 100%; padding: var(--space-2); font-family: var(--font-mono); font-size: var(--fs-sm); background: var(--color-surface-2); border: 1px solid var(--color-border); border-radius: var(--radius-sm); color: var(--color-text); user-select: all; cursor: text;"
      />
    `,
    actions: html`
      <button type="button" @click=${() => handle.close()}>Close</button>
      <button type="button" class="primary"
        @click=${async () => {
          try {
            await copyToClipboard(apiKey);
            showToast("API key copied to clipboard", "success");
            handle.close();
          } catch {
            showToast("Copy failed — select the text in the field above and press Ctrl+C", "warning");
          }
        }}
      >Copy</button>
    `,
  });
}

/**
 * Reorder the accounts of `providerId` so the resulting order becomes the
 * priority order (first = highest priority). Optimistic: the caller's array is
 * patched in place and the server response re-read on failure.
 */
export async function reorderAccounts(
  providerId: string,
  orderedIds: number[],
): Promise<void> {
  const scoped = (state.accounts || []).filter(
    (a) => a.provider_id === providerId,
  );
  const byId = new Map(scoped.map((a) => [a.id, a]));
  const reordered = orderedIds
    .map((id) => byId.get(id))
    .filter((a): a is (typeof scoped)[number] => a != null);
  if (reordered.length === 0) return;

  // Optimistic: move the provider's accounts into the requested order.
  state.accounts = [
    ...(state.accounts || []).filter((a) => a.provider_id !== providerId),
    ...reordered,
  ];
  requestUpdate();

  try {
    await api("/accounts/reorder", {
      method: "POST",
      body: JSON.stringify({ provider_id: providerId, account_ids: orderedIds }),
    });
  } catch (err: unknown) {
    showApiError(err, "Failed to reorder accounts");
  } finally {
    state.accounts = (await api("/accounts")) as typeof state.accounts;
    requestUpdate();
  }
}

/**
 * Move one account one slot up (`delta = -1`) or down (`delta = +1`) within its
 * provider, then persist the whole new order.
 */
export async function moveAccount(id: number, delta: number): Promise<void> {
  const account = (state.accounts || []).find((a) => a.id === id);
  if (!account) return;
  const scoped = (state.accounts || []).filter(
    (a) => a.provider_id === account.provider_id,
  );
  const idx = scoped.findIndex((a) => a.id === id);
  const newIdx = idx + delta;
  if (idx < 0 || newIdx < 0 || newIdx >= scoped.length) return;
  const [moved] = scoped.splice(idx, 1);
  if (!moved) return;
  scoped.splice(newIdx, 0, moved);
  await reorderAccounts(
    account.provider_id,
    scoped.map((a) => a.id),
  );
}
