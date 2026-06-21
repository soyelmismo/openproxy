use serde::Deserialize;

use super::smart_truncate::{smart_truncate, TruncateConfig};

/// Filtro RTK declarativo para un tipo de comando.
#[derive(Debug, Clone, Deserialize)]
pub struct RtkFilter {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    /// IDs de comando que activan este filtro (ej: "git-status")
    #[serde(default)]
    pub command_types: Vec<String>,
    /// Patrones regex para identificar el output en el texto
    #[serde(default)]
    pub match_patterns: Vec<String>,
    /// Eliminar códigos ANSI
    #[serde(default)]
    pub strip_ansi: bool,
    /// Prefijos stderr a limpiar
    #[serde(default)]
    pub filter_stderr: bool,
    /// Patrones de reemplazo regex
    #[serde(default)]
    pub replace: Vec<ReplaceRule>,
    /// Short-circuit: si el output matchea, reemplazar con mensaje corto
    #[serde(default)]
    pub match_output: Vec<MatchOutputRule>,
    /// Líneas que coinciden con estos regex se eliminan
    #[serde(default)]
    pub strip_patterns: Vec<String>,
    /// Solo conserva líneas que coinciden con estos regex
    #[serde(default)]
    pub keep_patterns: Vec<String>,
    /// Líneas que coinciden con estos regex se colapsan (non-consecutive dedup)
    #[serde(default)]
    pub collapse_patterns: Vec<String>,
    /// Máximo de caracteres por línea (truncación unicode-safe)
    #[serde(default)]
    pub truncate_line_at: usize,
    /// Texto a mostrar si el output queda vacío
    #[serde(default)]
    pub on_empty: String,
    /// Configuración de truncación
    #[serde(default)]
    pub truncate: Option<TruncateConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReplaceRule {
    pub pattern: String,
    pub replacement: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MatchOutputRule {
    pub pattern: String,
    pub message: String,
    #[serde(default)]
    pub unless: Option<String>,
}

/// Aplica el pipeline de filtrado de un RtkFilter al texto.
pub fn apply_line_filter(text: &str, filter: &RtkFilter) -> (String, Vec<String>) {
    let mut applied_rules: Vec<String> = Vec::new();
    let mut result = text.to_string();

    // 1. Strip ANSI codes
    if filter.strip_ansi {
        let stripped = strip_ansi(&result);
        if stripped != result {
            applied_rules.push(format!("{}::strip_ansi", filter.id));
            result = stripped;
        }
    }

    // 2. Filter stderr prefixes
    if filter.filter_stderr {
        let filtered = filter_stderr_prefixes(&result);
        if filtered != result {
            applied_rules.push(format!("{}::filter_stderr", filter.id));
            result = filtered;
        }
    }

    // 3. Regex replacements
    for rule in &filter.replace {
        if let Ok(re) = regex::Regex::new(&rule.pattern) {
            let replaced = re.replace_all(&result, rule.replacement.as_str()).to_string();
            if replaced != result {
                applied_rules.push(format!("{}::replace", filter.id));
                result = replaced;
            }
        }
    }

    // 4. Match output (short-circuit)
    for rule in &filter.match_output {
        if let Ok(re) = regex::Regex::new(&rule.pattern) {
            if re.is_match(&result) {
                let should_skip = rule
                    .unless
                    .as_ref()
                    .and_then(|p| regex::Regex::new(p).ok())
                    .map(|u| u.is_match(&result))
                    .unwrap_or(false);
                if !should_skip {
                    applied_rules.push(format!("{}::match_output", filter.id));
                    return (rule.message.clone(), applied_rules);
                }
            }
        }
    }

    // 5. Strip patterns (drop matching lines)
    if !filter.strip_patterns.is_empty() {
        let strip_re: Vec<regex::Regex> = filter
            .strip_patterns
            .iter()
            .filter_map(|p| regex::Regex::new(p).ok())
            .collect();
        let stripped: Vec<&str> = result
            .lines()
            .filter(|l| !strip_re.iter().any(|r| r.is_match(l)))
            .collect();
        if stripped.len() != result.lines().count() {
            applied_rules.push(format!("{}::strip", filter.id));
            result = stripped.join("\n");
        }
    }

    // 6. Keep patterns (only keep matching lines)
    if !filter.keep_patterns.is_empty() {
        let keep_re: Vec<regex::Regex> = filter
            .keep_patterns
            .iter()
            .filter_map(|p| regex::Regex::new(p).ok())
            .collect();
        let kept: Vec<&str> = result
            .lines()
            .filter(|l| keep_re.iter().any(|r| r.is_match(l)))
            .collect();
        if !kept.is_empty() {
            applied_rules.push(format!("{}::keep", filter.id));
            result = kept.join("\n");
        }
    }

    // 7. Collapse patterns (non-consecutive dedup)
    if !filter.collapse_patterns.is_empty() {
        let collapse_re: Vec<regex::Regex> = filter
            .collapse_patterns
            .iter()
            .filter_map(|p| regex::Regex::new(p).ok())
            .collect();
        let mut seen = std::collections::HashSet::new();
        let collapsed: Vec<&str> = result
            .lines()
            .filter(|l| {
                if collapse_re.iter().any(|r| r.is_match(l)) {
                    let key = l.trim();
                    if seen.contains(key) {
                        return false;
                    }
                    seen.insert(key);
                }
                true
            })
            .collect();
        if collapsed.len() != result.lines().count() {
            applied_rules.push(format!("{}::collapse", filter.id));
            result = collapsed.join("\n");
        }
    }

    // 8. Truncate lines
    if filter.truncate_line_at > 0 {
        let truncated: Vec<String> = result
            .lines()
            .map(|l| truncate_unicode_safe(l, filter.truncate_line_at))
            .collect();
        let new = truncated.join("\n");
        if new != result {
            applied_rules.push(format!("{}::truncate_line", filter.id));
            result = new;
        }
    }

    // 9. Smart truncate
    if let Some(ref tc) = filter.truncate {
        let (truncated, did_truncate, _dropped) = smart_truncate(&result, tc);
        if did_truncate {
            applied_rules.push(format!("{}::truncate", filter.id));
            result = truncated;
        }
    }

    // 10. On empty fallback
    if result.trim().is_empty() && !filter.on_empty.is_empty() {
        applied_rules.push(format!("{}::on_empty", filter.id));
        result = filter.on_empty.clone();
    }

    (result, applied_rules)
}

fn strip_ansi(text: &str) -> String {
    let re = regex::Regex::new(r"\u{1b}\[[0-?]*[ -/]*[@-~]").unwrap();
    re.replace_all(text, "").to_string()
}

fn filter_stderr_prefixes(text: &str) -> String {
    let re = regex::Regex::new(r"(?m)^\s*(?:stderr|err)\s*(?:\||:)\s*").unwrap();
    re.replace_all(text, "").to_string()
}

fn truncate_unicode_safe(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        return s.to_string();
    }
    if max_chars <= 3 {
        return chars[..max_chars].iter().collect();
    }
    let prefix: String = chars[..max_chars - 3].iter().collect();
    format!("{}...", prefix)
}

/// Obtiene el filtro built-in para un tipo de comando detectado.
pub fn get_builtin_filter(detected_id: &str) -> Option<RtkFilter> {
    match detected_id {
        "git-status" => Some(RtkFilter {
            id: "git-status".into(),
            label: "Git Status".into(),
            description: "Keep branch, staged/unstaged, and changed file lines.".into(),
            command_types: vec!["git-status".into()],
            match_patterns: vec![],
            strip_ansi: true,
            filter_stderr: false,
            replace: vec![],
            match_output: vec![],
            strip_patterns: vec![r"^\s*(\(use .*\))$".to_string(), r"^\s*$".to_string()],
            keep_patterns: vec![
                r"^On branch ".to_string(),
                r"^Your branch ".to_string(),
                r"^Changes ".to_string(),
                r"^Untracked files:".to_string(),
                r"^\s*(modified|new file|deleted|renamed):".to_string(),
                r"^\s*[MADRCU?!]{1,2}\s+".to_string(),
                r"nothing (added|committed)".to_string(),
            ],
            collapse_patterns: vec![],
            truncate_line_at: 0,
            on_empty: String::new(),
            truncate: Some(TruncateConfig {
                max_lines: 60,
                head_lines: 15,
                tail_lines: 15,
                priority_patterns: vec![r"(?i)(modified|deleted|Untracked)".to_string()],
            }),
        }),
        "git-diff" => Some(RtkFilter {
            id: "git-diff".into(),
            label: "Git Diff".into(),
            description: "Condensed diff output.".into(),
            command_types: vec!["git-diff".into()],
            match_patterns: vec![],
            strip_ansi: true,
            filter_stderr: false,
            replace: vec![],
            match_output: vec![],
            strip_patterns: vec![r"^\s*$".to_string()],
            keep_patterns: vec![
                r"^diff --git ".to_string(),
                r"^index ".to_string(),
                r"^--- ".to_string(),
                r"^\+\+\+ ".to_string(),
                r"^@@ ".to_string(),
                r"^[+-]".to_string(),
            ],
            collapse_patterns: vec![],
            truncate_line_at: 0,
            on_empty: String::new(),
            truncate: Some(TruncateConfig {
                max_lines: 100,
                head_lines: 25,
                tail_lines: 25,
                priority_patterns: vec![r"^@@ ".to_string()],
            }),
        }),
        "cargo-test" => Some(RtkFilter {
            id: "cargo-test".into(),
            label: "Cargo Test".into(),
            description: "Compact test output — failures only.".into(),
            command_types: vec!["cargo-test".into()],
            match_patterns: vec![],
            strip_ansi: true,
            filter_stderr: false,
            replace: vec![],
            match_output: vec![
                MatchOutputRule {
                    pattern: r"test result:.*ok\b".to_string(),
                    message: "✓ all tests passed".to_string(),
                    unless: Some(r"FAILED".to_string()),
                },
            ],
            strip_patterns: vec![
                r"^\s*$".to_string(),
                r"^\s*(Compiling|Finished|warning:)".to_string(),
                r"^\s*(running \d+ tests?)".to_string(),
            ],
            keep_patterns: vec![
                r"^test .* FAILED".to_string(),
                r"^test result:".to_string(),
                r"^failures:".to_string(),
                r"^\s+-->".to_string(),
                r"^error\[".to_string(),
            ],
            collapse_patterns: vec![],
            truncate_line_at: 0,
            on_empty: "✓ all tests passed".to_string(),
            truncate: Some(TruncateConfig {
                max_lines: 60,
                head_lines: 5,
                tail_lines: 10,
                priority_patterns: vec![r"(?i)(FAILED|error|panic)".to_string()],
            }),
        }),
        "npm-test" => Some(RtkFilter {
            id: "npm-test".into(),
            label: "NPM / Vitest / Jest".into(),
            description: "Compact test runner output.".into(),
            command_types: vec!["npm-test".into()],
            match_patterns: vec![],
            strip_ansi: true,
            filter_stderr: false,
            replace: vec![],
            match_output: vec![
                MatchOutputRule {
                    pattern: r"Tests:\s+\d+\s+passed".to_string(),
                    message: "✓ tests passed".to_string(),
                    unless: Some(r"failed".to_string()),
                },
            ],
            strip_patterns: vec![
                r"^\s*$".to_string(),
                r"^\s*(PASS|FAIL)\s+".to_string(),
            ],
            keep_patterns: vec![
                r"FAIL\s+".to_string(),
                r"✖\s+".to_string(),
                r"×\s+".to_string(),
                r"❯\s+".to_string(),
                r"✓\s+".to_string(),
            ],
            collapse_patterns: vec![],
            truncate_line_at: 0,
            on_empty: "✓ tests passed".to_string(),
            truncate: Some(TruncateConfig {
                max_lines: 60,
                head_lines: 5,
                tail_lines: 10,
                priority_patterns: vec![r"(?i)(FAIL|error|✖)".to_string()],
            }),
        }),
        "docker-ps" => Some(RtkFilter {
            id: "docker-ps".into(),
            label: "Docker PS".into(),
            description: "Compact container list.".into(),
            command_types: vec!["docker-ps".into()],
            match_patterns: vec![],
            strip_ansi: true,
            filter_stderr: false,
            replace: vec![],
            match_output: vec![],
            strip_patterns: vec![],
            keep_patterns: vec![
                r"^CONTAINER ID".to_string(),
                r"^[0-9a-f]{12}".to_string(),
            ],
            collapse_patterns: vec![],
            truncate_line_at: 0,
            on_empty: "(no containers)".to_string(),
            truncate: Some(TruncateConfig {
                max_lines: 50,
                head_lines: 10,
                tail_lines: 5,
                priority_patterns: vec![],
            }),
        }),
        "error-stacktrace" => Some(RtkFilter {
            id: "error-stacktrace".into(),
            label: "Error Stacktrace".into(),
            description: "Keep error context, collapse frames.".into(),
            command_types: vec!["error-stacktrace".into()],
            match_patterns: vec![],
            strip_ansi: true,
            filter_stderr: false,
            replace: vec![],
            match_output: vec![],
            strip_patterns: vec![],
            keep_patterns: vec![
                r"^(thread|panicked|Error|error)".to_string(),
                r"^\s+at ".to_string(),
                r"^\s+\d+:" .to_string(),
                r"^\s+\[".to_string(),
                r"^Caused by:".to_string(),
                r"^  .*: ".to_string(),
            ],
            collapse_patterns: vec![r"^\s+at ".to_string()],
            truncate_line_at: 200,
            on_empty: String::new(),
            truncate: Some(TruncateConfig {
                max_lines: 50,
                head_lines: 5,
                tail_lines: 5,
                priority_patterns: vec![r"(?i)(panicked|fatal|Error)".to_string()],
            }),
        }),
        "shell-ls" => Some(RtkFilter {
            id: "shell-ls".into(),
            label: "ls / find".into(),
            description: "Compact file listing.".into(),
            command_types: vec!["shell-ls".into()],
            match_patterns: vec![],
            strip_ansi: true,
            filter_stderr: false,
            replace: vec![
                ReplaceRule {
                    pattern: r"^total \d+".to_string(),
                    replacement: String::new(),
                },
            ],
            match_output: vec![],
            strip_patterns: vec![r"^\s*$".to_string()],
            keep_patterns: vec![],
            collapse_patterns: vec![],
            truncate_line_at: 0,
            on_empty: "(empty directory)".to_string(),
            truncate: Some(TruncateConfig {
                max_lines: 80,
                head_lines: 20,
                tail_lines: 10,
                priority_patterns: vec![],
            }),
        }),
        "generic-error" => Some(RtkFilter {
            id: "generic-error".into(),
            label: "Generic Error".into(),
            description: "Keep error lines.".into(),
            command_types: vec!["generic-error".into()],
            match_patterns: vec![],
            strip_ansi: true,
            filter_stderr: false,
            replace: vec![],
            match_output: vec![],
            strip_patterns: vec![],
            keep_patterns: vec![
                r"(?i)(error|failed|exception|traceback|panic|FAIL)".to_string(),
            ],
            collapse_patterns: vec![],
            truncate_line_at: 0,
            on_empty: String::new(),
            truncate: Some(TruncateConfig {
                max_lines: 30,
                head_lines: 5,
                tail_lines: 5,
                priority_patterns: vec![r"(?i)(error|failed)".to_string()],
            }),
        }),
        _ => None,
    }
}

/// Obtiene el filtro genérico de propósito general.
pub fn get_generic_filter() -> RtkFilter {
    RtkFilter {
        id: "generic".into(),
        label: "Generic Output".into(),
        description: "Strip ANSI + dedup + truncate.".into(),
        command_types: vec![],
        match_patterns: vec![],
        strip_ansi: true,
        filter_stderr: true,
        replace: vec![],
        match_output: vec![],
        strip_patterns: vec![
            r"^\s*$".to_string(),
            r"^\s*(warning:)".to_string(),
        ],
        keep_patterns: vec![],
        collapse_patterns: vec![],
        truncate_line_at: 0,
        on_empty: String::new(),
        truncate: Some(TruncateConfig {
            max_lines: 120,
            head_lines: 15,
            tail_lines: 10,
            priority_patterns: vec![r"(?i)(error|failed|warning)".to_string()],
        }),
    }
}

#[cfg(test)]
mod tests {
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
        assert!(rules.contains(&"cargo-test::match_output".to_string()));
        assert_eq!(result, "✓ all tests passed");
    }

    #[test]
    fn test_strip_ansi_removes_codes() {
        let input = "\u{1b}[32mgreen\u{1b}[0m";
        let output = strip_ansi(input);
        assert_eq!(output, "green");
    }
}
