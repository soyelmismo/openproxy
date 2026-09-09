// components/uplot-chart/index.ts
// ==========
// Facade for the uPlot chart subsystem. Re-exports the public API of
// the three internal modules (colors / lifecycle / builders) so
// consumers keep importing from `components/uplot-chart.js`:
//
//   import { buildThroughputChart, CHART_COLORS, observeResize }
//     from "../components/uplot-chart.js";
//
// The public surface is identical to the pre-split monolithic file —
// no consumer-facing changes.

export { CHART_COLORS, cssVar, type ChartColors } from "./colors.js";

export {
  injectUplotCss,
  createLiveChart,
  createSparkline,
  resizeChart,
  observeResize,
  smoothSpline,
  smoothPath,
  type ChartData,
  type LiveChartOpts,
} from "./lifecycle.js";

export {
  buildThroughputChart,
  buildStatusCodesChart,
  buildLatencyChart,
  buildDailyUsageChart,
} from "./builders.js";
