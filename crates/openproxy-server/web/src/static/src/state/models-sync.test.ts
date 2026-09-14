// state/models-sync.test.ts — tests for real-time model synchronization.

import { describe, it, expect, vi, beforeEach } from "vitest";
import { initModelsSync } from "./models-sync.js";
import { dispatchWs } from "./ws-bus.js";
import { state } from "./index.js";
import * as apiModule from "./api.js";
import * as sharedView from "../views/providers/shared.js";
import type { WsEnvelope } from "../views/logs.js";

describe("models-sync", () => {
  beforeEach(() => {
    state.models = [];
    state.providers = [];
    sharedView.setDetailProviderId(null);
    vi.restoreAllMocks();
  });

  it("updates state.models and state.providers when models_refreshed event matches detailProviderId", async () => {
    initModelsSync();
    sharedView.setDetailProviderId("my-provider");

    const fakeModels = [
      {
        row_id: 1,
        provider_id: "my-provider",
        model_id: "model-1",
        display_name: null,
        discovered_at: "2026-01-01",
        expires_at: null,
        timeout_overrides_json: null,
        last_test_at: null,
        context_length: null,
        max_output_tokens: null,
        capabilities_json: null,
        family: null,
        model_type: "chat",
        input_modalities_json: null,
        output_modalities_json: null,
        last_test_status: null,
        target_format: "openai",
        active: true,
        custom: false,
      },
    ];

    const fakeProviders = [
      {
        id: "my-provider",
        name: "My Provider",
        base_url: "https://api.example.com",
        format: "openai",
        auth_type: "api_key",
        active: true,
      },
    ];

    vi.spyOn(apiModule, "api").mockImplementation(async (path: string) => {
      if (path.startsWith("/models?provider_id=my-provider")) {
        return fakeModels;
      }
      if (path === "/providers") {
        return fakeProviders;
      }
      return [];
    });

    const envelope: WsEnvelope = {
      type: "models_refreshed",
      data: {
        provider_id: "my-provider",
        models_refreshed: 1,
        new_model_ids: ["model-1"],
        models_activated: 1,
      },
    };

    dispatchWs(envelope);

    // Wait microtasks
    await new Promise((resolve) => setTimeout(resolve, 10));

    expect(state.models).toHaveLength(1);
    expect(state.models[0]?.model_id).toBe("model-1");
    expect(state.providers).toHaveLength(1);
  });

  it("ignores models_refreshed event for different provider when in detail view", async () => {
    initModelsSync();
    sharedView.setDetailProviderId("other-provider");
    state.models = [];

    vi.spyOn(apiModule, "api").mockImplementation(async (path: string) => {
      if (path === "/providers") return [];
      if (path.startsWith("/models")) return [{ model_id: "should-not-reach" }];
      return [];
    });

    const envelope: WsEnvelope = {
      type: "models_refreshed",
      data: {
        provider_id: "unrelated-provider",
        models_refreshed: 5,
      },
    };

    dispatchWs(envelope);
    await new Promise((resolve) => setTimeout(resolve, 10));

    expect(state.models).toHaveLength(0);
  });
});
