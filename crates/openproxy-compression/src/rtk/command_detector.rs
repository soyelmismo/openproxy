/// Detecta el tipo de comando CLI a partir del texto de output.
///
/// Usa `commandPatterns` (sobre la primera línea) y `contentPatterns`
/// (sobre el contenido completo) para clasificar el output.

#[derive(Debug, Clone, PartialEq)]
pub struct Detection {
    /// Identificador del detector (ej: "git-status", "cargo-test")
    pub id: String,
    /// Confianza 0.0–1.0
    pub confidence: f64,
    /// Primer comando detectado en el texto (si hay)
    pub command: Option<String>,
}

type DetectorFn = fn(&str, Option<&str>) -> Option<(&'static str, f64)>;

/// Lista de detectores registrados. Cada detector recibe el texto completo
/// y el comando detectado (si se pudo extraer), y retorna `(id, confidence)` si matchea.
///
/// `const` slice — zero allocation per `detect()` call. (Previously this
/// was a `Vec<DetectorFn>` rebuilt on every call.)
const DETECTORS: &[DetectorFn] = &[
    detect_git_status,
    detect_git_diff,
    detect_git_log,
    detect_git_branch,
    detect_cargo_test,
    detect_cargo_build,
    detect_npm_test,
    detect_npm_install,
    detect_docker_ps,
    detect_docker_logs,
    detect_kubernetes,
    detect_shell_ls,
    detect_shell_grep,
    detect_error_stacktrace,
    detect_generic_error,
];

/// Extrae el comando de las primeras líneas del texto.
pub fn extract_command(text: &str) -> Option<String> {
    for line in text.lines().take(4) {
        let trimmed = line.trim().trim_start_matches("$ ");
        if !trimmed.is_empty() {
            // Primer token
            let first_word = trimmed.split_whitespace().next()?;
            let known = [
                "git",
                "cargo",
                "npm",
                "pnpm",
                "yarn",
                "docker",
                "kubectl",
                "ls",
                "grep",
                "rg",
                "find",
                "cat",
                "make",
                "terraform",
                "systemctl",
                "ps",
                "df",
                "du",
                "curl",
                "wget",
            ];
            if known.contains(&first_word) {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Detecta el comando en el texto. Retorna el mejor match.
pub fn detect(text: &str) -> Detection {
    let command = extract_command(text);
    let cmd_ref = command.as_deref();

    let mut best: Option<(&'static str, f64)> = None;
    for detector in DETECTORS {
        if let Some(result) = detector(text, cmd_ref) {
            let (_id, conf) = &result;
            if best
                .as_ref()
                .is_none_or(|b: &(&'static str, f64)| *conf > b.1)
            {
                best = Some(result);
            }
        }
    }

    match best {
        Some((id, conf)) => Detection {
            id: id.to_string(),
            confidence: conf,
            command,
        },
        None => Detection {
            id: "unknown".into(),
            confidence: 0.1,
            command,
        },
    }
}

// ─── Individual detectors ───────────────────────────────────────────────────

macro_rules! define_detector {
    ($name:ident, $id:literal, cmds: [$($cmd:literal),+ $(,)?], conf: $conf:literal) => {
        fn $name(_text: &str, cmd: Option<&str>) -> Option<(&'static str, f64)> {
            let cmd_match = cmd.is_some_and(|c| $(c.starts_with($cmd))||+);
            if cmd_match {
                Some(($id, $conf))
            } else {
                None
            }
        }
    };
    ($name:ident, $id:literal, content: |$text:ident| $content_expr:expr, conf: $conf:literal) => {
        fn $name($text: &str, _cmd: Option<&str>) -> Option<(&'static str, f64)> {
            if $content_expr {
                Some(($id, $conf))
            } else {
                None
            }
        }
    };
    ($name:ident, $id:literal, cmds: [$($cmd:literal),+ $(,)?], content: |$text:ident| $content_expr:expr, both: $both:literal, content: $content_conf:literal $(, cmd_only: $cmd_conf:literal)?) => {
        fn $name($text: &str, cmd: Option<&str>) -> Option<(&'static str, f64)> {
            let cmd_match = cmd.is_some_and(|c| $(c.starts_with($cmd))||+);
            let content_match = $content_expr;
            if cmd_match && content_match {
                Some(($id, $both))
            } else if content_match {
                Some(($id, $content_conf))
            } $(
            else if cmd_match {
                Some(($id, $cmd_conf))
            }
            )?
            else {
                None
            }
        }
    };
}

define_detector!(
    detect_git_status,
    "git-status",
    cmds: ["git status"],
    content: |text| text.contains("On branch ")
        || text.contains("Changes not staged for commit")
        || text.contains("Changes to be committed")
        || text.contains("Untracked files:"),
    both: 0.95,
    content: 0.75,
    cmd_only: 0.50
);

define_detector!(
    detect_git_diff,
    "git-diff",
    cmds: ["git diff", "git show"],
    content: |text| text.contains("diff --git ") || text.contains("@@ -"),
    both: 0.95,
    content: 0.70
);

define_detector!(
    detect_git_log,
    "git-log",
    cmds: ["git log"],
    content: |text| text.contains('\n')
        && text.lines().any(|l| {
            l.starts_with("commit ")
                && l.len() > 40
                && l[7..].bytes().all(|b| b.is_ascii_hexdigit())
        }),
    both: 0.95,
    content: 0.70
);

define_detector!(
    detect_git_branch,
    "git-branch",
    cmds: ["git branch", "git checkout", "git switch"],
    conf: 0.80
);

define_detector!(
    detect_cargo_test,
    "cargo-test",
    cmds: ["cargo test", "cargo nextest"],
    content: |text| text.contains("running ") && text.contains(" tests")
        || text
            .lines()
            .any(|l| l.starts_with("test ") && l.contains("... ok")),
    both: 0.95,
    content: 0.55,
    cmd_only: 0.60
);

define_detector!(
    detect_cargo_build,
    "cargo-build",
    cmds: ["cargo build", "cargo check", "cargo clippy"],
    content: |text| text.contains("Compiling ")
        || text.contains("error[E")
        || text.contains("warning[")
        || (text.contains("Finished ") && text.contains("profile")),
    both: 0.90,
    content: 0.50,
    cmd_only: 0.55
);

define_detector!(
    detect_npm_test,
    "npm-test",
    cmds: ["npm test", "npm run test", "npx vitest", "npx jest"],
    content: |text| text.contains("PASS ")
        || text.contains("FAIL ")
        || text.contains("Test Suites:")
        || text.contains("Tests:"),
    both: 0.95,
    content: 0.50,
    cmd_only: 0.55
);

define_detector!(
    detect_npm_install,
    "npm-install",
    cmds: ["npm install", "npm add", "pnpm install", "yarn add"],
    content: |text| text.contains("added ") && text.contains(" packages")
        || text.contains("audited ") && text.contains(" packages"),
    both: 0.90,
    content: 0.45,
    cmd_only: 0.55
);

define_detector!(
    detect_docker_ps,
    "docker-ps",
    cmds: ["docker ps"],
    content: |text| text.contains("CONTAINER ID") && text.contains("IMAGE"),
    both: 0.95,
    content: 0.70
);

define_detector!(
    detect_docker_logs,
    "docker-logs",
    cmds: ["docker logs", "docker compose logs"],
    conf: 0.80
);

define_detector!(
    detect_kubernetes,
    "kubernetes",
    cmds: ["kubectl ", "oc "],
    content: |text| (text.contains("NAMESPACE") && text.contains("STATUS"))
        || (text.contains("Ready ") && text.contains("Running")),
    both: 0.90,
    content: 0.40,
    cmd_only: 0.50
);

define_detector!(
    detect_shell_ls,
    "shell-ls",
    cmds: ["ls", "find"],
    conf: 0.70
);

define_detector!(
    detect_shell_grep,
    "shell-grep",
    cmds: ["grep", "rg ", "ag "],
    conf: 0.70
);

define_detector!(
    detect_error_stacktrace,
    "error-stacktrace",
    content: |text| text.contains("Traceback (most recent call last)")
        || text.contains("panicked at")
        || (text.contains("thread '") && text.contains("panicked at"))
        || (text.contains("at ") && text.contains(".rs:")),
    conf: 0.80
);

define_detector!(
    detect_generic_error,
    "generic-error",
    content: |text| text.contains("Error:") || text.contains("error:"),
    conf: 0.30
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_git_status_detection() {
        let text = "On branch main\nChanges not staged for commit:\n\tmodified: foo.rs\n";
        let d = detect(text);
        assert_eq!(d.id, "git-status");
        assert!(d.confidence > 0.7);
    }

    #[test]
    fn test_cargo_test_detection() {
        let text = "running 5 tests\ntest utils::test_parse ... ok\ntest result: ok\n";
        let d = detect(text);
        assert_eq!(d.id, "cargo-test");
    }

    #[test]
    fn test_error_stacktrace_detection() {
        let text = "thread 'main' panicked at src/main.rs:42:\nsomething went wrong\n";
        let d = detect(text);
        assert_eq!(d.id, "error-stacktrace");
    }

    #[test]
    fn test_unknown_returns_with_low_confidence() {
        let text = "Some random output\n";
        let d = detect(text);
        assert_eq!(d.id, "unknown");
        assert!(d.confidence < 0.5);
    }

    #[test]
    fn test_extract_command_git() {
        let text = "$ git status\nOn branch main\n";
        assert_eq!(extract_command(text), Some("git status".into()));
    }
}
