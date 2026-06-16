// components/phase-cell.js — the "phase" column pill for a log
// row. Thin wrapper around components/log-row.js that exposes
// just the phase cell for callers that want to update it in
// place (the WS handler does this on stage events).

import { renderLogRowHtml } from "./log-row.js";
export { renderLogPhaseHtml } from "./log-row.js";
export { renderLogRowHtml };
