//! Binary detection (SPEC §5.1, REQ-CTX-004): the algorithm, as written.

/// How a file's first bytes classify it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Content {
    /// Readable text, with the charset name §5.1 step 5 records.
    Text { charset: &'static str },
    /// Not text: report `binary: true`, never read, grep skips it.
    Binary,
    /// UTF-16 text: readable as a file but not editable (`E-FS-ENCODING`).
    Utf16 { little_endian: bool },
    /// Mostly printable ASCII but not valid UTF-8 — a Latin-1 or similar
    /// legacy file. Not binary, so the right report is `E-FS-ENCODING`.
    NotUtf8,
}

/// §5.1: only the first 8,192 bytes are examined.
pub const SNIFF_BYTES: usize = 8192;

fn is_control(byte: u8) -> bool {
    // 0x00–0x08, 0x0B, 0x0E–0x1F — i.e. control bytes other than
    // \t (09), \n (0A), \f (0C) and \r (0D).
    matches!(byte, 0x00..=0x08 | 0x0B | 0x0E..=0x1F)
}

/// Whether `bytes` is valid UTF-8 *or* only fails because the sample ended in
/// the middle of a multi-byte sequence (the sniff window cuts anywhere).
fn utf8_prefix_ok(bytes: &[u8], truncated: bool) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(_) => true,
        Err(e) => truncated && e.error_len().is_none(),
    }
}

fn utf16_ok(bytes: &[u8], little_endian: bool) -> bool {
    if bytes.len() < 2 || bytes.len() % 2 != 0 {
        return false;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    char::decode_utf16(units).all(|unit| unit.is_ok())
}

/// Classify a file from its leading bytes. `file_len` is the whole file's
/// size, so a sample that is the whole file is held to strict UTF-8.
#[must_use]
pub fn classify(sample: &[u8], file_len: u64) -> Content {
    let sample = &sample[..sample.len().min(SNIFF_BYTES)];
    if sample.is_empty() {
        return Content::Text { charset: "utf-8" };
    }
    // A BOM is a declaration, not an accident: honour it before any heuristic.
    if sample.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Content::Text {
            charset: "utf-8-bom",
        };
    }
    if sample.starts_with(&[0xFF, 0xFE]) {
        return Content::Utf16 {
            little_endian: true,
        };
    }
    if sample.starts_with(&[0xFE, 0xFF]) {
        return Content::Utf16 {
            little_endian: false,
        };
    }
    if sample.contains(&0) {
        return Content::Binary;
    }
    let controls = sample.iter().filter(|b| is_control(**b)).count();
    if controls * 10 > sample.len() {
        return Content::Binary;
    }
    let truncated = (sample.len() as u64) < file_len;
    if utf8_prefix_ok(sample, truncated) {
        return Content::Text { charset: "utf-8" };
    }
    // Invalid UTF-8. Mostly-ASCII data decodes as "valid UTF-16" (as CJK) by
    // accident, so UTF-16 is only believed when the bytes are *not* mostly
    // ASCII; mostly-ASCII data is a legacy-encoded text file instead.
    let ascii = sample
        .iter()
        .filter(|b| matches!(**b, 0x20..=0x7E | b'\t' | b'\n' | b'\r' | 0x0C))
        .count();
    if ascii * 10 >= sample.len() * 9 {
        return Content::NotUtf8;
    }
    for little_endian in [true, false] {
        if utf16_ok(sample, little_endian) {
            return Content::Utf16 { little_endian };
        }
    }
    Content::Binary
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text() -> Content {
        Content::Text { charset: "utf-8" }
    }

    #[test]
    fn plain_text_is_text() {
        assert_eq!(classify(b"fn main() {}\n", 13), text());
        assert_eq!(classify(b"", 0), text());
        assert_eq!(classify("héllo wörld\n".as_bytes(), 14), text());
    }

    #[test]
    fn a_nul_byte_means_binary() {
        assert_eq!(classify(b"abc\0def", 7), Content::Binary);
        assert_eq!(
            classify(&[0x7F, b'E', b'L', b'F', 0, 1, 2], 7),
            Content::Binary
        );
    }

    #[test]
    fn too_many_control_bytes_means_binary_but_whitespace_does_not() {
        let mut noisy = vec![b'a'; 80];
        noisy.extend(std::iter::repeat_n(0x01, 20)); // 20% control bytes
        assert_eq!(classify(&noisy, 100), Content::Binary);
        let mut quiet = vec![b'a'; 95];
        quiet.extend(std::iter::repeat_n(0x01, 5)); // 5%
        assert_eq!(classify(&quiet, 100), text());
        // Tabs, newlines, form feeds and carriage returns are not controls.
        assert_eq!(classify(b"a\tb\r\nc\x0cd\n", 10), text());
    }

    #[test]
    fn a_utf8_bom_is_recorded_and_kept() {
        assert_eq!(
            classify(b"\xEF\xBB\xBFhello", 8),
            Content::Text {
                charset: "utf-8-bom"
            }
        );
    }

    #[test]
    fn utf16_is_recognised_but_not_text() {
        let le: Vec<u8> = "hi".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut with_bom = vec![0xFF, 0xFE];
        with_bom.extend(&le);
        assert_eq!(
            classify(&with_bom, with_bom.len() as u64),
            Content::Utf16 {
                little_endian: true
            }
        );
        let be_bom = [0xFE, 0xFF, 0x00, b'h', 0x00, b'i'];
        assert_eq!(
            classify(&be_bom, 6),
            Content::Utf16 {
                little_endian: false
            }
        );
    }

    /// A Latin-1 file is text in the wrong encoding, not a blob.
    #[test]
    fn mostly_ascii_with_a_stray_byte_is_a_legacy_encoding() {
        assert_eq!(
            classify(b"caf\xE9 au lait, all plain text here\n", 36),
            Content::NotUtf8
        );
    }

    #[test]
    fn invalid_utf8_that_is_not_utf16_is_binary() {
        assert_eq!(
            classify(&[0xC3, 0x28, 0xA0, 0xA1, 0xFF, 0xFE, 0xFD], 7),
            Content::Binary
        );
    }

    /// The sniff window can cut a multi-byte character in half; that is not
    /// evidence of binary data.
    #[test]
    fn a_window_that_splits_a_character_is_still_text() {
        let mut bytes = vec![b'a'; SNIFF_BYTES - 1];
        bytes.extend("é".as_bytes()); // 2 bytes: the second falls outside the window
        let file_len = bytes.len() as u64;
        assert_eq!(classify(&bytes, file_len), text());
        // …but the same truncation of a *complete* file is an invalid file.
        let broken = &bytes[..SNIFF_BYTES];
        assert_eq!(classify(broken, broken.len() as u64), Content::NotUtf8);
    }

    #[test]
    fn only_the_first_8192_bytes_decide() {
        let mut bytes = vec![b'a'; SNIFF_BYTES + 100];
        bytes[SNIFF_BYTES + 10] = 0;
        assert_eq!(classify(&bytes, bytes.len() as u64), text());
    }
}
