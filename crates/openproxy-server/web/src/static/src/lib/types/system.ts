export interface DebugLogEntry {
  seq: number;
  timestamp: string;
  level: string;
  target: string;
  message: string;
  request_id: string | null;
  trace_id: string | null;
  span_path: string | null;
}

export interface DebugLogsResponse {
  entries: DebugLogEntry[];
  latest_seq: number;
  total_in_buffer: number;
}

export interface FreeProxy {
  id: string;
  source: string;
  host: string;
  port: number;
  type: string;
  country_code: string | null;
  status: string;
  latency_ms: number | null;
  last_validated: string | null;
  created_at: string;
  updated_at: string;
}

export interface ProxySource {
  id: string;
  name: string;
  url: string;
  priority: number;
  active: boolean;
  is_builtin: boolean;
  proxies_total: number;
  proxies_alive: number;
  proxies_dead: number;
  created_at: string;
  updated_at: string;
}

export interface CreateProxySourceInput {
  name: string;
  url: string;
  priority?: number;
}

export interface UpdateProxySourceInput {
  name?: string;
  url?: string;
  priority?: number;
}
