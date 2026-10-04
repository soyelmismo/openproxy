// handlers/oauth-code-listener.ts — unified subscriber for OAuth authorization code callbacks.
// Deduplicates transport listener logic across window.message, window.storage, and BroadcastChannel.

function isOAuthMessage(data: unknown): data is { type: "oauth_code"; code: string } {
  if (data === null || typeof data !== "object") return false;
  const record = data as { type?: unknown; code?: unknown };
  return record.type === "oauth_code" && typeof record.code === "string";
}

export function subscribeOAuthCode(
  onCode: (code: string) => void,
  channel: BroadcastChannel | null,
  listenMessages: boolean = false,
): () => void {
  const storageHandler = (event: StorageEvent): void => {
    if (event.key === "openproxy_oauth_code" && event.newValue) {
      try {
        const parsed = JSON.parse(event.newValue) as { code?: unknown } | null;
        if (parsed && typeof parsed === "object" && typeof parsed.code === "string") {
          localStorage.removeItem("openproxy_oauth_code");
          onCode(parsed.code);
        }
      } catch {}
    }
  };

  window.addEventListener("storage", storageHandler);

  let messageHandler: ((event: MessageEvent) => void) | null = null;
  if (listenMessages) {
    messageHandler = (event: MessageEvent): void => {
      const isAllowedOrigin =
        event.origin === window.location.origin ||
        event.origin.replace("127.0.0.1", "localhost") ===
          window.location.origin.replace("127.0.0.1", "localhost");
      if (!isAllowedOrigin) return;
      if (isOAuthMessage(event.data)) {
        onCode(event.data.code);
      }
    };
    window.addEventListener("message", messageHandler);
  }

  if (channel) {
    channel.onmessage = (event: MessageEvent): void => {
      if (isOAuthMessage(event.data)) {
        onCode(event.data.code);
      }
    };
  }

  return (): void => {
    window.removeEventListener("storage", storageHandler);
    if (messageHandler) {
      window.removeEventListener("message", messageHandler);
    }
  };
}
