// lib/format.ts — small pure formatters.

export function formatContext(n: unknown): string {
  if (n == null) return "—";
  const num = Number(n);
  if (!Number.isFinite(num)) return "—";
  if (num < 1000) return String(num);
  if (num < 10000) return (num / 1000).toFixed(1) + "k";
  if (num < 1_000_000) return Math.round(num / 1000) + "k";
  return (num / 1_000_000).toFixed(1) + "M";
}

export function formatCost(usd: unknown): string {
  return "$" + (Number(usd) || 0).toFixed(4);
}

export function formatMs(ms: unknown): string {
  if (ms == null) return "—";
  return Math.round(Number(ms)) + "ms";
}

// Localised-friendly number for currency / counts. Not a full i18n
// helper — just a one-liner we use in a few places.
export function formatNumber(n: number, opts: Intl.NumberFormatOptions = {}): string {
  return new Intl.NumberFormat(undefined, opts).format(n);
}
