import type { ModelId, ModelRowId, ProviderId, TargetFormat } from "./common";

export interface ModelCapabilities {
  vision: boolean | null;
  tool_calling: boolean | null;
  reasoning: boolean | null;
  thinking: boolean | null;
  attachment: boolean | null;
  structured_output: boolean | null;
  temperature: boolean | null;
}

export interface Model {
  row_id: ModelRowId;
  provider_id: ProviderId;
  model_id: ModelId;
  display_name: string | null;
  target_format: TargetFormat;
  discovered_at: string;
  expires_at: string | null;
  timeout_overrides_json: string | null;
  active: boolean;
  last_test_status: number | null;
  last_test_at: string | null;
  custom: boolean;
  context_length: number | null;
  max_output_tokens: number | null;
  capabilities_json: string | null;
  family: string | null;
  model_type: string;
  input_modalities_json: string | null;
  output_modalities_json: string | null;
}

export interface DiscoveredModel {
  model_id: ModelId;
  display_name: string | null;
  target_format: TargetFormat;
  context_length: number | null;
  max_output_tokens: number | null;
  input_modalities: string[] | null;
  output_modalities: string[] | null;
  model_type: string | null;
  family: string | null;
  capabilities: ModelCapabilities | null;
}

export interface CreateCustomModelInput {
  provider_id: string;
  model_id: string;
  display_name: string | null;
  target_format: string;
  ttl_seconds: number;
}

export interface BulkToggleInput {
  provider_id: string;
  active: boolean;
}
