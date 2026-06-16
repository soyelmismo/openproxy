// lib/json.js — pretty-print JSON. Returns the string `'null'`
// for null/undefined inputs (mirroring the original behaviour).

export function prettyJson(value) {
  if (value == null) return "null";
  if (typeof value === "string") {
    try { return JSON.stringify(JSON.parse(value), null, 2); }
    catch (_) { return value; }
  }
  return JSON.stringify(value, null, 2);
}
