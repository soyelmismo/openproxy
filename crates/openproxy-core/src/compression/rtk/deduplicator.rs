/// Colapsa líneas consecutivas repetidas.
///
/// Si una línea aparece ≥ `threshold` veces seguidas, la colapsa a una
/// sola línea y agrega un marcador `[repeated Nx]`.

pub fn dedup_consecutive_lines(text: &str, threshold: usize) -> (String, usize) {
    let threshold = threshold.max(2);
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut collapsed: usize = 0;

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let mut run: usize = 1;
        while i + run < lines.len() && lines[i + run] == line {
            run += 1;
        }

        if !line.trim().is_empty() && run >= threshold {
            out.push(line.to_string());
            out.push(format!("    [repeated {}x]", run - 1));
            collapsed += run - 1;
            i += run;
            continue;
        }

        out.push(line.to_string());
        i += 1;
    }

    (out.join("\n"), collapsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_dedup_below_threshold() {
        let text = "a\nb\na\nb\n";
        let (result, collapsed) = dedup_consecutive_lines(text, 3);
        assert_eq!(collapsed, 0);
        assert_eq!(result, text);
    }

    #[test]
    fn test_dedup_repeated_lines() {
        let text = "a\nb\nb\nb\nb\nc\n";
        let (result, collapsed) = dedup_consecutive_lines(text, 3);
        assert_eq!(collapsed, 3);
        assert!(result.contains("[repeated 3x]"));
        assert!(result.contains("a\n"));
        assert!(result.contains("c\n"));
    }

    #[test]
    fn test_dedup_empty_lines_ignored() {
        let text = "\n\n\n\n";
        let (result, collapsed) = dedup_consecutive_lines(text, 3);
        assert_eq!(collapsed, 0);
        assert_eq!(result, "\n\n\n\n");
    }
}
