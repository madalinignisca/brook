//! Two-factor sign-in (TOTP): the parts of the GNOME UI that don't touch the network.
//!
//! QR rendering, the manual-entry key, input clean-up and wording live here as plain
//! functions with tests; the dialogs wire them to core's calls (PROTOCOL.md §1.2).

// Temporary, until the dialogs land on top of core's TOTP API (same PR, never merged alone).
#![allow(dead_code)]

use brook_core::Error;
use gtk::{gdk, glib};

/// Pixels per QR module, and the quiet zone around the code in modules (the spec's 4).
const MODULE_PX: usize = 6;
const QUIET_MODULES: usize = 4;

/// The QR code for `uri` as greyscale pixels: `(side, bytes)`, one byte per pixel,
/// black modules on white with a quiet zone. Rendered here, never fetched: the URI
/// carries the shared secret.
pub fn qr_pixels(uri: &str) -> Option<(usize, Vec<u8>)> {
    let code = qrcode::QrCode::new(uri.as_bytes()).ok()?;
    let width = code.width();
    let modules = code.to_colors();
    let side = (width + 2 * QUIET_MODULES) * MODULE_PX;
    let mut pixels = vec![0xFF; side * side];
    for (i, color) in modules.iter().enumerate() {
        if *color != qrcode::Color::Dark {
            continue;
        }
        let (mx, my) = (i % width + QUIET_MODULES, i / width + QUIET_MODULES);
        for y in my * MODULE_PX..(my + 1) * MODULE_PX {
            pixels[y * side + mx * MODULE_PX..y * side + (mx + 1) * MODULE_PX].fill(0x00);
        }
    }
    Some((side, pixels))
}

/// The QR code for `uri` as a texture for a `gtk::Picture`.
pub fn qr_texture(uri: &str) -> Option<gdk::MemoryTexture> {
    let (side, grey) = qr_pixels(uri)?;
    let side_i32 = i32::try_from(side).ok()?;
    // RGB, since greyscale memory formats need GTK 4.12 and this client targets 4.10.
    let rgb: Vec<u8> = grey.iter().flat_map(|p| [*p, *p, *p]).collect();
    Some(gdk::MemoryTexture::new(
        side_i32,
        side_i32,
        gdk::MemoryFormat::R8g8b8,
        &glib::Bytes::from_owned(rgb),
        side * 3,
    ))
}

/// The base32 secret from an `otpauth://` URI, for typing into an authenticator by hand.
pub fn secret_from_uri(uri: &str) -> Option<String> {
    let query = uri.strip_prefix("otpauth://")?.split_once('?')?.1;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("secret="))
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(str::to_string)
}

/// A key in groups of four, the way authenticator apps show and accept it.
pub fn group4(key: &str) -> String {
    key.as_bytes()
        .chunks(4)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A typed or pasted TOTP code: spaces are dropped ("123 456"); it must then be six
/// ASCII digits, else `None` (the submit button stays off).
pub fn clean_code(input: &str) -> Option<String> {
    let code: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    (code.len() == 6 && code.chars().all(|c| c.is_ascii_digit())).then_some(code)
}

/// A typed recovery code: spaces and dashes dropped, nothing else judged. The server
/// forgives case and look-alikes (O for 0, I or L for 1), so rejecting those letters
/// here would refuse codes the server accepts.
pub fn clean_recovery(input: &str) -> Option<String> {
    let code: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    (!code.is_empty()).then_some(code)
}

/// Which second-factor input the user typed, for the error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Factor {
    Code,
    Recovery,
}

/// A failed second step, in words. `auth.invalid_code` never burns the challenge, so
/// the user stays on the code page; `auth.totp_expired` sends them back to the password.
pub fn step_error_text(err: &Error, factor: Factor) -> String {
    match err {
        Error::Api { code, .. } => match (code.as_str(), factor) {
            // The replay guard is shared by every endpoint, so a code just used to turn
            // 2FA on (or to sign in elsewhere) is refused until the next one.
            ("auth.invalid_code", Factor::Code) => {
                "Wrong or already-used code. Wait for the next one.".into()
            }
            ("auth.invalid_code", Factor::Recovery) => {
                "That recovery code is wrong or already used. Try another one.".into()
            }
            ("auth.totp_expired", _) => "That took too long. Sign in again.".into(),
            ("auth.rate_limited", _) => "Too many attempts. Wait a while, then try again.".into(),
            _ => "The server refused the code.".into(),
        },
        Error::Http(_) | Error::Timeout => "Couldn't reach the server. Try again.".into(),
        _ => "Something went wrong. Sign in again.".into(),
    }
}

/// The notice after signing in with a recovery code, or when `/me` shows few left.
pub fn low_codes_notice(left: u32) -> Option<String> {
    match left {
        0 => Some(
            "You have no recovery codes left. Create new ones in Two-Factor Sign-In settings."
                .into(),
        ),
        1 => Some("You have 1 recovery code left. Create new ones soon.".into()),
        2 => Some("You have 2 recovery codes left. Create new ones soon.".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URI: &str = "otpauth://totp/Brook%3Aalice?secret=JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP\
                       &issuer=Brook&algorithm=SHA1&digits=6&period=30";

    #[test]
    fn the_qr_code_scans_back_to_the_exact_uri() {
        let (side, pixels) = qr_pixels(URI).unwrap();
        let mut img =
            rqrr::PreparedImage::prepare_from_greyscale(side, side, |x, y| pixels[y * side + x]);
        let grids = img.detect_grids();
        assert_eq!(grids.len(), 1);
        let (_meta, content) = grids[0].decode().unwrap();
        assert_eq!(content, URI);
    }

    #[test]
    fn the_qr_code_has_its_quiet_zone() {
        let (side, pixels) = qr_pixels(URI).unwrap();
        assert_eq!(pixels.len(), side * side);
        assert_eq!(side % MODULE_PX, 0);
        // Quiet zone: the whole border band is white.
        let quiet = QUIET_MODULES * MODULE_PX;
        assert!(pixels[..quiet * side].iter().all(|p| *p == 0xFF));
        // Finder pattern: the first dark module sits right after the quiet zone.
        assert_eq!(pixels[quiet * side + quiet], 0x00);
        assert_eq!(pixels[quiet * side + quiet - 1], 0xFF);
    }

    #[test]
    fn the_manual_key_comes_from_the_uri_in_groups_of_four() {
        let key = secret_from_uri(URI).unwrap();
        assert_eq!(key, "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP");
        assert_eq!(group4(&key), "JBSW Y3DP EHPK 3PXP JBSW Y3DP EHPK 3PXP");
        assert_eq!(secret_from_uri("https://example.com/?secret=AAAA"), None);
        assert_eq!(secret_from_uri("otpauth://totp/x?issuer=Brook"), None);
    }

    #[test]
    fn codes_accept_pasted_spaces_and_nothing_else() {
        assert_eq!(clean_code("123 456").as_deref(), Some("123456"));
        assert_eq!(clean_code(" 123456\n").as_deref(), Some("123456"));
        assert_eq!(clean_code("12345"), None);
        assert_eq!(clean_code("12a456"), None);
        assert_eq!(clean_code("١٢٣٤٥٦"), None); // non-ASCII digits
    }

    #[test]
    fn recovery_input_keeps_look_alikes_for_the_server() {
        assert_eq!(
            clean_recovery("ab1c-DEFG hjkm-NPQR-stvw").as_deref(),
            Some("ab1cDEFGhjkmNPQRstvw")
        );
        assert_eq!(clean_recovery("OIL0-...").as_deref(), Some("OIL0..."));
        assert_eq!(clean_recovery(" - "), None);
    }

    #[test]
    fn wrong_code_and_wrong_recovery_code_read_differently() {
        let api = |code: &str| Error::Api {
            code: code.into(),
            message: String::new(),
        };
        let code = step_error_text(&api("auth.invalid_code"), Factor::Code);
        let recovery = step_error_text(&api("auth.invalid_code"), Factor::Recovery);
        assert!(code.contains("Wait for the next one"));
        assert!(recovery.contains("Try another one"));
        assert!(step_error_text(&api("auth.totp_expired"), Factor::Code).contains("Sign in again"));
        assert!(step_error_text(&api("auth.rate_limited"), Factor::Code).contains("Too many"));
    }

    #[test]
    fn the_low_codes_notice_starts_at_two() {
        assert!(low_codes_notice(3).is_none());
        assert!(low_codes_notice(2).unwrap().contains("2 recovery codes"));
        assert!(low_codes_notice(1)
            .unwrap()
            .contains("1 recovery code left"));
        assert!(low_codes_notice(0).unwrap().contains("no recovery codes"));
    }
}
