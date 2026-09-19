import { html, render } from "lit-html";
import { state } from "../../state/index.js";
import { api } from "../../state/api.js";
import { requestUpdate } from "../../state/reactive.js";
import { showToast } from "../../components/toast.js";
import { showApiError, ensureModalRoot } from "../../lib/ui-utils.js";
import { pollForDiscoveredModels } from "../../state/models-sync.js";
import {
  summarizeApiKey,
  parseBulkApiKeys,
  type ParsedKeyEntry,
} from "./validation.js";
import { closeCreateAccount } from "./operations.js";
import {
  createOAuthTabHandler,
  type OAuthTabHandler,
} from "./oauth-tab.js";

export function showCreateAccount(providerId: string): void {
  const provider = (state.providers || []).find((p) => p.id === providerId);
  const oauthFlows = provider?.oauth_flows || [];
  const hasOAuth = Boolean(
    provider &&
      (provider.auth_type === "oauth" || oauthFlows.length > 0)
  );
  const hasPkce =
    oauthFlows.includes("pkce") ||
    oauthFlows.includes("auth_code") ||
    (!oauthFlows.length && provider?.auth_type === "oauth");
  const hasDeviceCode = oauthFlows.includes("device");

  let activeTab: "oauth" | "single" | "bulk" = hasOAuth ? "oauth" : "single";
  let singleLabel = "";
  let singleSecret = "";
  let singleScopes = "";
  let bulkText = "";
  let bulkScopes = "";
  let parsedKeys: ParsedKeyEntry[] = [];
  let isSubmitting = false;

  const root = ensureModalRoot();
  const wrapper = document.createElement("div");
  root.appendChild(wrapper);

  let closed = false;

  let oauthHandler: OAuthTabHandler | null = null;
  if (hasOAuth) {
    oauthHandler = createOAuthTabHandler({
      providerId,
      provider,
      hasPkce,
      hasDeviceCode,
      onSuccess: () => {
        close();
        requestUpdate();
        pollForDiscoveredModels(providerId);
      },
      requestRender: () => renderModal(),
      isClosed: () => closed,
    });
  }

  const close = (): void => {
    if (closed) return;
    closed = true;
    oauthHandler?.stop();
    window.removeEventListener("keydown", onKeydown, true);
    render(html``, wrapper);
    wrapper.remove();
  };

  const onKeydown = (e: KeyboardEvent): void => {
    if (e.key === "Escape") {
      e.stopPropagation();
      close();
    }
  };
  window.addEventListener("keydown", onKeydown, true);

  const saveCurrentInputs = (): void => {
    const labelInput = wrapper.querySelector<HTMLInputElement>("#account-label");
    if (labelInput) singleLabel = labelInput.value;
    const secretInput = wrapper.querySelector<HTMLInputElement>("#account-secret");
    if (secretInput) singleSecret = secretInput.value;
    const scopesInput = wrapper.querySelector<HTMLInputElement>("#account-scopes");
    if (scopesInput) singleScopes = scopesInput.value;
    const bulkInput = wrapper.querySelector<HTMLTextAreaElement>("#bulk-keys-input");
    if (bulkInput) bulkText = bulkInput.value;
    const bulkScopesInput = wrapper.querySelector<HTMLInputElement>("#bulk-account-scopes");
    if (bulkScopesInput) bulkScopes = bulkScopesInput.value;
    const callbackInput = wrapper.querySelector<HTMLInputElement>("#oauth-callback-input");
    if (callbackInput && oauthHandler) oauthHandler.manualCallbackUrl = callbackInput.value;
  };

  const submitSingle = async (): Promise<void> => {
    const rawSecret = singleSecret.trim();
    if (!rawSecret) {
      showToast("API Key / Secret is required", "error");
      renderModal();
      return;
    }
    const rawLabel = singleLabel.trim();
    const label = rawLabel || summarizeApiKey(rawSecret);
    const scopes = singleScopes.split(",").map((s) => s.trim()).filter(Boolean);
    const body = {
      provider_id: providerId,
      label,
      api_key: rawSecret,
      scopes,
    };
    try {
      await api("/accounts", { method: "POST", body: JSON.stringify(body) });
      state.accounts = (await api("/accounts")) as typeof state.accounts;
      close();
      requestUpdate();
      showToast("Account created. Discovering models...", "info");
      pollForDiscoveredModels(providerId);
    } catch (err: unknown) {
      showApiError(err, "Error");
      renderModal();
    }
  };

  const submitBulk = async (): Promise<void> => {
    if (parsedKeys.length === 0) {
      showToast("No API keys detected", "error");
      renderModal();
      return;
    }
    const scopes = bulkScopes.split(",").map((s) => s.trim()).filter(Boolean);
    const payload = {
      provider_id: providerId,
      items: parsedKeys.map((item) => ({
        api_key: item.key,
        label: item.label || null,
      })),
      scopes,
    };
    try {
      const res = (await api("/accounts/bulk", {
        method: "POST",
        body: JSON.stringify(payload),
      })) as { created: number; ids: number[] };
      state.accounts = (await api("/accounts")) as typeof state.accounts;
      close();
      requestUpdate();
      if (res.created === 0) {
        showToast("No new accounts added (all keys already exist).", "info");
      } else {
        const skipped = parsedKeys.length - res.created;
        const msg =
          skipped > 0
            ? `Created ${res.created} accounts (${skipped} duplicate${skipped > 1 ? "s" : ""} skipped). Discovering models...`
            : `Created ${res.created} accounts. Discovering models...`;
        showToast(msg, "info");
        pollForDiscoveredModels(providerId);
      }
    } catch (err: unknown) {
      showApiError(err, "Error");
      renderModal();
    }
  };

  const renderModal = (): void => {
    const title = `New account for ${providerId}`;
    const body = html`
      <div class="filter-tabs" style="margin-bottom: var(--space-3);">
        ${hasOAuth
          ? html`
              <button
                type="button"
                class="filter-tab ${activeTab === "oauth" ? "active" : ""}"
                @click=${() => {
                  saveCurrentInputs();
                  activeTab = "oauth";
                  renderModal();
                }}
              >
                OAuth
              </button>
            `
          : html``}
        <button
          type="button"
          class="filter-tab ${activeTab === "single" ? "active" : ""}"
          @click=${() => {
            saveCurrentInputs();
            if (activeTab === "oauth") oauthHandler?.stop();
            activeTab = "single";
            renderModal();
          }}
        >
          Single key
        </button>
        <button
          type="button"
          class="filter-tab ${activeTab === "bulk" ? "active" : ""}"
          @click=${() => {
            saveCurrentInputs();
            if (activeTab === "oauth") oauthHandler?.stop();
            activeTab = "bulk";
            renderModal();
          }}
        >
          Bulk keys
        </button>
      </div>

      ${activeTab === "oauth" && oauthHandler
        ? oauthHandler.renderTab()
        : activeTab === "single"
        ? html`
            <div class="field">
              <label for="account-label">Label (optional)</label>
              <input
                id="account-label"
                name="label"
                type="text"
                placeholder="auto-generated from key if empty"
                .value=${singleLabel}
                @input=${(e: Event) => {
                  singleLabel = (e.target as HTMLInputElement).value;
                }}
              />
            </div>
            <div class="field">
              <label for="account-secret">API Key / Secret</label>
              <input
                id="account-secret"
                name="secret"
                type="password"
                placeholder="paste the API key here"
                .value=${singleSecret}
                @input=${(e: Event) => {
                  singleSecret = (e.target as HTMLInputElement).value;
                }}
              />
            </div>
            <div class="field">
              <label for="account-scopes">Scopes (comma separated)</label>
              <input
                id="account-scopes"
                name="scopes"
                type="text"
                placeholder="chat,manage"
                .value=${singleScopes}
                @input=${(e: Event) => {
                  singleScopes = (e.target as HTMLInputElement).value;
                }}
              />
            </div>
          `
        : html`
            <div class="field">
              <label for="bulk-keys-input">
                API Keys
                <small style="color: var(--color-text-muted); font-weight: normal; margin-left: var(--space-2);">
                  1 per line or paste dirty text (env, json, list, comments)
                </small>
              </label>
              <textarea
                id="bulk-keys-input"
                name="bulk_keys"
                rows="6"
                placeholder="Paste keys here:&#10;sk-ant-api03-...&#10;Primary: sk-proj-...&#10;export OPENAI_API_KEY=&quot;sk-...&quot;"
                .value=${bulkText}
                @input=${(e: Event) => {
                  bulkText = (e.target as HTMLTextAreaElement).value;
                  parsedKeys = parseBulkApiKeys(bulkText);
                  renderModal();
                }}
                style="font-family: var(--font-mono); font-size: var(--fs-xs); width: 100%; resize: vertical;"
              ></textarea>
            </div>

            <div class="bulk-keys-preview" style="margin-top: var(--space-2); margin-bottom: var(--space-3);">
              ${parsedKeys.length === 0
                ? html`<div style="font-size: var(--fs-xs); color: var(--color-text-muted); font-style: italic;">
                    No API keys detected yet
                  </div>`
                : html`
                    <div style="font-size: var(--fs-xs); font-weight: 600; margin-bottom: var(--space-1); color: var(--color-primary);">
                      ${parsedKeys.length} ${parsedKeys.length === 1 ? "key" : "keys"} detected:
                    </div>
                    <div style="max-height: 120px; overflow-y: auto; background: var(--color-surface-2); border: 1px solid var(--color-border); border-radius: var(--radius-sm); padding: var(--space-2);">
                      ${parsedKeys.map(
                        (item, idx) => html`
                          <div style="display: flex; justify-content: space-between; font-size: var(--fs-xs); font-family: var(--font-mono); padding: 2px 0; border-bottom: 1px dashed var(--color-border);">
                            <span>#${idx + 1} ${item.label ? html`<strong>${item.label}</strong>: ` : html``}${summarizeApiKey(item.key)}</span>
                            <span style="color: var(--color-text-muted);">${item.key.length} chars</span>
                          </div>
                        `
                      )}
                    </div>
                  `}
            </div>

            <div class="field">
              <label for="bulk-account-scopes">Scopes (comma separated, applied to all)</label>
              <input
                id="bulk-account-scopes"
                name="scopes"
                type="text"
                placeholder="chat,manage"
                .value=${bulkScopes}
                @input=${(e: Event) => {
                  bulkScopes = (e.target as HTMLInputElement).value;
                }}
              />
            </div>
          `}
    `;

    const actions =
      activeTab === "oauth" && oauthHandler
        ? oauthHandler.renderFooterActions(() => close())
        : html`
            <button type="button" @click=${() => close()}>Cancel</button>
            <button
              type="button"
              class="primary"
              ?disabled=${isSubmitting || (activeTab === "bulk" && parsedKeys.length === 0)}
              @click=${async (e: Event) => {
                e.preventDefault();
                saveCurrentInputs();
                if (isSubmitting) return;
                isSubmitting = true;
                renderModal();
                try {
                  if (activeTab === "single") {
                    await submitSingle();
                  } else {
                    await submitBulk();
                  }
                } finally {
                  isSubmitting = false;
                }
              }}
            >
              ${activeTab === "bulk" && parsedKeys.length > 0
                ? `Create (${parsedKeys.length} accounts)`
                : "Create"}
            </button>
          `;

    const template = html`
      <div
        class="modal-bg"
        role="dialog"
        aria-modal="true"
        aria-labelledby="create-account-modal-title"
        @click=${(e: Event) => {
          if (e.target === e.currentTarget) close();
        }}
      >
        <div class="modal" @click=${(e: Event) => e.stopPropagation()}>
          <div class="modal-header">
            <h2 id="create-account-modal-title">${title}</h2>
            <button
              type="button"
              class="close-btn"
              aria-label="Close"
              @click=${() => close()}
            >&times;</button>
          </div>
          <div class="modal-body">${body}</div>
          <div class="modal-footer">${actions}</div>
        </div>
      </div>
    `;

    render(template, wrapper);
  };

  renderModal();
}

export async function createAccount(providerId: string, e?: Event, close?: () => void): Promise<void> {
  let rawLabel = "";
  let rawSecret = "";
  let scopes: string[] = [];

  const target = e?.target;
  if (target instanceof HTMLFormElement) {
    const f = new FormData(target);
    rawLabel = (f.get("label") || "").toString().trim();
    rawSecret = (f.get("secret") || "").toString().trim();
    scopes = (f.get("scopes") || "").toString().split(",").map((s) => s.trim()).filter(Boolean);
  } else {
    const root = document.getElementById("modal-root");
    const modal = root?.lastElementChild;
    const labelInput = modal?.querySelector<HTMLInputElement>("#account-label");
    const secretInput = modal?.querySelector<HTMLInputElement>("#account-secret");
    const scopesInput = modal?.querySelector<HTMLInputElement>("#account-scopes");
    if (labelInput) rawLabel = labelInput.value.trim();
    if (secretInput) rawSecret = secretInput.value.trim();
    if (scopesInput) scopes = scopesInput.value.split(",").map((s) => s.trim()).filter(Boolean);
  }

  const label = rawLabel || (rawSecret ? summarizeApiKey(rawSecret) : null);
  const body = {
    provider_id: providerId,
    label,
    api_key: rawSecret || null,
    scopes,
  };
  try {
    await api("/accounts", { method: "POST", body: JSON.stringify(body) });
    state.accounts = (await api("/accounts")) as typeof state.accounts;
    if (close) close();
    else closeCreateAccount();
    requestUpdate();
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}
