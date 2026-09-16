use super::super::*;
use openproxy_types::config::PiiEntity;

#[test]
fn test_secrets_and_api_keys_redaction() {
    let engine = PiiEngine::new(&[PiiEntity::Secret]);
    let mut session = PiiSession::new(true);
    let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
    let input = format!(
        "Keys: Bearer secret_bearer_token_1234567890, OpenAI sk-proj-1234567890abcdefghijklmn, AWS AKIAIOSFODNN7EXAMPLE, GitHub ghp_123456789012345678901234567890123456, JWT: {jwt}."
    );
    let redacted = engine.redact_text(&input, &mut session);
    assert!(
        redacted.contains("Bearer sec_")
            && redacted.contains("sk-proj-")
            && redacted.contains("ghp_")
    );
    for leaked in [
        "secret_bearer_token_1234567890",
        "sk-proj-1234567890abcdefghijklmn",
        "AKIAIOSFODNN7EXAMPLE",
        "ghp_123456789012345678901234567890123456",
        jwt,
    ] {
        assert!(!redacted.contains(leaked), "leaked: {leaked}");
    }
    assert_eq!(session.restore_text(&redacted), input);
}

#[test]
fn test_base64_secrets_and_google_api_keys() {
    let engine = PiiEngine::new(&[PiiEntity::Secret]);
    let mut session = PiiSession::new(true);
    let (gk, bb64, lb64) = (
        "AIzaSyDaP5x9qY7z1W3e5R7t9Y1u3I5o7P9a1S3",
        "Bearer abcd+efgh/ijkl=1234567890123456",
        "api_key: \"secret+key/with=special/chars1234567\"",
    );
    let text = format!("Credentials: Google {gk}, {bb64}, {lb64}.");
    let red = engine.redact_text(&text, &mut session);
    assert!(
        !red.contains(gk)
            && !red.contains("abcd+efgh/ijkl=1234567890123456")
            && !red.contains("secret+key/with=special/chars1234567")
    );
    assert_eq!(session.restore_text(&red), text);
}

#[test]
fn test_cascading_substitution_prevention() {
    let mut session = PiiSession::new(true);
    let k1 = session.get_or_create_placeholder(PiiEntity::Secret, "<EMAIL_1>");
    let e1 = session.get_or_create_placeholder(PiiEntity::Email, "alice@example.com");
    let text = format!("Key is {k1} and email is {e1}.");
    assert_eq!(
        session.restore_text(&text),
        "Key is <EMAIL_1> and email is alice@example.com."
    );
}

#[test]
fn test_base64_bearer_with_padding_redaction() {
    let engine = PiiEngine::new(&[PiiEntity::Secret]);
    let mut session = PiiSession::new(true);
    let text = "Auth: Bearer c29tZXRva2VuZXhhbXBsZTEyMw== in header.";
    let red = engine.redact_text(text, &mut session);
    assert!(!red.contains("c29tZXRva2VuZXhhbXBsZTEyMw==") && !red.contains("=="));
    assert_eq!(session.restore_text(&red), text);
}

#[test]
fn test_cloudflare_credentials_redaction() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let mut session = PiiSession::new(true);
    let input = "ID de cuenta\ne1f8a9b2c3d4e5f67890abcdef123456\nTu token de API\ncfat_1A2B3C4D5E6F7a8b9c0d1e2f3a4b5c6d7e8f9a0b\nID de clave de acceso\na1b2c3d4e5f67890abcdef1234567890\nClave de acceso secreta\n0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n";
    let red = engine.redact_text(input, &mut session);
    assert_eq!(session.restore_text(&red), input);
}

#[test]
fn test_env_vars_and_service_credentials() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let cases = [
        ("CF_Token=lSy1234567890abcdef1234567890abcdef12", true),
        (
            "SEARXNG_SECRET=000111222333444555666777888999aaabbbcccdddeeefff0001112223334445",
            true,
        ),
        ("SECRET=1i8s9dt1y98234710928374109283741", true),
        ("WORDPRESS_DB_PASSWORD=act_secure_password_here_12345", true),
        ("GATECHA_ADMIN_PASSWORD=u4Tsuperadminpass123", true),
        (
            "TELEGRAM_BOT_TOKEN=7281928374:Agk12345678901234567890123456789012",
            true,
        ),
        (
            "INSTATIC_SECRET_KEY=a1z9uwsupersecretkeybase64padding==",
            true,
        ),
        ("MYSQL_ROOT_PASSWORD=my_super_root_pass_123", true),
        ("N8N_BASIC_AUTH_USER=admin", false),
        ("PORT=8080", false),
        ("DEBUG=true", false),
    ];
    for (input, should_redact) in cases {
        let mut session = PiiSession::new(true);
        let red = engine.redact_text(input, &mut session);
        if should_redact {
            assert!(!session.is_empty(), "Expected redact: {input}");
            assert_eq!(session.restore_text(&red), input);
        } else {
            assert!(session.is_empty(), "Expected no redact: {input}");
            assert_eq!(red, input);
        }
    }
}

#[test]
fn test_connection_strings_and_new_platform_secrets() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let check = |input: &str, must_hide: &[&str]| {
        let mut session = PiiSession::new(true);
        let red = engine.redact_text(input, &mut session);
        for h in must_hide {
            assert!(!red.contains(h), "Found {h} in {red}");
        }
        assert_eq!(session.restore_text(&red), input);
    };
    check(
        "Connect postgres://usuario:MI_PASSWORD_SECRETO@db.internal:5432/app",
        &["MI_PASSWORD_SECRETO"],
    );
    check(
        "Authorization: Basic dXNlcjpwYXNzd29yZDEyMzQ1Njc4",
        &["dXNlcjpwYXNzd29yZDEyMzQ1Njc4"],
    );
    let (sk_or, sk_live) = (
        format!("sk-or-v1-{}", "a1b2".repeat(16)),
        format!("sk_live_{}", "c9d0".repeat(6)),
    );
    check(
        &format!("Tokens: {sk_or} and {sk_live}"),
        &[&sk_or, &sk_live],
    );
    check(
        "Run mysql -u root -p'supersecret123' and curl -u user:apipassword",
        &["supersecret123", "apipassword"],
    );
    check(
        "User IBAN ES9121000418450200051332.",
        &["ES9121000418450200051332"],
    );
    check(
        "-----BEGIN PGP PRIVATE KEY BLOCK-----\nlCoEYWJjZGVmZ2hpams=\n-----END PGP PRIVATE KEY BLOCK-----",
        &["lCoEYWJjZGVmZ2hpams="],
    );
}

#[test]
fn test_production_config_fixtures_redaction() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let fixtures = [
        "CLOUDFLARE_API_TOKEN=mock_cf_token_40_chars_abcdef1234567890\nZONE_ID=f185016a2dfa10e924d8fa05e2aedfcd\n",
        "https://media.example.com/serve?token=MockFamilyToken32CharsValue12345\n",
        "MYSQL_ROOT_PASSWORD=MockRootPassword_32CharsRandom01\n",
        "GATECHA_DB_DSN: \"appuser:supersecretpass123@tcp(db:3306)/app\"\n",
        "password: $2a$10$N9qo8uLOickgx2ZMRZoMyeIjZAgcfl7p92ldGxad68LJZdL17lhWy\n",
        "SAVED_CF_Token='cfat_mocktoken1234567890abcdef1234567890'\n",
        "-----BEGIN EC PRIVATE KEY-----\nMHcCAQEEIG1vY2tfcHJpdmF0ZV9rZXlfZm9yX3Rlc3Rpbmdfb25seV9ub3RfcmVhbA==\n-----END EC PRIVATE KEY-----\n",
        "ENDPOINT_KEY=op_live_mocklivekey1234567890abcdef1234567890\n",
    ];
    for f in fixtures {
        let mut session = PiiSession::new(true);
        let red = engine.redact_text(f, &mut session);
        assert!(!session.is_empty());
        assert_eq!(session.restore_text(&red), f);
    }
}

#[test]
fn test_seed_existing_placeholders_utf8_char_boundaries() {
    let mut session = PiiSession::new(true);
    let text = "Here is a truncated secret: sk-… and another sk-proj-🚀 and sec_…\n\
                Also IP prefix truncated: 10.240.… and 10.240.árbol\n\
                Also numeric prefix truncated: 89410294… and 89410294🚀\n\
                Email prefix with non-ascii: testñ99@outlook.com and …12@fastmail.com\n\
                Bracketed format: <KEY_…> and <KEY_15> and (P…)\n\
                Persona placeholder with multi-byte: Alex Vance (P3) and Marcus Sterling [P7]";
    session.seed_existing_placeholders(text);
    assert_eq!(*session.counts.get(&PiiEntity::Secret).unwrap_or(&0), 15);
    assert_eq!(*session.counts.get(&PiiEntity::Person).unwrap_or(&0), 7);
    assert_eq!(*session.counts.get(&PiiEntity::Email).unwrap_or(&0), 99);
}

#[test]
fn test_seed_existing_placeholders_exact_sk_ellipsis_panic() {
    let mut session = PiiSession::new(true);
    let padding = "a".repeat(1000);
    session.seed_existing_placeholders(&format!(
        "{padding}sk-…more text sk-proj-… op_live_… 10.240.…"
    ));
}

#[test]
fn test_deterministic_compact_secret_placeholders() {
    let key = "6df92a6a557b4e72898e81d430db23a6";
    let mut s1 = PiiSession::new(true);
    let p1 = s1.get_or_create_placeholder(PiiEntity::Secret, key);
    assert_eq!(p1.len(), 12);
    assert!(p1.starts_with("sec_"));

    let mut s2 = PiiSession::new(true);
    let p2 = s2.get_or_create_placeholder(PiiEntity::Secret, key);
    assert_eq!(p1, p2);
    assert_eq!(
        s1.restore_text(&format!("token: {p1}")),
        format!("token: {key}")
    );

    let proj_key = "sk-proj-1234567890abcdefghijklmn";
    let pp1 = s1.get_or_create_placeholder(PiiEntity::Secret, proj_key);
    assert_eq!(pp1.len(), 16);
    assert!(pp1.starts_with("sk-proj-"));
    assert_eq!(
        pp1,
        s2.get_or_create_placeholder(PiiEntity::Secret, proj_key)
    );
}
