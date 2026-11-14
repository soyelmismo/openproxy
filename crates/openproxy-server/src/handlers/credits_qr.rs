//! Dot-matrix QR rendering in OpenProxy's own visual language.
//!
//! `qrcode` gives us the module matrix; this module turns it into a
//! transparent SVG of rounded dots with the finder patterns drawn as OpenProxy
//! squares, all in the brand red. Server-side and dependency-light: the string
//! is returned directly to the client, so a checkout page never needs a JS QR
//! library and the payload never leaves our origin.
//!
//! Scanability contract: phone wallet scanners (MetaMask, Binance) are strict,
//! so the finder patterns stay CLASSIC squares (rounded rings failed MetaMask's
//! decoder). The payload encodes at EC level M — on a screen the matrix is not
//! damaged, so redundancy would only buy density (a 65×65 matrix at 240 px is
//! harder to scan than a 49×49 one), not reliability. The background stays
//! transparent: the checkout page paints a fixed light-gray panel behind the
//! SVG so contrast holds in both themes.

use qrcode::{Color, EcLevel, QrCode};
use std::fmt::Write as _;

/// Rendered SVG plus the payload it encodes (for accessibility/tests).
#[derive(Debug, Clone)]
pub struct StyledQr {
    pub svg: String,
    pub modules: usize,
    payload_len: usize,
}

/// Render `payload` as a dot-matrix SVG QR code (transparent background).
///
/// # Errors
/// Returns a message when the payload cannot be encoded at EC level M.
pub fn render(payload: &str, accent: &str) -> Result<StyledQr, String> {
    render_styled(payload, accent, false)
}

/// Render `payload` as a CLASSIC QR: solid white background, sharp black
/// modules, square finders. Maximum decoder compatibility — the stylized
/// light-on-dark variant defeated phone cameras in the field (Miguel's
/// phone could not scan it and MetaMask refused to open the URI), so the
/// checkout ships this plain variant. Same EC level and quiet zone.
///
/// # Errors
/// Returns a message when the payload cannot be encoded at EC level M.
pub fn render_classic(payload: &str) -> Result<StyledQr, String> {
    render_styled(payload, "#000000", true)
}

fn render_styled(payload: &str, accent: &str, classic: bool) -> Result<StyledQr, String> {
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
    // Writing into a `String` cannot fail; `let _` keeps the infallible
    // `fmt::Write` signature without an `unwrap`.
    let _ = write!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {total} {total}" width="100%" height="100%" shape-rendering="geometricPrecision" role="img" aria-label="Payment QR code">"#
    );
    // The classic variant carries its own solid white background: scanners
    // want dark-on-light with a real quiet zone, not transparency over an
    // unknown page paint.
    if classic {
        let _ = write!(
            svg,
            r##"<rect x="0" y="0" width="{total}" height="{total}" fill="#ffffff"/>"##
        );
    }

    let is_finder = |x: usize, y: usize| -> bool {
        let in_box = |ox: usize, oy: usize| x >= ox && x < ox + 7 && y >= oy && y < oy + 7;
        in_box(0, 0) || in_box(width - 7, 0) || in_box(0, width - 7)
    };

    // Data modules: rounded dots in the accent (finders are drawn apart).
    // The classic variant uses SHARP squares: plain black-on-white with no
    // shape games is what every decoder handles first.
    let data_rx = if classic { "" } else { r#" rx="0.3""# };
    let _ = write!(svg, r#"<g fill="{accent}">"#);
    for y in 0..width {
        for x in 0..width {
            if !modules[y * width + x] || is_finder(x, y) {
                continue;
            }
            let cx = x + quiet;
            let cy = y + quiet;
            let _ = write!(
                svg,
                r#"<rect x="{cx}" y="{cy}" width="1" height="1"{data_rx}/>"#
            );
        }
    }
    svg.push_str("</g>");

    // Finder patterns. The CLASSIC variant draws the spec silhouette as
    // three nested SOLID squares (7×7 dark, 5×5 white, 3×3 dark): a stroked
    // perimeter paints half a module OUTSIDE the grid (ink into the quiet
    // zone) and leaves a 0.5-module outer ring — both poison decoders, which
    // is why the stylized variant failed phones in the field. The stylized
    // variant keeps its softened ring for the brand surfaces that still use
    // it (transparent page background shows through the white separator).
    for (ox, oy) in [(0usize, 0usize), (width - 7, 0), (0, width - 7)] {
        let fx = ox + quiet;
        let fy = oy + quiet;
        if classic {
            let _ = write!(
                svg,
                r##"<g fill="{accent}"><rect x="{fx}" y="{fy}" width="7" height="7"/><rect x="{x1}" y="{y1}" width="5" height="5" fill="#ffffff"/><rect x="{x2}" y="{y2}" width="3" height="3"/></g>"##,
                x1 = fx + 1,
                y1 = fy + 1,
                x2 = fx + 2,
                y2 = fy + 2,
            );
        } else {
            let ring_rx = r#" rx="1""#;
            let _ = write!(
                svg,
                r#"<g fill="none" stroke="{accent}" stroke-width="1"><rect x="{fx}" y="{fy}" width="7" height="7"{ring_rx}/></g>"#
            );
            let centre_rx = r#" rx="0.5""#;
            let _ = write!(
                svg,
                r#"<g fill="{accent}"><rect x="{cx}" y="{cy}" width="3" height="3"{centre_rx}/></g>"#,
                cx = fx + 2,
                cy = fy + 2,
            );
        }
    }
    svg.push_str("</svg>");

    Ok(StyledQr {
        svg,
        modules: width,
        payload_len: payload.len(),
    })
}

/// EIP-681 payment URI for a USDC transfer on Base.
///
/// Only standard ERC-681 params go in (`address`, `uint256`): extra params
/// shrink the usable payload at EC level H and some decoders reject unknown
/// keys. Wallets that understand EIP-681 prefill the token, chain, recipient
/// and exact amount; the jittered amount IS the order identifier, so it must
/// be reproduced to the last micro-unit.
#[must_use]
pub fn eip681_usdc(chain_id: u64, usdc: &str, to: &str, amount_units: i64) -> String {
    format!("ethereum:{usdc}@{chain_id}/transfer?address={to}&uint256={amount_units}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const USDC: &str = "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913";
    const COLLECTOR: &str = "0x12c7ab78f21ba0253a21d34a0151d44ddb824950";

    #[test]
    fn renders_a_transparent_svg_with_dots_and_square_finders() {
        let qr = render(
            "ethereum:0xabc@8453/transfer?address=0xdef&uint256=123",
            "#e91d2a",
        )
        .expect("render");
        assert!(qr.svg.starts_with("<svg"));
        assert!(qr.svg.ends_with("</svg>"));
        assert!(
            !qr.svg.contains("<rect width="),
            "no background rect: the SVG is transparent"
        );
        assert!(qr.svg.contains("rx=\"0.3\""), "data dots are rounded");
        assert!(qr.svg.contains("#e91d2a"), "accent colour applied");
        assert!(qr.svg.contains("rx=\"1\""), "finder rings are soft squares");
        assert!(qr.modules >= 21);
        assert!(qr.payload_len > 0);
    }

    #[test]
    fn longer_payloads_use_a_denser_matrix() {
        let short = render("a", "#e91d2a").expect("short");
        let long = render(&"x".repeat(300), "#e91d2a").expect("long");
        assert!(
            long.modules > short.modules,
            "denser payload, denser matrix"
        );
    }

    #[test]
    fn a_realistic_checkout_payload_stays_scannable() {
        // The worst real payload: full ERC-681 with a jittered micro amount.
        // At EC level M it must land in a matrix a phone camera reads off a
        // 240 px panel without pinching to zoom (≤ 53 modules ≈ 4.5 px/module).
        let uri = eip681_usdc(8453, USDC, COLLECTOR, 5_004_103);
        let qr = render(&uri, "#e91d2a").expect("real payload encodes");
        assert!(
            qr.modules <= 53,
            "keeps scanning density sane, got {}",
            qr.modules
        );
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
        assert!(!uri.contains("amount="), "no non-standard params");
    }

    /// El checkout envía el QR CLÁSICO (blanco/negro, cuadrados afilados):
    /// la variante estilizada claro-sobre-oscuro no la escaneaban los móviles
    /// ni MetaMask la abría. Mismo payload, misma densidad que `render`.
    #[test]
    fn sec_37c5309c() {
        let uri = eip681_usdc(8453, USDC, COLLECTOR, 5_004_103);
        let qr = render_classic(&uri).expect("classic render");
        assert!(qr.svg.contains(r##"fill="#ffffff""##), "fondo blanco sólido");
        assert!(qr.svg.contains(r##"fill="#000000""##), "módulos negros");
        assert!(
            !qr.svg.contains("rx="),
            "sin esquinas redondeadas: cuadrados afilados"
        );
        let styled = render(&uri, "#e91d2a").expect("styled render");
        assert_eq!(qr.modules, styled.modules, "misma matriz");
        assert!(
            qr.modules <= 53,
            "densidad de escaneo intacta, got {}",
            qr.modules
        );
    }
}
