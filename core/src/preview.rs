//! Inline image previews, core half (spec 2026-09-26-image-previews.md §2): which bytes may be
//! handed to a decoder at all. The kind is sniffed (never taken from the name), and the
//! dimensions are read from the header and capped before anything decodes. The parsers here
//! are pure and bounded: they look at no more than the first `HEADER_MAX` bytes.

/// Largest file a preview is made of (checked before anything is fetched).
pub const PREVIEW_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// Largest side, in pixels.
pub const PREVIEW_MAX_SIDE: u32 = 8192;
/// Largest area, in pixels.
pub const PREVIEW_MAX_PIXELS: u64 = 40_000_000;
/// How far into the file the header readers look.
const HEADER_MAX: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
}

/// An image ready for a sandboxed decoder: its sniffed kind, its header's size (within the
/// caps), and its bytes (decrypted in memory, never written to disk).
#[derive(Clone, PartialEq, Eq)]
pub struct ImagePreview {
    pub kind: ImageKind,
    pub width: u32,
    pub height: u32,
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for ImagePreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImagePreview")
            .field("kind", &self.kind)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// The kind of these bytes, if it's one a preview is made of.
pub(crate) fn sniff(bytes: &[u8]) -> Option<ImageKind> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageKind::Png)
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some(ImageKind::Jpeg)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageKind::Gif)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageKind::Webp)
    } else {
        None
    }
}

/// Width and height as the header states them, if it can be read.
pub(crate) fn dimensions(kind: ImageKind, bytes: &[u8]) -> Option<(u32, u32)> {
    let b = &bytes[..bytes.len().min(HEADER_MAX)];
    match kind {
        // Signature (8), IHDR length (4), "IHDR" (4), width (4), height (4).
        ImageKind::Png => {
            (b.get(12..16)? == b"IHDR").then_some(())?;
            Some((be32(b, 16)?, be32(b, 20)?))
        }
        // The logical screen: little-endian u16s after the 6-byte signature.
        ImageKind::Gif => Some((le16(b, 6)? as u32, le16(b, 8)? as u32)),
        ImageKind::Jpeg => jpeg_dimensions(b),
        ImageKind::Webp => webp_dimensions(b),
    }
}

/// Within the caps: both sides present, neither too long, not too many pixels.
pub(crate) fn within_caps(width: u32, height: u32) -> bool {
    width > 0
        && height > 0
        && width <= PREVIEW_MAX_SIDE
        && height <= PREVIEW_MAX_SIDE
        && width as u64 * height as u64 <= PREVIEW_MAX_PIXELS
}

/// Walk the JPEG markers to the first start-of-frame (SOF0 to SOF15, not DHT, JPG or DAC).
fn jpeg_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2; // after SOI
    loop {
        // Fill bytes (0xff) may precede a marker.
        while *b.get(i)? == 0xff && *b.get(i + 1)? == 0xff {
            i += 1;
        }
        if *b.get(i)? != 0xff {
            return None;
        }
        let marker = *b.get(i + 1)?;
        match marker {
            // Markers without a length.
            0xd0..=0xd9 | 0x01 => {
                i += 2;
                continue;
            }
            0xc0..=0xcf if !matches!(marker, 0xc4 | 0xc8 | 0xcc) => {
                // Length (2), precision (1), height (2), width (2).
                let h = be16(b, i + 5)? as u32;
                let w = be16(b, i + 7)? as u32;
                return Some((w, h));
            }
            _ => {
                let len = be16(b, i + 2)? as usize;
                if len < 2 {
                    return None;
                }
                i += 2 + len;
            }
        }
    }
}

/// VP8 (lossy), VP8L (lossless) or VP8X (extended) right after the RIFF header.
fn webp_dimensions(b: &[u8]) -> Option<(u32, u32)> {
    let chunk = b.get(12..16)?;
    let d = 20; // chunk data
    match chunk {
        b"VP8 " => {
            // Frame tag (3), start code 9d 01 2a (3), then 14-bit width and height.
            (b.get(d + 3..d + 6)? == [0x9d, 0x01, 0x2a]).then_some(())?;
            Some((
                (le16(b, d + 6)? & 0x3fff) as u32,
                (le16(b, d + 8)? & 0x3fff) as u32,
            ))
        }
        b"VP8L" => {
            (*b.get(d)? == 0x2f).then_some(())?;
            let bits = u32::from_le_bytes(b.get(d + 1..d + 5)?.try_into().ok()?);
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        b"VP8X" => {
            // Flags (4), then 24-bit canvas width - 1 and height - 1.
            let w = le24(b, d + 4)? + 1;
            let h = le24(b, d + 7)? + 1;
            Some((w, h))
        }
        _ => None,
    }
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn le24(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at + 3)?;
    Some(s[0] as u32 | (s[1] as u32) << 8 | (s[2] as u32) << 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        b.extend(w.to_be_bytes());
        b.extend(h.to_be_bytes());
        b.extend([8, 6, 0, 0, 0]);
        b
    }

    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let mut b = vec![0xff, 0xd8];
        // An APP0 segment first, then a DHT (not a frame), then SOF2.
        b.extend([0xff, 0xe0, 0x00, 0x04, 0x4a, 0x46]);
        b.extend([0xff, 0xc4, 0x00, 0x03, 0x00]);
        b.extend([0xff, 0xc2, 0x00, 0x11, 0x08]);
        b.extend(h.to_be_bytes());
        b.extend(w.to_be_bytes());
        b
    }

    fn gif(w: u16, h: u16) -> Vec<u8> {
        let mut b = b"GIF89a".to_vec();
        b.extend(w.to_le_bytes());
        b.extend(h.to_le_bytes());
        b
    }

    fn webp(chunk: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut b = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        b.extend(chunk);
        b.extend([0, 0, 0, 0]);
        b.extend(data);
        b
    }

    fn read(bytes: &[u8]) -> Option<(ImageKind, u32, u32)> {
        let kind = sniff(bytes)?;
        let (w, h) = dimensions(kind, bytes)?;
        Some((kind, w, h))
    }

    #[test]
    fn kinds_and_sizes_come_from_the_bytes() {
        assert_eq!(read(&png(640, 480)), Some((ImageKind::Png, 640, 480)));
        assert_eq!(read(&jpeg(1920, 1080)), Some((ImageKind::Jpeg, 1920, 1080)));
        assert_eq!(read(&gif(32, 16)), Some((ImageKind::Gif, 32, 16)));
        // VP8: frame tag, start code, 14-bit sizes.
        let lossy = webp(
            b"VP8 ",
            &[0, 0, 0, 0x9d, 0x01, 0x2a, 0x80, 0x02, 0xe0, 0x01],
        );
        assert_eq!(read(&lossy), Some((ImageKind::Webp, 640, 480)));
        // VP8L: signature 0x2f, then width-1 (14 bits) and height-1 (14 bits).
        let bits: u32 = 99 | (49 << 14);
        let mut l = vec![0x2f];
        l.extend(bits.to_le_bytes());
        assert_eq!(read(&webp(b"VP8L", &l)), Some((ImageKind::Webp, 100, 50)));
        // VP8X: flags, then 24-bit width-1 and height-1.
        let x = [0, 0, 0, 0, 0xff, 0x01, 0x00, 0x7f, 0x00, 0x00];
        assert_eq!(read(&webp(b"VP8X", &x)), Some((ImageKind::Webp, 512, 128)));
    }

    #[test]
    fn anything_else_or_unreadable_is_no_preview() {
        for bytes in [
            b"%PDF-1.7".as_slice(),
            b"<svg onload=x>",
            b"BM\x00\x00",        // BMP: not on the list
            b"\x00\x00\x01\x00",  // ICO
            b"\x89PNG\r\n\x1a\n", // a PNG cut before IHDR
            b"\xff\xd8\xff",      // a JPEG with no frame
            b"GIF89a\x01",        // a GIF cut in its screen
            b"RIFF\x00\x00\x00\x00WEBPVP8Z",
        ] {
            assert_eq!(read(bytes), None, "{bytes:?}");
        }
        // A PNG whose first chunk isn't IHDR.
        let mut odd = png(10, 10);
        odd[12..16].copy_from_slice(b"tEXt");
        assert_eq!(read(&odd), None);
        // A JPEG segment with a length under 2 can't be walked.
        assert_eq!(read(&[0xff, 0xd8, 0xff, 0xe0, 0x00, 0x01]), None);
    }

    #[test]
    fn the_name_plays_no_part() {
        // A ".png" upload that is a JPEG is a JPEG (the caller never passes the name).
        assert_eq!(sniff(&jpeg(10, 10)), Some(ImageKind::Jpeg));
    }

    #[test]
    fn caps_hold_before_decoding() {
        assert!(within_caps(8192, 4096));
        assert!(!within_caps(8193, 10), "a side too long");
        assert!(!within_caps(10, 70_000), "a side too long");
        assert!(!within_caps(8000, 8000), "64 MP, over the area cap");
        assert!(!within_caps(0, 10) && !within_caps(10, 0));
        // A lying header claiming a huge canvas is refused as stated.
        let (w, h) = dimensions(ImageKind::Png, &png(100_000, 100_000)).unwrap();
        assert!(!within_caps(w, h));
    }
}
