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

type DetectorFn = fn(&str, Option<&str>) -> Option<(String, f64)>;

/// Lista de detectores registrados. Cada detector recibe el texto completo
/// y el comando detectado (si se pudo extraer), y retorna `(id, confidence)` si matchea.
fn detectors() -> Vec<DetectorFn> {
    vec![
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
    ]
}

/// Extrae el comando de las primeras líneas del texto.
pub fn extract_command(text: &str) -> Option<String> {
    for line in text.lines().take(4) {
        let trimmed = line.trim().trim_start_matches("$ ");
        if !trimmed.is_empty() {
            // Primer token
            let first_word = trimmed.split_whitespace().next()?;
            let known = [
                "git", "cargo", "npm", "pnpm", "yarn", "docker", "kubectl",
                "ls", "grep", "rg", "find", "cat", "make", "terraform",
                "systemctl", "ps", "df", "du", "curl", "wget",
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

    let mut best: Option<(String, f64)> = None;
    for detector in detectors() {
        if let Some(result) = detector(text, cmd_ref) {
            let (_id, conf) = &result;
            if best.as_ref().map_or(true, |b: &(String, f64)| *conf > b.1) {
                best = Some(result);
            }
        }
    }

    match best {
        Some((id, conf)) => Detection { id, confidence: conf, command },
        None => Detection {
            id: "unknown".into(),
            confidence: 0.1,
            command,
        },
    }
}

// ─── Individual detectors ───────────────────────────────────────────────────

fn detect_git_status(text: &str, cmd: Option<&str>) -> Option<(String, f64)> {
    let cmd_match = cmd.map_or(false, |c| c.starts_with("git status"));
    let content_match = text.contains("On branch ")
        || text.contains("Changes not staged for commit")
        || text.contains("Changes to be committed")
        || text.contains("Untracked files:");
    if cmd_match && content_match {
        Some(("git-status".into(), 0.95))
    } else if content_match {
        Some(("git-status".into(), 0.75))
    } else if cmd_match {
        Some(("git-status".into(), 0.50))
    } else {
        None
    }
}

fn detect_git_diff(text: &str, cmd: Option<&str>) -> Option<(String, f64)> {
    let cmd_match = cmd.map_or(false, |c| c.starts_with("git diff") || c.starts_with("git show"));
    let content_match = text.contains("diff --git ") || text.contains("@@ -");
    if cmd_match && content_match {
        Some(("git-diff".into(), 0.95))
    } else if content_match {
        Some(("git-diff".into(), 0.70))
    } else {
        None
    }
}

fn detect_git_log(text: &str, cmd: Option<&str>) -> Option<(String, f64)> {
    let cmd_match = cmd.map_or(false, |c| c.starts_with("git log"));
    let content_match = text.contains('\n') && text.lines().any(|l| {
        l.starts_with("commit ") && l.len() > 40 && l[7..].chars().all(|c| c.is_ascii_hexdigit())
    });
    if cmd_match && content_match {
        Some(("git-log".into(), 0.95))
    } else if content_match {
        Some(("git-log".into(), 0.70))
    } else {
        None
    }
}

fn detect_git_branch(_text: &str, cmd: Option<&str>) -> Option<(String, f64)> {
    let cmd_match = cmd.map_or(false, |c| {
        c.starts_with("git branch") || c.starts_with("git checkout") || c.starts_with("git switch")
    });
    if cmd_match {
        Some(("git-branch".into(), 0.80))
    } else {
        None
    }
}

fn detect_cargo_test(text: &str, cmd: Option<&str>) -> Option<(String, f64)> {
    let cmd_match = cmd.map_or(false, |c| c.starts_with("cargo test") || c.starts_with("cargo nextest"));
    let content_match = text.contains("running ") && text.contains(" tests")
        || text.lines().any(|l| l.starts_with("test ") && l.contains("... ok"));
    if cmd_match {
        Some(("cargo-test".into(), if content_match { 0.95 } else { 0.60 }))
    } else if content_match {
        Some(("cargo-test".into(), 0.55))
    } else {
        None
    }
}

fn detect_cargo_build(text: &str, cmd: Option<&str>) -> Option<(String, f64)> {
    let cmd_match = cmd.map_or(false, |c| {
        c.starts_with("cargo build") || c.starts_with("cargo check") || c.starts_with("cargo clippy")
    });
    let content_match = text.contains("Compiling ")
        || text.contains("error[E")
        || text.contains("warning[")
        || (text.contains("Finished ") && text.contains("profile"));
    if cmd_match {
        Some(("cargo-build".into(), if content_match { 0.90 } else { 0.55 }))
    } else if content_match {
        Some(("cargo-build".into(), 0.50))
    } else {
        None
    }
}

fn detect_npm_test(text: &str, cmd: Option<&str>) -> Option<(String, f64)> {
    let cmd_match = cmd.map_or(false, |c| {
        c.starts_with("npm test") || c.starts_with("npm run test") || c.starts_with("npx vitest") || c.starts_with("npx jest")
    });
    let content_match = text.contains("PASS ") || text.contains("FAIL ")
        || text.contains("Test Suites:") || text.contains("Tests:");
    if cmd_match {
        Some(("npm-test".into(), if content_match { 0.95 } else { 0.55 }))
    } else if content_match {
        Some(("npm-test".into(), 0.50))
    } else {
        None
    }
}

fn detect_npm_install(text: &str, cmd: Option<&str>) -> Option<(String, f64)> {
    let cmd_match = cmd.map_or(false, |c| {
        c.starts_with("npm install") || c.starts_with("npm add") || c.starts_with("pnpm install") || c.starts_with("yarn add")
    });
    let content_match = text.contains("added ") && text.contains(" packages")
        || text.contains("audited ") && text.contains(" packages");
    if cmd_match {
        Some(("npm-install".into(), if content_match { 0.90 } else { 0.55 }))
    } else if content_match {
        Some(("npm-install".into(), 0.45))
    } else {
        None
    }
}

fn detect_docker_ps(text: &str, cmd: Option<&str>) -> Option<(String, f64)> {
    let cmd_match = cmd.map_or(false, |c| c.starts_with("docker ps"));
    let content_match = text.contains("CONTAINER ID") && text.contains("IMAGE");
    if cmd_match && content_match {
        Some(("docker-ps".into(), 0.95))
    } else if content_match {
        Some(("docker-ps".into(), 0.70))
    } else {
        None
    }
}

fn detect_docker_logs(_text: &str, cmd: Option<&str>) -> Option<(String, f64)> {
    let cmd_match = cmd.map_or(false, |c| c.starts_with("docker logs") || c.starts_with("docker compose logs"));
    if cmd_match {
        Some(("docker-logs".into(), 0.80))
    } else {
        None
    }
}

fn detect_kubernetes(text: &str, cmd: Option<&str>) -> Option<(String, f64)> {
    let cmd_match = cmd.map_or(false, |c| c.starts_with("kubectl ") || c.starts_with("oc "));
    let content_match = text.contains("NAMESPACE") && text.contains("STATUS")
        || text.contains("Ready ") && text.contains("Running");
    if cmd_match {
        Some(("kubernetes".into(), if content_match { 0.90 } else { 0.50 }))
    } else if content_match {
        Some(("kubernetes".into(), 0.40))
    } else {
        None
    }
}

fn detect_shell_ls(_text: &str, cmd: Option<&str>) -> Option<(String, f64)> {
    let cmd_match = cmd.map_or(false, |c| c.starts_with("ls") || c.starts_with("find"));
    if cmd_match {
        Some(("shell-ls".into(), 0.70))
    } else {
        None
    }
}

fn detect_shell_grep(_text: &str, cmd: Option<&str>) -> Option<(String, f64)> {
    let cmd_match = cmd.map_or(false, |c| {
        c.starts_with("grep") || c.starts_with("rg ") || c.starts_with("ag ")
    });
    if cmd_match {
        Some(("shell-grep".into(), 0.70))
    } else {
        None
    }
}

fn detect_error_stacktrace(text: &str, _cmd: Option<&str>) -> Option<(String, f64)> {
    if text.contains("Traceback (most recent call last)")
        || text.contains("panicked at")
        || text.contains("thread '") && text.contains("panicked at")
        || (text.contains("at ") && text.contains(".rs:") && text.contains(':'))
    {
        Some(("error-stacktrace".into(), 0.80))
    } else {
        None
    }
}

fn detect_generic_error(text: &str, _cmd: Option<&str>) -> Option<(String, f64)> {
    if text.contains("Error:") || text.contains("error:") {
        Some(("generic-error".into(), 0.30))
    } else {
        None
    }
}

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
