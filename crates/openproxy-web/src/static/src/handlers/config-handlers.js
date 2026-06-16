// handlers/config-handlers.js — config view handlers. The "Save"
// click lives in views/config.js (it's a single PUT). This file
// is a placeholder for future config actions (toggle, import).
//
// Per spec §3 + §13.8 we do not attach to `window.*`. The
// `exportConfig` symbol is registered in handlers/registry.js so
// the data-action shim can dispatch to it.

import { showToast } from "../components/toast.js";

export function exportConfig() {
  showToast("Config export is not implemented yet.", "info");
}
