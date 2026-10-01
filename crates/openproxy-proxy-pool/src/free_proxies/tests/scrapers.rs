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
async fn sync_github_lists_hits_injected_base_and_parses_text() {
    use httpmock::prelude::*;
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/komutan234/Proxy-List-Free/main/proxies/http.txt");
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
