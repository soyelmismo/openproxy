import { html, type TemplateResult } from "lit-html";
import { copyToClipboard } from "../../lib/clipboard.js";
import type { Provider } from "../../lib/types/api.js";

export type OAuthTabStatus =
  | "idle"
  | "authorizing_popup"
  | "manual_paste"
  | "device_polling"
  | "submitting";

export interface OAuthViewProps {
  status: OAuthTabStatus;
  error: string | null;
  provider: Provider | undefined;
  providerId: string;
  hasPkce: boolean;
  hasDeviceCode: boolean;
  deviceInfo: { verificationUri: string; userCode: string; deviceCode: string } | null;
  manualAuthData: {
    authorizationUrl: string;
    redirectUri: string;
    codeVerifier: string;
    state?: string | null;
  } | null;
  manualCallbackUrl: string;
  currentAuthUrl: string;
  onStartPkce: () => void;
  onStartDeviceCode: () => void;
  onStartManualPaste: () => void;
  onSubmitManualCallback: () => void;
  onManualCallbackInput: (val: string) => void;
  onResetToIdle: () => void;
}

export function renderOAuthContent(p: OAuthViewProps): TemplateResult {
  return html`
    <div class="oauth-tab-content">
      ${p.error
        ? html`<div class="banner banner-error" style="margin-bottom: var(--space-3); font-size: var(--fs-xs);">${p.error}</div>`
        : html``}
      ${p.status === "idle"
        ? html`
            <div style="font-size: var(--fs-sm); color: var(--color-text-muted); margin-bottom: var(--space-3);">
              Authenticate with <strong>${p.provider?.name || p.providerId}</strong> using official OAuth.
            </div>
            <div class="oauth-buttons" style="display: flex; gap: var(--space-2); flex-wrap: wrap; margin-bottom: var(--space-3);">
              ${p.hasPkce
                ? html`<button type="button" class="primary" @click=${p.onStartPkce}>Log in with ${p.provider?.name || p.providerId}</button>`
                : html``}
              ${p.hasDeviceCode
                ? html`<button type="button" class=${p.hasPkce ? "btn-secondary" : "primary"} @click=${p.onStartDeviceCode}>Log in via Device Code</button>`
                : html``}
            </div>
            ${p.hasPkce
              ? html`
                  <div style="margin-top: var(--space-3); border-top: 1px solid var(--color-border); padding-top: var(--space-2);">
                    <button type="button" class="btn-secondary small" style="font-size: var(--fs-xs);" @click=${p.onStartManualPaste}>
                      Manual URL paste fallback
                    </button>
                  </div>
                `
              : html``}
          `
        : p.status === "authorizing_popup"
        ? html`
            <div class="oauth-status-card" style="padding: var(--space-3); background: var(--color-surface-2); border: var(--border-w) var(--border-style) var(--color-border); border-radius: var(--radius-sm); text-align: center;">
              <div style="font-weight: 600; font-size: var(--fs-sm); margin-bottom: var(--space-1);">Waiting for browser authorization...</div>
              <p style="font-size: var(--fs-xs); color: var(--color-text-muted); margin-bottom: var(--space-3);">
                Complete the login in the opened window. Your account will be added automatically.
              </p>
              <div style="display: flex; gap: var(--space-2); justify-content: center;">
                <button type="button" class="btn-secondary small" @click=${() => { window.open(p.currentAuthUrl, "oauth popup", "width=600,height=700,top=100,left=100"); }}>Reopen window</button>
                <button type="button" class="btn-secondary small" @click=${p.onStartManualPaste}>Switch to manual paste</button>
              </div>
            </div>
          `
        : p.status === "device_polling"
        ? html`
            <div id="oauth-device-info" class="device-code-flow" style="margin-top: 0; padding: var(--space-3); background: var(--color-surface-2); border: var(--border-w) var(--border-style) var(--color-border); border-radius: var(--radius-sm);">
              <p style="font-size: var(--fs-sm); margin-bottom: var(--space-2);">To log in with <strong>${p.provider?.name || p.providerId}</strong>:</p>
              <ol style="margin: var(--space-2) 0; padding-left: var(--space-4); font-size: var(--fs-sm);">
                <li style="margin-bottom: var(--space-2);">
                  Open <a href=${p.deviceInfo?.verificationUri || ""} target="_blank" rel="noopener" style="color: var(--color-primary); word-break: break-all;">${p.deviceInfo?.verificationUri || ""}</a>
                </li>
                <li style="margin-bottom: var(--space-2);">
                  Enter code: <strong class="copy-text" style="font-family: var(--font-mono); font-size: var(--fs-lg); color: var(--color-primary); margin-right: var(--space-2);">${p.deviceInfo?.userCode || ""}</strong>
                  <button type="button" class="btn-secondary small" style="padding: 2px var(--space-2); font-size: var(--fs-xs);" @click=${() => { if (p.deviceInfo?.userCode) void copyToClipboard(p.deviceInfo.userCode).catch(() => {}); }}>Copy</button>
                </li>
              </ol>
              <p class="polling-status" style="color: var(--color-text-muted); font-size: var(--fs-xs); font-style: italic; margin-top: var(--space-2);">
                Waiting for authorization...
              </p>
            </div>
          `
        : p.status === "manual_paste"
        ? html`
            <div id="oauth-manual-section" class="oauth-manual-card" style="margin-top: 0; padding: var(--space-3); background: var(--color-surface-2); border: var(--border-w) var(--border-style) var(--color-border); border-radius: var(--radius-sm);">
              <h4 style="margin: 0 0 var(--space-1); font-size: var(--fs-sm); font-weight: 600;">1. Authorize</h4>
              <p style="font-size: var(--fs-xs); color: var(--color-text-muted); margin-bottom: var(--space-2);">Open this URL in a new tab and complete the login:</p>
              <div class="oauth-manual-url" style="display: flex; gap: var(--space-2); margin-bottom: var(--space-3);">
                <input id="oauth-auth-url" type="text" readonly .value=${p.manualAuthData?.authorizationUrl || ""} style="flex: 1; font-family: var(--font-mono); font-size: var(--fs-xs); padding: var(--space-2); background: var(--color-surface); color: var(--color-text); border: var(--border-w) var(--border-style) var(--color-border);">
                <button type="button" class="btn-secondary small" @click=${() => { if (p.manualAuthData?.authorizationUrl) void copyToClipboard(p.manualAuthData.authorizationUrl).catch(() => {}); }}>Copy</button>
                <button type="button" class="btn-secondary small" @click=${() => { if (p.manualAuthData?.authorizationUrl) window.open(p.manualAuthData.authorizationUrl, "_blank"); }}>Open</button>
              </div>
              <h4 style="margin: 0 0 var(--space-1); font-size: var(--fs-sm); font-weight: 600;">2. Paste callback URL or code</h4>
              <p style="font-size: var(--fs-xs); color: var(--color-text-muted); margin-bottom: var(--space-2);">After the provider redirects, copy the full URL from your address bar and paste it here:</p>
              <div class="oauth-manual-input" style="display: flex; gap: var(--space-2);">
                <input
                  id="oauth-callback-input"
                  type="text"
                  placeholder="https://... or authorization code"
                  .value=${p.manualCallbackUrl}
                  @input=${(e: Event) => p.onManualCallbackInput((e.target as HTMLInputElement).value)}
                  @keydown=${(e: KeyboardEvent) => {
                    if (e.key === "Enter") {
                      e.preventDefault();
                      p.onSubmitManualCallback();
                    }
                  }}
                  style="flex: 1; font-family: var(--font-mono); font-size: var(--fs-xs); padding: var(--space-2); background: var(--color-surface); color: var(--color-text); border: var(--border-w) var(--border-style) var(--color-border);"
                >
                <button type="button" class="primary small" ?disabled=${!p.manualCallbackUrl.trim()} @click=${p.onSubmitManualCallback}>Submit</button>
              </div>
            </div>
          `
        : html`
            <div style="padding: var(--space-4); text-align: center; color: var(--color-text-muted); font-size: var(--fs-sm);">
              Connecting and completing OAuth...
            </div>
          `}
    </div>
  `;
}

export function renderOAuthFooterActions(p: OAuthViewProps, onCancel: () => void): TemplateResult {
  return html`
    <button type="button" @click=${onCancel}>Cancel</button>
    ${p.status === "manual_paste"
      ? html`
          <button type="button" class="btn-secondary" @click=${p.onResetToIdle}>Back</button>
          <button type="button" class="primary" ?disabled=${!p.manualCallbackUrl.trim()} @click=${p.onSubmitManualCallback}>Submit</button>
        `
      : p.status === "device_polling" || p.status === "authorizing_popup"
      ? html`<button type="button" class="btn-secondary" @click=${p.onResetToIdle}>Back</button>`
      : p.status === "submitting"
      ? html`<button type="button" class="primary" disabled>Submitting...</button>`
      : html``}
  `;
}
