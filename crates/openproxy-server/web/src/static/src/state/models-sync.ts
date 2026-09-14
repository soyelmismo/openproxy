// state/models-sync.ts — real-time synchronization for provider models.
//
// Listens to WebSocket "models_refreshed" events dispatched by ws-bus,
// and automatically updates state.models and state.providers without
// requiring a full page reload. Also exports an active poll watcher
// for when accounts/keys are created or updated.

import { subscribeWs } from "./ws-bus.js";
import { state } from "./index.js";
import { api } from "./api.js";
import { requestUpdate } from "./reactive.js";
import { showToast } from "../components/toast.js";
import { detailProviderId } from "../views/providers/shared.js";

let initialized = false;

export function initModelsSync(): void {
  if (initialized) return;
  initialized = true;

  subscribeWs("models_refreshed", async (msg) => {
    const data = msg.data as
      | {
          provider_id: string;
          models_refreshed: number;
          new_model_ids?: string[];
          models_activated?: number;
        }
      | undefined;

    if (!data || !data.provider_id) return;

    try {
      state.providers = (await api("/providers")) as typeof state.providers;
    } catch {
      // Best-effort
    }

    if (detailProviderId === data.provider_id) {
      try {
        state.models = (await api(
          "/models?provider_id=" + encodeURIComponent(data.provider_id)
        )) as typeof state.models;
        state.modelsComplete = false;
        requestUpdate();
        if (data.models_refreshed > 0) {
          showToast(
            `Refreshed ${data.models_refreshed} model${data.models_refreshed === 1 ? "" : "s"} for ${data.provider_id}`,
            "success"
          );
        }
      } catch (err) {
        console.error("[openproxy] failed to fetch updated models:", err);
      }
    } else if (location.hash.startsWith("#/models")) {
      try {
        state.models = (await api("/models")) as typeof state.models;
        state.modelsComplete = true;
        requestUpdate();
      } catch {
        // Best-effort
      }
    } else {
      requestUpdate();
    }
  });
}

/** Active watcher when accounts or keys are mutated to guarantee UI updates
 *  even if the WebSocket is momentarily reconnecting or lagged. */
export function pollForDiscoveredModels(providerId: string): void {
  let attempts = 0;
  const maxAttempts = 6;
  const timer = setInterval(async () => {
    attempts++;
    if (detailProviderId && detailProviderId !== providerId) {
      clearInterval(timer);
      return;
    }
    try {
      const fresh = (await api(
        "/models?provider_id=" + encodeURIComponent(providerId)
      )) as typeof state.models;
      const currentCount = (state.models || []).filter(
        (m) => m.provider_id === providerId
      ).length;
      if (fresh.length > 0 && fresh.length !== currentCount) {
        state.models = fresh;
        state.modelsComplete = false;
        state.providers = (await api("/providers")) as typeof state.providers;
        requestUpdate();
        clearInterval(timer);
      } else if (attempts >= maxAttempts) {
        clearInterval(timer);
      }
    } catch {
      if (attempts >= maxAttempts) clearInterval(timer);
    }
  }, 1500);
}
