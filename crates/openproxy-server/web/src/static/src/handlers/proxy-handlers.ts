// handlers/proxy-handlers.ts — sync, test, delete proxies, open the custom proxy modal.

import { html, render } from 'lit-html';
import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { requestUpdate } from "../state/reactive.js";
import { showToast } from "../components/toast.js";
import { ensureModalRoot, showApiError } from "../lib/ui-utils.js";
import { t } from "../i18n/index.js";

export async function reloadProxies(queryParams?: Record<string, string | number>): Promise<void> {
  try {
    const params = new URLSearchParams();
    if (queryParams) {
      Object.entries(queryParams).forEach(([k, v]) => {
        if (v !== undefined && v !== null && v !== "") {
          params.set(k, String(v));
        }
      });
    }
    const queryString = params.toString();
    const url = queryString ? `/proxies?${queryString}` : "/proxies?limit=50";

    const [proxies, summary] = await Promise.all([
      api(url),
      api("/proxies/summary"),
    ]);
    state.proxies = proxies as typeof state.proxies;
    state.proxySummary = summary as typeof state.proxySummary;
    requestUpdate();
  } catch (err: unknown) {
    console.error("reloadProxies failed", err);
  }
}

export async function syncProxies(): Promise<void> {
  showToast(t("proxies.toast.sync_started"), "info");
  try {
    const res = (await api("/proxies/sync", { method: "POST" })) as {
      fetched: number;
      added: number;
      errors: string[];
    };
    showToast(
      t("proxies.toast.sync_success", { added: res.added, fetched: res.fetched }),
      "success"
    );
    if (res.errors && res.errors.length > 0) {
      showToast("Sync warning:\n" + res.errors.join("\n"), "warning");
    }
    await reloadProxies();
  } catch (e: unknown) {
    showApiError(e, "Sync failed");
  }
}

export async function testProxy(id: string): Promise<void> {
  try {
    const res = (await api(`/proxies/${id}/test`, { method: "POST" })) as {
      host: string;
      port: number;
      status: string;
      latency_ms: number | null;
    };
    if (res.status === "alive") {
      showToast(
        t("proxies.toast.test_success", {
          host: res.host,
          port: res.port,
          status: res.status,
          latency: res.latency_ms || 0,
        }),
        "success"
      );
    } else {
      showToast(t("proxies.toast.test_failed", { host: res.host, port: res.port }), "error");
    }
    await reloadProxies();
  } catch (e: unknown) {
    showApiError(e, "Test failed");
  }
}

export async function testAllProxies(): Promise<void> {
  try {
    await api("/proxies/test-all", { method: "POST" });
    showToast(t("proxies.toast.test_all_started"), "info");
  } catch (e: unknown) {
    showApiError(e, "Test All failed");
  }
}

export async function deleteProxy(id: string): Promise<void> {
  if (!confirm("Are you sure you want to delete this proxy?")) return;
  try {
    await api(`/proxies/${id}`, { method: "DELETE" });
    showToast(t("proxies.toast.delete_success"), "success");
    await reloadProxies();
  } catch (e: unknown) {
    showApiError(e, "Delete failed");
  }
}

export function showAddCustomProxy(): void {
  const wrapper = document.createElement("div");
  ensureModalRoot().appendChild(wrapper);
  render(
    html`
      <div
        class="modal-bg"
        id="add-proxy-modal"
        @click=${(e: Event) => {
        if (e.target === e.currentTarget) wrapper.remove();
      }}
      >
        <div class="modal">
          <div class="modal-header">
            <h2>${t("proxies.add_modal.title")}</h2>
            <button
              type="button"
              class="close-btn"
              @click=${() => wrapper.remove()}
              aria-label="Close"
            >
              &times;
            </button>
          </div>
          <form
            @submit=${(e: Event) => {
        e.preventDefault();
        void createCustomProxy(e, wrapper);
      }}
          >
            <div class="modal-body">
              <div class="field">
                <label for="proxy-host">${t("proxies.add_modal.host")}</label>
                <input
                  id="proxy-host"
                  name="host"
                  type="text"
                  placeholder="1.2.3.4 or example.com"
                  required
                />
              </div>
              <div class="field">
                <label for="proxy-port">${t("proxies.add_modal.port")}</label>
                <input
                  id="proxy-port"
                  name="port"
                  type="number"
                  min="1"
                  max="65535"
                  value="8080"
                  required
                />
              </div>
              <div class="field">
                <label for="proxy-type">${t("proxies.add_modal.type")}</label>
                <select id="proxy-type" name="type">
                  <option value="http">HTTP</option>
                  <option value="https">HTTPS</option>
                  <option value="socks4">SOCKS4</option>
                  <option value="socks5">SOCKS5</option>
                </select>
              </div>
              <div class="field">
                <label for="proxy-country">${t("proxies.add_modal.country")}</label>
                <input
                  id="proxy-country"
                  name="country_code"
                  type="text"
                  placeholder="US"
                  maxlength="2"
                />
              </div>
            </div>
            <div class="modal-footer">
              <button type="button" @click=${() => wrapper.remove()}>
                ${t("common.cancel")}
              </button>
              <button type="submit" class="primary">${t("proxies.btn.add")}</button>
            </div>
          </form>
        </div>
      </div>
    `,
    wrapper
  );
}

export async function createCustomProxy(e: Event, wrapper: HTMLElement): Promise<void> {
  const target = e.target;
  if (!(target instanceof HTMLFormElement)) return;
  const f = new FormData(target);
  const host = (f.get("host") || "").toString();
  const port = Number(f.get("port"));
  const type = (f.get("type") || "http").toString();
  const country_code = f.get("country_code")?.toString().toUpperCase() || null;
  const body = {
    host,
    port,
    type,
    country_code,
  };
  try {
    await api("/proxies", { method: "POST", body: JSON.stringify(body) });
    showToast(
      t("proxies.toast.add_success", { host: body.host, port: body.port }),
      "success"
    );
    wrapper.remove();
    await reloadProxies();
  } catch (err: unknown) {
    showApiError(err, "Error");
  }
}
