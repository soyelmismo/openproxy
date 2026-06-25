/**
 * lib/escape.ts — HTML-escape helpers. `escapeAttr` is just an alias
 * used at the call site to make the intent obvious (we're putting the
 * value inside an attribute, not text).
 */

export function escapeHtml(s: unknown): string {
  if (s == null) return "";
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

export function escapeAttr(s: unknown): string {
  return escapeHtml(s);
}

/**
 * Pull the human-readable `message` field out of the JSON envelope
 * produced by the server's `ApiError` impl. The thrower is `api()`,
 * which raises `new Error("<status>: <body>")`; the JSON body lives
 * as a string suffix on `e.message`, and we re-parse it here.
 */
export function extractApiErrorMessage(e: unknown): string | null {
  if (!e || typeof (e as { message?: unknown }).message !== "string") return null;
  const message = (e as { message: string }).message;
  const m = message.match(/"error"\s*:\s*\{[\s\S]*?"message"\s*:\s*"((?:[^"\\]|\\.)*)"/);
  if (!m) return null;
  try { return JSON.parse('"' + (m[1] ?? "") + '"') as string; }
  catch (_err: unknown) { return m[1] ?? null; }
}
