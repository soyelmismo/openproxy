// handlers/provider-handlers.js — provider-level handlers.
//
// Per spec §3 + §13.8 we do not attach to `window.*`. Every
// function here is exported by name and registered in
// handlers/registry.js so the central data-action shim can find
// it.
//
// Naming convention: functions that take an `e` event as a
// trailing argument (submit handlers) receive the DOM event last
// in the shim dispatch. Functions that take a single `id`-style
// argument receive it as `arg1`. Functions that need a button
// reference (e.g. refreshProvider) take the event element as a
// trailing argument so they can disable + relabel the button
// while in flight.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { escapeHtml, escapeAttr, extractApiErrorMessage } from "../lib/escape.js";
import { appendModal } from "../lib/dom.js";
import { showToast } from "../components/toast.js";
import { rerenderCurrentView, navigate } from "../state/router.js";
import { QUOTA_CAPABLE_PROVIDERS } from "../lib/constants.js";

// Briefly paint a button a colour to confirm a click landed.
// 1.5s is enough for the user to see the result before the label
// reverts. Mirrors the old `flashButton()` helper in app.js.
function flashButton(btn, text, color) {
  if (!btn) return;
  btn.textContent = text;
  btn.style.background = color;
  setTimeout(() => { btn.style.background = ""; }, 1500);
}

// POST /v1/admin/providers/:id/refresh — re-discover the model
// list for one provider. The button is disabled and relabeled
// "Refreshing..." while in flight. The optional `e` parameter
// lets the data-action shim pass the triggering element so the
// UI feedback is scoped to the button the user clicked.
export async function refreshProvider(providerId, e) {
  const btn = e && e.target ? e.target : null;
  const original = btn ? btn.textContent : null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = "Refreshing...";
  }
  try {
    const result = await api(
      "/providers/" + encodeURIComponent(providerId) + "/refresh",
      { method: "POST" },
    );
    const n = (result && typeof result.models_refreshed === "number")
      ? result.models_refreshed
      : 0;
    const newIds = Array.isArray(result && result.new_model_ids)
      ? result.new_model_ids
      : [];
    // Compose a toast that surfaces the headline count plus a
    // short list of any newly-discovered model_ids. When the
    // refresh found nothing new (the common case for a steady-
    // state provider) we fall back to the previous "Refreshed N
    // models" wording so the UI doesn't suddenly get chatty.
    const summary = n === 0
      ? `Nothing to refresh for ${providerId}.`
      : `Refreshed ${n} models for ${providerId}.`;
    const newSuffix = newIds.length === 0
      ? ""
      : newIds.length <= 3
        ? ` New: ${newIds.join(", ")}.`
        : ` New: ${newIds.slice(0, 3).join(", ")} (+${newIds.length - 3} more).`;
    showToast(summary + newSuffix, "success");
    // Force a refetch instead of relying on the polling interval —
    // the user explicitly asked for fresh data.
    state.providers = await api("/providers");
    state.models = await api("/models");
    rerenderCurrentView();
  } catch (err) {
    showToast("Error: " + err.message, "error");
  } finally {
    if (btn) {
      btn.disabled = false;
      btn.textContent = original;
    }
  }
}

// Walk every provider and POST to its /refresh endpoint. Per-
// provider failures are logged but don't abort the loop — a
// single misbehaving upstream shouldn't block the rest.
export async function refreshAllProviders() {
  try {
    const providers = await api("/providers");
    for (const p of providers) {
      try {
        await api("/providers/" + encodeURIComponent(p.id) + "/refresh", { method: "POST" });
      } catch (err) {
        console.error("Failed to refresh", p.id, err);
      }
    }
    state.providers = await api("/providers");
    state.models = await api("/models");
    rerenderCurrentView();
  } catch (err) {
    showToast("Error: " + err.message, "error");
  }
}

// ===== Create provider =====

export function showCreateProvider() {
  const html = `
    <div class="modal-bg" id="create-provider-modal" data-action="closeCreateProvider" data-arg1="self">
      <div class="modal">
        <div class="modal-header">
          <h2>New provider</h2>
          <button type="button" class="close-btn" data-action="closeCreateProvider" aria-label="Close">&times;</button>
        </div>
        <form data-action="createProvider">
          <div class="modal-body">
            <div class="field">
              <label for="provider-id">ID</label>
              <input id="provider-id" name="id" type="text" required placeholder="openrouter">
            </div>
            <div class="field">
              <label for="provider-name">Name</label>
              <input id="provider-name" name="name" type="text" required placeholder="OpenRouter">
            </div>
            <div class="field">
              <label for="provider-base-url">Base URL</label>
              <input id="provider-base-url" name="base_url" type="text" required placeholder="https://openrouter.ai/api/v1">
            </div>
            <div class="field">
              <label for="provider-auth">Auth</label>
              <select id="provider-auth" name="auth_type">
                <option value="bearer">bearer</option>
                <option value="x-api-key">x-api-key</option>
              </select>
            </div>
            <div class="field">
              <label for="provider-format">Format</label>
              <select id="provider-format" name="format">
                <option value="openai">openai</option>
                <option value="anthropic">anthropic</option>
                <option value="mixed">mixed</option>
              </select>
            </div>
          </div>
          <div class="modal-footer">
            <button type="button" data-action="closeCreateProvider">Cancel</button>
            <button type="submit" class="primary">Create</button>
          </div>
        </form>
      </div>
    </div>
  `;
  // Mount on <body> via appendModal (not #main) so the 3s background
  // poll doesn't destroy the form mid-edit. See lib/dom.js appendModal.
  appendModal(html);
}

export function closeCreateProvider() {
  const m = document.getElementById("create-provider-modal");
  if (m) m.remove();
}

export async function createProvider(e) {
  // The submit shim already called preventDefault(); we don't have
  // to do it again. The form is `e.target`.
  const f = new FormData(e.target);
  try {
    await api("/providers", {
      method: "POST",
      body: JSON.stringify(Object.fromEntries(f)),
    });
    state.providers = await api("/providers");
    closeCreateProvider();
    navigate();
  } catch (err) {
    showToast("Error: " + err.message, "error");
  }
}

// ===== Delete provider =====

// Soft-confirm path: kept because some callers (and old URLs)
// still hit `window.deleteProvider`. The dashboard's "Delete"
// button uses `confirmDeleteProvider` (two-step: type the id).
export async function deleteProvider(id) {
  if (!confirm(`Delete provider ${id}? This will cascade-delete its accounts and models.`)) return;
  try {
    await api("/providers/" + encodeURIComponent(id), { method: "DELETE" });
    state.providers = state.providers.filter((p) => p.id !== id);
    state.models = state.models.filter((m) => m.provider_id !== id);
    state.accounts = state.accounts.filter((a) => a.provider_id !== id);
    navigate();
  } catch (err) {
    showToast("Error: " + err.message, "error");
  }
}

// Two-step confirmation: type the provider id verbatim, then a
// final "Really?" prompt. The typed step is enough friction to
// catch most misclicks. The second step is a plain confirm for
// the final go-ahead.
export async function confirmDeleteProvider(providerId) {
  const typed = prompt(`Type the provider ID to confirm deletion: ${providerId}`);
  if (typed !== providerId) {
    if (typed != null) {
      alert(`Provider id "${typed}" does not match. Nothing was deleted.`);
    }
    return;
  }
  if (!confirm(`Really delete ${providerId}? This cascades to all its accounts and models.`)) return;
  try {
    await api("/providers/" + encodeURIComponent(providerId), { method: "DELETE" });
    state.providers = state.providers.filter((p) => p.id !== providerId);
    state.models = state.models.filter((m) => m.provider_id !== providerId);
    state.accounts = state.accounts.filter((a) => a.provider_id !== providerId);
    // The user just deleted the provider they're looking at: send
    // them back to the providers grid.
    location.hash = "#/providers";
  } catch (err) {
    // The server returns `{"error": {"code", "message"}}` for a
    // 4xx. The most common rejection on this path is a built-in
    // (which the UI normally hides via the "🔒 Delete (built-in)"
    // button, but the server is the source of truth and might
    // reject for any other validation reason). Show the message
    // verbatim so the operator sees "cannot be deleted. Use
    // POST .../active to deactivate it" instead of a generic
    // "Error: 400: ...".
    const friendly = extractApiErrorMessage(err) || err.message;
    alert("Cannot delete: " + friendly);
  }
}

// ===== Toggle active / rename =====

// Deactivating a provider is the soft, reversible alternative to
// deleting it: the row stays in the DB (accounts and models
// preserved), and reactivation brings everything back. The
// button just flips `active` via the dedicated endpoint.
//
// Reactivation skips the confirm — going from "off" to "on" is
// safe and the user clearly intended it by clicking "Activate".
export async function toggleProviderActive(providerId, newActive) {
  if (!newActive) {
    const ok = confirm(
      `Deactivate provider "${providerId}"?\n\n` +
      `Its accounts and models will be preserved, but it won't be ` +
      `usable in combos until you reactivate it.`
    );
    if (!ok) return;
  }
  try {
    await api("/providers/" + encodeURIComponent(providerId) + "/active", {
      method: "POST",
      body: JSON.stringify({ active: newActive }),
    });
    // Refetch providers so the card / detail reflects the new state.
    state.providers = await api("/providers");
    navigate();
  } catch (err) {
    showToast("Error: " + err.message, "error");
  }
}

// The `name` field is a *display* label — the `id` is the slug
// used in URLs and FKs, so the rename only touches `name`. PATCH
// `/v1/admin/providers/:id` already exists in the backend, this
// is just the UX.
export async function renameProviderPrompt(providerId, currentName) {
  const newName = prompt(`Rename provider "${providerId}":`, currentName);
  if (newName == null) return; // cancel
  const trimmed = newName.trim();
  if (trimmed === "") {
    alert("Name cannot be empty");
    return;
  }
  if (trimmed === currentName) return; // no-op

  // Names are not unique in the schema (only ids are), so a name
  // collision is allowed — we just warn so the operator can notice.
  const collision = state.providers.find(
    (p) => p.id !== providerId && p.name === trimmed,
  );
  if (collision) {
    const ok = confirm(
      `A provider with this name already exists (${collision.id}). ` +
      `Use this name anyway?`
    );
    if (!ok) return;
  }

  try {
    await api("/providers/" + encodeURIComponent(providerId), {
      method: "PATCH",
      body: JSON.stringify({ name: trimmed }),
    });
    state.providers = await api("/providers");
    navigate();
  } catch (err) {
    showToast("Error: " + err.message, "error");
  }
}

// ===== Bulk toggle (enable/disable all non-custom models) =====

export async function bulkToggleModels(providerId, active) {
  const models = (state.models || []).filter((m) => m.provider_id === providerId);
  const customCount = models.filter((m) => m.custom).length;
  const toToggleCount = models.filter((m) => !m.custom && m.active !== active).length;
  if (toToggleCount === 0) {
    showToast("Nothing to toggle.", "info");
    return;
  }
  const msg = active
    ? `Enable ${toToggleCount} non-custom models? (${customCount} custom models will not be touched)`
    : `Disable ${toToggleCount} non-custom models? (${customCount} custom models will not be touched)`;
  if (!confirm(msg)) return;
  try {
    await api("/models/bulk-toggle", {
      method: "POST",
      body: JSON.stringify({ provider_id: providerId, active }),
    });
    state.models = await api("/models");
    rerenderCurrentView();
  } catch (err) {
    showToast("Error: " + err.message, "error");
  }
}

// ===== Account health / quota =====

// POST /v1/admin/accounts/:id/health — force-set the health
// flag. The select's value is read off the change event, not from
// data-arg, so the shim passes the event and we read `e.target.value`.
export async function setHealth(id, e) {
  const health = e && e.target ? e.target.value : null;
  if (!health) return;
  try {
    await api("/accounts/" + id + "/health", {
      method: "POST",
      body: JSON.stringify({ health }),
    });
    // Update the cached account so the background poll's diff is
    // a no-op and the next render is correct.
    const a = (state.accounts || []).find((x) => x.id === id);
    if (a) a.health_status = health;
  } catch (err) {
    showToast("Error: " + err.message, "error");
    rerenderCurrentView();
  }
}

// POST /v1/admin/accounts/:id/refresh-quota — fetch a fresh
// quota. The button gets a coloured flash so the click feels
// acknowledged even when the request takes a few seconds.
export async function refreshAccountQuota(accountId, e) {
  const btn = e && e.target ? e.target : null;
  const oldText = btn ? btn.textContent : null;
  if (btn) {
    btn.disabled = true;
    btn.textContent = "...";
  }
  try {
    const result = await api("/accounts/" + accountId + "/refresh-quota", { method: "POST" });
    if (result && result.supported === false) {
      if (btn) flashButton(btn, "n/a", "#9399b2");
    } else if (result && result.error) {
      if (btn) flashButton(btn, "✗ err", "#f38ba8");
    } else {
      if (btn) flashButton(btn, "✓", "#a6e3a1");
    }
    state.accounts = await api("/accounts");
    rerenderCurrentView();
  } catch (err) {
    if (btn) flashButton(btn, "✗", "#f38ba8");
    setTimeout(() => showToast("Error: " + err.message, "error"), 100);
  } finally {
    if (btn) {
      setTimeout(() => { btn.disabled = false; btn.textContent = oldText; }, 1500);
    }
  }
}

// Walk every quota-capable account of a provider and refresh
// each. The "not supported" alert only appears when there's
// actually nothing to refresh.
export async function refreshAllQuotas(providerId) {
  const accounts = (state.accounts || []).filter((a) => a.provider_id === providerId);
  const supported = accounts.filter((a) => QUOTA_CAPABLE_PROVIDERS.includes(a.provider_id));
  if (supported.length === 0) {
    showToast("No accounts with quota support (only " + QUOTA_CAPABLE_PROVIDERS.join(", ") + ").", "info");
    return;
  }
  if (!confirm(`Refresh quota for ${supported.length} accounts?`)) return;
  for (const a of supported) {
    try {
      await api("/accounts/" + a.id + "/refresh-quota", { method: "POST" });
    } catch (err) {
      console.error("Failed to refresh quota for", a.id, err);
    }
  }
  state.accounts = await api("/accounts");
  rerenderCurrentView();
  showToast("Quotas refreshed.", "success");
}
