use super::setup_test_db;
use crate::free_proxies::*;
use rusqlite::Connection;

#[test]
fn test_crud_custom_proxy() {
    let conn = setup_test_db();

    let p = add_custom_proxy(&conn, "1.2.3.4", 8080, "http", Some("US"), None, None).unwrap();
    assert_eq!(p.host, "1.2.3.4");
    assert_eq!(p.port, 8080);
    assert_eq!(p.r#type, "http");
    assert_eq!(p.country_code.as_deref(), Some("US"));
    assert_eq!(p.status, "unknown");

    let list = list_proxies(&conn, None, None, None, None, None, None).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, p.id);

    update_proxy_status(&conn, &p.id, "alive", Some(150)).unwrap();

    let list2 = list_proxies(&conn, None, Some("alive"), None, None, None, None).unwrap();
    assert_eq!(list2.len(), 1);
    assert_eq!(list2[0].status, "alive");
    assert_eq!(list2[0].latency_ms, Some(150));

    delete_proxy(&conn, &p.id).unwrap();
    let list3 = list_proxies(&conn, None, None, None, None, None, None).unwrap();
    assert_eq!(list3.len(), 0);
}

#[test]
fn test_upsert_scraped_proxies() {
    let mut conn = setup_test_db();

    let scraped = vec![
        super::scraped_with_country("proxifly", "10.0.0.1", 3128, "https", "FR"),
        super::scraped("iplocate", "10.0.0.2", 1080, "socks5"),
    ];

    upsert_scraped_proxies(&mut conn, &scraped).unwrap();

    let list = list_proxies(&conn, None, None, None, None, None, None).unwrap();
    assert_eq!(list.len(), 2);

    upsert_scraped_proxies(&mut conn, &scraped).unwrap();
    let list2 = list_proxies(&conn, None, None, None, None, None, None).unwrap();
    assert_eq!(list2.len(), 2);
}

#[test]
fn test_get_or_assign_provider_proxy_flow() {
    let conn = setup_test_db();

    let provider_id = crate::ids::ProviderId::new("test-provider");

    conn.execute(
        "INSERT INTO providers (id, name, base_url, auth_type, format) VALUES (?1, 'Test', 'http://localhost', 'bearer', 'openai')",
        rusqlite::params![provider_id.0],
    ).unwrap();

    let proxy = get_or_assign_provider_proxy(&conn, &provider_id, None).unwrap();
    assert_eq!(proxy, None);

    conn.execute(
        "UPDATE providers SET use_proxies = 1 WHERE id = ?1",
        rusqlite::params![provider_id.0],
    )
    .unwrap();

    assert!(get_or_assign_provider_proxy(&conn, &provider_id, None).is_err());

    let p = add_custom_proxy(&conn, "1.2.3.4", 8080, "socks5", None, None, None).unwrap();
    update_proxy_status(&conn, &p.id, "alive", Some(100)).unwrap();

    let proxy = get_or_assign_provider_proxy(&conn, &provider_id, None).unwrap();
    assert_eq!(proxy, Some("socks5://1.2.3.4:8080".to_string()));

    openproxy_db::cooldowns::add_provider_proxy_cooldown(
        &conn,
        "test-provider",
        &p.id,
        std::time::Duration::from_mins(15),
    )
    .unwrap();

    assert!(get_or_assign_provider_proxy(&conn, &provider_id, None).is_err());

    let p2 = add_custom_proxy(&conn, "5.6.7.8", 9090, "http", None, None, None).unwrap();
    update_proxy_status(&conn, &p2.id, "alive", Some(120)).unwrap();

    let proxy_after_cooldown = get_or_assign_provider_proxy(&conn, &provider_id, None).unwrap();
    assert_eq!(
        proxy_after_cooldown,
        Some("http://5.6.7.8:9090".to_string())
    );

    let bound_id: Option<String> = conn
        .query_row(
            "SELECT current_proxy_id FROM providers WHERE id = ?1",
            rusqlite::params![provider_id.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(bound_id.as_deref(), Some(p2.id.as_str()));

    let proxy2 = get_or_assign_provider_proxy(&conn, &provider_id, None).unwrap();
    assert_eq!(proxy2, Some("http://5.6.7.8:9090".to_string()));

    update_proxy_status(&conn, &p2.id, "dead", Some(9999)).unwrap();
    assert!(get_or_assign_provider_proxy(&conn, &provider_id, None).is_err());
}

#[test]
fn test_get_candidate_proxies_for_provider() {
    let mut conn = Connection::open_in_memory().unwrap();
    openproxy_db::migrations::run(&mut conn).unwrap();

    let provider_id = openproxy_types::ids::ProviderId::new("zen-provider");
    crate::providers::create(
        &conn,
        crate::providers::NewProvider {
            id: &provider_id,
            name: "Zen Provider",
            base_url: "https://api.example.com",
            auth_type: openproxy_types::providers::AuthType::None,
            format: openproxy_types::providers::ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
        },
    )
    .unwrap();
    conn.execute(
        "UPDATE providers SET use_proxies = 1 WHERE id = 'zen-provider'",
        [],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO free_proxies (id, source, host, port, type, status, latency_ms) VALUES ('p1', 'test', '1.1.1.1', 8080, 'socks5', 'alive', 10)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO free_proxies (id, source, host, port, type, username, password, status, latency_ms) VALUES ('p2', 'test', '2.2.2.2', 8080, 'http', 'u', 'p', 'alive', 20)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO free_proxies (id, source, host, port, type, status, latency_ms) VALUES ('p3', 'test', '3.3.3.3', 8080, 'http', 'alive', 30)",
        [],
    )
    .unwrap();

    let candidates = get_candidate_proxies_for_provider(&conn, &provider_id, 2).unwrap();
    assert_eq!(candidates.len(), 2);

    openproxy_db::cooldowns::add_provider_proxy_cooldown(
        &conn,
        provider_id.as_str(),
        "p1",
        std::time::Duration::from_secs(3600),
    )
    .unwrap();

    let candidates_after_cd = get_candidate_proxies_for_provider(&conn, &provider_id, 3).unwrap();
    assert_eq!(candidates_after_cd.len(), 2);
    assert!(!candidates_after_cd.iter().any(|(id, _)| id == "p1"));
}

#[test]
fn test_proxyscrape_cdn_json_parsing() {
    let json_data = r#"[
        {
            "protocol": "socks4",
            "ip": "95.217.167.252",
            "port": 11117,
            "country": "Finland",
            "country_code": "FI"
        }
    ]"#;
    let items: Vec<serde_json::Value> = serde_json::from_str(json_data).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["ip"], "95.217.167.252");
    assert_eq!(items[0]["port"], 11117);
    assert_eq!(items[0]["protocol"], "socks4");
    assert_eq!(items[0]["country_code"], "FI");
}

#[test]
fn test_geonode_json_parsing() {
    let json_data = r#"{
        "data": [
            {
                "ip": "193.233.72.56",
                "port": "988",
                "protocols": ["socks5"],
                "country": "US"
            }
        ]
    }"#;
    let body: serde_json::Value = serde_json::from_str(json_data).unwrap();
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    assert_eq!(body["data"][0]["ip"], "193.233.72.56");
    assert_eq!(body["data"][0]["port"], "988");
}

#[test]
fn test_get_proxy_status_by_url() {
    let conn = setup_test_db();

    let p = add_custom_proxy(&conn, "1.2.3.4", 8080, "socks5", None, None, None).unwrap();

    assert_eq!(
        get_proxy_status_by_url(&conn, "socks5://1.2.3.4:8080"),
        Some("unknown".to_string())
    );

    update_proxy_status(&conn, &p.id, "alive", Some(100)).unwrap();
    assert_eq!(
        get_proxy_status_by_url(&conn, "socks5://1.2.3.4:8080"),
        Some("alive".to_string())
    );

    assert_eq!(get_proxy_status_by_url(&conn, "1.2.3.4:8080"), None);
    assert_eq!(get_proxy_status_by_url(&conn, "socks5://1.2.3.4"), None);
    assert_eq!(
        get_proxy_status_by_url(&conn, "socks5://9.9.9.9:8080"),
        None
    );
}

#[test]
fn test_proxy_sources_crud() {
    let conn = setup_test_db();

    let src = create_proxy_source(
        &conn,
        &CreateProxySourceInput {
            name: "Test Source".to_string(),
            url: "http://example.com/proxies.txt".to_string(),
            priority: Some(5),
            active: Some(true),
        },
    )
    .unwrap();

    assert_eq!(src.name, "Test Source");
    assert_eq!(src.url, "http://example.com/proxies.txt");
    assert_eq!(src.priority, 5);

    let list = list_proxy_sources(&conn).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, src.id);

    let fetched = get_proxy_source(&conn, &src.id).unwrap().unwrap();
    assert_eq!(fetched.name, "Test Source");

    let updated = update_proxy_source(
        &conn,
        &src.id,
        UpdateProxySourceInput {
            name: Some("Updated Source".to_string()),
            url: None,
            priority: Some(10),
            active: None,
        },
    )
    .unwrap();

    assert_eq!(updated.name, "Updated Source");
    assert_eq!(updated.priority, 10);

    let list_after = list_proxy_sources(&conn).unwrap();
    assert_eq!(list_after.len(), 1);
}
