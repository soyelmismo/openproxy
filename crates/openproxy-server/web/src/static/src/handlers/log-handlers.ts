// handlers/log-handlers.ts — log view handlers. Mostly thin
import { showToast } from "../components/toast.js";
// shims that the views already have via views/logs.ts; this
// module exists so log-specific click handlers can be moved
// out of app.ts (and so the e2e selector contract stays in
// one place).
//
// Per spec §3 + §13.8 we do not attach to `window.*`.


export function exportLogsCSV(): void {
  // Reserved for a future feature. Right now we just toast.
  showToast("CSV export is not implemented yet.", "info");
}
