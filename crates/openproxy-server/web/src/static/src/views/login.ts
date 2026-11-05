// The only screen reachable without a token: the auth gate in
// `state/router.ts` redirects every other route here when
// `isLoggedIn()` is false. It collects a manage-scope API key, persists
// it via `state/auth.ts::setToken`, validates it with a single
// authenticated GET, and on success navigates to `#/`, which the gate
// then lets through.
//
// Uses the same lit-html + mountView pattern as views/keys.ts and
// views/config.ts; the dashboard has no `lit` dependency.

import { html, type TemplateResult } from "lit-html";
import { setToken, clearToken, setProfile, type UserSessionProfile } from "../state/auth.js";
import { api } from "../state/api.js";
import { mountView, requestUpdate } from "../state/reactive.js";
import { t } from "../i18n/index.js";


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


/** Validate the key against `/admin/api/notifications/unread-count`:
 *  a tiny `{count: N}` payload behind the same `admin_auth_middleware`
 *  as every other `/admin/api/*` route, so a 2xx proves the key is
 *  valid AND has `manage` scope.
 *
 *  On success: keep the optimistically-set token and navigate to `#/`.
 *  On failure: clear the token so no stale value stays in localStorage,
 *  then show the i18n error. */
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
  // Set the token first so the validation call's Authorization header
  // carries it. Cleared below if validation fails.
  setToken(trimmed);
  try {
    // Validate credentials and introspect scope permissions
    const me = (await api("/auth/me")) as UserSessionProfile;
    setProfile(me);

    // If the key has full manage scope: navigate to home.
    // If the key has restricted scope (analytics only): navigate directly to #/analytics.
    if (me.is_admin) {
      location.hash = "#/";
    } else {
      location.hash = "#/analytics";
    }
  } catch (err: unknown) {
    clearToken();
    // Log the raw error for operators; the user sees the i18n string.
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
  // Clear the error on edit: a stale "invalid key" under a freshly
  // edited input is confusing.
  if (errorMsg !== null) {
    errorMsg = null;
  }
}


function renderLogin(): TemplateResult {
  const subtitle: string = t("login.subtitle");
  const helpText: string = t("login.help_text");
  const apiKeyLabel: string = t("login.api_key_label");
  const submitLabel: string = t("login.submit");
  // Always render the slot (with `nothing` when there is no error) so
  // the layout does not shift when one appears.
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


export async function mountLogin(): Promise<(() => void) | void> {
  const main = document.getElementById("main");
  if (!main) return;
  // Reset on mount so a logout/login round trip shows no stale error
  // or half-filled input.
  keyValue = "";
  errorMsg = null;
  submitting = false;
  const cleanup = mountView(main, renderLogin);
  return cleanup;
}
