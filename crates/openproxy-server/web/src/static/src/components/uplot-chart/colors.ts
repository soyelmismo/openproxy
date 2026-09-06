// components/uplot-chart/colors.ts
// ==========
// CHART_COLORS palette + CSS variable resolver.
//
// `cssVar` lives here (not in lifecycle.ts) because it's a theme-color
// helper used by the chart builders (`cssVar("--color-border-soft")` etc.)
// and by the design-token alignment of `CHART_COLORS`. Keeping both in
// the same file makes the theme/canvas-color contract self-contained.

/** Resolve a CSS custom property to its computed value at call time.
 *  Returns a sensible dark-grey fallback if `window` is unavailable
 *  (SSR) or the property is not defined. */
export function cssVar(name: string): string {
  if (typeof window === "undefined") return "#5a5a5a";
  const v: string = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || "#5a5a5a";
}

/** Chart series colors. Kept in sync with the design tokens; if the theme
 *  changes these colors, the charts pick them up on the next mount (the
 *  values are read at module load, NOT at theme-change time — for live
 *  theme switching we'd need to read CSS custom properties via
 *  `getComputedStyle(document.documentElement)`. Out of scope for F6. */
export const CHART_COLORS = {
  blue: "#38bdf8",
  green: "#4ade80",
  orange: "#fb923c",
  red: "#f87171",
  purple: "#a855f7",
  gray: "#94a3b8",
  status2xx: "#4ade80",
  status4xx: "#fbbf24",
  status5xx: "#f87171",
} as const;

/** Re-exported type so consumers can refer to `uplot-chart.ChartColors`
 *  if they need to constrain their own constants to the palette. */
export type ChartColors = typeof CHART_COLORS;
