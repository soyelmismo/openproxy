// views/credits/types.ts — wire shapes of the credits surface.
// Mirrors handlers/admin/credits.rs + credits_gateway.rs + core packages/tiers.

export interface LotView {
  remaining_usd: string;
  expires_at: string | null;
}

export interface Balance {
  key_id: number;
  credit_based: boolean;
  balance_usd: string;
  balance_units: number;
  lots: LotView[];
}

export interface TxRow {
  kind: string;
  amount_usd: string;
  remaining_usd: string;
  status: string;
  order_code: string | null;
  tx_hash: string | null;
  basescan_url: string | null;
  created_at: string;
}

export interface QuotaWindow {
  cap_usd: number | null;
  spent_usd: number;
  resets_at: string | null;
}

export interface QuotaStatus {
  enabled: boolean;
  month: QuotaWindow;
  week: QuotaWindow;
  five_hours: QuotaWindow;
}

/** Public `/v1/credits/packages` payload (checkout + panel top-up). */
export interface PackageOffer {
  package_id: number | null;
  usd: number;
  ttl_days: number;
  label: string;
  credit_usd?: number;
  tier?: string;
}

export interface PackagesResponse {
  enabled: boolean;
  ttl_days: number[];
  min_topup_usd: number;
  offers: PackageOffer[];
}

export interface AdminPackage {
  id: number;
  name: string;
  price_usd: number;
  ttl_days: number;
  config: Record<string, unknown> | null;
  active: boolean;
  credit_usd: number | null;
  tier_id: number | null;
}

export interface AdminTierConfig {
  monthly_usd_cap: number | null;
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
  package_count: number;
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
  package_id: number | null;
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

/** Order created by POST /credits/topup (panel checkout, with QR). */
export interface Order {
  order_code: string;
  amount_units: number;
  amount_display: string;
  credit_units?: number;
  credit_display?: string;
  collector_address: string;
  qr_svg: string;
  expires_at: string;
}

/** Draft state of the tier editor (module-scoped, survives re-renders). */
export interface TierDraft {
  id: number | null;
  name: string;
  models: string[];
  monthly: string;
  weekly: string;
  fiveHour: string;
  active: boolean;
}

export interface CreditsData {
  /** Consumer surfaces — absent on the admin-only view (noise there). */
  balance?: Balance;
  quota?: QuotaStatus;
  packages?: PackagesResponse;
  transactions: TxRow[];
  adminPackages: AdminPackage[];
  adminTiers: AdminTier[];
}
