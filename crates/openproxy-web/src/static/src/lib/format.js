// lib/format.js — small pure formatters.

export function formatContext(n) {
  if (n == null) return "—";
  if (n < 1000) return String(n);
  if (n < 10000) return (n / 1000).toFixed(1) + "k";
  if (n < 1_000_000) return Math.round(n / 1000) + "k";
  return (n / 1_000_000).toFixed(1) + "M";
}

export function formatCost(usd) {
  return "$" + (Number(usd) || 0).toFixed(4);
}

export function formatMs(ms) {
  if (ms == null) return "—";
  return Math.round(ms) + "ms";
}

// Localised-friendly number for currency / counts. Not a full i18n
// helper — just a one-liner we use in a few places.
export function formatNumber(n, opts = {}) {
  return new Intl.NumberFormat(undefined, opts).format(n);
}
