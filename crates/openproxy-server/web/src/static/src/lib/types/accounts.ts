import type {
  AccountId,
  ApiKeyId,
  AuthType,
  HealthStatus,
  ProviderFormat,
  ProviderId,
} from "./common";

export interface ProviderMetadata {
  built_in: boolean;
  deletable: boolean;
  supports_quota: boolean;
  quota_refresh_supported: boolean;
}

export interface Provider {
  id: ProviderId;
  name: string;
  base_url: string;
  auth_type: AuthType;
  format: ProviderFormat;
  extra_headers_json: string | null;
  auto_activate_keyword: string | null;
  active: boolean;
  created_at: string;
  use_proxies: boolean;
  current_proxy_id: string | null;
  proxy_rotation_errors: string;
  proxy_rotation_mode: string;
  has_favicon?: boolean;
  favicon_base64?: string | null;
  oauth_flows?: string[];
  metadata?: ProviderMetadata;
  active_models?: number;
  total_models?: number;
  notif_keyword_only?: boolean;
}

export interface CreateProviderInput {
  id: string;
  name: string;
  base_url: string;
  auth_type: string;
  format: string;
  extra_headers_json: string | null;
}

export interface UpdateProviderInput {
  name?: string;
  base_url?: string;
  extra_headers_json?: string;
  auto_activate_keyword?: string | null;
}

export interface ModelQuotaDetail {
  model_id: string;
  session_used: number;
  session_limit: number;
  session_reset_at: string | null;
  remaining_fraction: number;
}

export interface AccountQuota {
  session_used: number | null;
  session_limit: number | null;
  session_reset_at: string | null;
  weekly_used: number | null;
  weekly_limit: number | null;
  weekly_reset_at: string | null;
  plan_name: string | null;
  last_fetched_at: string;
  fetch_error: string | null;
  model_details?: ModelQuotaDetail[] | null;
}

export interface Account {
  id: AccountId;
  provider_id: ProviderId;
  label: string | null;
  priority: number;
  extra_config_json: string | null;
  health_status: HealthStatus;
  rate_limited_until: string | null;
  quota_session_used: number | null;
  quota_session_limit: number | null;
  quota_session_reset_at: string | null;
  quota_weekly_used: number | null;
  quota_weekly_limit: number | null;
  quota_weekly_reset_at: string | null;
  quota_plan_name: string | null;
  quota_last_fetched_at: string | null;
  quota_fetch_error: string | null;
  quota_model_details?: ModelQuotaDetail[] | null;
  auth_type: string;
  email: string | null;
  oauth_scope: string | null;
  oauth_provider_specific: string | null;
  expires_at: string | null;
  created_at: string;
}

export interface CreateAccountInput {
  provider_id: string;
  api_key: string | null;
  label: string | null;
  priority: number | null;
  extra_config_json: string | null;
}

export interface BulkCreateAccountItem {
  api_key: string;
  label?: string | null;
  priority?: number | null;
  extra_config_json?: string | null;
}

export interface BulkCreateAccountsInput {
  provider_id: string;
  items: BulkCreateAccountItem[];
}

export interface BulkCreateAccountsResponse {
  created: number;
  ids: number[];
}

export interface ApiKey {
  id: ApiKeyId;
  key_prefix: string | null;
  label: string | null;
  scopes: string[];
  allowed_models: string[] | null;
  allowed_combos: number[] | null;
  blacklisted_providers: string[] | null;
  blacklisted_models: string[] | null;
  is_active: boolean;
  revoked_at: string | null;
  expires_at: string | null;
  last_used_at: string | null;
  created_at: string;
  created_by: string | null;
}
