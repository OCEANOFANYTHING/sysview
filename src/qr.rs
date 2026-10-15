//! Terminal QR code for the dashboard URL: scan it from a phone instead of
//! typing an address and a token.

use qrcodegen::{QrCode, QrCodeEcc};

/// Renders `text` as a QR code for a terminal.
///
/// Modules are drawn with background colours (dark on light) instead of
/// half-blocks, so the code scans the same from a dark or a light terminal
/// theme. A two-module quiet zone is included. Returns an empty string when
/// the text is too long for a QR symbol.
pub fn render(text: &str) -> String {
    let qr = match QrCode::encode_text(text, QrCodeEcc::Low) {
        Ok(q) => q,
        Err(_) => return String::new(),
    };
    let size = qr.size();
    let border = 2;
    let mut out = String::new();
    for y in -border..size + border {
        let mut current: Option<bool> = None;
        out.push_str("  ");
        for x in -border..size + border {
            let m = x >= 0 && y >= 0 && x < size && y < size && qr.get_module(x, y);
            if current != Some(m) {
                out.push_str(if m { "\x1b[40m" } else { "\x1b[47m" });
                current = Some(m);
            }
            out.push_str("  ");
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn renders_both_colours_and_a_quiet_zone() {
        let out = render("http://192.168.1.10:8080/");
        assert!(out.contains("\x1b[40m"), "dark modules are drawn");
        assert!(out.contains("\x1b[47m"), "the quiet zone is drawn");
        let rows = out.lines().count();
        assert!((25..=60).contains(&rows), "unexpected symbol size: {rows}");
    }

    #[test]
    fn text_that_cannot_fit_is_empty_not_a_panic() {
        assert_eq!(render(&"x".repeat(5000)), "");
    }
}
