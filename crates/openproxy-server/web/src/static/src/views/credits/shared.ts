// views/credits/shared.ts — module state of the credits surface.
//
// `requestRefresh` re-mounts into the surface the view currently lives in
// (tile or shell #main), never a detached container.

import { showToast } from "../../components/toast.js";
import { api } from "../../state/api.js";
import type { CatalogModel, TierDraft } from "./types.js";

// ── module state ────────────────────────────────────────────────────
export let tierDraft: TierDraft | null = null;
export let catalogModels: CatalogModel[] | null = null;
// Tier editor catalog browser state: kept module-scoped so the searchable,
// paged multi-select re-renders locally (no full view refetch per keystroke).
export let tierSearchQuery = "";
export let tierCatalogPage = 0;

export function setTierSearchQuery(v: string): void {
  tierSearchQuery = v;
}
export function setTierCatalogPage(v: number): void {
  tierCatalogPage = v;
}
export function resetTierCatalogBrowser(): void {
  tierSearchQuery = "";
  tierCatalogPage = 0;
}

let mountedContainer: HTMLElement | null = null;
let remount: ((el?: HTMLElement | null) => Promise<unknown>) | null = null;

export function setTierDraft(v: TierDraft | null): void {
  tierDraft = v;
}
export function setCatalogModels(v: CatalogModel[] | null): void {
  catalogModels = v;
}

/** The index view binds itself here so every module can re-render in place. */
export function bindRefresh(
  container: HTMLElement,
  mount: (el?: HTMLElement | null) => Promise<unknown>,
): void {
  mountedContainer = container;
  remount = mount;
}

/** Re-render in place on the live surface (tile or shell #main). */
export function requestRefresh(): void {
  const el = mountedContainer && mountedContainer.isConnected ? mountedContainer : undefined;
  void remount?.(el);
}

// ── model catalog (tier editor) ─────────────────────────────────────
// Full admin catalog: every model (all providers, active or not) plus every
// combo as its virtual `combo:<name>` id. The tier editor pages through it.
export async function ensureCatalog(): Promise<void> {
  if (catalogModels != null) return;
  try {
    const res = (await api("/credits/catalog")) as { models: Array<{ id: string; provider: string; active: boolean; providers: number }>; combos: Array<{ id: string; name: string; active: boolean }> };
    const models: CatalogModel[] = (res.models ?? []).map((m) => ({
      id: m.id,
      provider: m.provider,
      kind: "model" as const,
      active: m.active,
      providers: m.providers,
    }));
    const combos: CatalogModel[] = (res.combos ?? []).map((c) => ({
      id: c.id,
      name: c.name,
      kind: "combo" as const,
      active: c.active,
    }));
    setCatalogModels([...combos, ...models]);
  } catch (err) {
    // Never silently empty: a broken catalog must be visible, not a blank list.
    showToast(err instanceof Error ? `catalog: ${err.message}` : "catalog load failed", "error");
    setCatalogModels([]);
  }
}
