import type {
  AccountId,
  ComboId,
  ComboTargetId,
  CooldownMode,
  ModelRowId,
  PriorityMode,
  ProviderId,
  Strategy,
} from "./common";

export interface Combo {
  id: ComboId;
  name: string;
  strategy: Strategy;
  race_size: number;
  created_at: string;
  context_window?: number | null;
  priority_mode?: PriorityMode | null;
  cooldown_mode?: CooldownMode | null;
  cooldown_base_secs?: number | null;
  cooldown_max_secs?: number | null;
  cooldown_factor?: number | null;
  lkgp_exploration_rate?: number | null;
  selection_window_secs?: number | null;
  preventive_rate_limit?: boolean;
  decision_model?: string | null;
  decision_timeout_ms?: number | null;
}

export interface ComboTarget {
  id: ComboTargetId;
  combo_id: ComboId;
  provider_id: ProviderId;
  account_id: AccountId | null;
  model_row_id: ModelRowId | null;
  sub_combo_id: ComboId | null;
  priority_order: number;
  weight?: number;
  active: boolean;
  cooldown_mode?: CooldownMode | null;
  cooldown_base_secs?: number | null;
  thinking_effort?: string | null;
  description?: string | null;
}

export interface ComboTargetWithModel extends ComboTarget {
  sub_combo_name: string | null;
  model_id: string;
  model_display_name: string | null;
  in_cooldown: boolean;
  cooldown_until: string | null;
  cooldown_reason: string | null;
  context_length: number | null;
  max_output_tokens: number | null;
  provider_active: boolean;
}

export interface ComboSummary {
  id: number;
  name: string;
}

export interface CreateComboInput {
  name: string;
  strategy: string;
  race_size: number | null;
  priority_mode?: string;
  cooldown_mode?: string;
  cooldown_base_secs?: number;
  cooldown_max_secs?: number;
  cooldown_factor?: number;
  lkgp_exploration_rate?: number;
  selection_window_secs?: number;
  preventive_rate_limit?: boolean;
  decision_model?: string;
  decision_timeout_ms?: number;
}

export interface AddTargetInput {
  provider_id: string;
  account_id: AccountId | null;
  model_row_id: ModelRowId | null;
  sub_combo_id: ComboId | null;
  priority_order: number;
  description?: string;
}
