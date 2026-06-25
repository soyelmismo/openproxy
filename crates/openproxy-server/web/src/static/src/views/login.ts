// views/login.ts — minimal login view for the dashboard.
//
// DASHBOARD-FIX (Bug 2 / Step 2d): the dashboard previously had no
// auth flow at all — every API call returned 401 and the WebSocket
// upgrade was rejected, which is what surfaced as "live-store
// initial rehydrate failed", "fetchRecordingState failed", and the
// "ws://.../admin/ws no pudo establecer una conexión" Firefox error.
//
// This view is the only screen accessible without a token (the
// router's auth gate in `state/router.ts` redirects every other
// route here when `isLoggedIn()` is false). It collects the user's
// manage-scope API key, persists it via `state/auth.ts::setToken`,
// validates it with a single authenticated GET, and on success
// navigates to the home route (`#/`) which the auth gate now lets
// them through.
//
// The view uses the same lit-html + mountView pattern as the other
// views (see views/keys.ts, views/config.ts). The task brief sketched
// it as a LitElement, but the rest of the dashboard is plain lit-html
// (no `lit` dependency in package.json), so we follow the existing
// pattern for consistency.

import { html, type TemplateResult } from "lit-html";
import { setToken, clearToken } from "../state/auth.js";
import { api } from "../state/api.js";
import { mountView, requestUpdate } from "../state/reactive.js";
import { t } from "../i18n/index.js";

// ---- Module-local view state ----

/** The current value of the API key input. Preserved across
 *  re-renders so a failed validation doesn't blank the field —
 *  the user can see what they typed and edit it. */
let keyValue: string = "";

/** Error message shown below the submit button. Null = no error.
 *  Set when the validation call rejects; cleared on the next
 *  submit attempt. */
let errorMsg: string | null = null;

/** True while the validation call is in flight. Disables the
 *  submit button + input so the user can't double-submit. */
let submitting: boolean = false;

// ---- Submit handler ----

/** Validate the entered key by calling a lightweight authenticated
 *  endpoint. `/admin/api/notifications/unread-count` returns
 *  `{count: N}` (a tiny payload) and is gated by the same
 *  `admin_auth_middleware` as every other `/admin/api/*` route, so
 *  a 2xx response proves the key is valid AND has `manage` scope.
 *
 *  On success: leave the token in place (it was set optimistically
 *  before the call) and navigate to `#/`. The router's auth gate
 *  sees `isLoggedIn()` is true and lets the home view mount.
 *
 *  On failure: clear the token (so a stale/invalid value isn't
 *  left in localStorage) and show the i18n error string. The user
 *  stays on the login page and can re-enter the key. */
async function onSubmit(e: Event): Promise<void> {
  e.preventDefault();
  if (submitting) return;
  const trimmed: string = keyValue.trim();
  if (!trimmed) {
    errorMsg = t("login.error_invalid");
    requestUpdate();
    return;
  }
  submitting = true;
  errorMsg = null;
  requestUpdate();
  // Optimistically set the token so the validation call's
  // `Authorization: Bearer <token>` header (attached by
  // `state/api.ts::api()`) carries it. If validation fails we
  // clear it below.
  setToken(trimmed);
  try {
    // We don't care about the response body — only that the call
    // succeeds (2xx). A 401 throws `Error("401: ...")` which we
    // catch below. A 5xx is "server is down" rather than "bad
    // key" but we surface the same generic error — the user can't
    // act on the distinction from the login form.
    await api("/notifications/unread-count");
    // Success: navigate to home. Setting `location.hash` fires
    // `hashchange`, which calls `navigate()`, which sees the user
    // is now logged in and mounts the home view via the auth
    // gate. We don't call `navigate()` directly because the
    // hashchange event is the canonical trigger and ensures the
    // URL bar updates.
    location.hash = "#/";
  } catch (err: unknown) {
    clearToken();
    // Log the underlying error for operator debugging — the user-
    // facing message is the generic i18n string.
    console.warn("[login] token validation failed:", err);
    errorMsg = t("login.error_invalid");
  } finally {
    submitting = false;
    requestUpdate();
  }
}

function onInput(e: Event): void {
  const target = e.target as HTMLInputElement;
  keyValue = target.value;
  // Clear any prior error as soon as the user edits the field —
  // a stale "invalid key" message below a freshly-edited input
  // is confusing.
  if (errorMsg !== null) {
    errorMsg = null;
  }
}

// ---- Template ----

function renderLogin(): TemplateResult {
  const subtitle: string = t("login.subtitle");
  const helpText: string = t("login.help_text");
  const apiKeyLabel: string = t("login.api_key_label");
  const submitLabel: string = t("login.submit");
  // The error block is rendered as a lit-html template (or
  // `nothing` when there's no error) so the layout doesn't shift
  // when an error appears — the slot is always reserved.
  const errorBlock: TemplateResult = errorMsg !== null
    ? html`<div class="banner banner-error login-error" role="alert">${errorMsg}</div>`
    : html`<div class="login-error-slot"></div>`;
  return html`
    <div class="login-page">
      <div class="page-header"><h2>${t("login.title")}</h2></div>
      <section class="card login-card">
        <p class="muted login-subtitle">${subtitle}</p>
        <form class="login-form" @submit=${onSubmit}>
          <label class="config-field login-field">
            <span class="config-label">${apiKeyLabel}</span>
            <input
              type="password"
              name="api_key"
              .value=${keyValue}
              ?disabled=${submitting}
              autocomplete="current-password"
              spellcheck="false"
              autocapitalize="off"
              autocorrect="off"
              aria-label=${apiKeyLabel}
              @input=${onInput}
              required
            />
          </label>
          ${errorBlock}
          <button
            type="submit"
            class="primary login-submit"
            ?disabled=${submitting}
          >${submitting ? "…" : submitLabel}</button>
        </form>
        <p class="muted login-help">${helpText}</p>
      </section>
    </div>
  `;
}

// ---- Mount ----

export async function mountLogin(): Promise<(() => void) | void> {
  const main = document.getElementById("main");
  if (!main) return;
  // Reset view-local state on each mount so a user who logs out
  // and back in doesn't see a stale error or a half-filled input.
  keyValue = "";
  errorMsg = null;
  submitting = false;
  const cleanup = mountView(main, renderLogin);
  return cleanup;
}
