//! Dot-matrix QR rendering in OpenProxy's own visual language.
//!
//! `qrcode` gives us the module matrix; this module turns it into an SVG made
//! of rounded dots instead of hard squares, with the finder patterns drawn as
//! OpenProxy rings. Server-side and dependency-light: the string is returned
//! directly to the client, so a checkout page never needs a JS QR library and
//! the payload never leaves our origin.

use qrcode::{Color, EcLevel, QrCode};

/// Rendered SVG plus the payload it encodes (for accessibility/tests).
#[derive(Debug, Clone)]
pub struct StyledQr {
    pub svg: String,
    pub modules: usize,
    pub payload_len: usize,
}

/// Render `payload` as a dot-matrix SVG QR code.
///
/// # Errors
/// Returns a message when the payload cannot be encoded at EC level M.
pub fn render(payload: &str, accent: &str, dark: &str, light: &str) -> Result<StyledQr, String> {
    let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::M)
        .map_err(|e| format!("qr encode: {e}"))?;
    let width = code.width();
    let modules: Vec<bool> = code
        .to_colors()
        .into_iter()
        .map(|c| c == Color::Dark)
        .collect();
    let quiet = 4usize;
    let total = width + quiet * 2;

    let mut svg = String::with_capacity(total * total * 12);
    svg.push_str(&format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {total} {total}" width="100%" height="100%" shape-rendering="geometricPrecision" role="img" aria-label="Payment QR code">"#
    ));
    svg.push_str(&format!(
        r#"<rect width="{total}" height="{total}" rx="3" fill="{light}"/>"#
    ));

    let is_finder = |x: usize, y: usize| -> bool {
        let in_box = |ox: usize, oy: usize| x >= ox && x < ox + 7 && y >= oy && y < oy + 7;
        in_box(0, 0) || in_box(width - 7, 0) || in_box(0, width - 7)
    };

    svg.push_str(&format!(r#"<g fill="{accent}">"#));
    for y in 0..width {
        for x in 0..width {
            if !modules[y * width + x] || is_finder(x, y) {
                continue;
            }
            let cx = (x + quiet) as f64;
            let cy = (y + quiet) as f64;
            svg.push_str(&format!(
                r#"<rect x="{cx}" y="{cy}" width="1" height="1" rx="0.42"/>"#
            ));
        }
    }
    svg.push_str("</g>");

    svg.push_str(&format!(r#"<g fill="none" stroke="{dark}" stroke-width="1">"#));
    for (ox, oy) in [(0usize, 0usize), (width - 7, 0), (0, width - 7)] {
        let cx = (ox + quiet) as f64;
        let cy = (oy + quiet) as f64;
        svg.push_str(&format!(
            r#"<rect x="{cx}" y="{cy}" width="7" height="7" rx="2.1"/>"#
        ));
    }
    svg.push_str("</g>");

    svg.push_str(&format!(r#"<g fill="{dark}">"#));
    for (ox, oy) in [(0usize, 0usize), (width - 7, 0), (0, width - 7)] {
        let cx = (ox + quiet) as f64 + 2.0;
        let cy = (oy + quiet) as f64 + 2.0;
        svg.push_str(&format!(
            r#"<rect x="{cx}" y="{cy}" width="3" height="3" rx="0.9"/>"#
        ));
    }
    svg.push_str("</g></svg>");

    Ok(StyledQr {
        svg,
        modules: width,
        payload_len: payload.len(),
    })
}

/// EIP-681 payment URI for a USDC transfer on Base.
///
/// Wallets that understand EIP-681 prefill the token, chain, recipient and
/// exact amount. The jittered amount IS the order identifier, so it must be
/// reproduced to the last micro-unit.
#[must_use]
pub fn eip681_usdc(chain_id: u64, usdc: &str, to: &str, amount_units: i64) -> String {
    let whole = amount_units / 1_000_000;
    let frac = amount_units % 1_000_000;
    let human = if frac == 0 {
        format!("{whole}")
    } else {
        format!("{whole}.{frac:06}")
    };
    format!(
        "ethereum:{usdc}@{chain_id}/transfer?address={to}&uint256={amount_units}&amount={human}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
    const COLLECTOR: &str = "0x12c7ab78f21ba0253a21d34a0151d44ddb824950";

    #[test]
    fn renders_an_svg_with_dots_and_rings() {
        let qr = render(
            "ethereum:0xabc@8453/transfer?address=0xdef&uint256=123",
            "#5b8def",
            "#0b0d12",
            "#ffffff",
        )
        .expect("render");
        assert!(qr.svg.starts_with("<svg"));
        assert!(qr.svg.ends_with("</svg>"));
        assert!(qr.svg.contains("rx=\"0.42\""), "data dots are rounded");
        assert!(qr.svg.contains("#5b8def"), "accent colour applied");
        assert!(qr.modules >= 21);
        assert!(qr.payload_len > 0);
    }

    #[test]
    fn longer_payloads_use_a_denser_matrix() {
        let short = render("a", "#fff", "#000", "#fff").expect("short");
        let long = render(&"x".repeat(300), "#fff", "#000", "#fff").expect("long");
        assert!(long.modules > short.modules, "denser payload, denser matrix");
    }

    #[test]
    fn eip681_carries_token_chain_recipient_and_exact_amount() {
        let uri = eip681_usdc(8453, USDC, COLLECTOR, 10_987_654);
        assert!(
            uri.starts_with("ethereum:0x833589fcd6edb6e08f4c7c32d4f71b54bda02913@8453/transfer"),
            "got {uri}"
        );
        assert!(uri.contains("address=0x12c7ab78f21ba0253a21d34a0151d44ddb824950"));
        assert!(uri.contains("uint256=10987654"), "micro units are exact");
        assert!(uri.contains("amount=10.987654"), "six decimals preserved");
    }

    #[test]
    fn eip681_renders_whole_dollar_amounts_without_a_fraction() {
        let uri = eip681_usdc(8453, USDC, COLLECTOR, 25_000_000);
        assert!(uri.contains("&amount=25"), "got {uri}");
        assert!(uri.contains("uint256=25000000"));
    }
}
