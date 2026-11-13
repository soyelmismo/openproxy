// lib/keys-api.ts — the API key list together with its ownership scope.
//
// `/keys` keeps its original payload; ownership lives in `api_keys.kind` and
// is served by `/keys/kinds`. The kind map is cached here instead of being
// stuck onto the rows: `state.apiKeys` has several writers (playground,
// logs, workspace prefetch) that would silently drop an attached field, so
// every view resolves ownership through `kindOf(id)` at render time.

import { api } from "../state/api.js";
import type { ApiKey } from "./types/api.js";

export type ApiKeyKind = "personal" | "customer";

/** An API key row with its ownership scope attached. */
export type ApiKeyWithKind = ApiKey & { kind: ApiKeyKind };

let kindById = new Map<number, ApiKeyKind>();

/** Refresh the ownership map. Cheap: two columns per key. Returns the raw
 *  rows so a caller can merge them into its own payload in the same pass. */
export async function loadKeyKinds(): Promise<{ id: number; kind: ApiKeyKind }[]> {
  const rows = await api("/keys/kinds") as { id: number; kind: ApiKeyKind }[];
  kindById = new Map(rows.map((r) => [r.id, r.kind]));
  return rows;
}

/** Ownership of one key. Unknown keys are personal: that is the default the
 *  schema gives them and the safe side for a UI filter. */
export function kindOf(id: number): ApiKeyKind {
  return kindById.get(id) ?? "personal";
}

/** Record a flip without a refetch. */
export function rememberKind(id: number, kind: ApiKeyKind): void {
  kindById.set(id, kind);
}

/** Fetch every key with its ownership scope merged in. */
export async function fetchKeysWithKind(): Promise<ApiKeyWithKind[]> {
  const [keys] = await Promise.all([api("/keys") as Promise<ApiKey[]>, loadKeyKinds()]);
  return keys.map((k) => ({ ...k, kind: kindOf(k.id) }));
}
