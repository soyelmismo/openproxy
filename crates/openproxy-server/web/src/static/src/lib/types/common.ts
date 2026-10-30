export type ProviderId = string;
export type AccountId = number;
export type ComboId = number;
export type ComboTargetId = number;
export type ModelId = string;
export type ModelRowId = number;
export type UsageId = number;
export type ApiKeyId = number;

export type HealthStatus = "healthy" | "degraded" | "unhealthy";
export type ProviderFormat = "openai" | "anthropic" | "mixed" | "gemini" | "responses" | "atomesus" | "commandcodego" | "systemone" | "devin";
export type AuthType = "bearer" | "x-api-key" | "goog-api-key" | "oauth" | "none";
export type Strategy = "priority" | "round_robin" | "shuffle";
export type PriorityMode = "strict" | "lkgp" | "weighted" | "least_used" | "p2c" | "decision";
export type CooldownMode = "flat" | "exponential" | "none";
export type TargetFormat = "openai" | "anthropic" | "gemini" | "responses" | "atomesus" | "commandcodego" | "systemone" | "devin";

export interface ApiErrorBody {
  code: string;
  message: string;
}

export interface ApiErrorEnvelope {
  error: ApiErrorBody;
}

export interface TargetFormatDescriptor {
  id: TargetFormat;
  name: string;
  label: string;
  endpoint: string | null;
  description: string;
}

export interface ProviderFormatDescriptor {
  id: ProviderFormat;
  name: string;
  label: string;
  default_target_format: TargetFormat;
  description: string;
}

export interface FormatsMetadata {
  target_formats: TargetFormatDescriptor[];
  provider_formats: ProviderFormatDescriptor[];
}

export const FALLBACK_TARGET_FORMATS: readonly TargetFormatDescriptor[] = [
  { id: "openai", name: "OpenAI Chat Completions", label: "OpenAI Chat Completions (/v1/chat/completions)", endpoint: "/v1/chat/completions", description: "Standard OpenAI chat completions API wire format" },
  { id: "responses", name: "OpenAI Responses", label: "OpenAI Responses (/v1/responses)", endpoint: "/v1/responses", description: "OpenAI Responses API wire format" },
  { id: "anthropic", name: "Anthropic Messages", label: "Anthropic Messages (/v1/messages)", endpoint: "/v1/messages", description: "Anthropic Claude Messages API wire format" },
  { id: "gemini", name: "Google Gemini", label: "Google Gemini (generateContent)", endpoint: "v1beta/models/...:generateContent", description: "Google Gemini REST API generateContent wire format" },
  { id: "systemone", name: "SystemOne Decision", label: "SystemOne (Decision / Fast Engine)", endpoint: "/v1/decisions", description: "SystemOne fast decision engine wire format" },
  { id: "atomesus", name: "Atomesus", label: "Atomesus", endpoint: null, description: "Atomesus upstream wire format" },
  { id: "commandcodego", name: "CommandCodeGo", label: "CommandCodeGo", endpoint: null, description: "CommandCode Go bridge wire format" },
  { id: "devin", name: "Devin", label: "Devin (Connect-RPC)", endpoint: null, description: "Devin Connect-RPC protobuf wire format" },
];

export const FALLBACK_PROVIDER_FORMATS: readonly ProviderFormatDescriptor[] = [
  { id: "openai", name: "OpenAI Chat Completions", label: "OpenAI Chat Completions (/v1/chat/completions)", default_target_format: "openai", description: "OpenAI Chat Completions protocol" },
  { id: "responses", name: "OpenAI Responses", label: "OpenAI Responses (/v1/responses)", default_target_format: "responses", description: "OpenAI Responses protocol" },
  { id: "anthropic", name: "Anthropic Messages", label: "Anthropic Messages (/v1/messages)", default_target_format: "anthropic", description: "Anthropic Claude Messages protocol" },
  { id: "gemini", name: "Google Gemini", label: "Google Gemini (generateContent)", default_target_format: "gemini", description: "Google Gemini native API protocol" },
  { id: "mixed", name: "Mixed", label: "Mixed (per-model target format)", default_target_format: "openai", description: "Multi-protocol provider with model-level routing" },
  { id: "systemone", name: "SystemOne Decision", label: "SystemOne (Decision / Fast Engine)", default_target_format: "systemone", description: "SystemOne fast decision engine protocol" },
  { id: "atomesus", name: "Atomesus", label: "Atomesus", default_target_format: "atomesus", description: "Atomesus upstream protocol" },
  { id: "commandcodego", name: "CommandCodeGo", label: "CommandCodeGo", default_target_format: "commandcodego", description: "CommandCode Go bridge protocol" },
  { id: "devin", name: "Devin", label: "Devin (Connect-RPC)", default_target_format: "devin", description: "Devin Connect-RPC protobuf protocol" },
];
