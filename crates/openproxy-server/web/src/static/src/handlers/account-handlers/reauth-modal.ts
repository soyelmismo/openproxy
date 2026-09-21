import { html, render } from "lit-html";
import { state } from "../../state/index.js";
import { api } from "../../state/api.js";
import { requestUpdate } from "../../state/reactive.js";
import { showToast } from "../../components/toast.js";
import { ensureModalRoot } from "../../lib/ui-utils.js";
import { pollForDiscoveredModels } from "../../state/models-sync.js";
import {
  createOAuthTabHandler,
  type OAuthTabHandler,
} from "./oauth-tab.js";

export function showReauthAccount(accountId: number): void {
  const account = (state.accounts || []).find((a) => a.id === accountId);
  if (!account) {
    showToast(`Account #${accountId} not found`, "error");
    return;
  }

  const providerId = account.provider_id;
  const provider = (state.providers || []).find((p) => p.id === providerId);
  const oauthFlows = provider?.oauth_flows || [];
  const hasDeviceCode = oauthFlows.includes("device");
  const hasPkce =
    oauthFlows.includes("pkce") ||
    oauthFlows.includes("auth_code") ||
    (!oauthFlows.length && (provider?.auth_type === "oauth" || account.auth_type === "oauth")) ||
    !hasDeviceCode;

  const root = ensureModalRoot();
  const wrapper = document.createElement("div");
  root.appendChild(wrapper);

  let closed = false;

  const close = (): void => {
    if (closed) return;
    closed = true;
    oauthHandler.stop();
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

  const oauthHandler: OAuthTabHandler = createOAuthTabHandler({
    providerId,
    provider,
    accountId,
    hasPkce,
    hasDeviceCode,
    onSuccess: () => {
      close();
      requestUpdate();
      pollForDiscoveredModels(providerId);
      if (provider?.metadata?.supports_quota) {
        void api(`/accounts/${accountId}/refresh-quota`, { method: "POST" })
          .then(async () => {
            state.accounts = (await api("/accounts")) as typeof state.accounts;
            requestUpdate();
          })
          .catch((e: unknown) => {
            console.warn("Auto refresh quota after reauth:", e);
          });
      }
    },
    requestRender: () => renderModal(),
    isClosed: () => closed,
  });

  const renderModal = (): void => {
    const accountName = account.label || account.email || `#${accountId}`;
    const title = `Re-authenticate ${provider?.name || providerId} (${accountName})`;
    const body = html`
      <div style="margin-bottom: var(--space-3); font-size: var(--fs-sm); color: var(--color-text-muted);">
        Re-authenticating will renew OAuth credentials for this account without changing its ID, routing rules, or model assignments.
      </div>
      ${oauthHandler.renderTab()}
    `;
    const actions = oauthHandler.renderFooterActions(() => close());

    const template = html`
      <div
        class="modal-bg"
        role="dialog"
        aria-modal="true"
        aria-labelledby="reauth-modal-title"
        @click=${(e: Event) => {
          if (e.target === e.currentTarget) close();
        }}
      >
        <div class="modal" style="max-width: 520px;">
          <div class="modal-header">
            <h3 id="reauth-modal-title">${title}</h3>
            <button
              class="close-btn"
              type="button"
              aria-label="Close"
              @click=${() => close()}
            >
              ✕
            </button>
          </div>
          <div class="modal-body">${body}</div>
          <div class="modal-actions">${actions}</div>
        </div>
      </div>
    `;
    render(template, wrapper);
  };

  renderModal();
}
