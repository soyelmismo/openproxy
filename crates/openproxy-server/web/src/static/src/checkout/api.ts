// checkout/api.ts — wire shapes + fetch helpers of the public checkout.
//
// Mirrors `credits_gateway.rs` (PlansResponse / CheckoutResponse /
// OrderStatus / ledger rows). Money surface: keep the types tight, never let
// the browser cache a response that may carry a plaintext key.

import type { PricingSchedule } from "../lib/pricing-schedule.js";

/** One entry of `PlanOffer.model_pricing`: the model's REAL public rates, so
 *  the checkout can turn the plan's credited balance into a token estimate.
 *  Not the upstream cost, not the price paid for the plan. */
export interface ModelPricing {
  name: string;
  /** True when `name` is a combo id rather than a model id. */
  combo: boolean;
  pricing_input_per_1m: number | null;
  pricing_output_per_1m: number | null;
  pricing_cached_per_1m: number | null;
  /** Time-of-day tariff of the combo. The shape is the shared schedule
   *  contract (`lib/pricing-schedule.ts`: ISO weekdays 1=Mon…7=Sun, half-open
   *  UTC windows, "YYYY-MM-DD" off-peak dates); the checkout normalises it
   *  before pricing and withholds the estimate when it does not parse. */
  pricing_schedule?: PricingSchedule | null;
}

export interface PlanOffer {
  tier_id: number;
  name: string;
  usd: number;
  /** Balance credited on payment (leverage). Absent == 1:1. */
  credit_usd?: number;
  /** Plan lifetime: the credited balance expires this many days after payment. */
  ttl_days: number;
  /** Seats still sellable. Absent = unlimited; 0 = sold out. */
  seats_left?: number | null;
  /** Model ids the plan unlocks (display only). */
  models: string[];
  /** Combo ids the plan unlocks (display names, no `combo:` prefix). */
  combos: string[];
  /** Rates of the models above. Optional: an older gateway omits it and the
   *  cards simply show no token estimate. */
  model_pricing?: ModelPricing[];
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

/// POST /v1/credits/me/rotate: the new plaintext rides once in this response
/// (no-store); the previous key is already dead when it arrives.
export interface RotationResponse {
  rotated: boolean;
  key: string;
  prefix: string;
  warning: string;
}

export interface OrderStatus {
  order_code: string;
  status: string;
  block_number: number | null;
  expires_at: string;
  key_revealed: boolean;
}

export interface ApiResult<T> {
  ok: boolean;
  status: number;
  body: T & { error?: { code?: string; message?: string } };
}

/** Fetch JSON with the HTTP status preserved. Never cached.
 * Exported for the payment poll: the poll must SEE the status code to tell a
 * Cloudflare edge challenge (403/503 HTML, no JSON body) apart from our own
 * API errors (always JSON). */
export async function requestJson<T>(url: string, init?: RequestInit): Promise<ApiResult<T>> {
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
