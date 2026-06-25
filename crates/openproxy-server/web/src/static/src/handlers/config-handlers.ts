// handlers/config-handlers.ts — config view handlers. The "Save"
import { showToast } from "../components/toast.js";
// click lives in views/config.ts (it's a single PUT). This file
// is a placeholder for future config actions (toggle, import).
//
// Per spec §3 + §13.8 we do not attach to `window.*`. The
// `exportConfig` symbol is registered in handlers/registry.ts so
// the data-action shim can dispatch to it.


export function exportConfig(): void {
  showToast("Config export is not implemented yet.", "info");
}
