// handlers/oauth-handlers.ts — OAuth login flows (PKCE popup +
// manual paste + device code).
//
// Per spec §3 + §13.8 we do not attach to `window.*`. The
// `OAuthLogin` object is exported by name; the data-action shim
// in handlers/registry.ts exposes each method under a flat name
// (`oauthStartPKCE`, `oauthStartDeviceCode`,
// `oauthSubmitManualCallback`).

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { escapeHtml, escapeAttr } from "../lib/escape.js";
import { requestUpdate } from "../state/reactive.js";
import { showToast } from "../components/toast.js";

interface AuthData {
  authorization_url: string;
  redirect_uri: string;
  code_verifier: string;
  state?: string | null;
  [k: string]: unknown;
}

interface OAuthLoginShape {
  _currentAuth: AuthData | null;
  startPKCE(provider: string): Promise<void>;
  pkcePopup(provider: string, authData: AuthData): Promise<void>;
  showManualPasteForm(provider: string, authData: AuthData): void;
  submitManualCallback(): Promise<void>;
  startDeviceCode(provider: string): Promise<void>;
}

export const OAuthLogin: OAuthLoginShape = {
  _currentAuth: null,
  async startPKCE(provider: string): Promise<void> {
    try {
      const resp = (await api(`/oauth/${provider}/authorize`)) as { error?: string; authorization_url?: string } & AuthData;
      if (resp.error) throw new Error(resp.error);
      const isLocal = window.location.hostname === "localhost" || window.location.hostname === "127.0.0.1";
      if (isLocal) await this.pkcePopup(provider, resp as AuthData);
      else this.showManualPasteForm(provider, resp as AuthData);
    } catch (err: unknown) {
      const msg = err instanceof Error ? err.message : String(err);
      showToast(`OAuth failed: ${msg}`, "error");
    }
  },
  async pkcePopup(provider: string, authData: AuthData): Promise<void> {
    const popup = window.open(authData.authorization_url, "oauth popup", "width=600,height=700,top=100,left=100");
    const code: string = await new Promise((resolve, reject) => {
      const handler = (event: MessageEvent): void => {
        if (event.origin !== window.location.origin) return;
        const data = event.data as { type?: string; code?: string } | null;
        if (data && data.type === "oauth_code" && typeof data.code === "string") {
          window.removeEventListener("message", handler);
          popup?.close();
          resolve(data.code);
        }
      };
      window.addEventListener("message", handler);
      setTimeout(() => { window.removeEventListener("message", handler); reject(new Error("OAuth timeout")); }, 300000);
    });
    const exchangeResp = await api(`/oauth/${provider}/exchange`, {
      method: "POST",
      body: JSON.stringify({ code, redirect_uri: authData.redirect_uri, code_verifier: authData.code_verifier }),
    }) as { error?: string };
    if (exchangeResp.error) throw new Error(exchangeResp.error);
    showToast(`Logged in with ${provider}`, "success");
    state.accounts = await api("/accounts") as typeof state.accounts;
    requestUpdate();
  },
  showManualPasteForm(provider: string, authData: AuthData): void {
    const section = document.getElementById("oauth-manual-section");
    if (!section) return;
    section.style.display = "block";
    this._currentAuth = { ...authData, provider } as AuthData;
    const authUrlInput = document.getElementById("oauth-auth-url") as HTMLInputElement | null;
    if (authUrlInput) authUrlInput.value = authData.authorization_url;
    const callbackInput = document.getElementById("oauth-callback-input") as HTMLInputElement | null;
    if (callbackInput) callbackInput.value = "";
    const step1 = document.getElementById("oauth-manual-step1");
    const step2 = document.getElementById("oauth-manual-step2");
    if (step1) step1.style.display = "block";
    if (step2) step2.style.display = "none";
    window.open(authData.authorization_url, "_blank");
    setTimeout(() => {
      if (step1) step1.style.display = "none";
      if (step2) step2.style.display = "block";
    }, 2000);
  },
  async submitManualCallback(): Promise<void> {
    const inputEl = document.getElementById("oauth-callback-input") as HTMLInputElement | null;
    const input = (inputEl ? inputEl.value : "").trim();
    const authData = this._currentAuth;
    if (!authData) { showToast("No OAuth flow in progress", "error"); return; }
    if (!input) { showToast("Please paste the callback URL", "error"); return; }
    let code: string | null = null;
    let callbackState: string | null = null;
    try {
      const url = new URL(input);
      code = url.searchParams.get("code");
      callbackState = url.searchParams.get("state") || url.hash.replace(/^#/, "") || null;
    } catch {
      const parts = input.split("#", 2);
      code = parts[0] || null;
      callbackState = parts[1] || null;
    }
    if (!code) { showToast("No authorization code found. Paste the full callback URL.", "error"); return; }
    const exchangeResp = await api(`/oauth/${authData["provider"]}/exchange`, {
      method: "POST",
      body: JSON.stringify({ code, redirect_uri: authData.redirect_uri, code_verifier: authData.code_verifier, state: callbackState || authData.state }),
    }) as { error?: string };
    if (exchangeResp.error) throw new Error(exchangeResp.error);
    showToast(`Logged in with ${authData["provider"]}`, "success");
    const section = document.getElementById("oauth-manual-section");
    if (section) section.style.display = "none";
    state.accounts = await api("/accounts") as typeof state.accounts;
    requestUpdate();
  },
  async startDeviceCode(provider: string): Promise<void> {
    try {
      const resp = (await api(`/oauth/${provider}/device-code`, { method: "POST" })) as {
        error?: string;
        device_code?: string;
        verification_uri?: string;
        user_code?: string;
      };
      if (resp.error) throw new Error(resp.error);
      const deviceInfo = document.getElementById("oauth-device-info");
      if (deviceInfo) {
        deviceInfo.innerHTML = `
          <div class="device-code-flow">
            <p>To log in with ${escapeHtml(provider)}:</p>
            <ol>
              <li>Open <a href="${escapeAttr(resp.verification_uri || "")}" target="_blank" rel="noopener">${escapeHtml(resp.verification_uri || "")}</a></li>
              <li>Enter code: <strong class="copy-text">${escapeHtml(resp.user_code || "")}</strong></li>
            </ol>
            <p class="polling-status">Waiting for authorization...</p>
          </div>
        `;
        deviceInfo.style.display = "block";
      }
      const pollInterval = setInterval(async () => {
        try {
          const pollResp = (await api(`/oauth/${provider}/device-poll`, { method: "POST", body: JSON.stringify({ device_code: resp.device_code }) })) as { status?: string };
          if (pollResp.status === "complete") {
            clearInterval(pollInterval);
            if (deviceInfo) deviceInfo.style.display = "none";
            showToast(`Logged in with ${provider}`, "success");
            state.accounts = await api("/accounts") as typeof state.accounts;
            requestUpdate();
          } else if (pollResp.status === "expired") {
            clearInterval(pollInterval);
            if (deviceInfo) deviceInfo.style.display = "none";
            showToast("Device code expired", "error");
          }
        } catch (_e: unknown) { /* swallow, keep polling */ }
      }, 5000);
    } catch (err: unknown) {
      const msg = err instanceof Error ? err.message : String(err);
      showToast(`Device code failed: ${msg}`, "error");
    }
  },
};
