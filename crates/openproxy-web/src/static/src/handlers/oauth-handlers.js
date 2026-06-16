// handlers/oauth-handlers.js — OAuth login flows (PKCE popup +
// manual paste + device code).
//
// Per spec §3 + §13.8 we do not attach to `window.*`. The
// `OAuthLogin` object is exported by name; the data-action shim
// in handlers/registry.js exposes each method under a flat name
// (`oauthStartPKCE`, `oauthStartDeviceCode`,
// `oauthSubmitManualCallback`).

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { escapeHtml, escapeAttr } from "../lib/escape.js";
import { showToast } from "../components/toast.js";
import { OAUTH_PKCE_PROVIDERS } from "../lib/constants.js";
import { rerenderCurrentView } from "../state/router.js";

export const OAuthLogin = {
  async startPKCE(provider) {
    try {
      const resp = await api(`/oauth/${provider}/authorize`);
      if (resp.error) throw new Error(resp.error);
      const isLocal = window.location.hostname === "localhost" || window.location.hostname === "127.0.0.1";
      if (isLocal) await this.pkcePopup(provider, resp);
      else this.showManualPasteForm(provider, resp);
    } catch (err) { showToast(`OAuth failed: ${err.message}`, "error"); }
  },
  async pkcePopup(provider, authData) {
    const popup = window.open(authData.authorization_url, "oauth popup", "width=600,height=700,top=100,left=100");
    const code = await new Promise((resolve, reject) => {
      const handler = (event) => {
        if (event.origin !== window.location.origin) return;
        if (event.data && event.data.type === "oauth_code") {
          window.removeEventListener("message", handler);
          popup.close();
          resolve(event.data.code);
        }
      };
      window.addEventListener("message", handler);
      setTimeout(() => { window.removeEventListener("message", handler); reject(new Error("OAuth timeout")); }, 300000);
    });
    const exchangeResp = await api(`/oauth/${provider}/exchange`, {
      method: "POST",
      body: JSON.stringify({ code, redirect_uri: authData.redirect_uri, code_verifier: authData.code_verifier }),
    });
    if (exchangeResp.error) throw new Error(exchangeResp.error);
    showToast(`Logged in with ${provider}`, "success");
    state.accounts = await api("/accounts");
    rerenderCurrentView();
  },
  showManualPasteForm(provider, authData) {
    const section = document.getElementById("oauth-manual-section");
    if (!section) return;
    section.style.display = "block";
    this._currentAuth = { provider, ...authData };
    const authUrlInput = document.getElementById("oauth-auth-url");
    if (authUrlInput) authUrlInput.value = authData.authorization_url;
    const callbackInput = document.getElementById("oauth-callback-input");
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
  async submitManualCallback() {
    const input = document.getElementById("oauth-callback-input").value.trim();
    const authData = this._currentAuth;
    if (!input) { showToast("Please paste the callback URL", "error"); return; }
    let code = null;
    let callbackState = null;
    try {
      const url = new URL(input);
      code = url.searchParams.get("code");
      callbackState = url.searchParams.get("state") || url.hash.replace(/^#/, "") || null;
    } catch {
      const [rawCode, rawState] = input.split("#", 2);
      code = rawCode || null;
      callbackState = rawState || null;
    }
    if (!code) { showToast("No authorization code found. Paste the full callback URL.", "error"); return; }
    const exchangeResp = await api(`/oauth/${authData.provider}/exchange`, {
      method: "POST",
      body: JSON.stringify({ code, redirect_uri: authData.redirect_uri, code_verifier: authData.code_verifier, state: callbackState || authData.state }),
    });
    if (exchangeResp.error) throw new Error(exchangeResp.error);
    showToast(`Logged in with ${authData.provider}`, "success");
    document.getElementById("oauth-manual-section").style.display = "none";
    state.accounts = await api("/accounts");
    rerenderCurrentView();
  },
  async startDeviceCode(provider) {
    try {
      const resp = await api(`/oauth/${provider}/device-code`, { method: "POST" });
      if (resp.error) throw new Error(resp.error);
      const deviceInfo = document.getElementById("oauth-device-info");
      if (deviceInfo) {
        deviceInfo.innerHTML = `
          <div class="device-code-flow">
            <p>To log in with ${escapeHtml(provider)}:</p>
            <ol>
              <li>Open <a href="${escapeAttr(resp.verification_uri)}" target="_blank" rel="noopener">${escapeHtml(resp.verification_uri)}</a></li>
              <li>Enter code: <strong class="copy-text">${escapeHtml(resp.user_code)}</strong></li>
            </ol>
            <p class="polling-status">Waiting for authorization...</p>
          </div>
        `;
        deviceInfo.style.display = "block";
      }
      const pollInterval = setInterval(async () => {
        try {
          const pollResp = await api(`/oauth/${provider}/device-poll`, { method: "POST", body: JSON.stringify({ device_code: resp.device_code }) });
          if (pollResp.status === "complete") {
            clearInterval(pollInterval);
            if (deviceInfo) deviceInfo.style.display = "none";
            showToast(`Logged in with ${provider}`, "success");
            state.accounts = await api("/accounts");
            rerenderCurrentView();
          } else if (pollResp.status === "expired") {
            clearInterval(pollInterval);
            if (deviceInfo) deviceInfo.style.display = "none";
            showToast("Device code expired", "error");
          }
        } catch (_) { /* swallow, keep polling */ }
      }, 5000);
    } catch (err) { showToast(`Device code failed: ${err.message}`, "error"); }
  },
};
