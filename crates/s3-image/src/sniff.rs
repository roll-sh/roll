//! What an input is, from its bytes (never from its `Content-Type`), and whether it's animated.
//! Every scan here is bounded by the input's length and allocates nothing.

/// An input format roll recognises (decodable or not).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InputFormat {
    Jpeg,
    Png,
    Gif,
    Webp,
    /// Recognised, not decoded (no pure-Rust AV1 decoder roll can bound; see the crate docs).
    Avif,
    /// Recognised, not decoded (HEVC: no pure-Rust decoder exists).
    Heic,
}

impl InputFormat {
    pub fn content_type(self) -> &'static str {
        match self {
            InputFormat::Jpeg => "image/jpeg",
            InputFormat::Png => "image/png",
            InputFormat::Gif => "image/gif",
            InputFormat::Webp => "image/webp",
            InputFormat::Avif => "image/avif",
            InputFormat::Heic => "image/heic",
        }
    }
}

/// The input's format by its magic bytes; `None`: not an image roll knows.
pub fn sniff(b: &[u8]) -> Option<InputFormat> {
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(InputFormat::Jpeg);
    }
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(InputFormat::Png);
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return Some(InputFormat::Gif);
    }
    if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        return Some(InputFormat::Webp);
    }
    // ISO BMFF: a `ftyp` box first, with a major brand and compatible brands.
    if b.len() >= 16 && &b[4..8] == b"ftyp" {
        let size = (u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize).clamp(16, b.len().min(4096));
        let brands = std::iter::once(&b[8..12]).chain(b[16..size].as_chunks::<4>().0.iter().map(|c| c.as_slice()));
        let (mut avif, mut heic) = (false, false);
        for brand in brands {
            match brand {
                b"avif" | b"avis" => avif = true,
                b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"hevm" | b"hevs" => heic = true,
                _ => {}
            }
        }
        if avif {
            return Some(InputFormat::Avif);
        }
        if heic || &b[8..12] == b"mif1" || &b[8..12] == b"msf1" {
            return Some(InputFormat::Heic);
        }
    }
    None
}

/// Whether the image has more than one frame: a GIF with two or more images, an animated WebP
/// (the `VP8X` animation flag), an APNG (`acTL` before the image data).
pub fn is_animated(b: &[u8], f: InputFormat) -> bool {
    match f {
        InputFormat::Gif => gif_frames(b, 2) >= 2,
        InputFormat::Webp => b.len() > 20 && &b[12..16] == b"VP8X" && b[20] & 0x02 != 0,
        InputFormat::Png => png_has_actl(b),
        _ => false,
    }
}

/// Count a GIF's image descriptors, stopping at `stop`. Malformed data ends the count.
fn gif_frames(b: &[u8], stop: usize) -> usize {
    // Header (6) and logical screen descriptor (7), then the global colour table.
    if b.len() < 13 {
        return 0;
    }
    let mut i = 13;
    if b[10] & 0x80 != 0 {
        i += 3 << ((b[10] & 0x07) + 1);
    }
    let skip_blocks = |mut i: usize| -> Option<usize> {
        loop {
            let n = *b.get(i)? as usize;
            i += 1;
            if n == 0 {
                return Some(i);
            }
            i += n;
        }
    };
    let mut frames = 0;
    while let Some(&tag) = b.get(i) {
        match tag {
            0x21 => match skip_blocks(i + 2) {
                Some(next) => i = next,
                None => break,
            },
            0x2C => {
                frames += 1;
                if frames >= stop {
                    break;
                }
                let Some(&packed) = b.get(i + 9) else { break };
                i += 10;
                if packed & 0x80 != 0 {
                    i += 3 << ((packed & 0x07) + 1);
                }
                // LZW minimum code size, then the data sub-blocks.
                match skip_blocks(i + 1) {
                    Some(next) => i = next,
                    None => break,
                }
            }
            _ => break,
        }
    }
    frames
}

fn png_has_actl(b: &[u8]) -> bool {
    let mut i = 8;
    while i + 8 <= b.len() {
        let len = u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
        let kind = &b[i + 4..i + 8];
        if kind == b"acTL" {
            return true;
        }
        if kind == b"IDAT" || kind == b"IEND" {
            return false;
        }
        let Some(next) = i.checked_add(12).and_then(|n| n.checked_add(len)) else { return false };
        i = next;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_formats() {
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(InputFormat::Jpeg));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some(InputFormat::Png));
        assert_eq!(sniff(b"GIF89a...."), Some(InputFormat::Gif));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(InputFormat::Webp));
        assert_eq!(sniff(b"\0\0\0\x1cftypavif\0\0\0\0avifmif1miaf"), Some(InputFormat::Avif));
        assert_eq!(sniff(b"\0\0\0\x18ftypheic\0\0\0\0mif1heic"), Some(InputFormat::Heic));
        assert_eq!(sniff(b"\0\0\0\x18ftypmif1\0\0\0\0mif1heic"), Some(InputFormat::Heic));
        assert_eq!(sniff(b"\0\0\0\x18ftypisom\0\0\0\0isomiso2"), None, "an MP4 isn't an image");
        assert_eq!(sniff(b"<svg xmlns="), None);
        assert_eq!(sniff(b""), None);
        assert_eq!(sniff(b"BM"), None);
        // A huge declared box size is clamped to the bytes present.
        assert_eq!(sniff(b"\xff\xff\xff\xffftypmif1\0\0\0\0"), Some(InputFormat::Heic));
    }

    #[test]
    fn scans_are_bounded_on_garbage() {
        for n in 0..64 {
            let junk: Vec<u8> = (0..n).map(|i| (i * 37 % 256) as u8).collect();
            for f in [InputFormat::Gif, InputFormat::Png, InputFormat::Webp] {
                let mut b = match f {
                    InputFormat::Gif => b"GIF89a".to_vec(),
                    InputFormat::Png => b"\x89PNG\r\n\x1a\n".to_vec(),
                    _ => b"RIFF\0\0\0\0WEBP".to_vec(),
                };
                b.extend(&junk);
                let _ = is_animated(&b, f);
            }
        }
        // A PNG chunk length that would overflow.
        let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
        b.extend([0xFF, 0xFF, 0xFF, 0xFF]);
        b.extend(b"tEXt");
        assert!(!is_animated(&b, InputFormat::Png));
    }
}
