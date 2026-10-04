import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { OAuthLogin } from "./oauth-handlers.js";
import * as toastModule from "../components/toast.js";
import * as reactiveModule from "../state/reactive.js";
import { state } from "../state/index.js";

describe("OAuthLogin.pkcePopup integration", () => {
  const initialAccounts = state.accounts;

  beforeEach(() => {
    vi.useFakeTimers();
    vi.stubGlobal("BroadcastChannel", undefined);
    vi.spyOn(toastModule, "showToast").mockImplementation(() => {});
    vi.spyOn(reactiveModule, "requestUpdate").mockImplementation(() => {});
  });

  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    vi.clearAllTimers();
    vi.useRealTimers();
    state.accounts = initialAccounts;
  });

  it("valid window message triggers exchange payload unchanged and closes popup", async () => {
    const closePopup = vi.spyOn(window, "close").mockImplementation(() => {});
    vi.spyOn(window, "open").mockReturnValue(window);

    const fetchMock = vi.fn<typeof fetch>().mockImplementation(async (input) =>
      new Response(JSON.stringify(String(input).endsWith("/accounts") ? [] : {}), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    vi.stubGlobal("fetch", fetchMock);

    const authData = {
      authorization_url: "https://auth.example.com/oauth/authorize",
      redirect_uri: "http://localhost:8080/callback",
      code_verifier: "test_verifier_string_456",
    };

    const pkcePromise = OAuthLogin.pkcePopup("github", authData);

    window.dispatchEvent(
      new MessageEvent("message", {
        origin: window.location.origin,
        data: { type: "oauth_code", code: "received_code_abc" },
      }),
    );

    await pkcePromise;

    expect(closePopup).toHaveBeenCalledOnce();
    expect(fetchMock).toHaveBeenCalledWith(
      expect.stringContaining("/oauth/github/exchange"),
      expect.objectContaining({
        method: "POST",
        body: JSON.stringify({
          code: "received_code_abc",
          redirect_uri: authData.redirect_uri,
          code_verifier: authData.code_verifier,
        }),
      }),
    );
  });

  it("times out with exact legacy error message when no code is received", async () => {
    vi.spyOn(window, "open").mockReturnValue(window);

    const authData = {
      authorization_url: "https://auth.example.com/oauth/authorize",
      redirect_uri: "http://localhost:8080/callback",
      code_verifier: "test_verifier_string_789",
    };

    const pkcePromise = OAuthLogin.pkcePopup("google", authData);

    // Attach rejection handler immediately to avoid unhandled rejection warning
    const rejectionExpectation = expect(pkcePromise).rejects.toThrow("OAuth timeout");

    await vi.advanceTimersByTimeAsync(300000);

    await rejectionExpectation;
  });
});
