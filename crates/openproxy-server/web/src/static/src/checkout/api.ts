// checkout/api.ts — wire shapes + fetch helpers of the public checkout.
//
// Mirrors `credits_gateway.rs` (PlansResponse / CheckoutResponse /
// OrderStatus / ledger rows). Money surface: keep the types tight, never let
// the browser cache a response that may carry a plaintext key.

export interface PlanOffer {
  tier_id: number;
  name: string;
  usd: number;
  /** Balance credited on payment (leverage). Absent == 1:1. */
  credit_usd?: number;
  /** Plan lifetime: the credited balance expires this many days after payment. */
  ttl_days: number;
  /** Model ids the plan unlocks (display only). */
  models: string[];
  /** Combo ids the plan unlocks (display names, no `combo:` prefix). */
  combos: string[];
}

export interface PlansResponse {
  enabled: boolean;
  plans: PlanOffer[];
}

export interface CheckoutOrder {
  order_code: string;
  status: string;
  usd: number;
  base_usd: number;
  credit_usd?: number;
  amount_units: number;
  amount_display: string;
  chain_id: number;
  usdc_address: string;
  collector_address: string;
  qr_uri: string;
  qr_svg: string;
  expires_at: string;
  payment_window_secs: number;
  /** How long after `expires_at` a transfer is still accepted (block time). */
  confirmation_grace_secs?: number;
  /** Set when the order tops up an existing key (never mints/reveals). */
  api_key_id?: number | null;
}

/** GET /v1/credits/me/plan — the key's plan (null: no plan assigned). */
export interface MyPlanResponse {
  plan: {
    name: string;
    credit_usd: number;
    balance_usd: number;
    expires_at: string | null;
  } | null;
}

export interface OrderStatus {
  order_code: string;
  status: string;
  api_key_id: number | null;
  block_number: number | null;
  expires_at: string;
  key_revealed: boolean;
}

export interface ApiResult<T> {
  ok: boolean;
  status: number;
  body: T & { error?: { code?: string; message?: string } };
}

/** Fetch JSON with the HTTP status preserved. Never cached. */
async function requestJson<T>(url: string, init?: RequestInit): Promise<ApiResult<T>> {
  const r = await fetch(url, {
    cache: "no-store",
    ...init,
    headers: { Accept: "application/json", ...(init?.headers ?? {}) },
  });
  let body: ApiResult<T>["body"];
  try {
    body = (await r.json()) as ApiResult<T>["body"];
  } catch {
    body = {} as ApiResult<T>["body"];
  }
  return { ok: r.ok, status: r.status, body };
}

/** GET JSON; a non-2xx answer is an error, not a silently empty body. */
export async function getJson<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await requestJson<T>(url, init);
  if (!res.ok) {
    throw new Error(res.body.error?.message || "request failed (HTTP " + res.status + ")");
  }
  return res.body as T;
}

/** POST JSON → { ok, status, body } envelope the checkout reports to the buyer. */
export async function postJson<T>(
  url: string,
  body: unknown,
  headers?: Record<string, string>,
): Promise<ApiResult<T>> {
  return requestJson<T>(url, {
    method: "POST",
    headers: { "Content-Type": "application/json", ...headers },
    body: JSON.stringify(body),
  });
}

/** Bearer-auth GET (the buyer's own key, any buyer scope). */
export async function getAuth<T>(url: string, key: string): Promise<T> {
  const res = await requestJson<T>(url, {
    headers: { Authorization: "Bearer " + key },
  });
  if (!res.ok) throw new Error(res.body.error?.message || "invalid key or credits disabled");
  return res.body as T;
}
