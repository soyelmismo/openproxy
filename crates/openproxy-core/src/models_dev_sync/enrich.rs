//! Capability and metadata enrichment from the model_capabilities_sync table.

use super::backfill::backfill_model_id_normalized;
use crate::error::Result;
use rusqlite::Connection;

fn enrich_context_length(conn: &Connection) -> Result<usize> {
    conn.execute(
        "UPDATE models SET context_length = COALESCE(
        (SELECT s.context_length FROM model_capabilities_sync s
         WHERE s.model_id_normalized = models.model_id_normalized
           AND s.context_length IS NOT NULL
         LIMIT 1),
        models.context_length
     )
     WHERE models.custom = 0
       AND models.model_id_normalized IS NOT NULL
       AND EXISTS (
         SELECT 1 FROM model_capabilities_sync s
         WHERE s.model_id_normalized = models.model_id_normalized
           AND s.context_length IS NOT NULL
       )",
        [],
    )
    .map_err(openproxy_db::error::map_db_error)
}

fn enrich_max_output_tokens(conn: &Connection) -> Result<usize> {
    conn.execute(
        "UPDATE models SET max_output_tokens = COALESCE(
        (SELECT s.max_output_tokens FROM model_capabilities_sync s
         WHERE s.model_id_normalized = models.model_id_normalized
           AND s.max_output_tokens IS NOT NULL
         LIMIT 1),
        models.max_output_tokens
     )
     WHERE models.custom = 0
       AND models.model_id_normalized IS NOT NULL
       AND EXISTS (
         SELECT 1 FROM model_capabilities_sync s
         WHERE s.model_id_normalized = models.model_id_normalized
           AND s.max_output_tokens IS NOT NULL
       )",
        [],
    )
    .map_err(openproxy_db::error::map_db_error)
}

fn enrich_capabilities(conn: &Connection) -> Result<usize> {
    conn.execute(
        "UPDATE models SET capabilities_json = (
        SELECT json_patch(
            coalesce(models.capabilities_json, '{}'),
            json_object(
                'vision',            s.vision,
                'tool_calling',      s.tool_call,
                'reasoning',         s.reasoning,
                'structured_output', s.structured_output
            )
        )
        FROM model_capabilities_sync s
        WHERE s.model_id_normalized = models.model_id_normalized
          AND (s.vision IS NOT NULL OR s.tool_call IS NOT NULL
               OR s.reasoning IS NOT NULL OR s.structured_output IS NOT NULL)
        LIMIT 1
     )
     WHERE models.custom = 0
       AND models.model_id_normalized IS NOT NULL
       AND EXISTS (
         SELECT 1 FROM model_capabilities_sync s
         WHERE s.model_id_normalized = models.model_id_normalized
           AND (s.vision IS NOT NULL OR s.tool_call IS NOT NULL
                OR s.reasoning IS NOT NULL OR s.structured_output IS NOT NULL)
       )",
        [],
    )
    .map_err(openproxy_db::error::map_db_error)
}

fn enrich_metadata(conn: &Connection) -> Result<usize> {
    conn.execute(
        "UPDATE models SET
            family = COALESCE(
                (SELECT s.family FROM model_capabilities_sync s
                 WHERE s.model_id_normalized = models.model_id_normalized
                   AND s.family IS NOT NULL LIMIT 1),
                models.family
            ),
            input_modalities_json = COALESCE(
                (SELECT s.modalities_input FROM model_capabilities_sync s
                 WHERE s.model_id_normalized = models.model_id_normalized
                   AND s.modalities_input IS NOT NULL LIMIT 1),
                models.input_modalities_json
            ),
            output_modalities_json = CASE
                WHEN models.model_id_normalized LIKE '%embed%' OR models.model_id_normalized LIKE '%bge-%'
                    THEN '[\"embedding\"]'
                ELSE COALESCE(
                    (SELECT s.modalities_output FROM model_capabilities_sync s
                     WHERE s.model_id_normalized = models.model_id_normalized
                       AND s.modalities_output IS NOT NULL LIMIT 1),
                    models.output_modalities_json
                )
            END,
            model_type = CASE
                WHEN models.model_id_normalized LIKE '%embed%'
                  OR models.model_id_normalized LIKE '%bge-%'
                  OR EXISTS (
                    SELECT 1 FROM model_capabilities_sync s
                    WHERE s.model_id_normalized = models.model_id_normalized
                      AND s.modalities_output = '[\"embedding\"]'
                  ) THEN 'embedding'
                WHEN (models.model_id_normalized LIKE '%dall-e%'
                  OR models.model_id_normalized LIKE '%sdxl%'
                  OR models.model_id_normalized LIKE '%stable-diffusion%'
                  OR models.model_id_normalized LIKE '%midjourney%'
                  OR EXISTS (
                    SELECT 1 FROM model_capabilities_sync s
                    WHERE s.model_id_normalized = models.model_id_normalized
                      AND s.modalities_output = '[\"image\"]'
                  ))
                  AND NOT (models.model_id_normalized LIKE '%gemini%'
                           OR models.model_id_normalized LIKE '%gpt-%'
                           OR models.model_id_normalized LIKE '%claude%'
                           OR models.model_id_normalized LIKE '%diffusiongemma%')
                  THEN 'image'
                WHEN (models.model_id_normalized LIKE '%whisper%'
                  OR models.model_id_normalized LIKE '%tts%'
                  OR models.model_id_normalized LIKE '%elevenlabs%'
                  OR models.model_id_normalized LIKE '%melotts%'
                  OR EXISTS (
                    SELECT 1 FROM model_capabilities_sync s
                    WHERE s.model_id_normalized = models.model_id_normalized
                      AND s.modalities_output = '[\"audio\"]'
                      AND s.modalities_input NOT LIKE '%\"text\"%'
                  ))
                  AND NOT (models.model_id_normalized LIKE '%gemini%'
                           OR models.model_id_normalized LIKE '%gpt-%'
                           OR models.model_id_normalized LIKE '%claude%'
                           OR models.model_id_normalized LIKE '%qwen%')
                  THEN 'audio'
                WHEN (models.model_id_normalized LIKE '%gemini%'
                  OR models.model_id_normalized LIKE '%gpt-%'
                  OR models.model_id_normalized LIKE '%claude%'
                  OR models.model_id_normalized LIKE '%qwen%'
                  OR models.model_id_normalized LIKE '%llama%'
                  OR models.model_id_normalized LIKE '%mistral%'
                  OR models.model_id_normalized LIKE '%deepseek%'
                  OR models.model_id_normalized LIKE '%gemma%')
                  AND NOT (models.model_id_normalized LIKE '%whisper%'
                           OR models.model_id_normalized LIKE '%tts%'
                           OR models.model_id_normalized LIKE '%dall-e%'
                           OR models.model_id_normalized LIKE '%imagen%')
                  THEN 'chat'
                ELSE models.model_type
            END
         WHERE models.custom = 0
           AND models.model_id_normalized IS NOT NULL
           AND EXISTS (
             SELECT 1 FROM model_capabilities_sync s
             WHERE s.model_id_normalized = models.model_id_normalized
           )",
        [],
    )
    .map_err(openproxy_db::error::map_db_error)
}

/// After a sync, refresh `models.context_length`, `max_output_tokens`,
/// and `capabilities_json` from the `model_capabilities_sync` table.
pub fn enrich_models_from_sync(conn: &Connection) -> Result<usize> {
    backfill_model_id_normalized(conn)?;
    let ctx = enrich_context_length(conn)?;
    let tok = enrich_max_output_tokens(conn)?;
    let cap = enrich_capabilities(conn)?;
    let meta = enrich_metadata(conn)?;
    Ok(ctx + tok + cap + meta)
}
