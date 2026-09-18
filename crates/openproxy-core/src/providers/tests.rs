use super::*;
use crate::error::CoreError;
use openproxy_db::conn::DbPool;
use std::path::PathBuf;

fn fresh_pool() -> (DbPool, PathBuf) {
    let pool = DbPool::test_pool_with_prefix("openproxy-providers-test").expect("open pool");
    let path = pool.path().to_path_buf();
    (pool, path)
}

fn np<'a>(id: &'a ProviderId, name: &'a str) -> NewProvider<'a> {
    NewProvider {
        id,
        name,
        base_url: "https://x.example",
        auth_type: AuthType::Bearer,
        format: ProviderFormat::Openai,
        extra_headers_json: None,
        auto_activate_keyword: None,
        rate_limit_scope: crate::providers::RateLimitScope::Account,
    }
}

#[test]
fn create_and_get() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let id = ProviderId::new("openrouter");
    let mut p = np(&id, "OpenRouter");
    p.base_url = "https://openrouter.ai/api/v1";
    p.extra_headers_json = Some(r#"{"X-Title":"openproxy"}"#);
    p.auto_activate_keyword = Some("claude");
    create(&conn, p).expect("create");

    let got = get(&conn, &id).expect("get").expect("present");
    assert_eq!(got.id, id);
    assert_eq!(&*got.name, "OpenRouter");
    assert_eq!(&*got.base_url, "https://openrouter.ai/api/v1");
    assert_eq!(got.auth_type, AuthType::Bearer);
    assert_eq!(got.format, ProviderFormat::Openai);
    assert_eq!(
        got.extra_headers_json.as_deref(),
        Some(r#"{"X-Title":"openproxy"}"#)
    );
    assert_eq!(got.auto_activate_keyword.as_deref(), Some("claude"));
    assert!(!got.created_at.is_empty(), "created_at stamped by DB");
}

#[test]
fn create_duplicate_id_fails() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let id = ProviderId::new("anthropic");
    let mut p = np(&id, "Anthropic");
    p.base_url = "https://api.anthropic.com";
    p.auth_type = AuthType::XApiKey;
    p.format = ProviderFormat::Anthropic;
    create(&conn, p).expect("first create");

    let err = create(&conn, np(&id, "Dup")).expect_err("duplicate must fail");
    match err {
        CoreError::Validation(msg) => assert_eq!(msg, "provider id already exists"),
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[test]
fn list_returns_all() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    for (id, name) in [("a", "A"), ("b", "B"), ("c", "C")] {
        create(&conn, np(&ProviderId::new(id), name)).expect("create");
    }
    let all = list(&conn).expect("list");
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].id, ProviderId::new("a"));
    assert_eq!(all[1].id, ProviderId::new("b"));
    assert_eq!(all[2].id, ProviderId::new("c"));
}

#[test]
fn delete_removes_provider() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let id = ProviderId::new("to-delete");
    create(&conn, np(&id, "X")).expect("create");

    conn.execute(
        "INSERT INTO accounts(provider_id, api_key_encrypted) VALUES (?1, ?2)",
        rusqlite::params![id.as_str(), &[1u8, 2, 3][..]],
    )
    .expect("seed account");
    let accounts_before: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM accounts WHERE provider_id = ?1",
            rusqlite::params![id.as_str()],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(accounts_before, 1, "account seeded");

    delete(&conn, &id).expect("delete");
    assert!(get(&conn, &id).expect("get").is_none(), "provider gone");
    let accounts_after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM accounts WHERE provider_id = ?1",
            rusqlite::params![id.as_str()],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(accounts_after, 0, "FK cascade removed the account");
    delete(&conn, &id).expect("delete again is fine");
}

#[test]
fn update_modifies_fields() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let id = ProviderId::new("upd");
    let mut p0 = np(&id, "Original");
    p0.base_url = "https://original.example";
    p0.extra_headers_json = Some(r#"{"old":true}"#);
    create(&conn, p0).expect("create");

    update(
        &conn,
        &id,
        UpdateProviderParams {
            name: Some("Renamed"),
            ..Default::default()
        },
    )
    .expect("update name");
    let p = get(&conn, &id).expect("get").expect("present");
    assert_eq!(&*p.name, "Renamed");
    assert_eq!(&*p.base_url, "https://original.example");
    assert_eq!(p.extra_headers_json.as_deref(), Some(r#"{"old":true}"#));
    assert_eq!(p.auto_activate_keyword, None);

    update(
        &conn,
        &id,
        UpdateProviderParams {
            base_url: Some("https://new.example"),
            extra_headers_json: Some(Some(r#"{"new":true}"#)),
            auto_activate_keyword: Some(Some("claude")),
            ..Default::default()
        },
    )
    .expect("update");
    let p = get(&conn, &id).expect("get").expect("present");
    assert_eq!(&*p.name, "Renamed");
    assert_eq!(&*p.base_url, "https://new.example");
    assert_eq!(p.extra_headers_json.as_deref(), Some(r#"{"new":true}"#));
    assert_eq!(p.auto_activate_keyword.as_deref(), Some("claude"));

    update(
        &conn,
        &id,
        UpdateProviderParams {
            auto_activate_keyword: Some(None),
            ..Default::default()
        },
    )
    .expect("clear keyword");
    let p = get(&conn, &id).expect("get").expect("present");
    assert_eq!(p.auto_activate_keyword, None);

    update(&conn, &id, UpdateProviderParams::default()).expect("no-op");
    let p = get(&conn, &id).expect("get").expect("present");
    assert_eq!(&*p.base_url, "https://new.example");

    let err = update(
        &conn,
        &ProviderId::new("nope"),
        UpdateProviderParams {
            name: Some("X"),
            ..Default::default()
        },
    )
    .expect_err("missing id");
    assert!(matches!(err, CoreError::ProviderNotFound(_)));
}

#[test]
fn provider_format_parse_roundtrip() {
    for (variant, s) in [
        (ProviderFormat::Openai, "openai"),
        (ProviderFormat::Anthropic, "anthropic"),
        (ProviderFormat::Mixed, "mixed"),
        (ProviderFormat::Gemini, "gemini"),
    ] {
        assert_eq!(variant.as_str(), s);
        assert_eq!(ProviderFormat::parse(s).expect("parse"), variant);
    }
    assert!(ProviderFormat::parse("bogus").is_err());
}

#[test]
fn auth_type_parse_roundtrip() {
    for (variant, s) in [
        (AuthType::Bearer, "bearer"),
        (AuthType::XApiKey, "x-api-key"),
        (AuthType::GoogApiKey, "goog-api-key"),
        (AuthType::OAuth, "oauth"),
        (AuthType::None, "none"),
    ] {
        assert_eq!(variant.as_str(), s);
        assert_eq!(AuthType::parse(s).expect("parse"), variant);
    }
    assert!(AuthType::parse("basic").is_err());
}

#[test]
fn new_providers_default_to_active() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let id = ProviderId::new("active-by-default");
    create(&conn, np(&id, "X")).expect("create");
    let got = get(&conn, &id).expect("get").expect("present");
    assert!(got.active, "freshly created providers are active");
}

#[test]
fn set_active_flips_and_idempotent() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let id = ProviderId::new("toggle");
    create(&conn, np(&id, "T")).expect("create");

    set_active(&conn, &id, false).expect("deactivate");
    let p = get(&conn, &id).expect("get").expect("present");
    assert!(!p.active, "deactivated");

    set_active(&conn, &id, false).expect("re-apply is a no-op");
    let p = get(&conn, &id).expect("get").expect("present");
    assert!(!p.active);

    set_active(&conn, &id, true).expect("reactivate");
    let p = get(&conn, &id).expect("get").expect("present");
    assert!(p.active, "reactivated");

    set_active(&conn, &ProviderId::new("does-not-exist"), false).expect("missing id is a no-op");
}

#[test]
fn list_active_filters_out_inactive() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    for (id, name) in [("a", "A"), ("b", "B"), ("c", "C")] {
        create(&conn, np(&ProviderId::new(id), name)).expect("create");
    }

    let active = list_active(&conn).expect("list active");
    assert_eq!(active.len(), 3);
    assert_eq!(list(&conn).expect("list").len(), 3);

    set_active(&conn, &ProviderId::new("b"), false).expect("deactivate b");
    let active = list_active(&conn).expect("list active");
    assert_eq!(active.len(), 2);
    let ids: Vec<&str> = active.iter().map(|p| p.id.as_str()).collect();
    assert!(ids.contains(&"a") && ids.contains(&"c") && !ids.contains(&"b"));

    let all = list(&conn).expect("list");
    assert_eq!(all.len(), 3);
    assert!(
        !all.iter()
            .find(|p| p.id == ProviderId::new("b"))
            .unwrap()
            .active
    );
}

#[test]
fn list_and_list_active_hide_virtual_combo_provider() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    for (id, name) in [("a", "A"), ("b", "B")] {
        create(&conn, np(&ProviderId::new(id), name)).expect("create");
    }
    crate::seed::seed_virtual_combo_provider(&conn).expect("seed virtual");

    let raw_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM providers", [], |r| r.get(0))
        .expect("count");
    assert_eq!(raw_count, 3);

    let all = list(&conn).expect("list");
    assert_eq!(all.len(), 2);
    assert!(
        all.iter()
            .all(|p| p.id.as_str() != crate::seed::VIRTUAL_COMBO_PROVIDER_ID)
    );

    let active = list_active(&conn).expect("list_active");
    assert_eq!(active.len(), 2);
    assert!(
        active
            .iter()
            .all(|p| p.id.as_str() != crate::seed::VIRTUAL_COMBO_PROVIDER_ID)
    );

    let got = get(
        &conn,
        &ProviderId::new(crate::seed::VIRTUAL_COMBO_PROVIDER_ID),
    )
    .expect("get")
    .expect("present");
    assert_eq!(got.id.as_str(), crate::seed::VIRTUAL_COMBO_PROVIDER_ID);
}

#[test]
fn test_validate_favicon_magic_bytes_valid_formats() {
    // PNG
    let png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR...";
    assert_eq!(validate_favicon_bytes(png), Some("image/png"));

    // ICO (type 1 icon)
    let ico = b"\x00\x00\x01\x00\x01\x00\x10\x10...";
    assert_eq!(validate_favicon_bytes(ico), Some("image/x-icon"));

    // ICO (type 2 cursor)
    let cur = b"\x00\x00\x02\x00\x01\x00\x10\x10...";
    assert_eq!(validate_favicon_bytes(cur), Some("image/x-icon"));

    // GIF (GIF87a / GIF89a)
    let gif = b"GIF89a\x10\x00\x10\x00...";
    assert_eq!(validate_favicon_bytes(gif), Some("image/gif"));

    // JPEG
    let jpeg = b"\xff\xd8\xff\xe0\x00\x10JFIF...";
    assert_eq!(validate_favicon_bytes(jpeg), Some("image/jpeg"));

    // WEBP
    let webp = b"RIFF\x20\x00\x00\x00WEBPVP8 ...";
    assert_eq!(validate_favicon_bytes(webp), Some("image/webp"));
}

#[test]
fn test_validate_favicon_rejects_html_and_text() {
    // HTML doctype
    let html1 = b"<!DOCTYPE html><html><head><title>Error</title></head></html>";
    assert_eq!(validate_favicon_bytes(html1), None);

    let html2 = b"<!doctype html>\n<html lang=\"en\">";
    assert_eq!(validate_favicon_bytes(html2), None);

    let html3 = b"<html><body>Not found</body></html>";
    assert_eq!(validate_favicon_bytes(html3), None);

    let html4 = b"<HTML><BODY>Redirect</BODY></HTML>";
    assert_eq!(validate_favicon_bytes(html4), None);

    // text/html
    let text = b"text/html; charset=utf-8";
    assert_eq!(validate_favicon_bytes(text), None);

    // plain text
    let plain = b"404 Not Found";
    assert_eq!(validate_favicon_bytes(plain), None);
}

#[test]
fn test_validate_favicon_size_ceiling_and_empty() {
    // Empty
    assert_eq!(validate_favicon_bytes(&[]), None);

    // Just under / exactly at 64 KiB ceiling
    let mut valid_png = vec![0u8; 64 * 1024];
    valid_png[..4].copy_from_slice(b"\x89PNG");
    assert_eq!(validate_favicon_bytes(&valid_png), Some("image/png"));

    // 64 KiB + 1 byte -> rejected
    let mut oversized = vec![0u8; 64 * 1024 + 1];
    oversized[..4].copy_from_slice(b"\x89PNG");
    assert_eq!(validate_favicon_bytes(&oversized), None);
}

#[test]
fn test_validate_favicon_adversarial_stress() {
    // 1. Truncated magic bytes (<4 bytes, 8 bytes, 11 bytes)
    let truncated_cases: &[&[u8]] = &[
        b"",
        b"\x89",
        b"\x89P",
        b"\x89PN",
        b"\x00",
        b"\x00\x00",
        b"\x00\x00\x01",
        b"\x00\x00\x02",
        b"G",
        b"GI",
        b"GIF",
        b"\xff",
        b"\xff\xd8",
        b"R",
        b"RI",
        b"RIF",
        b"RIFF",
        b"RIFF1234",
        b"RIFF1234WEB",
    ];
    for &tc in truncated_cases {
        assert_eq!(
            validate_favicon_bytes(tc),
            None,
            "truncated bytes {tc:?} must return None without panicking"
        );
    }

    // 2. Corrupted headers pretending to be valid image formats
    let corrupted_cases: &[&[u8]] = &[
        b"\x89PNC\r\n\x1a\n",
        b"\x00\x00\x03\x00\x01\x00",
        b"\x00\x00\x00\x00\x01\x00",
        b"GIFA89a",
        b"\xff\xd7\xff",
        b"RIFF\x20\x00\x00\x00WEBA",
        b"RIFF\x20\x00\x00\x00VP8 ",
    ];
    for &cc in corrupted_cases {
        assert_eq!(
            validate_favicon_bytes(cc),
            None,
            "corrupted header {cc:?} must be rejected"
        );
    }

    // 3. Real HTML documents & text errors
    let html_cases: &[&[u8]] = &[
        b"<!DOCTYPE html>\n<html lang=\"en\"><head><title>404 Not Found</title></head><body><h1>404</h1></body></html>",
        b"<!doctype html public \"-//W3C//DTD HTML 4.01//EN\">\n<html><body>Blocked</body></html>",
        b"\r\n\t   <!DOCTYPE html><html><body>Error</body></html>",
        b"   <html xmlns=\"http://www.w3.org/1999/xhtml\"><head></head><body>Not found</body></html>",
        b"<HTML><HEAD><TITLE>Service Unavailable</TITLE></HEAD><BODY>503</BODY></HTML>",
        b"text/html; charset=iso-8859-1",
        b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><svg></svg>",
        b"{\n  \"error\": {\n    \"message\": \"Not Found\",\n    \"code\": 404\n  }\n}",
    ];
    for &hc in html_cases {
        assert_eq!(
            validate_favicon_bytes(hc),
            None,
            "HTML/text document must be rejected: {:?}",
            std::str::from_utf8(&hc[..hc.len().min(40)])
        );
    }

    // Real-world 733 KiB HTML document simulation (e.g. ozdoev failure)
    let repeated_html =
        "<!DOCTYPE html><html><body>Large error payload</body></html>\n".repeat(12 * 1024);
    assert!(repeated_html.len() > 700 * 1024);
    assert_eq!(validate_favicon_bytes(repeated_html.as_bytes()), None);

    // 4. Payloads > 64 KiB
    // Exactly 64 KiB (65,536 bytes) with PNG magic -> Allowed
    let mut boundary_png = vec![0u8; 64 * 1024];
    boundary_png[..4].copy_from_slice(b"\x89PNG");
    assert_eq!(validate_favicon_bytes(&boundary_png), Some("image/png"));

    // 64 KiB + 1 byte (65,537 bytes) with PNG magic -> Strictly rejected
    let mut oversized_1 = vec![0u8; 64 * 1024 + 1];
    oversized_1[..4].copy_from_slice(b"\x89PNG");
    assert_eq!(validate_favicon_bytes(&oversized_1), None);

    // 65 KiB (66,560 bytes) with valid JPEG magic -> Strictly rejected
    let mut oversized_jpeg = vec![0u8; 65 * 1024];
    oversized_jpeg[..3].copy_from_slice(b"\xff\xd8\xff");
    assert_eq!(validate_favicon_bytes(&oversized_jpeg), None);

    // 733 KiB payload with valid ICO magic -> Strictly rejected
    let mut oversized_ico = vec![0u8; 733 * 1024];
    oversized_ico[..4].copy_from_slice(b"\x00\x00\x01\x00");
    assert_eq!(validate_favicon_bytes(&oversized_ico), None);

    // 5. Valid formats: PNG, ICO (1 & 2), GIF (87a & 89a), JPEG, WEBP
    assert_eq!(
        validate_favicon_bytes(b"\x89PNG\r\n\x1a\n"),
        Some("image/png")
    );
    assert_eq!(
        validate_favicon_bytes(b"\x00\x00\x01\x00\x01\x00"),
        Some("image/x-icon")
    );
    assert_eq!(
        validate_favicon_bytes(b"\x00\x00\x02\x00\x01\x00"),
        Some("image/x-icon")
    );
    assert_eq!(
        validate_favicon_bytes(b"GIF87a\x10\x00\x10\x00"),
        Some("image/gif")
    );
    assert_eq!(
        validate_favicon_bytes(b"GIF89a\x10\x00\x10\x00"),
        Some("image/gif")
    );
    assert_eq!(
        validate_favicon_bytes(b"\xff\xd8\xff\xe0\x00\x10JFIF"),
        Some("image/jpeg")
    );
    assert_eq!(
        validate_favicon_bytes(b"RIFF\x20\x00\x00\x00WEBPVP8 "),
        Some("image/webp")
    );
}

#[test]
fn test_domain_extraction_and_loopback_filtering() {
    // 1. extract_domain
    assert_eq!(
        extract_domain("https://api.openai.com/v1"),
        Some("api.openai.com".to_string())
    );
    assert_eq!(
        extract_domain("http://127.0.0.1:8787/v1"),
        Some("127.0.0.1".to_string())
    );
    assert_eq!(
        extract_domain("http://[::1]:8080/v1"),
        Some("::1".to_string())
    );
    assert_eq!(
        extract_domain("localhost:3000"),
        Some("localhost".to_string())
    );
    assert_eq!(extract_domain(""), None);

    // 2. extract_apex_domain
    assert_eq!(extract_apex_domain("api.fireworks.ai"), "fireworks.ai");
    assert_eq!(
        extract_apex_domain("sub.domain.example.co.uk"),
        "example.co.uk"
    );
    assert_eq!(extract_apex_domain("127.0.0.1"), "127.0.0.1");
    assert_eq!(extract_apex_domain("::1"), "::1");

    // 3. is_loopback_or_private_host
    // Local / loopback
    assert!(is_loopback_or_private_host("localhost"));
    assert!(is_loopback_or_private_host("my-service.localhost"));
    assert!(is_loopback_or_private_host("service.local"));
    assert!(is_loopback_or_private_host("127.0.0.1"));
    assert!(is_loopback_or_private_host("127.0.0.2"));
    assert!(is_loopback_or_private_host("::1"));
    assert!(is_loopback_or_private_host("[::1]"));
    assert!(is_loopback_or_private_host("0.0.0.0"));
    assert!(is_loopback_or_private_host("::"));

    // Private IPv4 ranges (RFC 1918)
    assert!(is_loopback_or_private_host("10.0.0.1"));
    assert!(is_loopback_or_private_host("172.16.0.1"));
    assert!(is_loopback_or_private_host("172.31.255.255"));
    assert!(is_loopback_or_private_host("192.168.1.1"));

    // Link-local & shared (RFC 3927, RFC 6598)
    assert!(is_loopback_or_private_host("169.254.1.1"));
    assert!(is_loopback_or_private_host("100.64.0.1"));

    // IPv6 ULA & link-local
    assert!(is_loopback_or_private_host("fc00::1"));
    assert!(is_loopback_or_private_host("fd12:3456::1"));
    assert!(is_loopback_or_private_host("fe80::1"));
    assert!(is_loopback_or_private_host("::ffff:127.0.0.1"));
    assert!(is_loopback_or_private_host("::ffff:192.168.0.1"));

    // Public / external hosts (must NOT be filtered)
    assert!(!is_loopback_or_private_host("api.openai.com"));
    assert!(!is_loopback_or_private_host("anthropic.com"));
    assert!(!is_loopback_or_private_host("8.8.8.8"));
    assert!(!is_loopback_or_private_host("1.1.1.1"));
    assert!(!is_loopback_or_private_host("2606:4700:4700::1111"));
}
