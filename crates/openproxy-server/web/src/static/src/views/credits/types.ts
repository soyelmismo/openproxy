// views/credits/types.ts — wire shapes of the credits surface.
// Mirrors handlers/admin/credits.rs + credits_gateway.rs + core credits/tiers.

export interface QuotaWindow {
  cap_usd: number | null;
  spent_usd: number;
  resets_at: string | null;
}

export interface PlanStatus {
  name: string;
  credit_usd: number;
  balance_usd: number;
  expires_at: string | null;
}

export interface QuotaStatus {
  enabled: boolean;
  week: QuotaWindow;
  five_hours: QuotaWindow;
  plan: PlanStatus | null;
}

/** Public `/v1/credits/plans` offer (checkout + panel top-up). */
export interface PlanOffer {
  tier_id: number;
  name: string;
  usd: number;
  credit_usd?: number;
  ttl_days: number;
  models: string[];
}

export interface PlansResponse {
  enabled: boolean;
  plans: PlanOffer[];
}

export interface AdminTierConfig {
  weekly_usd_cap: number | null;
  five_hour_usd_cap: number | null;
  allowed_models: string[] | null;
  allowed_providers: string[] | null;
  allowed_combos: string[] | null;
  only_whitelisted: boolean | null;
}

export interface AdminTier {
  id: number;
  name: string;
  config: AdminTierConfig;
  active: boolean;
  /** Plan commerce: what the buyer pays / gets / how long it lives. */
  price_usd: number | null;
  credit_usd: number | null;
  ttl_days: number;
  /** Live keys inheriting this tier (an edit re-shapes exactly these). */
  key_count: number;
}

export interface AdminOrder {
  id: number;
  order_code: string;
  key_id: number | null;
  amount_units: number;
  base_units: number;
  ttl_days: number;
  status: string;
  tx_hash: string | null;
  tier_id: number | null;
  tier_name: string | null;
  created_at: string;
  paid_at: string | null;
  credit_units: number;
  payer_email?: string | null;
}

/** GET /credits/admin/orders payload: page + filtered total. */
export interface OrdersPage {
  orders: AdminOrder[];
  total: number;
}

export interface CatalogModel {
  id: string;
  provider?: string;
  kind: "model" | "combo";
  active: boolean;
  /** models: how many providers serve the same id; combos: display name. */
  providers?: number;
  name?: string;
}

/** Draft state of the tier editor (module-scoped, survives re-renders). */
export interface TierDraft {
  id: number | null;
  name: string;
  models: string[];
  weekly: string;
  fiveHour: string;
  active: boolean;
  /** Plan commerce (blank = draft, not listed for sale). */
  price: string;
  credit: string;
  ttlDays: string;
}

export interface CreditsData {
  /** Consumer surfaces — absent on the admin-only view (noise there). */
  quota?: QuotaStatus;
  plans?: PlansResponse;
  adminTiers: AdminTier[];
}
