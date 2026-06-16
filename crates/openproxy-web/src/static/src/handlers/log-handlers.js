// handlers/log-handlers.js — log view handlers. Mostly thin
// shims that the views already have via views/logs.js; this
// module exists so log-specific click handlers can be moved
// out of app.js (and so the e2e selector contract stays in
// one place).
//
// Per spec §3 + §13.8 we do not attach to `window.*`.

import { showToast } from "../components/toast.js";

export function exportLogsCSV() {
  // Reserved for a future feature. Right now we just toast.
  showToast("CSV export is not implemented yet.", "info");
}
