export type ProviderId = string;
export type AccountId = number;
export type ComboId = number;
export type ComboTargetId = number;
export type ModelId = string;
export type ModelRowId = number;
export type UsageId = number;
export type ApiKeyId = number;

export type HealthStatus = "healthy" | "degraded" | "unhealthy";
export type ProviderFormat = "openai" | "anthropic" | "mixed" | "gemini";
export type AuthType = "bearer" | "x-api-key" | "goog-api-key" | "oauth" | "none";
export type Strategy = "priority" | "round_robin" | "shuffle";
export type PriorityMode = "strict" | "lkgp" | "weighted" | "least_used" | "p2c";
export type CooldownMode = "flat" | "exponential" | "none";
export type TargetFormat = "openai" | "anthropic" | "gemini";

export interface ApiErrorBody {
  code: string;
  message: string;
}

export interface ApiErrorEnvelope {
  error: ApiErrorBody;
}
