// lib/types/client-profile.ts — canonical client profile definitions for OpenProxy dashboard.
// Keeps the frontend in sync with openproxy_types::ClientProfile without hand-typed duplicates.

export const CLIENT_PROFILES = [
  'claude-code',
  'codex',
  'cline',
  'kilocode',
  'kiro',
  'minimax',
  'commandcode',
  'codebuddy',
  'opencode',
  'antigravity',
] as const;

export type ClientProfile = typeof CLIENT_PROFILES[number];

export interface ClientProfileOption {
  id: ClientProfile;
  label: string;
  description: string;
}

export const CLIENT_PROFILE_OPTIONS: readonly ClientProfileOption[] = [
  { id: 'claude-code', label: 'Claude Code', description: 'Anthropic Claude Code CLI spoofing & cloaking' },
  { id: 'codex', label: 'Codex', description: 'OpenAI Codex/ChatGPT CLI spoofing' },
  { id: 'cline', label: 'Cline', description: 'Cline client identity headers' },
  { id: 'kilocode', label: 'Kilocode', description: 'Kilocode headers preset' },
  { id: 'kiro', label: 'Kiro', description: 'Kiro AI client identity' },
  { id: 'minimax', label: 'MiniMax', description: 'MiniMax client identity headers' },
  { id: 'commandcode', label: 'Command Code', description: 'CommandCode Go headers preset' },
  { id: 'codebuddy', label: 'CodeBuddy', description: 'WorkBuddy / CodeBuddy headers preset' },
  { id: 'opencode', label: 'OpenCode', description: 'OpenCode Zen client headers' },
  { id: 'antigravity', label: 'Antigravity', description: 'Google Antigravity headers' },
];
