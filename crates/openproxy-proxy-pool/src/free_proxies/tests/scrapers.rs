use super::{fresh_pool, scraped_with_country};
use crate::free_proxies::*;

#[test]
fn parse_proxy_host_port_accepts_valid_ipv4_port() {
    let r = parse_proxy_host_port("1.2.3.4:8080");
    assert_eq!(r, Some(("1.2.3.4".to_string(), 8080)));
}

#[test]
fn parse_proxy_host_port_accepts_hostname_port() {
    let r = parse_proxy_host_port("  proxy.example.com : 3128 ");
    assert_eq!(r, Some(("proxy.example.com".to_string(), 3128)));
}

#[test]
fn parse_proxy_host_port_rejects_missing_port() {
    assert_eq!(parse_proxy_host_port("1.2.3.4"), None);
}

#[test]
fn parse_proxy_host_port_rejects_empty_host() {
    assert_eq!(parse_proxy_host_port(":8080"), None);
}

#[test]
fn parse_proxy_host_port_rejects_zero_port() {
    assert_eq!(parse_proxy_host_port("1.2.3.4:0"), None);
}

#[test]
fn parse_proxy_host_port_rejects_non_numeric_port() {
    assert_eq!(parse_proxy_host_port("1.2.3.4:abc"), None);
}

#[test]
fn parse_proxy_host_port_rejects_port_overflow() {
    assert_eq!(parse_proxy_host_port("1.2.3.4:99999"), None);
}

#[test]
fn parse_plain_proxy_lines_skips_comments_and_blanks() {
    let body = "\
# Header comment line
1.2.3.4:8080

   5.6.7.8:1080
# Another comment
9.10.11.12:3128   \t
";
    let parsed = parse_plain_proxy_lines(body, "testsrc", "http");
    assert_eq!(parsed.len(), 3);
    assert_eq!(parsed[0].host, "1.2.3.4");
    assert_eq!(parsed[0].port, 8080);
    assert_eq!(parsed[0].r#type, "http");
    assert_eq!(parsed[0].source, "testsrc");
    assert_eq!(parsed[1].host, "5.6.7.8");
    assert_eq!(parsed[2].host, "9.10.11.12");
    assert_eq!(parsed[2].port, 3128);
}

#[test]
fn parse_plain_proxy_lines_skips_malformed_rows() {
    let body = "\
1.2.3.4:8080
broken
1.2.3.4:notaport
5.6.7.8:1080
";
    let parsed = parse_plain_proxy_lines(body, "testsrc", "socks5");
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].host, "1.2.3.4");
    assert_eq!(parsed[0].port, 8080);
    assert_eq!(parsed[0].r#type, "socks5");
    assert_eq!(parsed[1].host, "5.6.7.8");
}

#[test]
fn parse_plain_proxy_lines_empty_input() {
    assert!(parse_plain_proxy_lines("", "src", "http").is_empty());
    assert!(parse_plain_proxy_lines("\n\n\n", "src", "http").is_empty());
    assert!(parse_plain_proxy_lines("# only comments\n# here\n", "src", "http").is_empty());
}

#[test]
fn parse_custom_proxy_auth_separates_user_and_password() {
    let (u, p) = parse_custom_proxy_auth(Some("alice:secret"));
    assert_eq!(u.as_deref(), Some("alice"));
    assert_eq!(p.as_deref(), Some("secret"));
}

#[test]
fn parse_custom_proxy_auth_token_without_colon() {
    let (u, p) = parse_custom_proxy_auth(Some("bearer-token-xyz"));
    assert_eq!(u.as_deref(), Some("bearer-token-xyz"));
    assert_eq!(p, None);
}

#[test]
fn parse_custom_proxy_auth_none() {
    let (u, p) = parse_custom_proxy_auth(None);
    assert_eq!(u, None);
    assert_eq!(p, None);
}

#[test]
fn parse_custom_proxy_auth_trims_whitespace() {
    let (u, p) = parse_custom_proxy_auth(Some("  alice : secret  "));
    assert_eq!(u.as_deref(), Some("alice"));
    assert_eq!(p.as_deref(), Some("secret"));
}

#[test]
fn parse_custom_proxy_line_default_protocol_is_http() {
    let p = parse_custom_proxy_line("1.2.3.4:8080", "mysrc", 5).unwrap();
    assert_eq!(p.source, "mysrc");
    assert_eq!(p.host, "1.2.3.4");
    assert_eq!(p.port, 8080);
    assert_eq!(p.r#type, "http");
    assert_eq!(p.priority, 5);
    assert_eq!(p.username, None);
    assert_eq!(p.password, None);
}

#[test]
fn parse_custom_proxy_line_with_scheme_and_auth() {
    let p =
        parse_custom_proxy_line("socks5://proxy.example.com:1080:user:pass", "mysrc", 3).unwrap();
    assert_eq!(p.r#type, "socks5");
    assert_eq!(p.host, "proxy.example.com");
    assert_eq!(p.port, 1080);
    assert_eq!(p.username.as_deref(), Some("user"));
    assert_eq!(p.password.as_deref(), Some("pass"));
    assert_eq!(p.priority, 3);
}

#[test]
fn parses_standard_legacy_and_typed_proxy_lists() {
    for (line, scheme, host, user, pass) in [
        (
            "HTTP://alice:p%40ss@[2001:db8::1]:1080",
            "http",
            "2001:db8::1",
            Some("alice"),
            Some("p@ss"),
        ),
        (
            "socks5h://alice%3Ab:p%25ss@PROXY.example:1080",
            "socks5",
            "proxy.example",
            Some("alice:b"),
            Some("p%ss"),
        ),
        (
            "socks4a://alice@proxy.example:1080",
            "socks4",
            "proxy.example",
            Some("alice"),
            None,
        ),
        (
            "[2001:db8::1]:1080:alice:p@ss",
            "http",
            "2001:db8::1",
            Some("alice"),
            Some("p@ss"),
        ),
    ] {
        let proxy = parse_custom_proxy_line(line, "fixture", 3).expect(line);
        assert_eq!(
            (
                &*proxy.r#type,
                &*proxy.host,
                proxy.port,
                proxy.username.as_deref(),
                proxy.password.as_deref()
            ),
            (scheme, host, 1080, user, pass)
        );
    }
    for line in [
        "ftp://proxy.example:1080",
        "socks5://alice@proxy.example:1080",
        "http://alice:%ZZ@proxy.example:1080",
        "http://alice:%0A@proxy.example:1080",
        "[not-ip]:1080",
        "proxy.example:1080/path",
    ] {
        assert!(
            parse_custom_proxy_line(line, "fixture", 0).is_none(),
            "{line}"
        );
    }
    let rows = parse_plain_proxy_lines(
        "proxy.example:1080\nhttp://example.com:80",
        "fixture",
        "socks5",
    );
    assert_eq!(
        rows.iter().map(|p| p.r#type.as_str()).collect::<Vec<_>>(),
        ["socks5", "http"]
    );
}

#[test]
fn parse_custom_proxy_line_skips_comments_and_blanks() {
    assert!(parse_custom_proxy_line("# a comment", "src", 0).is_none());
    assert!(parse_custom_proxy_line("", "src", 0).is_none());
    assert!(parse_custom_proxy_line("   \t  ", "src", 0).is_none());
}

#[test]
fn parse_custom_proxy_line_rejects_unparseable_port() {
    assert!(parse_custom_proxy_line("1.2.3.4:notaport", "src", 0).is_none());
}

#[test]
fn parse_custom_proxy_line_rejects_zero_port() {
    assert!(parse_custom_proxy_line("1.2.3.4:0", "src", 0).is_none());
}

#[test]
fn parse_vakhov_port_handles_number() {
    let v: serde_json::Value = serde_json::json!(8080);
    assert_eq!(parse_vakhov_port(&v), Some(8080));
}

#[test]
fn parse_vakhov_port_handles_string() {
    let v: serde_json::Value = serde_json::json!("1080");
    assert_eq!(parse_vakhov_port(&v), Some(1080));
}

#[test]
fn parse_vakhov_port_rejects_null_and_unparseable_string() {
    let null: serde_json::Value = serde_json::Value::Null;
    assert_eq!(parse_vakhov_port(&null), None);
    let unparseable: serde_json::Value = serde_json::json!("not-a-port");
    assert_eq!(parse_vakhov_port(&unparseable), None);
    let arr: serde_json::Value = serde_json::json!([8080]);
    assert_eq!(parse_vakhov_port(&arr), None);
    let obj: serde_json::Value = serde_json::json!({"p": 8080});
    assert_eq!(parse_vakhov_port(&obj), None);
    let boolean: serde_json::Value = serde_json::json!(true);
    assert_eq!(parse_vakhov_port(&boolean), None);
}

#[test]
fn resolve_scraped_sources_for_builtin_returns_scraped_list() {
    let v = resolve_scraped_sources(true, "builtin_proxifly", "ignored");
    assert_eq!(v, vec!["proxifly"]);
}

#[test]
fn resolve_scraped_sources_for_unknown_builtin_falls_back_to_name() {
    let v = resolve_scraped_sources(true, "builtin_unknown", "myName");
    assert_eq!(v, vec!["myName"]);
}

#[test]
fn resolve_scraped_sources_for_custom_returns_name() {
    let v = resolve_scraped_sources(false, "any-id", "Custom Source");
    assert_eq!(v, vec!["Custom Source"]);
}

#[test]
fn scraped_protocol_changes_reset_health_but_cannot_replace_manual_proxies() {
    let (_tmp, pool) = fresh_pool("proxy-upsert-health");
    let mut conn = pool.writer();
    let mut item = scraped_with_country("feed", "proxy.example", 1080, "socks5", "US");
    upsert_scraped_proxies(&mut conn, std::slice::from_ref(&item)).expect("import");
    let id: String = conn
        .query_row("SELECT id FROM free_proxies", [], |r| r.get(0))
        .expect("proxy");
    update_proxy_status(&conn, &id, "alive", Some(120)).expect("alive");
    item.r#type = "http".into();
    upsert_scraped_proxies(&mut conn, std::slice::from_ref(&item)).expect("new protocol");
    let changed = get_proxy(&conn, &id).expect("read").expect("proxy");
    assert_eq!(changed.r#type, "http");
    assert_eq!(changed.status, "unknown");
    assert!(changed.latency_ms.is_none() && changed.last_validated.is_none());
    add_custom_proxy(
        &conn,
        "proxy.example",
        1080,
        "socks5",
        None,
        Some("user"),
        Some("pass"),
    )
    .expect("manual");
    update_proxy_status(&conn, &id, "alive", Some(90)).expect("alive");
    upsert_scraped_proxies(&mut conn, &[item]).expect("refresh");
    let protected = get_proxy(&conn, &id).expect("read").expect("proxy");
    assert_eq!(protected.r#type, "socks5");
    assert_eq!(protected.username.as_deref(), Some("user"));
    assert_eq!(protected.status, "alive");
}

#[test]
fn e2e_upsert_is_idempotent_for_same_host_port() {
    let (_tmp, pool) = fresh_pool("idempotent");

    let scraped = vec![scraped_with_country("test", "10.0.0.1", 8080, "http", "US")];

    {
        let mut w = pool.writer();
        upsert_scraped_proxies(&mut w, &scraped).expect("upsert");
    }
    let count_after_first: i64 = {
        let r = pool.reader();
        r.query_row("SELECT COUNT(*) FROM free_proxies", [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(count_after_first, 1);

    std::thread::sleep(std::time::Duration::from_millis(10));
    {
        let mut w = pool.writer();
        upsert_scraped_proxies(&mut w, &scraped).expect("upsert #2");
    }
    let count_after_second: i64 = {
        let r = pool.reader();
        r.query_row("SELECT COUNT(*) FROM free_proxies", [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(count_after_second, 1, "upsert must not duplicate rows");

    let new_source: String = {
        let r = pool.reader();
        r.query_row(
            "SELECT source FROM free_proxies WHERE host = '10.0.0.1'",
            [],
            |row| row.get(0),
        )
        .unwrap()
    };
    assert_eq!(new_source, "test");
}

#[tokio::test]
async fn sync_proxifly_hits_injected_url_and_parses_json() {
    use httpmock::prelude::*;
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/proxy");
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"[{"ip":"1.2.3.4","port":8080,"protocol":"HTTP",
                      "geolocation":{"country":"US"}}]"#,
            );
    });

    let url = format!("{}/proxy?format=json&quantity=100", server.base_url());
    let proxies = sync_proxifly(&url).await.expect("proxifly sync");

    mock.assert();
    assert_eq!(proxies.len(), 1);
    assert_eq!(proxies[0].host, "1.2.3.4");
    assert_eq!(proxies[0].port, 8080);
    assert_eq!(proxies[0].r#type, "http");
    assert_eq!(proxies[0].country_code.as_deref(), Some("US"));
    assert_eq!(proxies[0].source, "proxifly");
}

#[tokio::test]
async fn sync_proxmint_hits_injected_url_and_parses_json() {
    use httpmock::prelude::*;
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/proxies/all.json");
        then.status(200)
            .header("content-type", "application/json")
            .body(
                r#"{"count": 1, "proxies": [{"ip":"107.174.30.92","port":1080,"protocol":"socks5","country":"us"}]}"#,
            );
    });

    let url = format!("{}/proxies/all.json", server.base_url());
    let proxies = sync_proxmint(&url).await.expect("proxmint sync");

    mock.assert();
    assert_eq!(proxies.len(), 1);
    assert_eq!(proxies[0].host, "107.174.30.92");
    assert_eq!(proxies[0].port, 1080);
    assert_eq!(proxies[0].r#type, "socks5");
    assert_eq!(proxies[0].country_code.as_deref(), Some("us"));
    assert_eq!(proxies[0].source, "proxmint");
    assert_eq!(proxies[0].priority, 10);
}

#[tokio::test]
async fn sync_github_lists_hits_injected_base_and_parses_text() {
    use httpmock::prelude::*;
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/kael-odin/awesome-free-proxy-list/main/proxies/http.txt");
        then.status(200).body("9.9.9.9:3128\n8.8.8.8:80\n");
    });
    server.mock(|when, then| {
        when.method(GET);
        then.status(404);
    });

    let proxies = sync_github_lists(&server.base_url())
        .await
        .expect("github sync");

    mock.assert_calls(1);
    assert!(
        proxies
            .iter()
            .any(|p| p.host == "9.9.9.9" && p.port == 3128)
    );
    assert!(proxies.iter().any(|p| p.host == "8.8.8.8" && p.port == 80));
}

#[tokio::test]
async fn public_text_builtins_import_their_feed_formats() {
    use httpmock::prelude::*;

    let server = MockServer::start();
    let v4 = server.mock(|when, then| {
        when.method(GET)
            .path("/v4/free-proxy-list/get")
            .query_param("request", "get_proxies")
            .query_param("proxy_format", "protocolipport")
            .query_param("format", "text")
            .query_param("limit", "15000");
        then.status(200).body(concat!(
            "\u{feff}socks5://1.2.3.4:1080\r\n",
            "socks4://1.2.3.5:1080\nhttp://1.2.3.6:8080\n",
            "https://[2606:4700::1]:443\n",
            "1.2.3.7:80\nftp://1.2.3.8:21\nhttp://127.0.0.1:80\n",
            "socks5://proxy.example:1080\nhttp://224.0.0.1:80\n",
            "http://1.2.3.9:8080:path\nhttp://1.2.3.10:65536\n",
        ));
    });
    let proxy_scraper = server.mock(|when, then| {
        when.method(GET)
            .path("/ProxyScraper/ProxyScraper/main/socks5.txt");
        then.status(200).body("1.2.3.4:1080\n1.2.3.11:1080\n");
    });
    let vpslab = server.mock(|when, then| {
        when.method(GET)
            .path("/VPSLabCloud/VPSLab-Free-Proxy-List/main/socks5_all.txt");
        then.status(200)
            .body("socks5://1.2.3.12:1080\nhttp://1.2.3.13:80\n10.0.0.1:1080\n");
    });
    let (_tmp, pool) = fresh_pool("public-text-builtins");
    for (id, label, protocols) in [
        (
            "builtin_proxyscrape_v4",
            "proxyscrape_v4",
            &["socks5", "socks4", "http", "https"][..],
        ),
        (
            "builtin_proxyscraper",
            "proxyscraper",
            &["socks5", "socks5"][..],
        ),
        ("builtin_vpslab", "vpslab", &["socks5"][..]),
    ] {
        let def = BuiltinProxySourceDef::find_by_id(id).expect("registered source");
        assert_eq!(resolve_scraped_sources(true, id, "ignored"), [label]);
        let uri: http::Uri = def.url.parse().expect("valid feed URL");
        let url = format!(
            "{}{}",
            server.base_url(),
            uri.path_and_query().expect("feed path")
        );
        let rows = (def.sync_fn)(&url).await.expect("download and parse feed");
        assert_eq!(
            rows.iter().map(|p| p.r#type.as_str()).collect::<Vec<_>>(),
            protocols
        );
        assert!(
            rows.iter()
                .all(|p| p.source == label && p.username.is_none() && p.password.is_none())
        );
        upsert_scraped_proxies(&mut pool.writer(), &rows).expect("store feed candidates");
    }
    v4.assert_calls(1);
    proxy_scraper.assert_calls(1);
    vpslab.assert_calls(1);
    let (total, unknown): (i64, i64) = pool
        .reader()
        .query_row(
            "SELECT count(*), sum(status = 'unknown') FROM free_proxies",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("read imported candidates");
    assert_eq!(
        (total, unknown),
        (6, 6),
        "overlapping feeds deduplicate; candidates are not alive yet"
    );
}

#[tokio::test]
async fn public_text_feed_reports_http_format_and_size_errors() {
    use httpmock::prelude::*;
    let server = MockServer::start();
    for (path, status, body, expected) in [
        (
            "/service-unavailable",
            503,
            String::new(),
            "HTTP status: 503",
        ),
        (
            "/not-a-feed",
            200,
            "<html>not a feed</html>".into(),
            "no valid public endpoints",
        ),
        (
            "/oversized",
            200,
            "a".repeat(2 * 1024 * 1024 + 1),
            "feed exceeds",
        ),
    ] {
        server.mock(|when, then| {
            when.method(GET).path(path);
            then.status(status).body(body);
        });
        let url = format!("{}{path}", server.base_url());
        let def = BuiltinProxySourceDef::find_by_id("builtin_proxyscrape_v4").expect("registered");
        let error = (def.sync_fn)(&url)
            .await
            .expect_err("reject unusable feed")
            .to_string();
        assert!(error.contains(expected), "{error}");
    }
}

#[tokio::test]
async fn sync_proxifly_returns_err_on_non_200() {
    use httpmock::prelude::*;
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/proxy");
        then.status(503);
    });
    let url = format!("{}/proxy", server.base_url());
    let res = sync_proxifly(&url).await;
    assert!(res.is_err(), "non-200 must surface a CoreError");
}

async fn dispatch_protocol_scraper(
    source: &str,
    url: &str,
) -> crate::error::Result<Vec<ScrapedProxy>> {
    match source {
        "proxyscrape_cdn" => sync_proxyscrape_cdn(url).await,
        "clearproxy" => sync_clearproxy(url).await,
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn protocol_scrapers_table_driven_shared_payload() {
    use httpmock::prelude::*;

    let payload = r#"[
        {"ip":"1.2.3.4","port":8080,"protocol":"HTTP","country_code":"US"},
        {"ip":"2.3.4.5","port":80,"protocol":"HTTPS","country_code":""},
        {"ip":"3.4.5.6","port":1080,"protocol":"SOCKS5","country_code":null},
        {"ip":"4.5.6.7","port":3128,"protocol":"HTTP","country_code":"   "},
        {"ip":"5.6.7.8","port":9050,"protocol":"HTTP\u0130","country_code":"TR"},
        {"ip":"6.7.8.9","port":0,"protocol":"http","country_code":"DE"},
        {"ip":"7.8.9.10","port":65535,"protocol":"socks4","country_code":"FR"},
        {"ip":"1.2.3.4","port":8080,"protocol":"HTTP","country_code":"US"}
    ]"#;

    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(GET).path("/proxies");
        then.status(200)
            .header("content-type", "application/json")
            .body(payload);
    });

    let url = format!("{}/proxies", server.base_url());
    let expected = [
        ("1.2.3.4", 8080, "http", Some("US")),
        ("2.3.4.5", 80, "https", None),
        ("3.4.5.6", 1080, "socks5", None),
        ("4.5.6.7", 3128, "http", Some("   ")),
        ("5.6.7.8", 9050, "httpi\u{0307}", Some("TR")),
        ("6.7.8.9", 0, "http", Some("DE")),
        ("7.8.9.10", 65535, "socks4", Some("FR")),
        ("1.2.3.4", 8080, "http", Some("US")),
    ];

    for src in ["proxyscrape_cdn", "clearproxy"] {
        let proxies = dispatch_protocol_scraper(src, &url)
            .await
            .expect("sync success");
        assert_eq!(proxies.len(), expected.len());
        for (p, &(host, port, proto, country)) in proxies.iter().zip(&expected) {
            assert_eq!(p.source, src);
            assert_eq!(p.host, host);
            assert_eq!(p.port, port);
            assert_eq!(p.r#type, proto);
            assert_eq!(p.country_code.as_deref(), country);
            assert_eq!(
                (p.username.as_deref(), p.password.as_deref(), p.priority),
                (None, None, 0)
            );
        }
    }
}

#[tokio::test]
async fn protocol_scrapers_table_driven_errors_and_labels() {
    use httpmock::prelude::*;

    let server = MockServer::start();
    let error_cases = [
        (
            "/overflow",
            200,
            r#"[{"ip":"1.2.3.4","port":65536,"protocol":"http"}]"#,
            "JSON error:",
        ),
        (
            "/negative",
            200,
            r#"[{"ip":"1.2.3.4","port":-1,"protocol":"http"}]"#,
            "JSON error:",
        ),
        (
            "/missing-proto",
            200,
            r#"[{"ip":"1.2.3.4","port":8080}]"#,
            "JSON error:",
        ),
        ("/malformed", 200, "not-valid-json", "JSON error:"),
        ("/non200", 503, "", "HTTP status: 503"),
    ];

    for &(path, status, body, _) in &error_cases {
        server.mock(|when, then| {
            when.method(GET).path(path);
            then.status(status).body(body);
        });
    }

    let sources = [
        ("proxyscrape_cdn", "ProxyScrape CDN"),
        ("clearproxy", "ClearProxy"),
    ];

    for (src, label) in sources {
        for &(path, _, _, expected_suffix) in &error_cases {
            let url = format!("{}{path}", server.base_url());
            let err = dispatch_protocol_scraper(src, &url).await.unwrap_err();
            assert!(
                err.to_string()
                    .contains(&format!("{label} {expected_suffix}")),
                "{src} on {path}: expected suffix '{expected_suffix}'"
            );
        }
    }
}
