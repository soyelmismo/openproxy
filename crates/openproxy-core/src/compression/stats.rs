use serde::{Deserialize, Serialize};

/// Estadísticas de compresión para un request.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompressionStats {
    /// Chars totales del contenido de todos los mensajes antes de comprimir.
    pub original_chars: usize,
    /// Chars después de comprimir.
    pub compressed_chars: usize,
    /// Porcentaje de ahorro (0.0 si no hubo compresión).
    pub savings_pct: f64,
    /// Técnicas aplicadas (ej: "lite::collapse_whitespace", "rtk::git-status").
    pub techniques: Vec<String>,
}

impl CompressionStats {
    /// Crea stats vacías para modo Off.
    pub fn empty() -> Self {
        Self {
            original_chars: 0,
            compressed_chars: 0,
            savings_pct: 0.0,
            techniques: Vec::new(),
        }
    }

    /// Crea stats después de aplicar compresión.
    pub fn new(original_chars: usize, compressed_chars: usize, techniques: Vec<String>) -> Self {
        let savings_pct = if original_chars > 0 {
            let saved = original_chars.saturating_sub(compressed_chars);
            (saved as f64 / original_chars as f64) * 100.0
        } else {
            0.0
        };
        Self {
            original_chars,
            compressed_chars,
            savings_pct,
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
