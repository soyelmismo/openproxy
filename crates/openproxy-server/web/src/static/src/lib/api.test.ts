import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { api, fetchDebugLogs } from "./api.js";
import { clearToken } from "../state/auth.js";
import { state } from "../state/index.js";

// Must match `STORAGE_KEY` in `state/auth.ts` — the prompt's
// `openproxy:adminToken` (colon) is not the key the auth store reads.
const TOKEN_KEY = "openproxy_admin_token";

function response(body: string, status = 200, contentType?: string): Response {
  const headers: Record<string, string> = {};
  if (contentType) headers["content-type"] = contentType;
  return new Response(body, { status, headers });
}

describe("api", () => {
  let fetchSpy: ReturnType<typeof vi.spyOn>;

  beforeEach(() => {
    localStorage.clear();
    clearToken();
    state.lastApiLatencyMs = 0;
    fetchSpy = vi.spyOn(window, "fetch");
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("prefixes the path and sends GET with a JSON content type", async () => {
    fetchSpy.mockResolvedValue(response('{"ok":true}', 200, "application/json"));

    await api("/x");

    expect(fetchSpy).toHaveBeenCalledWith("/admin/api/x", {
      method: "GET",
      headers: { "Content-Type": "application/json" },
    });
  });

  it("sends the stored admin token as a bearer header", async () => {
    localStorage.setItem(TOKEN_KEY, "tok");
    fetchSpy.mockResolvedValue(response("{}", 200, "application/json"));

    await api("/x");

    expect(fetchSpy.mock.calls[0]?.[1]).toMatchObject({
      headers: {
        "Content-Type": "application/json",
        Authorization: "Bearer tok",
      },
    });
  });

  it("omits Authorization when no token is stored", async () => {
    fetchSpy.mockResolvedValue(response("{}", 200, "application/json"));

    await api("/x");

    const init = fetchSpy.mock.calls[0]?.[1] as RequestInit;
    expect(init.headers).toEqual({ "Content-Type": "application/json" });
    expect((init.headers as Record<string, string>)["Authorization"]).toBeUndefined();
  });

  it("passes a string POST body through to fetch", async () => {
    fetchSpy.mockResolvedValue(response("{}", 200, "application/json"));

    await api("/x", { method: "POST", body: '{"name":"x"}' });

    expect(fetchSpy).toHaveBeenCalledWith("/admin/api/x", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: '{"name":"x"}',
    });
  });

  it("parses a successful JSON response", async () => {
    fetchSpy.mockResolvedValue(response('{"answer":42}', 200, "application/json; charset=utf-8"));

    await expect(api("/x")).resolves.toEqual({ answer: 42 });
  });

  it("returns a successful non-JSON response as raw text", async () => {
    fetchSpy.mockResolvedValue(response("plain body", 200, "text/plain"));

    await expect(api("/x")).resolves.toBe("plain body");
  });

  it("returns null for a 204 response without reading its body", async () => {
    // A 204 must be constructed with a null body (the fetch spec
    // rejects `new Response(body, { status: 204 })`).
    const r = new Response(null, { status: 204 });
    const textSpy = vi.spyOn(r, "text");
    fetchSpy.mockResolvedValue(r);

    await expect(api("/x")).resolves.toBeNull();
    expect(textSpy).not.toHaveBeenCalled();
  });

  it.each([400, 404, 500, 503])("throws status and body for HTTP %s", async (status) => {
    fetchSpy.mockResolvedValue(response("failure", status, "text/plain"));

    await expect(api("/x")).rejects.toThrow(`${status}: failure`);
  });

  it("reads the response body before throwing", async () => {
    const r = response("body was consumed", 502);
    const textSpy = vi.spyOn(r, "text");
    fetchSpy.mockResolvedValue(r);

    await expect(api("/x")).rejects.toThrow("502: body was consumed");
    expect(textSpy).toHaveBeenCalledTimes(1);
  });

  it("updates lastApiLatencyMs after a successful response", async () => {
    const now = vi.spyOn(performance, "now").mockReturnValueOnce(100).mockReturnValueOnce(137);
    fetchSpy.mockResolvedValue(response("{}", 200, "application/json"));

    await api("/x");

    expect(now).toHaveBeenCalledTimes(2);
    expect(state.lastApiLatencyMs).toBe(37);
  });

  it("omits absent fetchDebugLogs fields", async () => {
    fetchSpy.mockResolvedValue(response("{}", 200, "application/json"));

    await fetchDebugLogs({ level: "WARN" });

    expect(fetchSpy.mock.calls[0]?.[0]).toBe("/admin/api/debug/logs?level=WARN");
  });

  // NOTE: the JSDoc on `FetchDebugLogsOpts.since` says "Omit (or pass
  // 0) to fetch the whole buffer". That equivalence is SERVER-side:
  // seqs start at 1 and the handler takes the full-snapshot branch
  // for any `since <= 0` (crates/openproxy-server/src/handlers/admin/
  // debug.rs:56). The client serializes `since=0` verbatim — this
  // test pins that serialization so a future change either way is a
  // conscious decision, not an accident.
  it("serializes since=0 verbatim into the query string", async () => {
    fetchSpy.mockResolvedValue(response("{}", 200, "application/json"));

    await fetchDebugLogs({ since: 0 });

    expect(fetchSpy.mock.calls[0]?.[0]).toBe("/admin/api/debug/logs?since=0");
  });

  it("URL-encodes request_id in fetchDebugLogs", async () => {
    fetchSpy.mockResolvedValue(response("{}", 200, "application/json"));

    await fetchDebugLogs({ request_id: "a/b" });

    expect(fetchSpy.mock.calls[0]?.[0]).toBe("/admin/api/debug/logs?request_id=a%2Fb");
  });
});
