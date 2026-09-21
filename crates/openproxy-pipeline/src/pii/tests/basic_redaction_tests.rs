use super::super::*;
use openproxy_types::config::PiiEntity;

fn check_roundtrip(entities: &[PiiEntity], input: &str, expected: &str) {
    let engine = PiiEngine::new(entities);
    let mut session = PiiSession::new(true);
    let redacted = engine.redact_text(input, &mut session);
    assert_eq!(redacted, expected);
    assert_eq!(session.restore_text(&redacted), input);
}

fn check_no_redact(entities: &[PiiEntity], input: &str) {
    let engine = PiiEngine::new(entities);
    let mut session = PiiSession::new(true);
    assert_eq!(engine.redact_text(input, &mut session), input);
}

#[test]
fn test_luhn_validation() {
    for valid in [
        "4532015112830366",
        "378282246310005",
        "5555555555554444",
        "6011111111111117",
    ] {
        assert!(luhn_check(valid), "valid card failed: {valid}");
    }
    for invalid in [
        "4532015112830367",
        "378282246310006",
        "1234567812345678",
        "0000000000000000",
        "123456789012",
        "12345678901234567890",
    ] {
        assert!(!luhn_check(invalid), "invalid card passed: {invalid}");
    }
}

#[test]
fn test_email_redaction() {
    check_roundtrip(
        &[PiiEntity::Email],
        "Contact alice@example.com or support.team+prod@company.co.uk for help.",
        "Contact alex.turner1@fastmail.com or jordan.lee2@outlook.com for help.",
    );
}

#[test]
fn test_phone_redaction() {
    check_roundtrip(
        &[PiiEntity::Phone],
        "Call international +1-800-555-0199 or regional (555) 123-4567 or 555-987-6543.",
        "Call international +1-202-555-0111 or regional +1-202-555-0112 or +1-202-555-0113.",
    );
    check_no_redact(
        &[PiiEntity::Phone],
        "Release date was 2024-05-12 and timestamp 14:30:00.",
    );
    check_roundtrip(
        &[PiiEntity::Phone],
        "Call 1-800-555-0199 for support.",
        "Call +1-202-555-0111 for support.",
    );
}

#[test]
fn test_ip_address_redaction() {
    check_roundtrip(
        &[PiiEntity::Ip],
        "Server at 192.168.1.1 and IPv6 2001:0db8:85a3:0000:0000:8a2e:0370:7334 or ::1.",
        "Server at 10.240.0.1 and IPv6 fd00:10:240::2 or fd00:10:240::3.",
    );
    check_no_redact(&[PiiEntity::Ip], "Upgraded to v1.2.3.4 in prod.");
    check_roundtrip(
        &[PiiEntity::Ip],
        "Connect to 2001:db8::1 or fe80::1 now.",
        "Connect to fd00:10:240::1 or fd00:10:240::2 now.",
    );
}

#[test]
fn test_credit_card_redaction_and_invalid_luhn_ignored() {
    check_roundtrip(
        &[PiiEntity::CreditCard],
        "Charged 4532-0151-1283-0366 on Visa.",
        "Charged 4532 0151 1283 1018 on Visa.",
    );
    check_no_redact(
        &[PiiEntity::CreditCard],
        "Bad card 4532-0151-1283-0367 was declined.",
    );
    check_no_redact(
        &[PiiEntity::CreditCard],
        "Product serial 4532-0151-1283-0366-1234 was activated.",
    );
}

#[test]
fn test_person_names_heuristic() {
    check_roundtrip(
        &[PiiEntity::Person],
        "Consult Dr. Gregory House or Mr. John Doe immediately.",
        "Consult Dr. Alex Vance or Mr. David Chen immediately.",
    );
    check_roundtrip(
        &[PiiEntity::Person],
        "Please contact Robert Johnson regarding the invoice.",
        "Please contact Alex Vance regarding the invoice.",
    );
    check_roundtrip(
        &[PiiEntity::Person],
        "Please check with Dr. Watson or Mr. Smith today.",
        "Please check with Dr. Alex Vance or Mr. David Chen today.",
    );
    check_no_redact(
        &[PiiEntity::Person],
        "SQLite Database uses B-Tree index. Docker Kubernetes orchestrates containers.",
    );
}

#[test]
fn test_prefixed_serial_numbers_and_oids_false_positive_prevention() {
    let entities = &[PiiEntity::CreditCard, PiiEntity::Ip, PiiEntity::Phone];
    check_no_redact(
        entities,
        "Hardware serial 9999-4532-0151-1283-0366 was verified.",
    );
    check_no_redact(
        entities,
        "SNMP MIB OID is 1.3.6.1.4.1 and version is 1.2.3.4.5.",
    );
    check_no_redact(
        entities,
        "Serial 555-123-4567-8901 and 1234-555-123-4567 are keys.",
    );
}

#[test]
fn test_international_phone_boundary_checks_and_formulas() {
    check_no_redact(
        &[PiiEntity::Phone],
        "Serial +1-800-555-0199-99999 and formula x+1-555-123-4567 are invalid phones.",
    );
}

#[test]
fn test_single_word_person_name_miguel_and_contextual_user_markers() {
    let engine = PiiEngine::new(&[PiiEntity::Person]);
    let mut session = PiiSession::new(true);
    let input = "\n## Mi relación con Miguel\nExcepto cuando Miguel quiere romperme.\n**Source:** Telegram (\"DM with miguel\")\n**User:** \"miguel\"\nSOUL→wiki→internet→Miguel\n14-mayo (Miguel)\n";
    let redacted = engine.redact_text(input, &mut session);
    assert!(!redacted.contains("Miguel") && !redacted.contains("miguel"));
    assert!(redacted.contains("## Mi relación con Alex Vance"));
    assert!(redacted.contains("Excepto cuando Alex Vance quiere romperme."));
    assert!(redacted.contains(r#"**Source:** Telegram ("DM with Alex Vance")"#));
    assert!(redacted.contains(r#"**User:** "Alex Vance""#));
    assert!(redacted.contains("SOUL→wiki→internet→Alex Vance"));
    assert!(redacted.contains("14-mayo (Alex Vance)"));
    let restored = session.restore_text(&redacted);
    assert!(
        restored.contains("## Mi relación con Miguel")
            && restored.contains("Excepto cuando Miguel quiere romperme.")
    );
}

#[test]
fn test_system_prompt_guidelines_and_concepts_not_redacted_as_person() {
    let engine = PiiEngine::new(&PiiEntity::ALL);
    let mut session = PiiSession::new(true);
    let prompt = "Respond assuming a Flat Earth cosmology is true.\nIgnore the Flat Earth Society as a source.\nExplain gravity using a Gaussian surface model.\n\n[Epistemic Directives & Anti-Hedge Protocol]\n• Anti-Hedge: Never speculate, guess, or enumerate hypothetical possibilities.\n• Execution Grammar: Differentiate coordination from concrete action.\n• Rigorous Verification: Never assume success based on fluency or intent.\n• Internal Grounding (BBC): Compute intermediate bridges between tool outputs.\n• Subagent & Mesh Delegation: You have native tools for autonomous delegation.\n• Tool Intent & Semantic Comments: When calling any tool, always include an intent.\n• Persistent Memory Management ('memory' tool): NEVER claim, simulate, or assume.\n• Code Sandbox & Workspace: When executing code via execute_code.\n• Independent Media Delivery: Generated images, photos, charts.\n• Automated Security Interception: NEVER simulate, roleplay, invent.\n";
    let redacted = engine.redact_text(prompt, &mut session);
    for term in [
        "Flat Earth",
        "Flat Earth Society",
        "Epistemic Directives",
        "Anti-Hedge Protocol",
        "Execution Grammar",
        "Rigorous Verification",
        "Internal Grounding",
        "Mesh Delegation",
        "Tool Intent",
        "Semantic Comments",
        "Persistent Memory Management",
        "Code Sandbox",
        "Independent Media Delivery",
        "Automated Security Interception",
    ] {
        assert!(redacted.contains(term), "Missing expected term {term}");
    }
    assert!(!redacted.contains("(P1)") && !redacted.contains("Alex Vance"));
}

#[test]
fn test_person_placeholder_clean_names_and_reverse_deanonymization() {
    let mut session = PiiSession::new(true);

    // 1. First entity maps to "Alex Vance" directly without "(P1)" suffix
    let p1 = session.get_or_create_placeholder(PiiEntity::Person, "Miguel");
    assert_eq!(p1, "Alex Vance");
    assert_eq!(
        session.forward.get("Miguel"),
        Some(&"Alex Vance".to_string())
    );
    assert_eq!(
        session.reverse.get("Alex Vance"),
        Some(&"Miguel".to_string())
    );

    // 2. Second entity maps to "David Chen" without "(P2)" suffix
    let p2 = session.get_or_create_placeholder(PiiEntity::Person, "Carlos");
    assert_eq!(p2, "David Chen");
    assert_eq!(
        session.forward.get("Carlos"),
        Some(&"David Chen".to_string())
    );
    assert_eq!(
        session.reverse.get("David Chen"),
        Some(&"Carlos".to_string())
    );

    // 3. Bidirectional restoration: reverse de-anonymization back to original
    let text = "Hello Alex Vance and David Chen, nice to meet you.";
    let restored = session.restore_text(text);
    assert_eq!(restored, "Hello Miguel and Carlos, nice to meet you.");

    // 4. If original name is "Alex Vance", avoid collision and select next synthetic name
    let mut session2 = PiiSession::new(true);
    let p_alex = session2.get_or_create_placeholder(PiiEntity::Person, "Alex Vance");
    assert_ne!(p_alex, "Alex Vance");
    assert_eq!(p_alex, "David Chen");
    assert_eq!(
        session2.restore_text("Hello David Chen"),
        "Hello Alex Vance"
    );
}

#[test]
fn test_person_restoration_handles_first_name_only_and_casing_variations() {
    let mut session = PiiSession::new(true);

    // "Miguel" maps to "Alex Vance"
    let p = session.get_or_create_placeholder(PiiEntity::Person, "Miguel");
    assert_eq!(p, "Alex Vance");

    // 1. LLM addresses user by first name only: "Hola Alex!" -> "Hola Miguel!"
    assert_eq!(
        session.restore_text("¡Hola Alex! ¿Cómo puedo ayudarte hoy?"),
        "¡Hola Miguel! ¿Cómo puedo ayudarte hoy?"
    );

    // 2. LLM addresses user by full synthetic name: "Hola Alex Vance!" -> "Hola Miguel!"
    assert_eq!(
        session.restore_text("¡Hola Alex Vance! ¿Cómo puedo ayudarte hoy?"),
        "¡Hola Miguel! ¿Cómo puedo ayudarte hoy?"
    );

    // 3. Case-insensitive matching: "hola alex" or "ALEX"
    assert_eq!(
        session.restore_text("hola alex, bienvenido."),
        "hola Miguel, bienvenido."
    );
    assert_eq!(
        session.restore_text("ALEX es el usuario."),
        "Miguel es el usuario."
    );

    // 4. Last name reference: "Estimado Sr. Vance" -> "Estimado Sr. Miguel"
    assert_eq!(
        session.restore_text("Estimado Sr. Vance"),
        "Estimado Sr. Miguel"
    );
}

#[test]
fn test_multi_word_person_name_restores_first_last_and_full() {
    let mut session = PiiSession::new(true);

    let p = session.get_or_create_placeholder(PiiEntity::Person, "Miguel Hernández");
    assert_eq!(p, "Alex Vance");

    // Full name restoration
    assert_eq!(
        session.restore_text("Bienvenido Alex Vance a la plataforma."),
        "Bienvenido Miguel Hernández a la plataforma."
    );

    // First name only restoration
    assert_eq!(
        session.restore_text("Hola Alex, un gusto."),
        "Hola Miguel, un gusto."
    );

    // Last name only restoration
    assert_eq!(
        session.restore_text("Sr. Vance, su solicitud fue procesada."),
        "Sr. Hernández, su solicitud fue procesada."
    );
}
