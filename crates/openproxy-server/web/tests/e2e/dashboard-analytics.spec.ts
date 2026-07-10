import { test, expect, type Page } from "@playwright/test";

const json = (body: unknown) => ({ status: 200, contentType: "application/json", body: JSON.stringify(body) });

async function mockAnalytics(page: Page): Promise<void> {
  const summary = {
    unique_requests: 1000, total_rows: 1120, total_attempts: 1120, winners: 1000,
    losers: 120, errors: 30, total_prompt_tokens: 2_400_000,
    total_completion_tokens: 800_000, total_cost_usd: 48.5, avg_ttft_ms: 420,
    avg_total_ms: 2850, rows_with_null_pricing: 0,
  };
  const models = [
    { provider_id: "openrouter", upstream_model_id: "anthropic/claude-3.7-sonnet-long-name", unique_requests: 620, total_rows: 650, winners: 620, total_prompt_tokens: 1_400_000, total_completion_tokens: 520_000, total_cost_usd: 31.2 },
    { provider_id: "nvidia", upstream_model_id: "deepseek/deepseek-r1", unique_requests: 260, total_rows: 300, winners: 260, total_prompt_tokens: 700_000, total_completion_tokens: 210_000, total_cost_usd: 12.1 },
  ];
  const providers = [
    { provider_id: "openrouter", unique_requests: 700, total_rows: 780, winners: 700, total_prompt_tokens: 1_700_000, total_completion_tokens: 610_000, total_cost_usd: 36.4 },
    { provider_id: "nvidia", unique_requests: 300, total_rows: 340, winners: 300, total_prompt_tokens: 700_000, total_completion_tokens: 190_000, total_cost_usd: 12.1 },
  ];
  const responses: Array<[string, unknown]> = [
    ["summary", summary], ["by-model", models], ["by-provider", providers],
    ["monthly-by-provider", [
      { ...providers[0], month: "2026-07", provider_id: "openrouter" },
      { ...providers[1], month: "2026-07", provider_id: "nvidia" },
    ]],
    ["latency", { samples: 970, p50_connect_ms: 85, p95_connect_ms: 210, p50_ttft_ms: 390, p95_ttft_ms: 920, p50_total_ms: 2100, p95_total_ms: 5700, p50_tokens_per_sec: 72, p95_tokens_per_sec: 131 }],
    ["races", { total_races: 140, winners: 138, losers: 212, avg_winner_position: 1.4, avg_ttft_savings_ms: null, wins_by_target: [] }],
    ["by-day", [
      { date: "2026-07-08", unique_requests: 280, total_rows: 310, total_prompt_tokens: 620000, total_completion_tokens: 210000, total_cost_usd: 12.2, errors: 12 },
      { date: "2026-07-09", unique_requests: 340, total_rows: 380, total_prompt_tokens: 810000, total_completion_tokens: 250000, total_cost_usd: 16.1, errors: 8 },
      { date: "2026-07-10", unique_requests: 380, total_rows: 430, total_prompt_tokens: 970000, total_completion_tokens: 340000, total_cost_usd: 20.2, errors: 10 },
    ]],
    ["by-status", [{ status_code: 200, count: 970 }, { status_code: 429, count: 20 }, { status_code: 500, count: 10 }]],
    ["errors", [{ request_id: "req-1", trace_id: "trace-1", provider_id: "openrouter", upstream_model_id: "anthropic/claude-3.7-sonnet-long-name", status_code: 500, error_msg_redacted: "upstream unavailable", created_at: "2026-07-10T10:30:00Z" }]],
  ];
  for (const [endpoint, body] of responses) {
    await page.route(`**/admin/api/usage/${endpoint}*`, (route) => route.fulfill(json(body)));
  }
  await page.route("**/admin/api/providers", (route) => route.fulfill(json([{ id: "openrouter", name: "OpenRouter" }, { id: "nvidia", name: "NVIDIA" }])));
  await page.route("**/admin/api/keys", (route) => route.fulfill(json([{ id: 1, label: "Production", key_prefix: "op_live_123" }])));
}

test("analytics prioritizes operational KPIs and readable chart legends", async ({ page }) => {
  await mockAnalytics(page);
  await page.goto("/#/analytics?range=30d");

  await expect(page.locator(".analytics-metrics")).toBeVisible();
  await expect(page.locator(".analytics-metric", { hasText: "Success rate" })).toContainText("97.0%");
  await expect(page.locator(".analytics-ranking-name").first()).toHaveText("anthropic/claude-3.7-sonnet-long-name");
  await expect(page.locator("#chart-daily-usage .u-legend")).toContainText("Requests");
  await expect(page.locator("#chart-daily-usage .u-legend")).toContainText("Errors");
  await expect(page.locator("#chart-daily-usage .u-legend")).toContainText("Cost");
  await expect(page.locator(".analytics-status-hero")).toContainText("97.0%");
});

test("analytics stays inside the mobile viewport", async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 812 });
  await mockAnalytics(page);
  await page.goto("/#/analytics?range=30d");
  await expect(page.locator(".analytics-metrics")).toBeVisible();
  const overflow = await page.evaluate(() => document.body.scrollWidth - window.innerWidth);
  expect(overflow).toBeLessThanOrEqual(2);
});
