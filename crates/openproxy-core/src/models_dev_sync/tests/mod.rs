use rusqlite::Connection;

mod combos;
mod sync;

pub(crate) fn create_sync_table(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS model_capabilities_sync (
            provider_id       TEXT NOT NULL,
            model_id          TEXT NOT NULL,
            context_length    INTEGER,
            max_output_tokens INTEGER,
            pricing_input_per_1m  REAL,
            pricing_output_per_1m REAL,
            pricing_cached_per_1m REAL,
            tool_call         INTEGER,
            reasoning         INTEGER,
            vision            INTEGER,
            structured_output INTEGER,
            modalities_input  TEXT,
            modalities_output TEXT,
            family            TEXT,
            status            TEXT,
            fetched_at        TEXT,
            model_id_normalized TEXT,
            routing_format    TEXT,
            PRIMARY KEY (provider_id, model_id)
        )",
    )
    .unwrap();
}

pub(crate) const TEST_JSON: &str = r#"{
  "opencode": {
    "id": "opencode",
    "models": {
      "deepseek-v4-flash": {
        "id": "deepseek-v4-flash",
        "tool_call": true,
        "reasoning": false,
        "structured_output": true,
        "limit": { "context": 1000000, "output": 384000 },
        "cost": { "input": 0.14, "output": 0.28, "cache_read": 0.028 },
        "family": "deepseek-v4",
        "status": "active"
      },
      "deepseek-v4-flash-free": {
        "id": "deepseek-v4-flash-free",
        "tool_call": true,
        "reasoning": false,
        "structured_output": true,
        "limit": { "context": 200000, "output": 128000 },
        "cost": { "input": 0, "output": 0, "cache_read": 0 },
        "family": "deepseek-v4",
        "status": "active"
      }
    }
  },
  "google": {
    "id": "google",
    "models": {
      "gemini-2.5-pro": {
        "id": "gemini-2.5-pro",
        "tool_call": true,
        "reasoning": true,
        "structured_output": true,
        "limit": { "context": 1048576, "output": 65536 },
        "cost": { "input": 1.25, "output": 10.0, "cache_read": 0.25 },
        "family": "gemini-2.5",
        "status": "active"
      }
    }
  }
}"#;
