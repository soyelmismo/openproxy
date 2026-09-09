// views/providers/index.ts — providers view orchestrator.
//
// Public entry point: `mountProviders({detailId?})`. The router in
// `state/router.ts` imports this symbol directly:
//
//   - `#/providers`            → `mountProviders()`
//   - `#/providers/:id`        → `mountProviders({ detailId: ctx })`
//
// On mount we:
//   1. Set the module-local `detailProviderId` / `loadError` (shared
//      with list.ts / detail.ts via `./shared`).
//   2. Cold-paint providers / accounts / models / proxies via
//      `Promise.all` so the first render has data.
//   3. Schedule a background refresh so quotas and statuses don't
//      freeze when the user navigates back into a warm cache.
//
// All UI rendering lives in `./list.ts` (grid) and `./detail.ts`
// (per-provider). This file is intentionally thin — it owns only
// the lifecycle.

import { state } from '../../state/index.js';
import { api } from '../../state/api.js';
import { mountView, requestUpdate } from '../../state/reactive.js';
import type { Account, Model, Provider, FreeProxy } from '../../lib/types/api.js';
import { setDetailProviderId, setLoadError } from './shared.js';
import { renderProvidersGrid } from './list.js';
import { renderProviderDetail } from './detail.js';

export interface MountProvidersOpts {
  detailId?: string;
}

export async function mountProviders(
  opts: MountProvidersOpts = {},
): Promise<(() => void) | void> {
  const main = document.getElementById('main');
  if (!main) return;

  if (opts.detailId) {
    setDetailProviderId(opts.detailId);
    setLoadError(null);
    // Switching providers always starts with an empty selection —
    // visible row_ids live in the previous provider's table.
    if (state.selectedModelsProvider !== opts.detailId) {
      state.selectedModels.clear();
      state.selectedModelsProvider = opts.detailId;
    }
    const cleanup = mountView(main, renderProviderDetail);
    try {
      // Cold paint: fetch providers/accounts/models. Warm re-render
      // (from cache after navigate()) skips the network initially, but
      // does a background refresh to prevent frozen quotas and statuses.
      const proxiesPromise = api('/proxies?status=alive') as Promise<FreeProxy[]>;
      if (state.providers.length === 0) {
        const [providers, accounts, models, proxies] = await Promise.all([
          api('/providers') as Promise<Provider[]>,
          api('/accounts') as Promise<Account[]>,
          api(
            '/models?provider_id=' + encodeURIComponent(opts.detailId),
          ) as Promise<Model[]>,
          proxiesPromise,
        ]);
        state.providers = providers;
        state.accounts = accounts;
        state.models = models;
        state.modelsComplete = false;
        state.proxies = proxies;
      } else {
        state.proxies = await proxiesPromise;
        Promise.all([
          api('/providers') as Promise<Provider[]>,
          api('/accounts') as Promise<Account[]>,
          api(
            '/models?provider_id=' + encodeURIComponent(opts.detailId),
          ) as Promise<Model[]>,
        ])
          .then(([p, a, m]) => {
            state.providers = p;
            state.accounts = a;
            state.models = m;
            state.modelsComplete = false;
            requestUpdate();
          })
          .catch((e) => console.error('Background refresh failed:', e));
      }
      requestUpdate();
    } catch (e: unknown) {
      setLoadError(e instanceof Error ? e.message : String(e));
      requestUpdate();
    }
    return cleanup;
  }

  // Grid view.
  setDetailProviderId(null);
  setLoadError(null);
  const cleanup = mountView(main, renderProvidersGrid);
  try {
    const hasCache = state.providers && state.providers.length > 0;
    const [providers, accounts, proxies] = await Promise.all([
      hasCache
        ? Promise.resolve(state.providers)
        : (api('/providers') as Promise<Provider[]>),
      hasCache && state.accounts
        ? Promise.resolve(state.accounts)
        : (api('/accounts') as Promise<Account[]>),
      api('/proxies?status=alive') as Promise<FreeProxy[]>,
    ]);
    state.providers = providers;
    state.accounts = accounts;
    state.proxies = proxies;
    requestUpdate();

    if (hasCache) {
      Promise.all([
        api('/providers') as Promise<Provider[]>,
        api('/accounts') as Promise<Account[]>,
      ])
        .then(([p, a]) => {
          state.providers = p;
          state.accounts = a;
          requestUpdate();
        })
        .catch((e) => console.error('Background refresh failed:', e));
    }
  } catch (e: unknown) {
    setLoadError(e instanceof Error ? e.message : String(e));
    requestUpdate();
  }
  return cleanup;
}