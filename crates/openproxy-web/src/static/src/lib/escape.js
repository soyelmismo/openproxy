// lib/escape.js — HTML-escape helpers. `escapeAttr` is just an alias
// used at the call site to make the intent obvious (we're putting the
// value inside an attribute, not text).

export function escapeHtml(s) {
  if (s == null) return "";
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

export function escapeAttr(s) { return escapeHtml(s); }

// Pull the human-readable `message` field out of the JSON envelope
// produced by the server's `ApiError` impl. The thrower is `api()`,
// which raises `new Error("<status>: <body>")`; the JSON body lives
// as a string suffix on `e.message`, and we re-parse it here.
export function extractApiErrorMessage(e) {
  if (!e || typeof e.message !== "string") return null;
  const m = e.message.match(/"error"\s*:\s*\{[\s\S]*?"message"\s*:\s*"((?:[^"\\]|\\.)*)"/);
  if (!m) return null;
  try { return JSON.parse('"' + m[1] + '"'); }
  catch (_) { return m[1]; }
}
