// checkout/api.ts — wire shapes + fetch helpers of the public checkout.
//
// Mirrors `credits_gateway.rs` (PlansResponse / CheckoutResponse /
// OrderStatus / ledger rows). Money surface: keep the types tight.

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
  base_usd: number;
  credit_usd?: number;
  amount_display: string;
  collector_address: string;
  qr_svg: string;
  expires_at: string;
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
  status: string;
  block_number: number | null;
}

export interface ApiResult<T> {
  ok: boolean;
  body: T & { error?: { message?: string } };
}

/** GET JSON with a typed body (fetch rejects on network errors only). */
export async function getJson<T>(url: string, init?: RequestInit): Promise<T> {
  const r = await fetch(url, init);
  return (await r.json()) as T;
}

/** POST JSON → { ok, body } envelope the checkout reports to the buyer. */
export async function postJson<T>(url: string, body: unknown, headers?: Record<string, string>): Promise<ApiResult<T>> {
  const r = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json", ...headers },
    body: JSON.stringify(body),
  });
  const b = await r.json();
  return { ok: r.ok, body: b as ApiResult<T>["body"] };
}

/** Bearer-auth GET (the buyer's own key, `chat` scope). */
export async function getAuth<T>(url: string, key: string): Promise<T> {
  const r = await fetch(url, { headers: { Authorization: "Bearer " + key } });
  if (!r.ok) throw new Error("invalid key or credits disabled");
  return (await r.json()) as T;
}
