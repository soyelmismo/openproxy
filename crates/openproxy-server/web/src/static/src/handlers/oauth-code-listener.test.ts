import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { subscribeOAuthCode } from "./oauth-code-listener.js";

class FakeBroadcastChannel extends EventTarget implements BroadcastChannel {
  readonly name = "openproxy_oauth";
  onmessage: BroadcastChannel["onmessage"] = null;
  onmessageerror: BroadcastChannel["onmessageerror"] = null;
  close = vi.fn();
  postMessage = vi.fn();
}

describe("subscribeOAuthCode", () => {
  let unsubscribers: Array<() => void> = [];

  beforeEach(() => {
    unsubscribers = [];
  });

  afterEach(() => {
    for (const unsub of unsubscribers) {
      unsub();
    }
    unsubscribers = [];
    vi.restoreAllMocks();
  });

  const track = (unsub: () => void): () => void => {
    unsubscribers.push(unsub);
    return unsub;
  };

  describe("window.message transport", () => {
    it("calls onCode when origin matches window.location.origin exactly", () => {
      const onCode = vi.fn();
      track(subscribeOAuthCode(onCode, null, true));

      window.dispatchEvent(
        new MessageEvent("message", {
          origin: window.location.origin,
          data: { type: "oauth_code", code: "code_same_origin" },
        }),
      );

      expect(onCode).toHaveBeenCalledOnce();
      expect(onCode).toHaveBeenCalledWith("code_same_origin");
    });

    it("accepts localhost vs 127.0.0.1 aliased origin with exact port", () => {
      const onCode = vi.fn();
      track(subscribeOAuthCode(onCode, null, true));

      const port = window.location.port ? `:${window.location.port}` : "";
      const isCurrently127 = window.location.origin.includes("127.0.0.1");
      const aliasOrigin = isCurrently127
        ? `${window.location.protocol}//localhost${port}`
        : `${window.location.protocol}//127.0.0.1${port}`;

      window.dispatchEvent(
        new MessageEvent("message", {
          origin: aliasOrigin,
          data: { type: "oauth_code", code: "code_alias_origin" },
        }),
      );

      expect(onCode).toHaveBeenCalledOnce();
      expect(onCode).toHaveBeenCalledWith("code_alias_origin");
    });

    it("rejects foreign origins and port mismatches", () => {
      const onCode = vi.fn();
      track(subscribeOAuthCode(onCode, null, true));

      // Foreign domain
      window.dispatchEvent(
        new MessageEvent("message", {
          origin: "https://evil-attacker.example.com",
          data: { type: "oauth_code", code: "stolen_code" },
        }),
      );

      // Port mismatch on localhost
      window.dispatchEvent(
        new MessageEvent("message", {
          origin: "http://localhost:65530",
          data: { type: "oauth_code", code: "port_mismatch_code" },
        }),
      );

      // Port mismatch on 127.0.0.1
      window.dispatchEvent(
        new MessageEvent("message", {
          origin: "http://127.0.0.1:65530",
          data: { type: "oauth_code", code: "port_mismatch_127" },
        }),
      );

      expect(onCode).not.toHaveBeenCalled();
    });

    it("accepts empty string code and rejects wrong type or non-string", () => {
      const onCode = vi.fn();
      track(subscribeOAuthCode(onCode, null, true));

      const origin = window.location.origin;

      // Empty string code is accepted
      window.dispatchEvent(
        new MessageEvent("message", {
          origin,
          data: { type: "oauth_code", code: "" },
        }),
      );
      expect(onCode).toHaveBeenCalledWith("");

      onCode.mockClear();

      // Wrong type
      window.dispatchEvent(
        new MessageEvent("message", {
          origin,
          data: { type: "not_oauth", code: "abc" },
        }),
      );

      // Non-string code (number)
      window.dispatchEvent(
        new MessageEvent("message", {
          origin,
          data: { type: "oauth_code", code: 12345 },
        }),
      );

      // Non-string code (null)
      window.dispatchEvent(
        new MessageEvent("message", {
          origin,
          data: { type: "oauth_code", code: null },
        }),
      );

      // Primitive payload (string)
      window.dispatchEvent(
        new MessageEvent("message", {
          origin,
          data: "oauth_code",
        }),
      );

      // Primitive payload (null)
      window.dispatchEvent(
        new MessageEvent("message", {
          origin,
          data: null,
        }),
      );

      // Object without code
      window.dispatchEvent(
        new MessageEvent("message", {
          origin,
          data: { type: "oauth_code" },
        }),
      );

      expect(onCode).not.toHaveBeenCalled();
    });

    it("does not listen for window.message if listenMessages is false or omitted", () => {
      const onCode = vi.fn();
      track(subscribeOAuthCode(onCode, null));

      window.dispatchEvent(
        new MessageEvent("message", {
          origin: window.location.origin,
          data: { type: "oauth_code", code: "should_be_ignored" },
        }),
      );

      expect(onCode).not.toHaveBeenCalled();
    });
  });

  describe("window.storage transport", () => {
    it("calls onCode and removes localStorage key BEFORE calling onCode", () => {
      const onCode = vi.fn();
      const removeItemSpy = vi.spyOn(Storage.prototype, "removeItem");
      let removedBeforeCallback = false;

      onCode.mockImplementation(() => {
        removedBeforeCallback = removeItemSpy.mock.calls.some(
          (call) => call[0] === "openproxy_oauth_code",
        );
      });

      track(subscribeOAuthCode(onCode, null));

      window.dispatchEvent(
        new StorageEvent("storage", {
          key: "openproxy_oauth_code",
          newValue: JSON.stringify({ code: "storage_secret_123" }),
        }),
      );

      expect(removeItemSpy).toHaveBeenCalledWith("openproxy_oauth_code");
      expect(removedBeforeCallback).toBe(true);
      expect(onCode).toHaveBeenCalledOnce();
      expect(onCode).toHaveBeenCalledWith("storage_secret_123");
    });

    it("requires no type field in storage payload", () => {
      const onCode = vi.fn();
      track(subscribeOAuthCode(onCode, null));

      window.dispatchEvent(
        new StorageEvent("storage", {
          key: "openproxy_oauth_code",
          newValue: JSON.stringify({ code: "only_code_field" }),
        }),
      );

      expect(onCode).toHaveBeenCalledOnce();
      expect(onCode).toHaveBeenCalledWith("only_code_field");
    });

    it("handles malformed JSON, falsy newValue, null parsed, and wrong keys safely", () => {
      const onCode = vi.fn();
      track(subscribeOAuthCode(onCode, null));

      // Malformed JSON
      expect(() => {
        window.dispatchEvent(
          new StorageEvent("storage", {
            key: "openproxy_oauth_code",
            newValue: "{invalid_json_str",
          }),
        );
      }).not.toThrow();

      // Null/empty newValue
      window.dispatchEvent(
        new StorageEvent("storage", {
          key: "openproxy_oauth_code",
          newValue: null,
        }),
      );
      window.dispatchEvent(
        new StorageEvent("storage", {
          key: "openproxy_oauth_code",
          newValue: "",
        }),
      );

      // Null parsed JSON
      window.dispatchEvent(
        new StorageEvent("storage", {
          key: "openproxy_oauth_code",
          newValue: "null",
        }),
      );

      // Primitive number in JSON
      window.dispatchEvent(
        new StorageEvent("storage", {
          key: "openproxy_oauth_code",
          newValue: "12345",
        }),
      );

      // Non-string code in object
      window.dispatchEvent(
        new StorageEvent("storage", {
          key: "openproxy_oauth_code",
          newValue: JSON.stringify({ code: 999 }),
        }),
      );

      // Unrelated key ignored
      window.dispatchEvent(
        new StorageEvent("storage", {
          key: "some_other_key",
          newValue: JSON.stringify({ code: "ignore_me" }),
        }),
      );

      expect(onCode).not.toHaveBeenCalled();
    });

    it("swallows errors thrown by onCode or removeItem in storage handler", () => {
      const throwingOnCode = vi.fn().mockImplementation(() => {
        throw new Error("subscriber callback crash");
      });
      track(subscribeOAuthCode(throwingOnCode, null));

      expect(() => {
        window.dispatchEvent(
          new StorageEvent("storage", {
            key: "openproxy_oauth_code",
            newValue: JSON.stringify({ code: "throw_test" }),
          }),
        );
      }).not.toThrow();

      expect(throwingOnCode).toHaveBeenCalledOnce();

      // Test removeItem throwing (e.g. security/quota error)
      vi.spyOn(Storage.prototype, "removeItem").mockImplementation(() => {
        throw new Error("localStorage access denied");
      });
      const normalOnCode = vi.fn();
      track(subscribeOAuthCode(normalOnCode, null));

      expect(() => {
        window.dispatchEvent(
          new StorageEvent("storage", {
            key: "openproxy_oauth_code",
            newValue: JSON.stringify({ code: "remove_throws" }),
          }),
        );
      }).not.toThrow();
      expect(normalOnCode).not.toHaveBeenCalled();
    });
  });

  describe("BroadcastChannel transport", () => {
    it("receives code via channel.onmessage with empty code support and no origin check", () => {
      const onCode = vi.fn();
      const fakeBC = new FakeBroadcastChannel();
      track(subscribeOAuthCode(onCode, fakeBC));

      expect(fakeBC.onmessage).toBeTypeOf("function");

      // Valid code
      fakeBC.onmessage?.call(fakeBC,
        new MessageEvent("message", {
          data: { type: "oauth_code", code: "bc_code_abc" },
        }),
      );
      expect(onCode).toHaveBeenCalledWith("bc_code_abc");

      // Empty string code
      fakeBC.onmessage?.call(fakeBC,
        new MessageEvent("message", {
          data: { type: "oauth_code", code: "" },
        }),
      );
      expect(onCode).toHaveBeenCalledWith("");

      // Invalid payload ignored
      onCode.mockClear();
      fakeBC.onmessage?.call(fakeBC,
        new MessageEvent("message", {
          data: { type: "other", code: "bad" },
        }),
      );
      fakeBC.onmessage?.call(fakeBC,
        new MessageEvent("message", {
          data: null,
        }),
      );
      expect(onCode).not.toHaveBeenCalled();
    });

    it("works when channel is null", () => {
      expect(() => {
        const unsub = subscribeOAuthCode(() => {}, null);
        unsub();
      }).not.toThrow();
    });
  });

  describe("disposer lifecycle", () => {
    it("unsubscribes window events but does not close or clear BroadcastChannel", () => {
      const onCode = vi.fn();
      const fakeBC = new FakeBroadcastChannel();
      const unsub = subscribeOAuthCode(onCode, fakeBC, true);

      // Disposer call
      unsub();

      // Window message after dispose
      window.dispatchEvent(
        new MessageEvent("message", {
          origin: window.location.origin,
          data: { type: "oauth_code", code: "after_unsub" },
        }),
      );

      // Window storage after dispose
      window.dispatchEvent(
        new StorageEvent("storage", {
          key: "openproxy_oauth_code",
          newValue: JSON.stringify({ code: "after_unsub_storage" }),
        }),
      );

      expect(onCode).not.toHaveBeenCalled();

      // Caller retains BroadcastChannel: disposer did NOT close it or clear onmessage
      expect(fakeBC.close).not.toHaveBeenCalled();
      expect(fakeBC.onmessage).not.toBeNull();
    });

    it("supports repeated subscribe and dispose cycles without residual listeners", () => {
      const onCode1 = vi.fn();
      const onCode2 = vi.fn();

      const unsub1 = subscribeOAuthCode(onCode1, null, true);
      const unsub2 = subscribeOAuthCode(onCode2, null, true);

      unsub1();

      window.dispatchEvent(
        new MessageEvent("message", {
          origin: window.location.origin,
          data: { type: "oauth_code", code: "only_subscriber_2" },
        }),
      );

      expect(onCode1).not.toHaveBeenCalled();
      expect(onCode2).toHaveBeenCalledOnce();
      expect(onCode2).toHaveBeenCalledWith("only_subscriber_2");

      unsub2();

      window.dispatchEvent(
        new MessageEvent("message", {
          origin: window.location.origin,
          data: { type: "oauth_code", code: "nobody_listening" },
        }),
      );

      expect(onCode2).toHaveBeenCalledOnce();
    });
  });
});
