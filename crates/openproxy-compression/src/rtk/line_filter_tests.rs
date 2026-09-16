use super::*;

#[test]
fn test_git_status_strips_advice_lines() {
    let filter = get_builtin_filter("git-status").unwrap();
    let input = "On branch main\n  (use \"git add\" to update)\n\tmodified: foo.rs\n";
    let (result, rules) = apply_line_filter(input, &filter);
    assert!(!rules.is_empty());
    assert!(result.contains("On branch main"));
    assert!(result.contains("modified: foo.rs"));
    assert!(!result.contains("use \"git add\""));
}

#[test]
fn test_cargo_test_match_output_short_circuits() {
    let filter = get_builtin_filter("cargo-test").unwrap();
    let input = "running 5 tests\ntest result: ok. 5 passed\n";
    let (result, rules) = apply_line_filter(input, &filter);
    assert!(rules.contains(&"cargo-test::match_output"));
    assert_eq!(result, "✓ all tests passed");
}

#[test]
fn test_strip_ansi_cases() {
    const CASES: &[(&str, &str)] = &[
        ("\u{1b}[32mgreen\u{1b}[0m", "green"),
        ("\x1b[31mred text\x1b[0m", "red text"),
        ("\x1b[1;32mbold green\x1b[0m", "bold green"),
        ("just plain text", "just plain text"),
        ("", ""),
        ("text\x1b[", "text"),
        ("text\x1b", "text"),
        ("text\x1b[31", "text"),
        ("\x1b[32mhello 世界\x1b[0m 😀", "hello 世界 😀"),
        ("line1\x1b[Aline2", "line1line2"),
        ("\x1b[31m\x1b[1mbold red\x1b[0m\x1b[0m", "bold red"),
    ];
    for (input, expected) in CASES {
        assert_eq!(strip_ansi(input), *expected, "failed for input: {input:?}");
    }
}

#[test]
fn builtin_filters_registry_contains_all_eight_ids() {
    let ids = [
        "git-status",
        "git-diff",
        "cargo-test",
        "npm-test",
        "docker-ps",
        "error-stacktrace",
        "shell-ls",
        "generic-error",
    ];
    for id in ids {
        assert!(BUILTIN_FILTERS.contains_key(id), "missing builtin: {id}");
    }
}

#[test]
fn builtin_filters_share_one_instance_via_arc() {
    let a = get_builtin_filter("git-status").unwrap();
    let b = get_builtin_filter("git-status").unwrap();
    assert!(Arc::ptr_eq(&a, &b));
}

#[test]
fn generic_filter_is_static_singleton() {
    let a = get_generic_filter();
    let b = get_generic_filter();
    assert!(Arc::ptr_eq(&a, &b));
}

#[test]
fn unknown_filter_id_returns_none() {
    assert!(get_builtin_filter("nonexistent-id").is_none());
}

#[test]
fn filter_stderr_prefixes_strips_stderr_markers() {
    let input = "stderr| something\nerr: else\nplain line";
    let out = filter_stderr_prefixes(input);
    assert_eq!(out, "something\nelse\nplain line");
}

#[test]
fn test_truncate_unicode_safe() {
    assert_eq!(truncate_unicode_safe("hello world", 0), "hello world");
    assert_eq!(truncate_unicode_safe("hello", 5), "hello");
    assert_eq!(truncate_unicode_safe("hello", 10), "hello");
    assert_eq!(truncate_unicode_safe("hello", 2), "he");
    assert_eq!(truncate_unicode_safe("hello world", 5), "he...");
    assert_eq!(truncate_unicode_safe("🦀🦀🦀🦀", 4), "🦀🦀🦀🦀");
    assert_eq!(truncate_unicode_safe("🦀🦀🦀🦀🦀", 4), "🦀...");
    assert_eq!(truncate_unicode_safe("🦀🦀🦀🦀", 2), "🦀🦀");

    let s = "short";
    assert!(matches!(truncate_unicode_safe(s, 10), Cow::Borrowed(_)));
    let long = "longer string here";
    assert!(matches!(truncate_unicode_safe(long, 6), Cow::Owned(_)));
}

#[test]
fn test_rtk_rules_unified() {
    assert_eq!(RTK_RULES.len(), 15);
    for rule in RTK_RULES {
        assert!(!rule.id.is_empty());
        if let Some(builder) = rule.filter_builder {
            let compiled = builder();
            assert_eq!(compiled.id, rule.id);
        }
    }
}
