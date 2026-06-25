// lib/css-escape.ts — wrap the native CSS.escape with a no-op
// fallback for older test runners.

export function cssEscape(s: unknown): string {
  if (typeof CSS !== "undefined" && CSS.escape) return CSS.escape(String(s));
  return String(s).replace(/[^\w-]/g, (c) => "\\" + c);
}
