use super::*;
use parking_lot::Mutex;

#[allow(dead_code)]
static TEST_LOCK: Mutex<()> = Mutex::new(());

// ADVERSARIAL BUILD_BEARER_HEADER

#[test]
fn test_build_bearer_header_table() {
    for bad in ["abc\r\nX: bad", "abc\nX: bad", "abc\0def", "abc\x7Fdef"] {
        assert!(build_bearer_header(bad).is_err(), "should fail: {bad:?}");
    }
    let ok_cases = [
        ("", "Bearer "),
        ("abc\tdef", "Bearer abc\tdef"),
        ("a-b.c_d~e", "Bearer a-b.c_d~e"),
        ("!$*+", "Bearer !$*+"),
    ];
    for (tok, exp) in ok_cases {
        let h = build_bearer_header(tok).unwrap();
        assert_eq!(h.to_str().unwrap(), exp);
    }
    for (tok, exp_bytes) in [
        ("café", &b"Bearer caf\xC3\xA9"[..]),
        ("🔑", &b"Bearer \xF0\x9F\x94\x91"[..]),
        ("abc\u{00C0}def", &b"Bearer abc\xC3\x80def"[..]),
    ] {
        let h = build_bearer_header(tok).unwrap();
        assert_eq!(h.as_bytes(), exp_bytes);
        assert!(h.to_str().is_err());
    }
    let mut bytes = b"abc".to_vec();
    bytes.extend_from_slice(&[0xE2, 0x80, 0xAE]);
    bytes.extend_from_slice(b"def");
    let h_rtl = build_bearer_header(std::str::from_utf8(&bytes).unwrap()).unwrap();
    assert!(h_rtl.as_bytes().windows(3).any(|w| w == [0xE2, 0x80, 0xAE]));

    let big = "a".repeat(1024 * 1024);
    assert_eq!(
        build_bearer_header(&big).unwrap().to_str().unwrap().len(),
        7 + big.len()
    );
}

// ADVERSARIAL INSERT_BEARER

#[test]
fn test_insert_bearer_behavior() {
    let mut req =
        crate::upstream::UpstreamRequest::post_json("https://example.test/", bytes::Bytes::new());
    req.headers.insert(
        http::header::HeaderName::from_static("x-custom"),
        http::HeaderValue::from_static("val"),
    );

    insert_bearer(&mut req, "first").unwrap();
    insert_bearer(&mut req, "second").unwrap();
    assert_eq!(
        req.headers
            .get_all(http::header::AUTHORIZATION)
            .iter()
            .count(),
        1
    );
    assert_eq!(
        req.headers
            .get(http::header::AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap(),
        "Bearer second"
    );
    assert_eq!(req.headers.get("x-custom").unwrap(), "val");
    assert_eq!(
        req.headers.get(http::header::CONTENT_TYPE).unwrap(),
        "application/json"
    );

    let mut req2 = crate::upstream::UpstreamRequest::get("https://example.test/");
    insert_bearer(&mut req2, "abc.def-ghi_jkl").unwrap();
    let bytes = req2
        .headers
        .get(http::header::AUTHORIZATION)
        .unwrap()
        .as_bytes();
    assert_eq!(bytes, b"Bearer abc.def-ghi_jkl");
    assert!(!bytes.ends_with(b" "));
}

// ADVERSARIAL OAUTH_POST_JSON

#[cfg(feature = "upstream-hyper")]
#[tokio::test]
async fn test_oauth_post_json_adversarial() {
    use crate::upstream::tests_helper as mock_helper;
    use std::sync::Arc;

    let upstream = mock_helper::build_mock_upstream_routing(|path| {
        if path.contains("503") {
            (503, "service down".to_string())
        } else if path.contains("400") {
            (400, "abc\0def".to_string())
        } else if path.contains("empty") {
            (200, String::new())
        } else {
            (200, r#"{"echoed":true}"#.to_string())
        }
    })
    .await;

    // Empty URL error
    assert!(
        oauth_post_json(
            &upstream,
            "",
            &serde_json::json!({}),
            "tok",
            crate::upstream::TimeoutProfile::Chat
        )
        .await
        .is_err()
    );

    // NaN & Inf serialization as null
    let body =
        serde_json::json!({"nan": f64::NAN, "inf": f64::INFINITY, "neg_inf": f64::NEG_INFINITY});
    let raw = serde_json::to_string(&body).unwrap();
    assert!(
        raw.contains(r#""nan":null"#)
            && raw.contains(r#""inf":null"#)
            && raw.contains(r#""neg_inf":null"#)
    );
    let res = oauth_post_json(
        &upstream,
        "https://example.test/x",
        &body,
        "tok",
        crate::upstream::TimeoutProfile::Chat,
    )
    .await
    .unwrap();
    assert_eq!(&res[..], br#"{"echoed":true}"#);

    // 5xx and 4xx handling
    let err503 = oauth_post_json(
        &upstream,
        "https://example.test/v1internal:foo?503",
        &serde_json::json!({}),
        "tok",
        crate::upstream::TimeoutProfile::Chat,
    )
    .await
    .unwrap_err();
    assert!(
        err503.contains("503")
            && err503.contains("service down")
            && err503.contains("v1internal:foo")
    );
    let err400 = oauth_post_json(
        &upstream,
        "https://example.test/400",
        &serde_json::json!({}),
        "tok",
        crate::upstream::TimeoutProfile::Chat,
    )
    .await
    .unwrap_err();
    assert!(err400.contains("400"));

    // 2xx empty body
    let empty_res = oauth_post_json(
        &upstream,
        "https://example.test/empty",
        &serde_json::json!({}),
        "tok",
        crate::upstream::TimeoutProfile::Chat,
    )
    .await
    .unwrap();
    assert!(empty_res.is_empty());

    // Concurrent calls
    let mut handles = Vec::new();
    for _ in 0..4 {
        let u = Arc::clone(&upstream);
        handles.push(tokio::spawn(async move {
            oauth_post_json(
                &u,
                "https://example.test/x",
                &serde_json::json!({}),
                "tok",
                crate::upstream::TimeoutProfile::Chat,
            )
            .await
        }));
    }
    for h in handles {
        assert!(h.await.unwrap().is_ok());
    }

    // Large 1KB body
    let big = serde_json::json!({"blob": "x".repeat(1024)});
    let big_upstream = mock_helper::build_mock_upstream_returning_status(200, "{}").await;
    assert_eq!(
        oauth_post_json(
            &big_upstream,
            "https://example.test/x",
            &big,
            "tok",
            crate::upstream::TimeoutProfile::Chat
        )
        .await
        .unwrap(),
        bytes::Bytes::from("{}")
    );
}

// ADVERSARIAL FETCH_WITH_FALLBACK

#[cfg(feature = "upstream-hyper")]
#[tokio::test]
async fn test_fetch_with_fallback_routing_and_fallbacks() {
    use crate::upstream::tests_helper as mock_helper;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let counter = Arc::new(AtomicUsize::new(0));
    let c1 = Arc::clone(&counter);
    let upstream = mock_helper::build_mock_upstream_routing(move |path| {
        if path.contains("nasty") {
            return (200, "not json".into());
        }
        let n = c1.fetch_add(1, Ordering::SeqCst);
        match n {
            0..=4 => (503, format!("err-{}", n + 1)),
            5 => (401, "auth required".into()),
            _ => (200, r#"{"v":42}"#.into()),
        }
    })
    .await;

    // 5 endpoints all 5xx returns last
    let eps5 = [
        "https://a.test/1",
        "https://a.test/2",
        "https://a.test/3",
        "https://a.test/4",
        "https://a.test/5",
    ];
    let err = fetch_with_fallback::<_, serde_json::Value>(
        &upstream,
        &eps5,
        &serde_json::json!({}),
        "tok",
        crate::upstream::TimeoutProfile::Quota,
        "all-fail",
    )
    .await
    .unwrap_err();
    assert!(err.contains("err-5"));
    assert_eq!(counter.load(Ordering::SeqCst), 5);

    // First 401, second 200 uses second
    let eps2 = ["https://a.test/1", "https://a.test/2"];
    let res: serde_json::Value = fetch_with_fallback(
        &upstream,
        &eps2,
        &serde_json::json!({}),
        "tok",
        crate::upstream::TimeoutProfile::Quota,
        "ctx",
    )
    .await
    .unwrap();
    assert_eq!(res["v"], 42);

    // Empty slice
    let err_empty = fetch_with_fallback::<_, serde_json::Value>(
        &upstream,
        &[],
        &serde_json::json!({}),
        "tok",
        crate::upstream::TimeoutProfile::Quota,
        "ctx-empty",
    )
    .await
    .unwrap_err();
    assert_eq!(err_empty, "ctx-empty: all endpoints failed");

    // Context special chars preserved
    let nasty = "ctx{x}:100%\u{1F608}";
    let err_nasty = fetch_with_fallback::<_, serde_json::Value>(
        &upstream,
        &["https://a.test/nasty"],
        &serde_json::json!({}),
        "tok",
        crate::upstream::TimeoutProfile::Quota,
        nasty,
    )
    .await
    .unwrap_err();
    assert!(err_nasty.contains(nasty));
}

#[cfg(feature = "upstream-hyper")]
#[tokio::test]
async fn test_fetch_with_fallback_types_and_edge_cases() {
    use crate::upstream::tests_helper as mock_helper;

    let upstream = mock_helper::build_mock_upstream_routing(|path| {
        if path.contains("parse-err") {
            (200, "<html>nope</html>".into())
        } else {
            (
                200,
                r#"{"id":"abc","count":7,"nested":{"flag":true},"ok":true}"#.into(),
            )
        }
    })
    .await;

    // Parse error format
    let err = fetch_with_fallback::<_, serde_json::Value>(
        &upstream,
        &["https://a.test/parse-err"],
        &serde_json::json!({}),
        "tok",
        crate::upstream::TimeoutProfile::Quota,
        "my-ctx",
    )
    .await
    .unwrap_err();
    assert!(err.contains("my-ctx") && err.contains("parse") && err.contains("a.test/parse-err"));

    // Unicode path passes through
    let eps = ["https://a.test/v1internal:foo?emoji=%F0%9F%94%91&name=hello%20world&rtl=%E2%80%AE"];
    let res: serde_json::Value = fetch_with_fallback(
        &upstream,
        &eps,
        &serde_json::json!({}),
        "tok",
        crate::upstream::TimeoutProfile::Quota,
        "unicode-ctx",
    )
    .await
    .unwrap();
    assert_eq!(res["ok"], true);

    // Custom struct deserialization
    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct Outer {
        id: String,
        count: u32,
        nested: Inner,
    }
    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct Inner {
        flag: bool,
    }
    let parsed: Outer = fetch_with_fallback(
        &upstream,
        &["https://a.test/only"],
        &serde_json::json!({}),
        "tok",
        crate::upstream::TimeoutProfile::Quota,
        "struct-ctx",
    )
    .await
    .unwrap();
    assert_eq!(
        parsed,
        Outer {
            id: "abc".into(),
            count: 7,
            nested: Inner { flag: true }
        }
    );

    // Dropped future does not panic
    let drop_body = serde_json::json!({});
    let fut = fetch_with_fallback::<_, serde_json::Value>(
        &upstream,
        &["https://a.test/1"],
        &drop_body,
        "tok",
        crate::upstream::TimeoutProfile::Quota,
        "drop",
    );
    drop(fut);
}
