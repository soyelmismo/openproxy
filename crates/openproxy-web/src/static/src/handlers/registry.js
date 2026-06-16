// handlers/registry.js — central map from `data-action` attribute
// values to the real handler functions. app.js reads this and
// installs a single document-level listener that dispatches clicks
// / changes / submits based on data-action / data-arg-* attrs.
//
// Why: spec §3 + §13.8 forbid window.foo = fn global bridges and
// inline onclick="window.foo()" handlers. A single shim keeps the
// HTML tidy (data-action="X" data-arg1="...") without re-wiring
// every modal.
//
// Conventions:
//   * `action` is the function name in this map.
//   * `arg1`, `arg2`, ... are positional string args collected
//     from data-arg-* attributes (in numeric order).
//   * The trailing `e` is always the DOM event, so handlers can
//     call e.preventDefault() and reach e.target.
//   * Functions that take an `e` event first (forms: createKey,
//     updateKey, updateModel, addTarget, createAccount, createCombo)
//     receive the event as the LAST argument, matching the way
//     they used to be invoked from `onsubmit=`. The submit listener
//     calls preventDefault() before dispatching.
//   * For self-only closures (closeKeyForm, etc.) the listener
//     passes the bound element (data-arg1="self") as the arg.

import { showCreateAccount, createAccount, closeCreateAccount, deleteAccount, testAccount } from "./account-handlers.js";
import { showCreateCombo, createCombo, closeCreateCombo, deleteCombo, updateRaceSize, testAllTargets } from "./combo-handlers.js";
import {
  showAddTarget, closeAddTarget, onTargetKindChange, onTargetProviderChange,
  addTarget, deleteTarget, resetCooldown, changePriority,
  toggleTargetSelection, toggleSelectAllTargets, clearTargetSelection, bulkDeleteSelectedTargets,
} from "./combo-target-handlers.js";
import { showCreateKey, showEditKey, closeKeyForm, toggleExpiryAmount, createKey, updateKey, regenerateKey, revokeKey, viewKeyUsage, deleteKey } from "./key-handlers.js";
import {
  showEditModel, updateModel,
  toggleModel, testModel, deleteModel,
  toggleModelSelection, toggleSelectAllModels, clearModelSelection,
  bulkEnableSelected, bulkDisableSelected, bulkTestSelected, bulkDeleteSelected,
  updateProviderFilter, updateAutoActivate, createCustomModel, showCustomModelForm, closeCustomModelForm,
  cycleProviderSort,
} from "./model-handlers.js";
import {
  refreshProvider, refreshAllProviders,
  showCreateProvider, closeCreateProvider, createProvider,
  confirmDeleteProvider, deleteProvider,
  toggleProviderActive, renameProviderPrompt, bulkToggleModels,
  setHealth, refreshAccountQuota, refreshAllQuotas,
} from "./provider-handlers.js";
import { exportConfig } from "./config-handlers.js";
import { exportLogsCSV } from "./log-handlers.js";
import {
  openModelPickerModal, closeModelPickerModal, clearModelPicker,
  toggleModelPicker, filterModelPicker, removeModelFromKey,
} from "../components/model-picker.js";
import { mountThemeToggle } from "../components/theme-toggle.js";
import { toggleSidebar } from "../components/sidebar.js";
import { showToast } from "../components/toast.js";
import { navigate, rerenderCurrentView } from "../state/router.js";
import { OAuthLogin } from "./oauth-handlers.js";
import { logsPrevPage, logsNextPage, logsGoPage, logsSetFollow, toggleColumnsMenu, toggleColumn } from "../views/logs.js";
import { configSaveTimeouts } from "../views/config.js";
import { closeLogDetailModal } from "../components/log-detail.js";

// ---- Action registry ----
// Keys are the data-action values. Each value is the function to
// invoke. Positional args are filled from data-arg1, data-arg2, ...;
// the DOM event is always the last argument.
export const HANDLERS = {
  // Accounts
  showCreateAccount,
  createAccount,        // signature: (providerId, e)  — submit handler
  closeCreateAccount,
  deleteAccount,
  testAccount,

  // Combos
  showCreateCombo,
  createCombo,          // signature: (e)              — submit handler
  closeCreateCombo,
  deleteCombo,
  updateRaceSize,
  testAllTargets,       // signature: (comboId, e)     — button click

  // Combo targets
  showAddTarget,
  closeAddTarget,
  onTargetKindChange,
  onTargetProviderChange,
  addTarget,            // signature: (comboId, e)     — submit handler
  deleteTarget,
  resetCooldown,
  changePriority,
  toggleTargetSelection,
  toggleSelectAllTargets,
  clearTargetSelection,
  bulkDeleteSelectedTargets,

  // Keys
  showCreateKey,
  showEditKey,
  closeKeyForm,
  toggleExpiryAmount,
  createKey,            // signature: (e)              — submit handler
  updateKey,            // signature: (id, e)          — submit handler
  regenerateKey,
  revokeKey,
  viewKeyUsage,
  deleteKey,

  // Models (provider-detail)
  showEditModel,
  updateModel,          // signature: (rowId, e)       — submit handler
  toggleModel,          // (rowId, newActive, e)
  testModel,            // (rowId, modelId, e)
  deleteModel,          // (rowId)
  toggleModelSelection, // (rowId, e)
  toggleSelectAllModels,
  clearModelSelection,
  bulkEnableSelected,
  bulkDisableSelected,
  bulkTestSelected,
  bulkDeleteSelected,
  updateProviderFilter, // (providerId, key, value)
  updateAutoActivate,   // (providerId, e)
  createCustomModel,    // (providerId, e)              — submit handler
  showCustomModelForm,
  closeCustomModelForm,
  cycleProviderSort,    // (providerId, sortKey, e)    — click on sortable <th>

  // Providers (per-provider actions)
  refreshProvider,        // (providerId, e)
  refreshAllProviders,
  showCreateProvider,
  closeCreateProvider,
  createProvider,         // signature: (e)              — submit handler
  confirmDeleteProvider,
  deleteProvider,
  toggleProviderActive,   // (providerId, newActive)
  renameProviderPrompt,   // (providerId, currentName)
  bulkToggleModels,       // (providerId, active)

  // Account health / quota (per-account actions exposed on the
  // provider detail view)
  setHealth,              // (id, e)
  refreshAccountQuota,    // (accountId, e)
  refreshAllQuotas,       // (providerId)

  // Config
  configSaveTimeouts,
  exportConfig,

  // Logs
  logsPrevPage,
  logsNextPage,
  logsGoPage,
  logsSetFollow,
  exportLogsCSV,
  // Columns visibility (logs view)
  toggleColumnsMenu,
  toggleColumn,

  // Model picker (singleton)
  openModelPickerModal,
  closeModelPickerModal,
  clearModelPicker,
  toggleModelPicker,
  filterModelPicker,
  removeModelFromKey,

  // Log detail modal
  closeLogDetailModal,

  // Generic modal-bg closer: removes the closest .modal-bg of the
  // click target. Used by modals that don't have a stable ID.
  closeModalBg(e) {
    if (!e || !e.target) return;
    const el = e.target.closest && e.target.closest(".modal-bg");
    if (el) el.remove();
  },

  // Copy the value of #oauth-auth-url to the clipboard. Used by
  // the OAuth "Copy" button in views/providers.js.
  copyAuthUrl() {
    const el = document.getElementById("oauth-auth-url");
    if (el && navigator.clipboard) {
      navigator.clipboard.writeText(el.value || "").catch(() => {});
    }
  },

  // Used by the plaintext-key modal "I've saved it" button: close
  // the modal-bg and re-navigate so the key list repaints.
  closeAndNavigate(e) {
    if (e && e.target) {
      const el = e.target.closest && e.target.closest(".modal-bg");
      if (el) el.remove();
    }
    navigate();
  },

  // Toggles which section of the log detail modal is visible.
  // The tab indicator (`.active`) is set by an inline listener
  // registered in showLogDetail().
  logDetailTab(which) {
    document.querySelectorAll("#log-detail-content [data-log-tab]").forEach((sec) => {
      sec.style.display = (sec.getAttribute("data-log-tab") === which) ? "" : "none";
    });
  },

  // Theme toggle (called from sidebar; works through addEventListener
  // in mountThemeToggle, but exposed as an action for completeness).
  mountThemeToggle,

  // Sidebar collapse toggle. Lives on the sidebar's own button
  // (data-action="toggleSidebar") and persists the choice to
  // localStorage.
  toggleSidebar,

  // Router utilities (data-action friendly). The router keeps its
  // own window.navigate / window.rerenderCurrentView aliases for
  // internal callers (bg-poll, hand-written handlers).
  navigate,
  rerenderCurrentView,

  // OAuth (the OAuthLogin object's methods are exposed under flat
  // names so the HTML stays simple: data-action="oauthStartPKCE").
  oauthStartPKCE:        (provider) => OAuthLogin.startPKCE(provider),
  oauthStartDeviceCode:  (provider) => OAuthLogin.startDeviceCode(provider),
  oauthSubmitManualCallback: () => OAuthLogin.submitManualCallback(),

  // Toast — not strictly needed as a data-action, but useful for
  // ad-hoc debugging from the console.
  showToast,
};

// Collect positional data-arg-N attrs from an element. Skips the
// "action" key. Returns an array aligned to arg1..argN order. Numbers
// that parse as finite integers are returned as Numbers so handlers
// don't have to parseInt every time.
export function collectArgs(el) {
  const args = [];
  for (const key in el.dataset) {
    if (key === "action") continue;
    const m = key.match(/^arg(\d+)$/);
    if (!m) continue;
    const n = parseInt(m[1], 10) - 1;
    const v = el.dataset[key];
    // Only auto-coerce when the entire value is a JSON number;
    // strings with non-numeric chars (labels with spaces) stay as
    // strings so handlers can decide.
    if (/^-?\d+(\.\d+)?$/.test(v)) args[n] = Number(v);
    else args[n] = v;
  }
  return args;
}
