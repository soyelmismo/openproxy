// views/home/types.ts — shared types for the home dashboard modules.

import type uPlot from "uplot";
import type { Snapshot, SnapshotWindow, LiveConnectionState } from "../../state/live-store.js";

export type { Snapshot, SnapshotWindow, LiveConnectionState };

/** The uPlot instances + their resize observers. Created after the first
 *  lit-html render, destroyed on view unmount. Null before creation. */
export interface ChartInstances {
  throughput: uPlot;
  statusCodes: uPlot;
  latency: uPlot;
  sparkRequests: uPlot;
  sparkSuccess: uPlot;
  sparkLatency: uPlot;
  sparkTokens: uPlot;
  sparkCost: uPlot;
  resizeDisposers: Array<() => void>;
}

/** Saved scroll state for the activity feed. Set in the subscriber
 *  callback (before lit-html render), restored in the post-render
 *  `requestAnimationFrame`. */
export interface SavedScroll {
  scrollTop: number;
  scrollHeight: number;
}
