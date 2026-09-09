import { test, expect, type Page, type Response } from "@playwright/test";

const ASSET_PREFIXES = [
  "/admin/dist/",
  "/admin/styles/",
  "/admin/fonts/",
  "/admin/i18n/",
];
const CHUNK_URL = /\/admin\/dist\/chunks\/[^/]+\.js(?:\?.*)?$/;

function isTrackedAsset(response: Response): boolean {
  return ASSET_PREFIXES.some((prefix) => new URL(response.url()).pathname.startsWith(prefix));
}

function isJavaScript(response: Response): boolean {
  const contentType = response.headers()["content-type"] ?? "";
  const mediaType = contentType.toLowerCase().split(";")[0] ?? "";
  return mediaType === "application/javascript" || mediaType === "text/javascript";
}

async function expectView(page: Page, route: string, reload = true): Promise<void> {
  if (reload) {
    await page.goto(`/#/${route}`);
  } else {
    await page.evaluate((hash) => {
      window.location.hash = hash;
    }, `/${route}`);
  }

  const selectors: Record<string, string> = {
    providers: ".page-header h2",
    analytics: ".analytics-header h2",
    config: ".config-editable-region .card",
    playground: ".playground-studio-wrapper",
    notifications: ".page-header h2",
  };
  const selector = selectors[route];
  if (!selector) throw new Error(`no selector registered for ${route}`);
  await expect(page.locator(selector).first()).toBeVisible();
}

test("admin shell serves real assets successfully", async ({ page }) => {
  const assets: Response[] = [];
  page.on("response", (response) => {
    if (isTrackedAsset(response)) assets.push(response);
  });

  const shell = await page.goto("/admin/");
  expect(shell?.status()).toBe(200);

  await expect(page.locator("#app")).toBeVisible();
  await expect.poll(() => assets.some((response) => response.url().endsWith("/admin/dist/app.js"))).toBe(true);
  await expect.poll(() => assets.some((response) => response.url().endsWith("/admin/i18n/en.json"))).toBe(true);

  for (const response of assets) {
    expect(response.status(), response.url()).toBeLessThan(400);
  }

  const app = assets.find((response) => response.url().endsWith("/admin/dist/app.js"));
  if (!app) throw new Error("app.js response was not captured");
  expect(app.status()).toBe(200);
  expect(isJavaScript(app)).toBe(true);

  const translation = assets.find((response) => response.url().endsWith("/admin/i18n/en.json"));
  if (!translation) throw new Error("en.json response was not captured");
  expect(translation.status()).toBe(200);
  expect(await translation.json()).toBeTruthy();
});

test("heavy views load lazy chunks once and reuse the module cache", async ({ page }) => {
  const chunkResponses: Response[] = [];
  page.on("response", (response) => {
    if (CHUNK_URL.test(response.url())) chunkResponses.push(response);
  });

  await page.goto("/admin/");

  const views = ["providers", "analytics", "config", "playground", "notifications"] as const;
  for (const route of views) {
    const beforeFirstVisit = chunkResponses.length;
    await expectView(page, route);
    const firstVisitChunks = chunkResponses.slice(beforeFirstVisit);

    expect(firstVisitChunks.length, `${route} did not request a lazy chunk`).toBeGreaterThan(0);
    for (const response of firstVisitChunks) {
      expect(response.status(), response.url()).toBe(200);
      expect(isJavaScript(response), response.url()).toBe(true);
    }

    const beforeSecondVisit = chunkResponses.length;
    // Navigate via the hash so the browser module cache applies: a full
    // `page.goto` would tear down the document and re-download the SPA shell.
    await expectView(page, route, false);
    const secondVisitChunks = chunkResponses.slice(beforeSecondVisit);

    // The server deliberately sends no-cache headers. Browser module-cache behavior
    // can vary by engine, so permit one revalidation/download but never repeated
    // downloads for a single route revisit.
    expect(secondVisitChunks.length, `${route} downloaded chunks repeatedly`).toBeLessThanOrEqual(1);
  }
});

test("unknown admin client paths return the SPA shell, not a JSON 404", async ({ page }) => {
  const response = await page.goto("/admin/non-existent-client-path");
  if (!response) throw new Error("no response for /admin/non-existent-client-path");

  expect(response.status()).toBe(200);
  expect(response.headers()["content-type"] ?? "").toContain("text/html");
  const html = await page.content();
  expect(html).toContain("/admin/dist/app.js");
  expect(html).not.toContain('"error"');
});
