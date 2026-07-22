// state/api.ts — thin re-export from lib/api.ts for backwards
// compatibility.  The canonical implementation lives in lib/api.ts
// (auth headers, latency tracking, error handling, debug-log helpers).
//
// All views/handlers/components that currently import from
// "../state/api.js" will continue to work without changes.

export { api } from "../lib/api.js";
export type { ApiOptions as ApiCallOptions } from "../lib/api.js";
