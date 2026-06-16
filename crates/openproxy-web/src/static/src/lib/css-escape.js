// lib/css-escape.js — wrap the native CSS.escape with a no-op
// fallback for older test runners.

export function cssEscape(s) {
  if (typeof CSS !== "undefined" && CSS.escape) return CSS.escape(s);
  return String(s).replace(/[^\w-]/g, (c) => "\\" + c);
}
