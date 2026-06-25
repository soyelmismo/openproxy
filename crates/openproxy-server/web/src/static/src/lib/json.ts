// lib/json.ts — pretty-print JSON. Returns the string `'null'`
// for null/undefined inputs (mirroring the original behaviour).

export function prettyJson(value: unknown): string {
  if (value == null) return "null";
  if (typeof value === "string") {
    try { return JSON.stringify(JSON.parse(value) as unknown, null, 2); }
    catch (_err: unknown) { return value; }
  }
  return JSON.stringify(value, null, 2);
}
