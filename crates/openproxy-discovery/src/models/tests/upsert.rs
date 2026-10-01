use super::*;
use crate::models::{list_all, upsert_many};
use openproxy_types::{DiscoveredModel, TargetFormat};

#[test]
fn target_format_parse_roundtrip() {
    for (s, expected) in [
        ("openai", TargetFormat::Openai),
        ("anthropic", TargetFormat::Anthropic),
        ("gemini", TargetFormat::Gemini),
    ] {
        assert_eq!(TargetFormat::parse(s).unwrap(), expected);
        assert_eq!(expected.as_str(), s);
    }

    let openai_json = serde_json::to_string(&TargetFormat::Openai).unwrap();
    assert_eq!(openai_json, "\"openai\"");
    let back: TargetFormat = serde_json::from_str(&openai_json).unwrap();
    assert_eq!(back, TargetFormat::Openai);

    let err = TargetFormat::parse("xml").unwrap_err();
    assert!(matches!(err, CoreError::Validation(_)), "got {err:?}");
}

#[test]
fn upsert_inserts_new() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");

    let n = upsert_many(
        &conn,
        &provider,
        &[
            discovered("m1", TargetFormat::Openai),
            discovered("m2", TargetFormat::Anthropic),
        ],
        Duration::from_hours(1),
    )
    .expect("upsert_many");

    assert_eq!(n.touched, 2);

    let all = list_all(&conn).expect("list_all");
    assert_eq!(all.len(), 2);
    let ids: Vec<&str> = all.iter().map(|m| m.model_id.as_str()).collect();
    assert!(ids.contains(&"m1"));
    assert!(ids.contains(&"m2"));

    let m1 = all.iter().find(|m| m.model_id.as_str() == "m1").unwrap();
    assert_eq!(m1.target_format, TargetFormat::Openai);
    assert!(m1.expires_at.is_some());
}

#[test]
fn upsert_updates_existing() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");

    upsert_many(
        &conn,
        &provider,
        &[DiscoveredModel {
            display_name: Some("old".into()),
            ..minimal("m1")
        }],
        Duration::from_mins(1),
    )
    .expect("first upsert");

    let original = list_all(&conn).unwrap();
    assert_eq!(original.len(), 1);
    let original_row_id = original[0].row_id;
    let original_discovered = original[0].discovered_at.clone();
    let original_expires = original[0].expires_at.clone();

    let n = upsert_many(
        &conn,
        &provider,
        &[DiscoveredModel {
            display_name: Some("new".into()),
            target_format: TargetFormat::Anthropic,
            ..minimal("m1")
        }],
        Duration::from_hours(2),
    )
    .expect("second upsert");
    assert_eq!(n.touched, 1, "update should report 1 changed row");

    let all = list_all(&conn).unwrap();
    assert_eq!(all.len(), 1, "no new row, just update");
    let m = &all[0];
    assert_eq!(m.row_id, original_row_id, "row id stable across update");
    assert_eq!(m.target_format, TargetFormat::Anthropic);
    assert_eq!(m.display_name.as_deref(), Some("new"));
    assert_eq!(
        m.discovered_at, original_discovered,
        "discovered_at must be preserved on re-upsert"
    );
    assert_eq!(
        m.expires_at, original_expires,
        "expires_at must be preserved on re-upsert"
    );
}

#[test]
fn upsert_persists_openrouter_metadata() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    let mut model = minimal("qwen/qwen3-coder:free");
    model.display_name = Some("Qwen: Qwen3 Coder 480B A35B (free)".into());
    model.context_length = Some(1_048_576);
    model.max_output_tokens = Some(262_000);
    model.input_modalities = Some(vec!["text".into()].into());
    model.output_modalities = Some(vec!["text".into()].into());
    model.model_type = Some("chat".into());
    model.family = Some("Qwen3".into());
    model.capabilities = Some(openproxy_types::ModelCapabilities {
        tool_calling: Some(true),
        temperature: Some(true),
        structured_output: Some(true),
        ..openproxy_types::ModelCapabilities::empty()
    });

    upsert_many(&conn, &provider, &[model], Duration::from_hours(1)).unwrap();
    let row = list_all(&conn).unwrap().pop().unwrap();
    assert_eq!(
        (
            row.context_length,
            row.max_output_tokens,
            row.family.as_deref(),
            &*row.model_type
        ),
        (Some(1_048_576), Some(262_000), Some("Qwen3"), "chat")
    );
    assert_eq!(
        serde_json::from_str::<Vec<String>>(row.input_modalities_json.as_deref().unwrap()).unwrap(),
        vec!["text"]
    );
    assert_eq!(
        serde_json::from_str::<Vec<String>>(row.output_modalities_json.as_deref().unwrap())
            .unwrap(),
        vec!["text"]
    );
    let caps = row.capabilities_json.as_deref().unwrap();
    assert!(
        caps.contains("\"tool_calling\":true")
            && caps.contains("\"temperature\":true")
            && caps.contains("\"structured_output\":true")
            && !caps.contains("vision")
    );
}

#[test]
fn upsert_refreshes_metadata_on_re_upsert() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    let mut m = minimal("test/model");
    m.context_length = Some(128_000);
    m.max_output_tokens = Some(4_096);
    upsert_many(&conn, &provider, &[m.clone()], Duration::from_hours(1)).unwrap();

    m.context_length = Some(200_000);
    m.max_output_tokens = Some(8_192);
    upsert_many(&conn, &provider, &[m], Duration::from_hours(1)).unwrap();
    let row = list_all(&conn).unwrap().pop().unwrap();
    assert_eq!(
        (row.context_length, row.max_output_tokens),
        (Some(200_000), Some(8_192))
    );
}

#[test]
fn upsert_preserves_metadata_when_excluded_is_null() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    let mut m = minimal("test/model");
    m.context_length = Some(128_000);
    upsert_many(&conn, &provider, &[m.clone()], Duration::from_hours(1)).unwrap();
    m.context_length = None;
    upsert_many(&conn, &provider, &[m], Duration::from_hours(1)).unwrap();
    assert_eq!(
        list_all(&conn).unwrap().pop().unwrap().context_length,
        Some(128_000)
    );
}

#[test]
fn upsert_preserves_discovered_at_on_re_upsert() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    let mut m = minimal("anthropic/claude-sonnet-4");
    m.display_name = Some("Claude Sonnet 4".into());
    upsert_many(&conn, &provider, &[m.clone()], Duration::from_hours(1)).unwrap();
    let original = list_all(&conn).unwrap().pop().unwrap();

    conn.execute(
        "UPDATE models SET discovered_at = datetime('now', '-10 minutes') WHERE id = ?1",
        [original.row_id.0],
    )
    .unwrap();
    let backdated = list_all(&conn).unwrap().pop().unwrap();

    m.display_name = Some("Claude Sonnet 4 (renamed)".into());
    upsert_many(&conn, &provider, &[m], Duration::from_hours(1)).unwrap();
    let updated = list_all(&conn).unwrap().pop().unwrap();
    assert_eq!(updated.discovered_at, backdated.discovered_at);
    assert_eq!(updated.expires_at, backdated.expires_at);
    assert_eq!(
        updated.display_name.as_deref(),
        Some("Claude Sonnet 4 (renamed)")
    );
}

#[test]
fn upsert_many_deletes_models_dropped_by_upstream() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    let d = |id| discovered(id, TargetFormat::Openai);
    upsert_many(
        &conn,
        &provider,
        &[d("m1"), d("m2"), d("m3")],
        Duration::from_hours(1),
    )
    .unwrap();
    assert_eq!(list_all(&conn).unwrap().len(), 3);

    assert_eq!(
        upsert_many(
            &conn,
            &provider,
            &[d("m1"), d("m3")],
            Duration::from_hours(1)
        )
        .unwrap()
        .touched,
        2
    );
    let ids: Vec<String> = list_all(&conn)
        .unwrap()
        .into_iter()
        .map(|m| m.model_id.0)
        .collect();
    assert_eq!(ids, vec!["m1", "m3"]);
}

#[test]
fn upsert_many_preserves_custom_rows_when_not_in_diff() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    crate::models::create_custom(
        &conn,
        &provider,
        &ModelId::new("hand-curated"),
        Some("Hand Curated"),
        TargetFormat::Openai,
        3600,
        Some("chat"),
    )
    .unwrap();
    upsert_many(
        &conn,
        &provider,
        &[discovered("d1", TargetFormat::Openai)],
        Duration::from_hours(1),
    )
    .unwrap();
    assert_eq!(list_all(&conn).unwrap().len(), 2);

    upsert_many(
        &conn,
        &provider,
        &[discovered("d2", TargetFormat::Openai)],
        Duration::from_hours(1),
    )
    .unwrap();
    let all = list_all(&conn).unwrap();
    assert_eq!(all.len(), 2);
    assert!(
        all.iter()
            .find(|m| m.model_id.as_str() == "hand-curated")
            .unwrap()
            .custom
    );
}

#[test]
fn upsert_many_with_empty_discovered_deletes_all_non_custom() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    crate::models::create_custom(
        &conn,
        &provider,
        &ModelId::new("hand-curated"),
        Some("Hand Curated"),
        TargetFormat::Openai,
        3600,
        Some("chat"),
    )
    .unwrap();
    upsert_many(
        &conn,
        &provider,
        &[
            discovered("d1", TargetFormat::Openai),
            discovered("d2", TargetFormat::Openai),
        ],
        Duration::from_hours(1),
    )
    .unwrap();
    assert_eq!(list_all(&conn).unwrap().len(), 3);

    assert_eq!(
        upsert_many(&conn, &provider, &[], Duration::from_hours(1))
            .unwrap()
            .touched,
        0
    );
    let rem = list_all(&conn).unwrap();
    assert_eq!(rem.len(), 1);
    assert_eq!(rem[0].model_id.as_str(), "hand-curated");
}

#[test]
fn upsert_many_does_not_duplicate_notifications_on_rediscovery() {
    let mut conn = Connection::open_in_memory().unwrap();
    openproxy_db::migrations::run(&mut conn).unwrap();
    conn.execute("INSERT INTO providers (id, name, base_url, auth_type, format) VALUES ('p1', 'P1', 'https://example.com', 'none', 'openai')", []).unwrap();
    let provider = ProviderId::new("p1");
    let d = [discovered("m1", TargetFormat::Openai)];

    upsert_many(&conn, &provider, &d, Duration::from_hours(1)).unwrap();
    let count = |c: &Connection| -> i64 {
        c.query_row(
            "SELECT COUNT(*) FROM notifications WHERE kind = 'model_new' AND dedup_key = 'p1:m1'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(count(&conn), 1);
    upsert_many(&conn, &provider, &[], Duration::from_hours(1)).unwrap();
    upsert_many(&conn, &provider, &d, Duration::from_hours(1)).unwrap();
    assert_eq!(count(&conn), 1);
}

#[test]
fn sync_and_upsert_preserves_manually_configured_model_type() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    let mut m = minimal("claude-3-haiku");
    m.target_format = TargetFormat::Anthropic;
    m.model_type = Some("chat".into());

    upsert_many(&conn, &provider, &[m.clone()], Duration::from_hours(1)).unwrap();
    let row = list_all(&conn).unwrap().pop().unwrap();
    assert_eq!(&*row.model_type, "chat");

    crate::models::update_model_type(&conn, row.row_id, "embedding").unwrap();
    assert_eq!(
        &*list_all(&conn).unwrap().pop().unwrap().model_type,
        "embedding"
    );

    upsert_many(&conn, &provider, &[m], Duration::from_hours(1)).unwrap();
    assert_eq!(
        &*list_all(&conn).unwrap().pop().unwrap().model_type,
        "embedding"
    );
}
