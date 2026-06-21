use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct TruncateConfig {
    #[serde(default = "default_max_lines")]
    pub max_lines: usize,
    #[serde(default = "default_head")]
    pub head_lines: usize,
    #[serde(default = "default_tail")]
    pub tail_lines: usize,
    #[serde(default)]
    pub priority_patterns: Vec<String>,
}

fn default_max_lines() -> usize {
    120
}
fn default_head() -> usize {
    20
}
fn default_tail() -> usize {
    20
}

impl Default for TruncateConfig {
    fn default() -> Self {
        Self {
            max_lines: 120,
            head_lines: 20,
            tail_lines: 20,
            priority_patterns: vec![
                r"(?i)(error|failed|exception|traceback|FAIL|panic|✖|✗)".to_string(),
            ],
        }
    }
}

/// Trunca texto manteniendo head + priority + tail, con marcador.
pub fn smart_truncate(text: &str, config: &TruncateConfig) -> (String, bool, usize) {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() <= config.max_lines {
        return (text.to_string(), false, 0);
    }

    let head = &lines[..config.head_lines.min(lines.len())];
    let tail_start = lines.len().saturating_sub(config.tail_lines);
    let tail = &lines[tail_start..];

    // Priority lines from middle section
    let middle = if config.head_lines < tail_start {
        &lines[config.head_lines..tail_start]
    } else {
        &[]
    };

    let priority_re: Vec<regex::Regex> = config
        .priority_patterns
        .iter()
        .filter_map(|p| regex::Regex::new(p).ok())
        .collect();

    let priority_lines: Vec<&str> = middle
        .iter()
        .filter(|l| priority_re.iter().any(|r| r.is_match(l)))
        .copied()
        .collect();

    let mut selected: Vec<String> = Vec::with_capacity(config.head_lines + priority_lines.len() + config.tail_lines);

    // Head
    for l in head {
        selected.push(l.to_string());
    }

    // Marker
    let dropped = lines.len() - head.len() - tail.len();
    selected.push(format!("[rtk:truncated {} lines]", dropped));

    // Priority lines (non-duplicate of head/tail)
    for l in &priority_lines {
        let already =
            head.iter().any(|h| *h == *l) || tail.iter().any(|t| *t == *l);
        if !already {
            selected.push(l.to_string());
        }
    }

    // Tail
    for l in tail {
        selected.push(l.to_string());
    }

    (selected.join("\n"), true, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_truncation_when_under_limit() {
        let text = "a\nb\nc\n";
        let config = TruncateConfig {
            max_lines: 10,
            ..Default::default()
        };
        let (result, truncated, _) = smart_truncate(text, &config);
        assert!(!truncated);
        assert_eq!(result, text);
    }

    #[test]
    fn test_truncation_preserves_head_tail() {
        let text: Vec<String> = (0..100).map(|i| format!("line {}", i)).collect();
        let text = text.join("\n");
        let config = TruncateConfig {
            max_lines: 20,
            head_lines: 5,
            tail_lines: 5,
            ..Default::default()
        };
        let (result, truncated, dropped) = smart_truncate(&text, &config);
        assert!(truncated);
        assert!(dropped > 0);
        assert!(result.starts_with("line 0\nline 1\n"));
        assert!(result.ends_with("line 98\nline 99"));
        assert!(result.contains("[rtk:truncated"));
    }
}
