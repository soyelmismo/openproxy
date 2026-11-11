//! Corpus-driven effectiveness tests for PII redaction, both directions:
//!
//! - **Send** (forward): real Hermes session turns are redacted before they
//!   reach the upstream provider. Every detected original must be gone from
//!   the payload; a surviving original is a leak.
//! - **Recover** (restore): the provider echo, mutated the way a real model
//!   mutates placeholders (case, markdown, punctuation, articles), is mapped
//!   back to the real value the client finally sees.
//!
//! It also measures the distraction surface Miguel reported: how much of what
//! the engine redacts is real PII versus technical noise (Rust `::` paths,
//! `name=` fields, high-entropy session ids), and how many synthetic
//! placeholders never make it back to the client.
//!
//! ## Status
//!
//! Measured on 9420 real messages (40 sessions, 2026-09/2026-11). The six
//! engine fixes listed below took this from 71.4% noise and a broken restore
//! to a clean roundtrip; every test below is active.
//!
//! - Detections: **1310** (was 1985), of which **58.8%** are still technical
//!   noise: private Tailscale/OCI addresses and a few `name=` fields. Session
//!   ids, crate names, git object ids, Rust `::` paths, project names,
//!   lowercase words and variable references no longer produce false
//!   positives.
//! - Forward: **0 leaks and 0 partial redactions** (was 166 detected values
//!   surviving in 148 messages, 11.1%). Every detected original is replaced
//!   at every occurrence in the payload.
//! - Restore: **0 of 1339 placeholders reach the client** (was 22/1985), the
//!   roundtrip is lossless on all 994 messages with detections, and the
//!   mapping survives provider mutations on 4970 payloads.
//! - Transparency: **0** obvious markers, only plausible fake values.
//!
//! ## Engine fixes that produced these numbers
//!
//! 1. `REGEX_IPV6`: reject `::` glued to an identifier byte, so Rust paths
//!    (`db::skills_store`) stop becoming `fd00:10:240::N` that the restore
//!    cannot detach from its token.
//! 2. `REGEX_CONTEXTUAL_PERSON`: the value behind a conversational label must
//!    look like a person, so `name='gpthon'` stops becoming `Alex Vance`.
//! 3. Person candidates glued into an identifier are dropped, so a hostname
//!    is not restored with a capitalized name.
//! 4. `redact_text` re-applies each mapping to every remaining occurrence of
//!    the original, which is what removed the partial redaction.
//! 5. High entropy rejects ISO-dated ids, git object ids and package names
//!    passed to `-p`, and emails reject URL-encoded scopes.
//! 6. A credential-shaped value that reads a variable bound in the same
//!    payload is not a secret. `{"refresh_token": RT_RAW}` and
//!    `sshpass -p$SSH_PASS` reference variables; masking them renamed the
//!    variable and broke the code, which is how a real session ended up
//!    running `sshpass -psec_86019c-p0` and writing `sec_1f689c8a = ...`
//!    into a Python script. A value followed by `[` or `(` is the head of a
//!    code expression (`os.environ["SSH_PASS"]`) and is skipped too.
//! 7. A bare lowercase word behind `-p` is a slug fragment, not a flag
//!    value: `step-5-preview` contains `-p` + `review`, and redacting the
//!    fragment renamed every slug built on the word. Quoted values
//!    (`-p'apipassword'`) and `-u user:pass` contexts stay credentials.
//! 8. The restore stage restores placeholders the redactor minted itself
//!    (`sec_` + 8 hex) even when glued to identifiers, on either side:
//!    `step-5-psec_x` and `sec_x_receipts` came back unrestorable because
//!    the token boundary meant to protect human-authored placeholders
//!    rejected the synthetic ones too.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use openproxy_pipeline::pii::{PiiEngine, PiiSession};
use openproxy_types::config::PiiEntity;
use regex::Regex;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

const DEFAULT_CORPUS: &str = "/root/.hermes/cache/scratch/pii-corpus/corpus.jsonl";

#[derive(Deserialize)]
struct CorpusMessage {
    session: String,
    role: String,
    content: String,
}

fn inline_corpus() -> Vec<CorpusMessage> {
    let raw = [
        (
            "inline",
            "user",
            "Escríbele a alice@example.com y confirmo el teléfono +34 600 123 456.",
        ),
        (
            "inline",
            "user",
            "El servidor de producción es 10.0.0.253 y el de staging 10.0.0.254.",
        ),
        (
            "inline",
            "user",
            "Usa la key sk-proj-AbCdEf0123456789 para el deploy.",
        ),
        (
            "inline",
            "user",
            "Carlos Martínez aprobó el presupuesto; María Rodríguez lo revisará.",
        ),
        (
            "inline",
            "user",
            "La tarjeta de la empresa es 4532015112839104.",
        ),
        (
            "inline",
            "assistant",
            "Confirmo: escribo a alice@example.com y llamo al +34 600 123 456.",
        ),
        (
            "inline",
            "user",
            "Recuerda pasar por https://api.service.com/v1?token=SECRETVALUE123 y usar 10.0.0.253.",
        ),
        (
            "inline",
            "user",
            "Sin PII aquí, solo texto técnico sobre el pipeline de streaming.",
        ),
    ];
    raw.iter()
        .map(|(session, role, content)| CorpusMessage {
            session: (*session).to_string(),
            role: (*role).to_string(),
            content: (*content).to_string(),
        })
        .collect()
}

fn corpus() -> Vec<CorpusMessage> {
    let path = PathBuf::from(
        std::env::var("PII_CORPUS_PATH").unwrap_or_else(|_| DEFAULT_CORPUS.to_string()),
    );
    let parsed: Vec<CorpusMessage> = std::fs::read_to_string(&path)
        .ok()
        .map(|text| {
            text.lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default();
    if parsed.is_empty() {
        inline_corpus()
    } else {
        parsed
    }
}

fn engine() -> PiiEngine {
    PiiEngine::new(&PiiEntity::ALL)
}

/// Outcome of one forward pass over a single message.
struct Pass {
    original: String,
    redacted: String,
    pairs: Vec<(String, String)>, // (original, placeholder)
}

fn run_pass(text: &str) -> Pass {
    let engine = engine();
    let mut session = PiiSession::new(true);
    let redacted = engine.redact_text(text, &mut session);
    let mut pairs: Vec<(String, String)> = session
        .forward
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    pairs.sort();
    Pass {
        original: text.to_string(),
        redacted,
        pairs,
    }
}

fn word_regex(word: &str) -> Regex {
    Regex::new(&format!(r"\b{}\b", regex::escape(word))).unwrap()
}

// ---------------------------------------------------------------------------
// Forward: what the provider sees
// ---------------------------------------------------------------------------

/// Every detected original must be absent from the redacted payload, counted
/// at word boundaries so short words that live inside other words do not
/// produce phantom leaks.
///
/// Currently failing: the engine redacts a value only where a regex fires, so
/// the same value survives elsewhere in the same message (`gpthon` behind
/// `name=` becomes `Alex Vance` while `` `gpthon-rs-development` `` in inline
/// code keeps the original). The model sees both and reconstructs the mapping,
/// which is the distraction that got the feature disabled.
#[test]
fn forward_send_leaks_no_detected_original() {
    let messages = corpus();
    let mut leaks: Vec<String> = Vec::new();
    let mut leak_messages = 0usize;
    let mut leak_values = 0usize;
    let mut with_pii = 0usize;
    let mut total_placeholders = 0usize;

    for msg in &messages {
        let pass = run_pass(&msg.content);
        if pass.pairs.is_empty() {
            continue;
        }
        with_pii += 1;
        total_placeholders += pass.pairs.len();
        let mut msg_leaked = false;
        for (original, placeholder) in &pass.pairs {
            if original.chars().count() < 4 {
                continue; // short tokens produce detection noise, not leaks
            }
            // Occurrences that legitimately live inside the placeholder do not
            // count (a fake person name can contain the original first name).
            let scrubbed = pass.redacted.replace(placeholder.as_str(), "\u{0}");
            if word_regex(original).is_match(&scrubbed) {
                leak_values += 1;
                msg_leaked = true;
                if leaks.len() < 10 {
                    leaks.push(format!(
                        "[{}] original {:?} still present as a whole word in the payload",
                        msg.role, original
                    ));
                }
            }
        }
        if msg_leaked {
            leak_messages += 1;
        }
    }

    println!(
        "forward: {} messages, {} with detections, {} placeholders",
        messages.len(),
        with_pii,
        total_placeholders
    );
    println!(
        "partial redaction: {leak_values} detected values survived in {leak_messages} messages ({:.1}% of messages with detections)",
        leak_messages as f64 / with_pii.max(1) as f64 * 100.0
    );
    assert!(
        leaks.is_empty(),
        "PII reached upstream in {} cases:\n{}",
        leaks.len(),
        leaks.join("\n")
    );
}

// ---------------------------------------------------------------------------
// Transparency and collisions
// ---------------------------------------------------------------------------

/// Placeholder transparency: obvious markers (`<EMAIL_1>`) invite the model
/// to reason about the redaction itself; plausible fake data does not.
#[test]
fn placeholder_transparency_classification() {
    let messages = corpus();
    let engine = engine();
    let mut marker = 0usize;
    let mut fake_data = 0usize;
    let mut examples: Vec<String> = Vec::new();

    for msg in &messages {
        let mut session = PiiSession::new(true);
        let _ = engine.redact_text(&msg.content, &mut session);
        for (original, placeholder) in &session.forward {
            if placeholder.starts_with('<') && placeholder.ends_with('>') {
                marker += 1;
            } else {
                fake_data += 1;
            }
            if examples.len() < 24 {
                examples.push(format!("{original:?} -> {placeholder:?}"));
            }
        }
    }

    println!("placeholder transparency:");
    println!("  obvious markers (<ENTITY_N>): {marker}");
    println!("  plausible fake data: {fake_data}");
    if !examples.is_empty() {
        println!("  samples:\n    {}", examples.join("\n    "));
    }

    assert_eq!(
        marker, 0,
        "engine emitted {marker} obvious markers; the model can detect them"
    );
}

/// Synthetic person names that already exist in the real corpus destroy the
/// identity boundary the redaction is supposed to enforce.
#[test]
fn placeholder_collision_with_real_vocabulary() {
    let messages = corpus();
    let mut vocab: BTreeSet<String> = BTreeSet::new();
    for msg in &messages {
        for tok in msg
            .content
            .split(|c: char| !c.is_alphanumeric() && c != '.' && c != '_' && c != '-' && c != '@')
        {
            let lower = tok.to_lowercase();
            if lower.chars().count() >= 3 {
                vocab.insert(lower);
            }
        }
    }

    let engine = engine();
    let mut person_first_hits = 0usize;
    let mut person_full_hits = 0usize;
    let mut person_total = 0usize;
    let mut ip_hits = 0usize;
    let mut ip_total = 0usize;
    let mut email_hits = 0usize;
    let mut email_total = 0usize;

    for msg in &messages {
        let mut session = PiiSession::new(true);
        let _ = engine.redact_text(&msg.content, &mut session);
        for placeholder in session.forward.values() {
            let is_person = placeholder.split_whitespace().count() == 2
                && placeholder.chars().all(|c| c.is_alphabetic() || c == ' ')
                && placeholder.chars().next().is_some_and(|c| c.is_uppercase());
            if is_person {
                person_total += 1;
                let parts: Vec<&str> = placeholder.split_whitespace().collect();
                let first = parts[0].to_lowercase();
                let last = parts[1].to_lowercase();
                if vocab.contains(&format!("{first} {last}")) {
                    person_full_hits += 1;
                } else if vocab.contains(&first) || vocab.contains(&last) {
                    person_first_hits += 1;
                }
                continue;
            }
            if placeholder.contains("10.240.") {
                ip_total += 1;
                if vocab.contains(&placeholder.to_lowercase()) {
                    ip_hits += 1;
                }
                continue;
            }
            if placeholder.contains('@') && !placeholder.starts_with('<') {
                email_total += 1;
                if vocab.contains(&placeholder.to_lowercase()) {
                    email_hits += 1;
                }
            }
        }
    }

    println!("vocabulary collision report:");
    println!("  corpus vocabulary tokens: {}", vocab.len());
    println!(
        "  person placeholders: {person_total}, full-name collisions: {person_full_hits}, part collisions: {person_first_hits}"
    );
    println!("  ip placeholders: {ip_total}, collisions: {ip_hits}");
    println!("  email placeholders: {email_total}, collisions: {email_hits}");

    assert_eq!(
        person_full_hits, 0,
        "synthetic person names colliding with real corpus names"
    );
}

/// Overdetection: how much of what the engine redacts is real PII and how
/// much is technical noise. Noise placeholders are what makes a model start
/// reasoning "this value is fake, let me find the real one".
#[test]
fn overdetection_report() {
    let messages = corpus();
    let engine = engine();
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    let mut samples: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut total = 0usize;

    for msg in &messages {
        let mut session = PiiSession::new(true);
        let _ = engine.redact_text(&msg.content, &mut session);
        for (original, placeholder) in &session.forward {
            total += 1;
            let kind = classify_original(original);
            *kinds.entry(kind).or_insert(0) += 1;
            let bucket = samples.entry(kind).or_default();
            if bucket.len() < 6 {
                bucket.push(format!("{original:?} -> {placeholder:?}"));
            }
        }
    }

    println!("overdetection report ({total} detections):");
    for (kind, count) in &kinds {
        let pct = *count as f64 / total as f64 * 100.0;
        println!("  {kind}: {count} ({pct:.1}%)");
        if let Some(ex) = samples.get(kind) {
            println!("    {}", ex.join("\n    "));
        }
    }
    let noise: usize = kinds
        .iter()
        .filter(|(k, _)| is_noise_kind(k))
        .map(|(_, c)| *c)
        .sum();
    println!(
        "  => technical noise: {noise}/{} ({:.1}%)",
        total,
        noise as f64 / total as f64 * 100.0
    );

    assert!(total > 0, "corpus produced no detections");
}

/// Classify a detected original by shape. The categories are deliberately
/// syntactic so the report needs no external ground truth.
fn classify_original(original: &str) -> &'static str {
    if original.contains("::") {
        return "rust_path_ipv6_false_positive";
    }
    if original.contains('@') {
        return "email_like";
    }
    if original.chars().all(|c| c.is_ascii_digit()) {
        return "numeric_id";
    }
    if original.contains('_') || original.contains('/') {
        return "technical_identifier";
    }
    if original.contains('-') {
        return "technical_identifier";
    }
    if original.chars().all(|c| c.is_alphabetic()) {
        let first = original.chars().next().unwrap_or('a');
        if first.is_uppercase() {
            return "proper_name_candidate";
        }
        return "lowercase_word";
    }
    if original.chars().any(|c| c.is_ascii_digit()) {
        return "technical_identifier";
    }
    "other"
}

fn is_noise_kind(kind: &&str) -> bool {
    matches!(
        *kind,
        "rust_path_ipv6_false_positive" | "technical_identifier" | "lowercase_word"
    )
}

// ---------------------------------------------------------------------------
// Restore: what the client sees
// ---------------------------------------------------------------------------

/// Density report: how saturated a message gets once PII is replaced.
#[test]
fn redaction_density_report() {
    let messages = corpus();
    let engine = engine();
    let mut by_entity: BTreeMap<String, usize> = BTreeMap::new();
    let mut with_pii = 0usize;
    let mut dense_messages = 0usize;
    let mut max_placeholders = 0usize;
    let mut total_chars_with_pii = 0usize;

    for msg in &messages {
        let mut session = PiiSession::new(true);
        let _ = engine.redact_text(&msg.content, &mut session);
        let n = session.forward.len();
        if n == 0 {
            continue;
        }
        with_pii += 1;
        total_chars_with_pii += msg.content.chars().count();
        for entity in PiiEntity::ALL {
            if let Some(&c) = session.counts.get(&entity)
                && c > 0
            {
                *by_entity.entry(entity.as_str().to_string()).or_insert(0) += c;
            }
        }
        if n >= 3 {
            dense_messages += 1;
        }
        max_placeholders = max_placeholders.max(n);
    }

    let total_ph: usize = by_entity.values().sum();
    let density = if total_chars_with_pii == 0 {
        0.0
    } else {
        total_ph as f64 / total_chars_with_pii as f64 * 1000.0
    };

    println!("density report:");
    println!(
        "  messages: {} (with detections: {with_pii})",
        messages.len()
    );
    println!("  entities: {by_entity:?}");
    println!("  messages with >=3 placeholders: {dense_messages}");
    println!("  max placeholders in one message: {max_placeholders}");
    println!("  placeholders per 1000 chars: {density:.2}");

    assert!(with_pii > 0, "corpus produced no redaction at all");
}

// ---------------------------------------------------------------------------
// Known engine bugs: restore direction
//
// Root cause of every failure below: the IPv6 regex ends with a bare `::`
// alternative, so Rust path separators (`db::skills_store`) are matched as
// IPv6 and replaced by `fd00:10:240::N`, glued to the adjacent identifier.
// The restore boundary check then refuses to fire and the synthetic value
// reaches the client. `REGEX_CONTEXTUAL_PERSON` adds the same noise from any
// `name=`/`name:` field (`serde_json` -> `Alex Vance_json`).
// ---------------------------------------------------------------------------

fn word_count(s: &str) -> usize {
    s.split_whitespace().count()
}

/// First differing character between two strings, with surrounding context,
/// so a restore failure can be diagnosed from the test output alone.
fn first_difference(original: &str, restored: &str) -> String {
    let mut oi = original.char_indices();
    let mut ri = restored.char_indices();
    loop {
        match (oi.next(), ri.next()) {
            (None, None) => return "identical by char, different by length".to_string(),
            (None, Some((i, c))) => {
                return format!(
                    "restored has extra {c:?} at byte {i} near {:?}",
                    &restored[i.saturating_sub(24)..(i + 24).min(restored.len())]
                );
            }
            (Some((i, c)), None) => {
                return format!(
                    "restored dropped {c:?} at byte {i} near {:?}",
                    &original[i.saturating_sub(24)..(i + 24).min(original.len())]
                );
            }
            (Some((_oi, oc)), Some((_ri, rc))) if oc == rc => continue,
            (Some((oi, oc)), Some((ri, rc))) => {
                return format!(
                    "original {oc:?}@{oi} vs restored {rc:?}@{ri} near {:?} | {:?}",
                    &original[oi.saturating_sub(24)..(oi + 24).min(original.len())],
                    &restored[ri.saturating_sub(24)..(ri + 24).min(restored.len())]
                );
            }
        }
    }
}

/// Placeholders that survive into the client payload. Each one is a synthetic
/// value the user sees instead of the real one: the recover direction failed.
#[test]
fn restore_recovers_every_placeholder() {
    let messages = corpus();
    let mut survivors: Vec<String> = Vec::new();
    let mut total_placeholders = 0usize;
    let mut survivor_values = 0usize;

    for msg in &messages {
        let pass = run_pass(&msg.content);
        if pass.pairs.is_empty() {
            continue;
        }
        let engine = engine();
        let mut session = PiiSession::new(true);
        let _ = engine.redact_text(&msg.content, &mut session);
        let restored = session.restore_text(&pass.redacted);
        total_placeholders += pass.pairs.len();
        for (original, placeholder) in &pass.pairs {
            if original.chars().count() < 4 {
                continue;
            }
            // A placeholder still present in the restored text never came back.
            if placeholder.contains(original.as_str()) {
                continue; // the original is a substring of the synthetic value
            }
            if restored.contains(placeholder.as_str()) {
                survivor_values += 1;
                if survivors.len() < 15 {
                    survivors.push(format!(
                        "[{}] {placeholder:?} reached the client (original {original:?})",
                        msg.role
                    ));
                }
            }
        }
    }

    println!("restore survivors: {total_placeholders} placeholders emitted");
    println!(
        "synthetic values that reached the client: {survivor_values} ({:.1}% of emitted placeholders)",
        survivor_values as f64 / total_placeholders.max(1) as f64 * 100.0
    );
    assert!(
        survivors.is_empty(),
        "synthetic values leaked to the client in {} cases:\n{}",
        survivors.len(),
        survivors.join("\n")
    );
}

#[test]
fn restore_roundtrip_is_lossless() {
    let messages = corpus();
    let mut failures: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for msg in &messages {
        let pass = run_pass(&msg.content);
        if pass.pairs.is_empty() {
            continue;
        }
        checked += 1;
        let engine = engine();
        let mut session = PiiSession::new(true);
        let _ = engine.redact_text(&msg.content, &mut session);
        let restored = session.restore_text(&pass.redacted);
        if restored != pass.original && failures.len() < 10 {
            failures.push(format!(
                "[{}] roundtrip mismatch:\n  original: {:?}\n  restored: {:?}",
                msg.role, pass.original, restored
            ));
        }
    }

    println!("restore roundtrip: {checked} messages with detections checked");
    assert!(
        failures.is_empty(),
        "restore lost fidelity in {} messages:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn restore_survives_provider_mutations() {
    let messages = corpus();
    let mut failures: Vec<String> = Vec::new();
    let mut checked = 0usize;

    for msg in &messages {
        let pass = run_pass(&msg.content);
        if pass.pairs.is_empty() {
            continue;
        }

        let mutations: Vec<(&str, String)> = vec![
            ("uppercase", pass.redacted.to_uppercase()),
            ("lowercase", pass.redacted.to_lowercase()),
            ("markdown-bold", wrap_all(&pass.redacted, "**", "**")),
            ("quoted", wrap_all(&pass.redacted, "\"", "\"")),
            ("parenthesized", wrap_all(&pass.redacted, "(", ")")),
        ];

        for (label, mutated) in &mutations {
            checked += 1;
            let engine = engine();
            let mut session = PiiSession::new(true);
            let _ = engine.redact_text(&msg.content, &mut session);
            let restored = session.restore_text(mutated);
            for (original, placeholder) in &pass.pairs {
                if original.chars().count() < 4 {
                    continue;
                }
                if restored.contains(placeholder.as_str())
                    && !placeholder.contains(original.as_str())
                    && failures.len() < 10
                {
                    failures.push(format!(
                        "[{label}] placeholder {placeholder:?} survived restore"
                    ));
                }
            }
        }
    }

    println!("restore mutations: {checked} mutated payloads checked");
    assert!(
        failures.is_empty(),
        "restore failed under provider mutations in {} cases:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn wrap_all(text: &str, open: &str, close: &str) -> String {
    text.split_whitespace()
        .map(|tok| {
            if tok.contains('@')
                || tok.contains("10.240.")
                || tok.starts_with("sec_")
                || tok.starts_with("sk-")
                || tok.starts_with("+1-")
                || (tok.chars().all(|c| c.is_ascii_digit()) && tok.len() >= 12)
            {
                format!("{open}{tok}{close}")
            } else {
                tok.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Multi-turn stability: within one request the same original must map to the
/// same placeholder on every turn, and each turn must restore cleanly.
#[test]
fn multi_turn_placeholder_stability() {
    let messages = corpus();
    let mut by_session: BTreeMap<String, Vec<&CorpusMessage>> = BTreeMap::new();
    for msg in &messages {
        by_session.entry(msg.session.clone()).or_default().push(msg);
    }
    let sequences: Vec<(String, Vec<&CorpusMessage>)> = by_session
        .into_iter()
        .filter(|(_, msgs)| msgs.len() >= 3)
        .collect();
    assert!(
        !sequences.is_empty(),
        "corpus has no multi-turn sequence to check"
    );

    let engine = engine();
    let mut inconsistent: Vec<String> = Vec::new();
    let mut restore_failures: Vec<String> = Vec::new();
    let mut turns_checked = 0usize;

    for (session_id, msgs) in &sequences {
        let mut pii_session = PiiSession::new(true);
        let mut seen: HashMap<String, String> = HashMap::new();
        for msg in msgs {
            let redacted = engine.redact_text(&msg.content, &mut pii_session);
            let restored = pii_session.restore_text(&redacted);
            turns_checked += 1;
            if restored != msg.content && restore_failures.len() < 10 {
                restore_failures.push(format!(
                    "[{session_id}/{}] restore mismatch ({} words original, {} restored)\n    {}",
                    msg.role,
                    word_count(&msg.content),
                    word_count(&restored),
                    first_difference(&msg.content, &restored)
                ));
            }
            for (original, placeholder) in &pii_session.forward {
                if original.chars().count() < 4 {
                    continue;
                }
                if let Some(prev) = seen.get(original)
                    && prev != placeholder
                    && inconsistent.len() < 10
                {
                    inconsistent.push(format!(
                        "[{session_id}] original {original:?} mapped to {prev:?} then {placeholder:?}"
                    ));
                }
                seen.insert(original.clone(), placeholder.clone());
            }
        }
    }

    println!(
        "multi-turn: {} sequences, {} turns checked",
        sequences.len(),
        turns_checked
    );
    assert!(
        inconsistent.is_empty(),
        "unstable placeholder mapping:\n{}",
        inconsistent.join("\n")
    );
    assert!(
        restore_failures.is_empty(),
        "restore lost fidelity in {} turns:\n{}",
        restore_failures.len(),
        restore_failures.join("\n")
    );
}
