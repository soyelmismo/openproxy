import { describe, it, expect } from "vitest";
import { render } from "lit-html";
import { renderRequestTab, renderResponseTab } from "./index.js";

function renderToText(template: unknown): string {
  const host = document.createElement("div");
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  render(template as any, host);
  return host.innerHTML;
}

describe("embedding input in the Request tab", () => {
  it("renders a text input readably instead of raw JSON", () => {
    const body = { model: "embeddinggemma", input: "find a good restaurant" };
    const html = renderToText(renderRequestTab(body));
    expect(html).toContain("find a good restaurant");
    expect(html).toContain("log-detail-embedding-text");
    expect(html).not.toContain('"input"');
  });

  it("renders an array of strings as separate items", () => {
    const body = { model: "embeddinggemma", input: ["one", "two"] };
    const html = renderToText(renderRequestTab(body));
    expect(html).toContain("one");
    expect(html).toContain("two");
  });

  it("does not dump a huge base64 data URL into the page", () => {
    // Above the thumbnail cap: the whole blob must not land in the DOM, even
    // inside an <img src> attribute.
    const huge = "data:image/png;base64," + "A".repeat(400_000);
    const body = { model: "gemini-embedding-2", input: [{ type: "image", url: huge }] };
    const html = renderToText(renderRequestTab(body));
    expect(html).not.toContain("A".repeat(500));
    expect(html).toContain("inline image");
  });

  it("thumbnails a small inline image", () => {
    const small = "data:image/png;base64,QUJD"; // a few bytes
    const body = { model: "gemini-embedding-2", input: [{ type: "image", url: small }] };
    const html = renderToText(renderRequestTab(body));
    expect(html).toContain("<img");
    expect(html).toContain(small);
  });

  it("renders text and image parts of one multimodal item", () => {
    const body = {
      model: "gemini-embedding-2",
      input: [
        { type: "text", text: "describe this" },
        { type: "image", url: "data:image/png;base64,QUJD" },
      ],
    };
    const html = renderToText(renderRequestTab(body));
    expect(html).toContain("describe this");
    expect(html).toContain("inline image");
  });

  it("falls back to the generic viewer for an unrecognised input", () => {
    const body = { model: "m", input: 42 };
    const html = renderToText(renderRequestTab(body));
    // `42` is not a valid embedding input; the generic renderer shows it as JSON.
    expect(html).toContain("input");
    expect(html).toContain("42");
  });
});

describe("embedding output in the Response tab", () => {
  it("renders embedding vectors with the shared visualization", () => {
    const response = {
      object: "list",
      data: [{ object: "embedding", embedding: [0.1, -0.2, 0.05, 0.9], index: 0 }],
      model: "embeddinggemma",
    };
    const html = renderToText(renderResponseTab(response));
    expect(html).toContain("playground-vector-card");
    expect(html).toContain("vector-bar");
    // The numeric preview is always present even when i18n is not loaded.
    expect(html).toContain("0.100000");
    expect(html).toContain("-0.200000");
  });

  it("renders every vector of a batch", () => {
    const response = {
      data: [
        { embedding: [1, 2, 3], index: 0 },
        { embedding: [4, 5, 6], index: 1 },
      ],
    };
    const html = renderToText(renderResponseTab(response));
    expect((html.match(/playground-vector-card/g) || []).length).toBe(2);
    expect(html).toContain("4.000000");
  });

  it("still falls back for a non-embedding response", () => {
    const response = { data: [{ url: "https://x/img.png" }] };
    const html = renderToText(renderResponseTab(response));
    expect(html).not.toContain("playground-vector-card");
  });
});
