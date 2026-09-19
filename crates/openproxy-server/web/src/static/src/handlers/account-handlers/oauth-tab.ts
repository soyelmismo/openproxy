import type { TemplateResult } from "lit-html";
import { state } from "../../state/index.js";
import { api } from "../../state/api.js";
import { showToast } from "../../components/toast.js";
import { OAuthLogin } from "../oauth-handlers.js";
import type { Provider } from "../../lib/types/api.js";
import {
  renderOAuthContent,
  renderOAuthFooterActions,
  type OAuthTabStatus,
  type OAuthViewProps,
} from "./oauth-tab-view.js";

export type { OAuthTabStatus };

export interface OAuthTabContext {
  providerId: string;
  provider: Provider | undefined;
  hasPkce: boolean;
  hasDeviceCode: boolean;
  onSuccess: () => void;
  requestRender: () => void;
  isClosed: () => boolean;
}

export interface OAuthTabHandler {
  status: OAuthTabStatus;
  manualCallbackUrl: string;
  isSubmitting: boolean;
  stop: () => void;
  resetToIdle: () => void;
  renderTab: () => TemplateResult;
  renderFooterActions: (onCancel: () => void) => TemplateResult;
  submitManualCallback: () => Promise<void>;
}

export function createOAuthTabHandler(ctx: OAuthTabContext): OAuthTabHandler {
  const {
    providerId,
    provider,
    hasPkce,
    hasDeviceCode,
    onSuccess,
    requestRender,
    isClosed,
  } = ctx;

  let status: OAuthTabStatus = "idle";
  let error: string | null = null;
  let deviceInfo: {
    verificationUri: string;
    userCode: string;
    deviceCode: string;
  } | null = null;
  let manualAuthData: {
    authorizationUrl: string;
    redirectUri: string;
    codeVerifier: string;
    state?: string | null;
  } | null = null;
  let manualCallbackUrl = "";
  let currentAuthUrl = "";
  let devicePollInterval: ReturnType<typeof setInterval> | null = null;
  let oauthBc: BroadcastChannel | null = null;
  let oauthCleanupListeners: (() => void) | null = null;

  const stop = (): void => {
    if (devicePollInterval) {
      clearInterval(devicePollInterval);
      devicePollInterval = null;
    }
    if (oauthBc) {
      try {
        oauthBc.close();
      } catch {}
      oauthBc = null;
    }
    if (oauthCleanupListeners) {
      oauthCleanupListeners();
      oauthCleanupListeners = null;
    }
    OAuthLogin.cancelOAuth();
  };

  const resetToIdle = (): void => {
    stop();
    status = "idle";
    error = null;
    deviceInfo = null;
    manualAuthData = null;
    manualCallbackUrl = "";
    currentAuthUrl = "";
    requestRender();
  };

  const setupAutoCallbackDetection = (): void => {
    const bc =
      typeof BroadcastChannel !== "undefined"
        ? new BroadcastChannel("openproxy_oauth")
        : null;
    oauthBc = bc;
    const storageHandler = (e: StorageEvent) => {
      if (e.key === "openproxy_oauth_code" && e.newValue) {
        try {
          const parsed = JSON.parse(e.newValue) as { code?: string };
          if (parsed && typeof parsed.code === "string") {
            localStorage.removeItem("openproxy_oauth_code");
            manualCallbackUrl = parsed.code;
            void submitManualCallback();
          }
        } catch {}
      }
    };
    if (bc) {
      bc.onmessage = (e: MessageEvent) => {
        const data = e.data as { type?: string; code?: string } | null;
        if (data && data.type === "oauth_code" && typeof data.code === "string") {
          manualCallbackUrl = data.code;
          void submitManualCallback();
        }
      };
    }
    window.addEventListener("storage", storageHandler);
    oauthCleanupListeners = () => {
      window.removeEventListener("storage", storageHandler);
      if (bc) {
        try {
          bc.close();
        } catch {}
      }
    };
  };

  const runPkcePopup = async (
    authUrl: string,
    redirectUri: string,
    codeVerifier: string,
  ): Promise<void> => {
    const popup = window.open(
      authUrl,
      "oauth popup",
      "width=600,height=700,top=100,left=100",
    );
    const bc =
      typeof BroadcastChannel !== "undefined"
        ? new BroadcastChannel("openproxy_oauth")
        : null;
    oauthBc = bc;

    try {
      const code: string = await new Promise((resolve, reject) => {
        let timer: ReturnType<typeof setTimeout> | null = null;
        const cleanup = () => {
          if (timer) clearTimeout(timer);
          window.removeEventListener("message", handler);
          window.removeEventListener("storage", storageHandler);
          if (bc) {
            try {
              bc.close();
            } catch {}
          }
          oauthCleanupListeners = null;
        };
        oauthCleanupListeners = cleanup;

        const onCode = (receivedCode: string) => {
          cleanup();
          try {
            popup?.close();
          } catch {}
          resolve(receivedCode);
        };

        const handler = (event: MessageEvent): void => {
          const isAllowedOrigin =
            event.origin === window.location.origin ||
            event.origin.replace("127.0.0.1", "localhost") ===
              window.location.origin.replace("127.0.0.1", "localhost");
          if (!isAllowedOrigin) return;
          const data = event.data as { type?: string; code?: string } | null;
          if (data && data.type === "oauth_code" && typeof data.code === "string") {
            onCode(data.code);
          }
        };

        const storageHandler = (event: StorageEvent): void => {
          if (event.key === "openproxy_oauth_code" && event.newValue) {
            try {
              const parsed = JSON.parse(event.newValue) as { code?: string };
              if (parsed && typeof parsed.code === "string") {
                localStorage.removeItem("openproxy_oauth_code");
                onCode(parsed.code);
              }
            } catch {}
          }
        };

        if (bc) {
          bc.onmessage = (event: MessageEvent) => {
            const data = event.data as { type?: string; code?: string } | null;
            if (data && data.type === "oauth_code" && typeof data.code === "string") {
              onCode(data.code);
            }
          };
        }

        window.addEventListener("message", handler);
        window.addEventListener("storage", storageHandler);
        timer = setTimeout(() => {
          cleanup();
          reject(new Error("OAuth authorization timed out"));
        }, 300000);
      });

      status = "submitting";
      requestRender();

      const exchangeResp = (await api(
        `/oauth/${encodeURIComponent(providerId)}/exchange`,
        {
          method: "POST",
          body: JSON.stringify({
            code,
            redirect_uri: redirectUri,
            code_verifier: codeVerifier,
          }),
        },
      )) as { error?: string };

      if (exchangeResp.error) throw new Error(exchangeResp.error);

      stop();
      showToast(`Logged in with ${provider?.name || providerId}`, "success");
      state.accounts = (await api("/accounts")) as typeof state.accounts;
      onSuccess();
    } catch (err: unknown) {
      if (isClosed()) return;
      status = "idle";
      error = err instanceof Error ? err.message : String(err);
      requestRender();
    }
  };

  const startPkceFlow = async (): Promise<void> => {
    stop();
    error = null;
    status = "submitting";
    requestRender();
    try {
      const resp = (await api(
        `/oauth/${encodeURIComponent(providerId)}/authorize`,
      )) as {
        error?: string;
        authorization_url?: string;
        redirect_uri?: string;
        code_verifier?: string;
        state?: string | null;
      };
      if (resp.error) throw new Error(resp.error);
      const authUrl = resp.authorization_url || "";
      currentAuthUrl = authUrl;
      const forceManual =
        (window as Window & typeof globalThis & { force_manual?: boolean })
          .force_manual === true;
      const isLocal =
        !forceManual &&
        (window.location.hostname === "localhost" ||
          window.location.hostname === "127.0.0.1");

      if (isLocal) {
        status = "authorizing_popup";
        requestRender();
        await runPkcePopup(
          authUrl,
          resp.redirect_uri || "",
          resp.code_verifier || "",
        );
      } else {
        manualAuthData = {
          authorizationUrl: authUrl,
          redirectUri: resp.redirect_uri || "",
          codeVerifier: resp.code_verifier || "",
          state: resp.state ?? null,
        };
        status = "manual_paste";
        window.open(authUrl, "_blank");
        setupAutoCallbackDetection();
        requestRender();
      }
    } catch (err: unknown) {
      if (isClosed()) return;
      status = "idle";
      error = err instanceof Error ? err.message : String(err);
      requestRender();
    }
  };

  const startDeviceCodeFlow = async (): Promise<void> => {
    stop();
    error = null;
    status = "submitting";
    requestRender();
    try {
      const resp = (await api(
        `/oauth/${encodeURIComponent(providerId)}/device-code`,
        { method: "POST" },
      )) as {
        error?: string;
        device_code?: string;
        verification_uri?: string;
        verification_uri_complete?: string;
        user_code?: string;
      };
      if (resp.error) throw new Error(resp.error);

      deviceInfo = {
        verificationUri:
          resp.verification_uri_complete || resp.verification_uri || "",
        userCode: resp.user_code || "",
        deviceCode: resp.device_code || "",
      };
      status = "device_polling";
      requestRender();

      devicePollInterval = setInterval(async () => {
        try {
          const pollResp = (await api(
            `/oauth/${encodeURIComponent(providerId)}/device-poll`,
            {
              method: "POST",
              body: JSON.stringify({ device_code: resp.device_code }),
            },
          )) as { status?: string };

          if (pollResp.status === "complete" || pollResp.status === "ok") {
            stop();
            showToast(`Logged in with ${provider?.name || providerId}`, "success");
            state.accounts = (await api("/accounts")) as typeof state.accounts;
            onSuccess();
          } else if (pollResp.status === "expired") {
            stop();
            status = "idle";
            error = "Device code expired. Please try again.";
            requestRender();
          }
        } catch {
          // keep polling until timeout, cancel, or modal close
        }
      }, 5000);
    } catch (err: unknown) {
      if (isClosed()) return;
      status = "idle";
      error = err instanceof Error ? err.message : String(err);
      requestRender();
    }
  };

  const startManualPasteFlow = async (): Promise<void> => {
    stop();
    error = null;
    status = "submitting";
    requestRender();
    try {
      const resp = (await api(
        `/oauth/${encodeURIComponent(providerId)}/authorize`,
      )) as {
        error?: string;
        authorization_url?: string;
        redirect_uri?: string;
        code_verifier?: string;
        state?: string | null;
      };
      if (resp.error) throw new Error(resp.error);
      const authUrl = resp.authorization_url || "";
      manualAuthData = {
        authorizationUrl: authUrl,
        redirectUri: resp.redirect_uri || "",
        codeVerifier: resp.code_verifier || "",
        state: resp.state ?? null,
      };
      status = "manual_paste";
      window.open(authUrl, "_blank");
      setupAutoCallbackDetection();
      requestRender();
    } catch (err: unknown) {
      if (isClosed()) return;
      status = "idle";
      error = err instanceof Error ? err.message : String(err);
      requestRender();
    }
  };

  const submitManualCallback = async (): Promise<void> => {
    const input = manualCallbackUrl.trim();
    if (!manualAuthData) {
      showToast("No OAuth flow in progress", "error");
      return;
    }
    if (!input) {
      showToast("Please paste the callback URL or API key", "error");
      return;
    }
    let code: string | null = null;
    let callbackState: string | null = null;
    try {
      const url = new URL(input);
      code =
        url.searchParams.get("code") ||
        url.searchParams.get("apiKey") ||
        url.searchParams.get("token") ||
        url.searchParams.get("key");
      callbackState =
        url.searchParams.get("state") || url.hash.replace(/^#/, "") || null;
    } catch {
      // Not a full URL - treat as raw key or fragment
    }

    if (!code) {
      const keyMatch = input.match(
        /(cmd_[a-zA-Z0-9_\-]+|user_[a-zA-Z0-9_\-]+)/,
      );
      if (keyMatch) {
        code = keyMatch[1] ?? null;
      } else if (
        input.startsWith("cmd_") ||
        input.startsWith("user_") ||
        input.length >= 16
      ) {
        const parts = input.split("#", 2);
        code = parts[0] || null;
        callbackState = parts[1] || null;
      }
    }

    if (!code) {
      showToast(
        "No authorization code found. Copy the API key or code from the callback page and paste it here.",
        "error",
      );
      return;
    }

    error = null;
    status = "submitting";
    requestRender();

    try {
      const exchangeResp = (await api(
        `/oauth/${encodeURIComponent(providerId)}/exchange`,
        {
          method: "POST",
          body: JSON.stringify({
            code,
            redirect_uri: manualAuthData.redirectUri,
            code_verifier: manualAuthData.codeVerifier,
            state: callbackState || manualAuthData.state,
          }),
        },
      )) as { error?: string };

      if (exchangeResp.error) throw new Error(exchangeResp.error);

      stop();
      showToast(`Logged in with ${provider?.name || providerId}`, "success");
      state.accounts = (await api("/accounts")) as typeof state.accounts;
      onSuccess();
    } catch (err: unknown) {
      if (isClosed()) return;
      status = "manual_paste";
      error = err instanceof Error ? err.message : String(err);
      requestRender();
    }
  };

  const getViewProps = (): OAuthViewProps => ({
    status,
    error,
    provider,
    providerId,
    hasPkce,
    hasDeviceCode,
    deviceInfo,
    manualAuthData,
    manualCallbackUrl,
    currentAuthUrl,
    onStartPkce: () => void startPkceFlow(),
    onStartDeviceCode: () => void startDeviceCodeFlow(),
    onStartManualPaste: () => void startManualPasteFlow(),
    onSubmitManualCallback: () => void submitManualCallback(),
    onManualCallbackInput: (val: string) => {
      manualCallbackUrl = val;
    },
    onResetToIdle: () => resetToIdle(),
  });

  return {
    get status() {
      return status;
    },
    get manualCallbackUrl() {
      return manualCallbackUrl;
    },
    set manualCallbackUrl(val: string) {
      manualCallbackUrl = val;
    },
    get isSubmitting() {
      return status === "submitting";
    },
    stop,
    resetToIdle,
    renderTab: () => renderOAuthContent(getViewProps()),
    renderFooterActions: (onCancel: () => void) =>
      renderOAuthFooterActions(getViewProps(), onCancel),
    submitManualCallback,
  };
}
