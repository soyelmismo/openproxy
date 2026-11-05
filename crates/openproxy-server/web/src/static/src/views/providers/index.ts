// views/providers/index.ts — `mountProviders({detailId?}, target?)`, imported by
// state/router.ts for `#/providers` and `#/providers/:id`.
//
// Sets the module-local detail/loadError state shared with list.ts and
// detail.ts, cold-paints providers/accounts/models/proxies in parallel, then
// schedules a background refresh so warm-cache quotas don't freeze. UI lives
// in list.ts and detail.ts; this file owns only the lifecycle.

import { state } from '../../state/index.js';
import { api, fetchFormats } from '../../state/api.js';
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
  target?: HTMLElement,
): Promise<(() => void) | void> {
  const container = target ?? document.getElementById('main');
  if (!container) return;

  let active = true;
  let viewCleanup: (() => void) | null = null;
  const cleanup = (): void => {
    active = false;
    if (viewCleanup) {
      viewCleanup();
      viewCleanup = null;
    }
  };

  void fetchFormats();

  if (opts.detailId) {
    setDetailProviderId(opts.detailId);
    setLoadError(null);
    // Row ids are provider-scoped, so a provider switch clears the selection.
    if (state.selectedModelsProvider !== opts.detailId) {
      state.selectedModels.clear();
      state.selectedModelsProvider = opts.detailId;
    }
    viewCleanup = mountView(container, renderProviderDetail);
    try {
      // Warm re-render renders from cache first, then refreshes in the background
      // so quotas and statuses don't freeze.
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
        if (!active || !container.isConnected) return cleanup;
        state.providers = providers;
        state.accounts = accounts;
        state.models = models;
        state.modelsComplete = false;
        state.proxies = proxies;
      } else {
        const resolvedProxies = await proxiesPromise;
        if (!active || !container.isConnected) return cleanup;
        state.proxies = resolvedProxies;
        Promise.all([
          api('/providers') as Promise<Provider[]>,
          api('/accounts') as Promise<Account[]>,
          api(
            '/models?provider_id=' + encodeURIComponent(opts.detailId),
          ) as Promise<Model[]>,
        ])
          .then(([p, a, m]) => {
            if (!active || !container.isConnected) return;
            state.providers = p;
            state.accounts = a;
            state.models = m;
            state.modelsComplete = false;
            requestUpdate();
          })
          .catch((e) => console.error('Background refresh failed:', e));
      }
      if (!active || !container.isConnected) return cleanup;
      requestUpdate();
    } catch (e: unknown) {
      if (!active || !container.isConnected) return cleanup;
      setLoadError(e instanceof Error ? e.message : String(e));
      requestUpdate();
    }
    return cleanup;
  }

  setDetailProviderId(null);
  setLoadError(null);
  viewCleanup = mountView(container, renderProvidersGrid);
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
    if (!active || !container.isConnected) return cleanup;
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
          if (!active || !container.isConnected) return;
          state.providers = p;
          state.accounts = a;
          requestUpdate();
        })
        .catch((e) => console.error('Background refresh failed:', e));
    }
  } catch (e: unknown) {
    if (!active || !container.isConnected) return cleanup;
    setLoadError(e instanceof Error ? e.message : String(e));
    requestUpdate();
  }
  return cleanup;
}