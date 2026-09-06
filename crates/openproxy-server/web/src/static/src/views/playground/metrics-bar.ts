// views/playground/metrics-bar.ts — Response metrics strip.
//
// Renders the horizontal metrics row inside the response inspector: status
// pill, total latency, TTFT, streaming throughput (tokens/sec), and token
// counts or payload size.

import { html, type TemplateResult } from 'lit-html';
import type { PlaygroundState } from './shared.js';

function computeTokensPerSec(st: PlaygroundState): string | null {
  const { completionTokens, totalLatencyMs, ttftMs } = st.currentMetrics;
  if (completionTokens && totalLatencyMs && totalLatencyMs > 0) {
    const elapsedSec = (totalLatencyMs - (ttftMs || 0)) / 1000;
    if (elapsedSec > 0.05) {
      return `${(completionTokens / elapsedSec).toFixed(1)} t/s`;
    }
  }
  return null;
}

export function renderMetricsBar(st: PlaygroundState): TemplateResult {
  const status = st.currentMetrics.statusCode;
  const isOk = status !== null && status >= 200 && status < 300;
  const isErr = (status !== null && status >= 400) || st.responseError !== null;
  const tokensPerSec = computeTokensPerSec(st);

  return html`
    <div class="playground-metrics-bar">
      <div class="playground-metric">
        <span class="metric-label">Status</span>
        <span class="status-pill ${isOk ? 'on' : isErr ? 'off' : ''}">
          ${status ? `${status} ${st.currentMetrics.statusText || ''}` : st.responseError ? 'Error' : '—'}
        </span>
      </div>

      <div class="playground-metric">
        <span class="metric-label">Latency</span>
        <span class="metric-value">${st.currentMetrics.totalLatencyMs !== null ? `${st.currentMetrics.totalLatencyMs} ms` : '—'}</span>
      </div>

      ${st.currentMetrics.ttftMs !== null
        ? html`
            <div class="playground-metric">
              <span class="metric-label">TTFT</span>
              <span class="metric-value">${st.currentMetrics.ttftMs} ms</span>
            </div>
          `
        : html``}

      ${tokensPerSec !== null
        ? html`
            <div class="playground-metric">
              <span class="metric-label">Speed</span>
              <span class="metric-value">${tokensPerSec}</span>
            </div>
          `
        : html``}

      ${st.currentMetrics.totalTokens !== null
        ? html`
            <div class="playground-metric">
              <span class="metric-label">Tokens</span>
              <span class="metric-value">
                ${st.currentMetrics.totalTokens}
                ${st.currentMetrics.promptTokens !== null ? html`<small class="text-muted">(${st.currentMetrics.promptTokens}p / ${st.currentMetrics.completionTokens || 0}c)</small>` : html``}
              </span>
            </div>
          `
        : st.currentMetrics.payloadSizeBytes !== null
        ? html`
            <div class="playground-metric">
              <span class="metric-label">Size</span>
              <span class="metric-value">${Math.round(st.currentMetrics.payloadSizeBytes / 10.24) / 100} KB</span>
            </div>
          `
        : html``}
    </div>
  `;
}