use serde::{Deserialize, Serialize};

/// Estadísticas de compresión para un request.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompressionStats {
    /// Chars totales del contenido de todos los mensajes antes de comprimir.
    pub original_chars: usize,
    /// Chars después de comprimir.
    pub compressed_chars: usize,
    /// Tokens estimados (via cl100k_base BPE) antes de comprimir.
    /// 0 si no se calculó (modo Off o sin mensajes).
    pub original_tokens: usize,
    /// Tokens estimados después de comprimir.
    pub compressed_tokens: usize,
    /// Porcentaje de ahorro en TOKENS (no chars). 0.0 si no hubo compresión.
    /// Este es el porcentaje real que refleja el ahorro de costos —
    /// el ahorro en chars puede ser engañoso porque BPE no es lineal.
    pub savings_pct: f64,
    /// Porcentaje de ahorro en CHARS (bytes). Se conserva para
    /// diagnóstico — el savings en tokens puede diferir del savings
    /// en chars porque BPE no es lineal.
    pub savings_pct_chars: f64,
    /// Técnicas aplicadas (ej: "lite::collapse_whitespace", "rtk::git-status").
    pub techniques: Vec<String>,
}

impl CompressionStats {
    /// Crea stats vacías para modo Off.
    pub fn empty() -> Self {
        Self {
            original_chars: 0,
            compressed_chars: 0,
            original_tokens: 0,
            compressed_tokens: 0,
            savings_pct: 0.0,
            savings_pct_chars: 0.0,
            techniques: Vec::new(),
        }
    }

    /// Crea stats después de aplicar compresión. Calcula savings_pct
    /// basado en TOKENS (no chars), lo que da un porcentaje real de
    /// ahorro de costos. Si no hay tokens (vacío), cae a chars.
    pub fn new(
        original_chars: usize,
        compressed_chars: usize,
        original_tokens: usize,
        compressed_tokens: usize,
        techniques: Vec<String>,
    ) -> Self {
        // savings_pct basado en tokens si disponibles, sino chars
        let savings_pct = if original_tokens > 0 {
            let saved = original_tokens.saturating_sub(compressed_tokens);
            (saved as f64 / original_tokens as f64) * 100.0
        } else if original_chars > 0 {
            let saved = original_chars.saturating_sub(compressed_chars);
            (saved as f64 / original_chars as f64) * 100.0
        } else {
            0.0
        };
        // savings_pct_chars siempre basado en chars
        let savings_pct_chars = if original_chars > 0 {
            let saved = original_chars.saturating_sub(compressed_chars);
            (saved as f64 / original_chars as f64) * 100.0
        } else {
            0.0
        };
        Self {
            original_chars,
            compressed_chars,
            original_tokens,
            compressed_tokens,
            savings_pct,
            savings_pct_chars,
            techniques,
        }
    }

    /// Técnicas como string CSV para guardar en SQL.
    pub fn techniques_csv(&self) -> Option<String> {
        if self.techniques.is_empty() {
            None
        } else {
            Some(self.techniques.join(","))
        }
    }

    /// savings_pct como Option<f64> para SQL.
    pub fn savings_pct_opt(&self) -> Option<f64> {
        if self.savings_pct > 0.0 {
            Some(self.savings_pct)
        } else {
            None
        }
    }
}
