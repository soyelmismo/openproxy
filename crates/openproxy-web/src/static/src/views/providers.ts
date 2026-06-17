// views/providers.ts — entry point for the providers route. The
// router dispatches to `mountProviders` with either `{ detailId }`
// (when the URL is `#/providers/:id`) or `{}` (when the URL is
// `#/providers`). The grid and the detail live in their own
// modules: views/provider-grid.ts and views/provider-detail.ts.
//
// Per spec §3 + §13.8 we do not use inline onclick handlers —
// every button / select / checkbox is wired via
// `data-action="X" data-arg-N="..."` and the document-level shim
// in app.js dispatches them.

import { state } from "../state/index.js";
import { api } from "../state/api.js";
import { escapeHtml } from "../lib/escape.js";
import { renderProviderGrid } from "./provider-grid.js";
import { renderProviderDetail } from "./provider-detail.js";

export interface MountProvidersOpts {
  detailId?: string;
}

export async function mountProviders(opts: MountProvidersOpts = {}): Promise<void> {
  const main = document.getElementById("main");
  if (!main) return;
  if (opts && opts.detailId) {
    return renderProviderDetail(opts.detailId);
  }
  // Cold-paint: fetch before rendering. The background poll will
  // keep the cache fresh after the first paint.
  main.innerHTML = `<div class="loading">Loading...</div>`;
  try {
    const [providers, accounts, models] = await Promise.all([
      state.providers && state.providers.length ? Promise.resolve(state.providers) : api("/providers") as Promise<typeof state.providers>,
      state.accounts && state.accounts.length ? Promise.resolve(state.accounts) : api("/accounts") as Promise<typeof state.accounts>,
      state.models && state.models.length ? Promise.resolve(state.models) : api("/models") as Promise<typeof state.models>,
    ]);
    state.providers = providers;
    state.accounts = accounts;
    state.models = models;
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : String(e);
    main.innerHTML = `<div class="banner banner-error">${escapeHtml(msg)}</div>`;
    return;
  }
  renderProviderGrid();
}
