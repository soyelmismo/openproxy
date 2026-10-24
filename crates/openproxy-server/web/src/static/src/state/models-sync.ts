// Applies `models_refreshed` WS events to state.models and state.providers
// without a page reload.

import { subscribeWs } from "./ws-bus.js";
import { state } from "./index.js";
import type { Combo } from "../lib/types/combos.js";
import { api } from "./api.js";
import { requestUpdate } from "./reactive.js";
import { showToast } from "../components/toast.js";
import { detailProviderId } from "../views/providers/shared.js";

export async function ensureCombosLoaded(): Promise<Combo[]> {
  if (Array.isArray(state.combos) && state.combos.length > 0) {
    return state.combos;
  }
  try {
    const list = (await api("/combos")) as Combo[];
    if (Array.isArray(list)) {
      state.combos = list;
    }
    return state.combos;
  } catch {
    return state.combos || [];
  }
}

let initialized = false;

export function initModelsSync(): void {
  if (initialized) return;
  initialized = true;

  // Priming fetch of combos and providers so modals and log rows have names immediately
  void ensureCombosLoaded();

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
      // Best effort: the next event or poll refreshes.
    }

    if (detailProviderId === data.provider_id) {
      try {
        state.models = (await api(
          "/models?provider_id=" + encodeURIComponent(data.provider_id)
        )) as typeof state.models;
        state.modelsComplete = false;
        requestUpdate();
        let newSuffix = "";
        if (data.new_model_ids && data.new_model_ids.length > 0) {
          newSuffix = data.new_model_ids.length <= 3
            ? ` New: ${data.new_model_ids.join(", ")}.`
            : ` New: ${data.new_model_ids.slice(0, 3).join(", ")} (+${data.new_model_ids.length - 3} more).`;
        }
        if (data.models_refreshed > 0) {
          showToast(
            `Refreshed ${data.models_refreshed} model${data.models_refreshed === 1 ? "" : "s"} for ${data.provider_id}.${newSuffix}`,
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
        // Best effort: the next event or poll refreshes.
      }
    } else {
      requestUpdate();
    }
  });
}

/** Poll for newly discovered models after an account or key mutation, for the
 *  case where the WS is reconnecting or lagged. */
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
