//! Encoding detection and transcoding for content search (UTF-8, UTF-16LE/BE, Latin-1 and
//! the single-byte encodings of the WHATWG Encoding Standard).
//!
//! The searcher matches byte patterns against UTF-8, but files written by Windows tools
//! (Notepad "Unicode", PowerShell `>` redirection, `reg export`, MSBuild logs) are often
//! UTF-16LE. This module decides which encoding a buffer is in and converts it to UTF-8.
//!
//! Call it only when the user asked for an encoding (an `rg -E`/`--encoding`-style flag) or
//! when [`detect`] reports [`Encoding::Utf16Le`] or [`Encoding::Utf16Be`]. Transcoding copies
//! the whole file, so a scan that transcodes every candidate spends roughly twice the memory
//! and loses the zero-copy path; plain UTF-8 needs no transcode at all.
//!
//! Line numbers are counted on the returned UTF-8 buffer (line breaks survive transcoding),
//! and a byte offset inside it maps back to the original by encoding: identity for UTF-8 and
//! Latin-1 (plus the stripped BOM length), and `2 * utf16 unit index` for UTF-16.

use crate::encoding_tables::{Label, LABELS, SINGLE_BYTE};

/// A text encoding the searcher can match against after transcoding to UTF-8.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Latin1,
    /// A single-byte encoding of the WHATWG standard: an index into `SINGLE_BYTE`.
    SingleByte(u8),
    /// The standard's `replacement` encoding: non-empty input decodes to one U+FFFD.
    Replacement,
}

/// Why a `--encoding` name was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelError {
    Unknown,
    /// A label of the standard whose (multi-byte) encoding, named here, has no decoder.
    Unsupported(&'static str),
}

const BOM_UTF8: &[u8] = &[0xEF, 0xBB, 0xBF];
const BOM_UTF16LE: &[u8] = &[0xFF, 0xFE];
const BOM_UTF16BE: &[u8] = &[0xFE, 0xFF];
/// U+FFFD as UTF-8: one replacement character per undecodable byte or code unit.
const REPLACEMENT: &[u8] = &[0xEF, 0xBF, 0xBD];

impl Encoding {
    /// Parse a CLI name. `None` for anything [`Encoding::lookup`] refuses.
    pub fn from_name(s: &str) -> Option<Encoding> {
        Encoding::lookup(s).ok()
    }

    /// Parse a CLI name: "auto", the names dusage has always taken ("utf-8"/"utf8",
    /// "utf-16"/"utf-16le"/"utf16le", "utf-16be", "latin1"/"latin-1"/"iso-8859-1"), then
    /// every label of the WHATWG Encoding Standard, which is the set ripgrep accepts.
    ///
    /// Names are matched case-insensitively. "auto" yields [`Encoding::Utf8`], the passthrough
    /// state meaning "no forced encoding": a caller that accepted "auto" must then use
    /// [`detect`] on the buffer instead of trusting that `Utf8`.
    ///
    /// The five Latin-1 spellings stay true ISO-8859-1 (0x80..=0x9F are the C1 controls),
    /// where the standard, and so ripgrep, reads them as windows-1252. The standard's other
    /// labels for that encoding ("ascii", "cp1252", "l1", ...) do mean windows-1252.
    pub fn lookup(s: &str) -> Result<Encoding, LabelError> {
        let n = s.trim().to_ascii_lowercase();
        match n.as_str() {
            "auto" | "utf-8" | "utf8" => return Ok(Encoding::Utf8),
            "utf-16" | "utf-16le" | "utf16" | "utf16le" => return Ok(Encoding::Utf16Le),
            "utf-16be" | "utf16be" => return Ok(Encoding::Utf16Be),
            "latin1" | "latin-1" | "iso-8859-1" | "iso8859-1" | "iso_8859_1" => {
                return Ok(Encoding::Latin1)
            }
            _ => {}
        }
        let i = LABELS
            .binary_search_by(|(label, _)| (*label).cmp(n.as_str()))
            .map_err(|_| LabelError::Unknown)?;
        match LABELS[i].1 {
            Label::Utf8 => Ok(Encoding::Utf8),
            Label::Utf16Le => Ok(Encoding::Utf16Le),
            Label::Utf16Be => Ok(Encoding::Utf16Be),
            Label::Replacement => Ok(Encoding::Replacement),
            Label::SingleByte(t) => Ok(Encoding::SingleByte(t)),
            Label::Unsupported(name) => Err(LabelError::Unsupported(name)),
        }
    }

    /// Canonical CLI name, the inverse of [`Encoding::from_name`].
    pub fn name(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
            Encoding::Utf16Le => "utf-16le",
            Encoding::Utf16Be => "utf-16be",
            Encoding::Latin1 => "latin1",
            Encoding::SingleByte(t) => SINGLE_BYTE[t as usize].0,
            Encoding::Replacement => "replacement",
        }
    }
}

/// Detect from a BOM or, failing that, from the shape of the first bytes: UTF-16LE/BE is
/// recognised by a NUL in every other position over the sampled prefix (at least 8 sampled
/// bytes and a clear majority, so ordinary ASCII text is never mistaken for UTF-16),
/// otherwise UTF-8.
///
/// A BOM wins, including a 2-byte one at the very start of a short sample. The heuristic needs
/// mostly ASCII-range text: a BOM-less UTF-16 file of pure CJK or emoji carries no NUL pattern
/// and detects as UTF-8. NUL-heavy binary that is not alternating also detects as UTF-8 --
/// that is the caller's cue to treat the file as binary, not text. UTF-32 (whose BOM
/// `FF FE 00 00` starts like UTF-16LE) is out of scope and reads as [`Encoding::Utf16Le`].
pub fn detect(sample: &[u8]) -> Encoding {
    if sample.starts_with(BOM_UTF8) {
        return Encoding::Utf8;
    }
    if sample.starts_with(BOM_UTF16LE) {
        return Encoding::Utf16Le;
    }
    if sample.starts_with(BOM_UTF16BE) {
        return Encoding::Utf16Be;
    }
    null_pattern(sample).unwrap_or(Encoding::Utf8)
}

/// The alternating-NUL half of [`detect`]: at least 3/4 of pairs carry a NUL in one byte
/// position while under 1/4 carry one in the other. Both conditions together are what keeps a
/// zero-filled or NUL-heavy blob (NULs in both positions) out of the UTF-16 answers.
fn null_pattern(sample: &[u8]) -> Option<Encoding> {
    let pairs = sample.len() / 2;
    if pairs < 4 {
        return None;
    }
    // For UTF-16LE text the odd byte of each pair is the zero high byte; for UTF-16BE the even.
    let (mut odd_nul, mut even_nul) = (0usize, 0usize);
    for i in 0..pairs {
        if sample[2 * i + 1] == 0 {
            odd_nul += 1;
        }
        if sample[2 * i] == 0 {
            even_nul += 1;
        }
    }
    let majority = |n: usize| n * 4 >= pairs * 3;
    let minority = |n: usize| n * 4 < pairs;
    if majority(odd_nul) && minority(even_nul) {
        Some(Encoding::Utf16Le)
    } else if majority(even_nul) && minority(odd_nul) {
        Some(Encoding::Utf16Be)
    } else {
        None
    }
}

/// True when the encoding needs a transcode before matching.
pub fn needs_transcode(e: Encoding) -> bool {
    !matches!(e, Encoding::Utf8)
}

/// Transcode to UTF-8, stripping a BOM. UTF-8 input is returned as a copy with a leading BOM
/// removed, so the searcher sees the same bytes it would see in a BOM-less file; Latin-1 bytes
/// `0x80..=0xFF` become U+0080..=U+00FF. Each unpaired surrogate half and a trailing odd byte
/// become one U+FFFD, so the output is always valid UTF-8 and never loses bytes silently.
///
/// Allocates the output exactly once, sized for the worst case, so a 4 MiB UTF-16 file
/// transcodes in a single pass without reallocation.
pub fn to_utf8(bytes: &[u8], e: Encoding) -> Vec<u8> {
    let body = strip_bom(bytes, e);
    match e {
        Encoding::Utf8 => {
            let mut out = Vec::with_capacity(bytes.len());
            out.extend_from_slice(body);
            out
        }
        Encoding::Latin1 => {
            let mut out = Vec::with_capacity(body.len() * 2);
            for &b in body {
                if b < 0x80 {
                    out.push(b);
                } else {
                    out.push(0xC0 | (b >> 6));
                    out.push(0x80 | (b & 0x3F));
                }
            }
            out
        }
        Encoding::Utf16Le => decode_utf16(body, true),
        Encoding::Utf16Be => decode_utf16(body, false),
        Encoding::SingleByte(t) => {
            let table = &SINGLE_BYTE[t as usize].1;
            let mut out = Vec::with_capacity(body.len() * 3);
            for &b in body {
                match b {
                    0..=0x7F => out.push(b),
                    _ => match table[(b - 0x80) as usize] {
                        0 => out.extend_from_slice(REPLACEMENT),
                        cp => push_code_point(&mut out, cp as u32),
                    },
                }
            }
            out
        }
        Encoding::Replacement => {
            if body.is_empty() {
                Vec::new()
            } else {
                REPLACEMENT.to_vec()
            }
        }
    }
}

fn strip_bom(bytes: &[u8], e: Encoding) -> &[u8] {
    let bom: &[u8] = match e {
        Encoding::Utf8 => BOM_UTF8,
        Encoding::Utf16Le => BOM_UTF16LE,
        Encoding::Utf16Be => BOM_UTF16BE,
        Encoding::Latin1 | Encoding::SingleByte(_) | Encoding::Replacement => return bytes,
    };
    bytes.strip_prefix(bom).unwrap_or(bytes)
}

/// One pass over 16-bit code units: surrogate pairs combine, lone halves become U+FFFD, and a
/// trailing odd byte becomes one more U+FFFD. Capacity is 3 bytes per unit, the BMP maximum,
/// so surrogate pairs (4 bytes for 2 units) and replacements still fit without regrowth.
fn decode_utf16(bytes: &[u8], little_endian: bool) -> Vec<u8> {
    let units = bytes.len() / 2;
    let mut out = Vec::with_capacity(units * 3);
    let unit = |i: usize| -> u16 {
        let pair = [bytes[2 * i], bytes[2 * i + 1]];
        if little_endian {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    };
    let mut i = 0;
    while i < units {
        let u = unit(i);
        i += 1;
        match u {
            0xD800..=0xDBFF => {
                let next = if i < units { unit(i) } else { 0 };
                if (0xDC00..=0xDFFF).contains(&next) {
                    i += 1;
                    let cp = 0x1_0000 + ((u as u32 - 0xD800) << 10) + (next as u32 - 0xDC00);
                    push_code_point(&mut out, cp);
                } else {
                    out.extend_from_slice(REPLACEMENT);
                }
            }
            0xDC00..=0xDFFF => out.extend_from_slice(REPLACEMENT),
            _ => push_code_point(&mut out, u as u32),
        }
    }
    if bytes.len() % 2 == 1 {
        out.extend_from_slice(REPLACEMENT);
    }
    out
}

fn push_code_point(out: &mut Vec<u8>, cp: u32) {
    let mut buf = [0u8; 4];
    let c = char::from_u32(cp).unwrap_or('\u{FFFD}');
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn utf16_le(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    fn utf16_be(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(u16::to_be_bytes).collect()
    }

    fn with_bom(bom: &[u8], body: &[u8]) -> Vec<u8> {
        let mut v = bom.to_vec();
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn from_name_utf8_variants() {
        for n in ["utf-8", "utf8", "UTF-8", "Utf8", "UTF8", "uTf-8"] {
            assert_eq!(Encoding::from_name(n), Some(Encoding::Utf8), "{n}");
        }
    }

    #[test]
    fn from_name_utf16_le_variants() {
        for n in [
            "utf-16", "utf-16le", "utf16le", "UTF-16LE", "Utf-16", "UTF16LE", "utf16",
        ] {
            assert_eq!(Encoding::from_name(n), Some(Encoding::Utf16Le), "{n}");
        }
    }

    #[test]
    fn from_name_utf16_be_variants() {
        for n in ["utf-16be", "utf16be", "UTF-16BE", "Utf16BE"] {
            assert_eq!(Encoding::from_name(n), Some(Encoding::Utf16Be), "{n}");
        }
    }

    #[test]
    fn from_name_latin1_variants() {
        for n in [
            "latin1",
            "latin-1",
            "iso-8859-1",
            "LATIN1",
            "Latin-1",
            "ISO-8859-1",
            "iso8859-1",
        ] {
            assert_eq!(Encoding::from_name(n), Some(Encoding::Latin1), "{n}");
        }
    }

    #[test]
    fn from_name_auto_variants() {
        for n in ["auto", "AUTO", "Auto", " auto "] {
            assert_eq!(Encoding::from_name(n), Some(Encoding::Utf8), "{n}");
        }
    }

    #[test]
    fn from_name_unknown_is_none() {
        for n in [
            "",
            " ",
            "utf-32",
            "utf32",
            "utf-16le-bom",
            "latin11",
            "cp1259",
            "iso-8859-12",
            "binary",
            "utf",
            "-",
        ] {
            assert_eq!(Encoding::from_name(n), None, "{n:?}");
        }
    }

    #[test]
    fn name_round_trips() {
        for e in [
            Encoding::Utf8,
            Encoding::Utf16Le,
            Encoding::Utf16Be,
            Encoding::Latin1,
        ] {
            assert_eq!(Encoding::from_name(e.name()), Some(e), "{e:?}");
        }
        assert_eq!(Encoding::Utf8.name(), "utf-8");
        assert_eq!(Encoding::Utf16Le.name(), "utf-16le");
        assert_eq!(Encoding::Utf16Be.name(), "utf-16be");
        assert_eq!(Encoding::Latin1.name(), "latin1");
    }

    #[test]
    fn only_utf8_skips_transcode() {
        assert!(!needs_transcode(Encoding::Utf8));
        assert!(needs_transcode(Encoding::Utf16Le));
        assert!(needs_transcode(Encoding::Utf16Be));
        assert!(needs_transcode(Encoding::Latin1));
    }

    #[test]
    fn detect_plain_ascii_is_utf8() {
        assert_eq!(detect(b"hello world, the quick brown fox"), Encoding::Utf8);
    }

    #[test]
    fn detect_utf8_non_ascii_is_utf8() {
        assert_eq!(detect("héllo wörld ✓".as_bytes()), Encoding::Utf8);
        assert_eq!(detect("日本語のテキストです".as_bytes()), Encoding::Utf8);
    }

    #[test]
    fn detect_utf16_le_text() {
        assert_eq!(detect(&utf16_le("hello world")), Encoding::Utf16Le);
        assert_eq!(
            detect(&utf16_le("C:\\Windows\\System32")),
            Encoding::Utf16Le
        );
    }

    #[test]
    fn detect_utf16_be_text() {
        assert_eq!(detect(&utf16_be("hello world")), Encoding::Utf16Be);
        assert_eq!(
            detect(&utf16_be("C:\\Windows\\System32")),
            Encoding::Utf16Be
        );
    }

    #[test]
    fn detect_utf16_le_with_some_wide_chars() {
        // 12 of 13 code units have a zero high byte, so the majority rule still fires.
        assert_eq!(detect(&utf16_le("hello wörld ✓")), Encoding::Utf16Le);
    }

    #[test]
    fn detect_boms_win_over_shape() {
        assert_eq!(detect(&with_bom(BOM_UTF8, b"plain ascii")), Encoding::Utf8);
        assert_eq!(
            detect(&with_bom(BOM_UTF8, "héllo".as_bytes())),
            Encoding::Utf8
        );
        assert_eq!(
            detect(&with_bom(BOM_UTF16LE, &utf16_le("hello world"))),
            Encoding::Utf16Le
        );
        assert_eq!(
            detect(&with_bom(BOM_UTF16BE, &utf16_be("hello world"))),
            Encoding::Utf16Be
        );
    }

    #[test]
    fn detect_bare_bom_is_enough() {
        assert_eq!(detect(BOM_UTF8), Encoding::Utf8);
        assert_eq!(detect(BOM_UTF16LE), Encoding::Utf16Le);
        assert_eq!(detect(BOM_UTF16BE), Encoding::Utf16Be);
        assert_eq!(detect(b"\xFF"), Encoding::Utf8);
        assert_eq!(detect(b"\xFE"), Encoding::Utf8);
    }

    #[test]
    fn detect_binary_blob_is_utf8() {
        // NUL-heavy but not alternating: every position is zero, so no pattern.
        let zeros = vec![0u8; 256];
        assert_eq!(detect(&zeros), Encoding::Utf8);
        // Structured binary: every high byte is NUL but a third of the low bytes are too.
        let blob: Vec<u8> = (0..128u8).flat_map(|i| [i & 3, 0, 0, 0, 0x7F, 0]).collect();
        assert_eq!(detect(&blob), Encoding::Utf8);
        // A PNG/ELF header: ASCII magic followed by arbitrary bytes.
        assert_eq!(
            detect(b"\x7FELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00"),
            Encoding::Utf8
        );
    }

    #[test]
    fn detect_short_inputs_never_panic() {
        for n in 0..40usize {
            let ascii = vec![b'a'; n];
            assert_eq!(detect(&ascii), Encoding::Utf8, "len {n}");
            let zeros = vec![0u8; n];
            assert_eq!(detect(&zeros), Encoding::Utf8, "len {n}");
            let mixed: Vec<u8> = (0..n).map(|i| if i % 2 == 0 { b'a' } else { 0 }).collect();
            let _ = detect(&mixed);
        }
    }

    #[test]
    fn detect_needs_eight_bytes_for_utf16() {
        assert_eq!(detect(&utf16_le("abcd")), Encoding::Utf16Le);
        // "ab" is only four bytes, below the sampling floor.
        assert_eq!(detect(&utf16_le("ab")), Encoding::Utf8);
        assert_eq!(detect(&utf16_le("a")), Encoding::Utf8);
    }

    #[test]
    fn round_trip_utf16_le() {
        let s = "héllo wörld ✓";
        assert_eq!(to_utf8(&utf16_le(s), Encoding::Utf16Le), s.as_bytes());
    }

    #[test]
    fn round_trip_utf16_be() {
        let s = "héllo wörld ✓";
        assert_eq!(to_utf8(&utf16_be(s), Encoding::Utf16Be), s.as_bytes());
    }

    #[test]
    fn round_trip_emoji_beyond_bmp() {
        let s = "ship it 🚀 now 🌍";
        assert_eq!(to_utf8(&utf16_le(s), Encoding::Utf16Le), s.as_bytes());
        assert_eq!(to_utf8(&utf16_be(s), Encoding::Utf16Be), s.as_bytes());
        // The pair is really a pair: 4 bytes per code point in each endianness.
        assert_eq!(utf16_le("🚀").len(), 4);
        assert_eq!(utf16_be("🚀").len(), 4);
    }

    #[test]
    fn empty_input_all_encodings() {
        for e in [
            Encoding::Utf8,
            Encoding::Utf16Le,
            Encoding::Utf16Be,
            Encoding::Latin1,
        ] {
            assert!(to_utf8(&[], e).is_empty(), "{e:?}");
        }
    }

    #[test]
    fn one_byte_input_is_one_replacement() {
        for e in [Encoding::Utf16Le, Encoding::Utf16Be] {
            assert_eq!(to_utf8(b"a", e), REPLACEMENT, "{e:?}");
            assert_eq!(to_utf8(b"\xFF", e), REPLACEMENT, "{e:?}");
        }
        assert_eq!(to_utf8(b"a", Encoding::Latin1), b"a");
        assert_eq!(to_utf8(b"a", Encoding::Utf8), b"a");
    }

    #[test]
    fn odd_length_utf16_gets_one_trailing_replacement() {
        let mut le = utf16_le("hello");
        le.push(b'x');
        assert_eq!(to_utf8(&le, Encoding::Utf16Le), b"hello\xEF\xBF\xBD");
        let mut be = utf16_be("hello");
        be.push(0x41);
        assert_eq!(to_utf8(&be, Encoding::Utf16Be), b"hello\xEF\xBF\xBD");
    }

    #[test]
    fn unpaired_surrogates_become_replacements() {
        // Lone high surrogate: D800.
        let lone_high = [0x00, 0xD8];
        assert_eq!(to_utf8(&lone_high, Encoding::Utf16Le), REPLACEMENT);
        // Lone low surrogate: DC00.
        let lone_low = [0x00, 0xDC];
        assert_eq!(to_utf8(&lone_low, Encoding::Utf16Le), REPLACEMENT);
        // High surrogate followed by a non-surrogate.
        let high_then_text = [0x00, 0xD8, b'a', 0x00];
        assert_eq!(
            to_utf8(&high_then_text, Encoding::Utf16Le),
            b"\xEF\xBF\xBDa"
        );
        // High followed by high: one replacement each.
        let two_highs = [0x00, 0xD8, 0x01, 0xD8];
        assert_eq!(
            to_utf8(&two_highs, Encoding::Utf16Le),
            b"\xEF\xBF\xBD\xEF\xBF\xBD"
        );
        // Same halves in big-endian order.
        assert_eq!(to_utf8(&[0xD8, 0x00], Encoding::Utf16Be), REPLACEMENT);
        assert_eq!(to_utf8(&[0xDC, 0x00], Encoding::Utf16Be), REPLACEMENT);
    }

    #[test]
    fn latin1_boundary_bytes_map_to_latin1_block() {
        assert_eq!(to_utf8(&[0x00], Encoding::Latin1), b"\x00");
        assert_eq!(to_utf8(&[0x7F], Encoding::Latin1), b"\x7F");
        assert_eq!(to_utf8(&[0x80], Encoding::Latin1), "\u{80}".as_bytes());
        assert_eq!(to_utf8(&[0xFF], Encoding::Latin1), "\u{FF}".as_bytes());
        assert_eq!(
            to_utf8(b"caf\xE9 cr\xE8me", Encoding::Latin1),
            "café crème".as_bytes()
        );
    }

    #[test]
    fn latin1_all_bytes_is_valid_utf8_and_bijective() {
        let all: Vec<u8> = (0..=255u16).map(|b| b as u8).collect();
        let out = to_utf8(&all, Encoding::Latin1);
        let s = std::str::from_utf8(&out).expect("latin1 output is valid utf8");
        let back: Vec<u32> = s.chars().map(u32::from).collect();
        assert_eq!(back, (0..256u32).collect::<Vec<_>>());
        assert_eq!(out.len(), 128 + 128 * 2);
    }

    #[test]
    fn bom_stripped_utf8() {
        let body = "héllo".as_bytes();
        let with = with_bom(BOM_UTF8, body);
        assert_eq!(to_utf8(&with, Encoding::Utf8), body);
        assert_eq!(detect(&with), Encoding::Utf8);
    }

    #[test]
    fn bom_stripped_utf16_le() {
        let body = utf16_le("hello");
        let with = with_bom(BOM_UTF16LE, &body);
        assert_eq!(to_utf8(&with, Encoding::Utf16Le), b"hello");
        // A BOM is only stripped at the start: the same pair later is a real U+FEFF.
        let mut inner = utf16_le("a");
        inner.extend_from_slice(BOM_UTF16LE);
        inner.extend_from_slice(&utf16_le("b"));
        assert_eq!(to_utf8(&inner, Encoding::Utf16Le), "a\u{FEFF}b".as_bytes());
    }

    #[test]
    fn bom_stripped_utf16_be() {
        let body = utf16_be("hello");
        let with = with_bom(BOM_UTF16BE, &body);
        assert_eq!(to_utf8(&with, Encoding::Utf16Be), b"hello");
    }

    #[test]
    fn utf8_without_bom_is_unchanged() {
        let s = "plain ascii ✓ 🚀".as_bytes();
        assert_eq!(to_utf8(s, Encoding::Utf8), s);
        // Invalid UTF-8 bytes pass through untouched: Utf8 means "already UTF-8".
        assert_eq!(to_utf8(b"\xC3\x28\xFF", Encoding::Utf8), b"\xC3\x28\xFF");
    }

    #[test]
    fn transcoded_output_always_valid_utf8() {
        // Every single code unit, including both surrogate ranges, in both endians.
        for e in [Encoding::Utf16Le, Encoding::Utf16Be] {
            let mut bytes = Vec::new();
            for u in 0..=0xFFFFu32 {
                let u = u as u16;
                let pair = if e == Encoding::Utf16Be {
                    u.to_be_bytes()
                } else {
                    u.to_le_bytes()
                };
                bytes.extend_from_slice(&pair);
            }
            let out = to_utf8(&bytes, e);
            let s = std::str::from_utf8(&out).expect("output must be valid utf8");
            // Never more scalars than units, and at most one replacement per surrogate half.
            assert!(s.chars().count() <= 0x10000, "{e:?}");
            assert!(s.chars().count() >= 0x10000 - 0x800, "{e:?}");
        }
    }

    #[test]
    fn transcode_4mib_utf16_is_fast_and_exact() {
        let line = "The quick brown fox jumps over the lazy dog. ";
        let mut text = String::with_capacity(2 * 1024 * 1024 + line.len());
        while text.len() < 2 * 1024 * 1024 {
            text.push_str(line);
        }
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut bytes = Vec::with_capacity(units.len() * 2);
        for u in &units {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        assert!(bytes.len() >= 4 * 1024 * 1024);

        let start = Instant::now();
        let out = to_utf8(&bytes, Encoding::Utf16Le);
        let elapsed = start.elapsed();

        assert!(out == text.as_bytes(), "transcode corrupted the text");
        assert!(
            elapsed.as_millis() < 2000,
            "4 MiB UTF-16LE transcode took {elapsed:?}, expected well under 100 ms in release"
        );
        println!(
            "4 MiB UTF-16LE -> UTF-8: {} ms",
            elapsed.as_secs_f64() * 1000.0
        );
    }

    #[test]
    fn whatwg_labels_are_sorted_and_all_resolve() {
        assert!(LABELS.windows(2).all(|w| w[0].0 < w[1].0));
        for (label, kind) in LABELS {
            let got = Encoding::lookup(&label.to_ascii_uppercase());
            match kind {
                Label::Unsupported(name) => assert_eq!(got, Err(LabelError::Unsupported(name))),
                _ => assert!(got.is_ok(), "{label}"),
            }
        }
        assert_eq!(Encoding::lookup("klingon"), Err(LabelError::Unknown));
        assert_eq!(
            Encoding::lookup("sjis"),
            Err(LabelError::Unsupported("Shift_JIS"))
        );
    }

    #[test]
    fn whatwg_labels_keep_the_names_dusage_already_took() {
        assert_eq!(Encoding::lookup("latin1"), Ok(Encoding::Latin1));
        assert_eq!(Encoding::lookup("iso-8859-1"), Ok(Encoding::Latin1));
        assert_eq!(Encoding::lookup("unicode"), Ok(Encoding::Utf16Le));
        assert_eq!(Encoding::lookup("unicodefffe"), Ok(Encoding::Utf16Be));
        assert_eq!(Encoding::lookup("unicode11utf8"), Ok(Encoding::Utf8));
        assert_eq!(Encoding::lookup("replacement"), Ok(Encoding::Replacement));
        for label in ["ascii", "cp1252", "l1", "windows-1252"] {
            assert_eq!(Encoding::lookup(label).unwrap().name(), "windows-1252");
        }
        assert_eq!(Encoding::lookup("latin2").unwrap().name(), "iso-8859-2");
        assert_eq!(Encoding::lookup("visual").unwrap().name(), "iso-8859-8");
        assert_eq!(Encoding::lookup("logical").unwrap().name(), "iso-8859-8-i");
        for (name, _) in SINGLE_BYTE.iter() {
            assert_eq!(Encoding::lookup(name).unwrap().name(), *name);
        }
    }

    #[test]
    fn single_byte_tables_decode_known_bytes() {
        let dec = |label: &str, bytes: &[u8]| {
            String::from_utf8(to_utf8(bytes, Encoding::lookup(label).unwrap())).unwrap()
        };
        assert_eq!(
            dec("windows-1252", b"\x80 caf\xE9 \x93x\x94"),
            "\u{20AC} café \u{201C}x\u{201D}"
        );
        assert_eq!(dec("windows-1251", b"\xCF\xF0\xE8\xE2\xE5\xF2"), "Привет");
        assert_eq!(dec("koi8-r", b"\xF0\xD2\xC9\xD7\xC5\xD4"), "Привет");
        assert_eq!(dec("cp866", b"\x8F\xE0\xA8\xA2\xA5\xE2"), "Привет");
        assert_eq!(dec("iso-8859-2", b"\xA3\xF3d\xBC"), "Łódź");
        assert_eq!(dec("iso-8859-7", b"\xE1\xE2\xE3"), "αβγ");
        assert_eq!(dec("iso-8859-15", b"\xA4"), "€");
        assert_eq!(dec("windows-1255", b"\xF9\xEC\xE5\xED"), "שלום");
        assert_eq!(dec("windows-874", b"\xA1\xA2"), "กข");
        assert_eq!(dec("macintosh", b"caf\x8E"), "café");
        assert_eq!(dec("x-user-defined", b"a\x80\xFF"), "a\u{F780}\u{F7FF}");
        // An unmapped byte is one U+FFFD, and no single-byte encoding has a BOM to strip.
        assert_eq!(dec("windows-1253", b"\xAA"), "\u{FFFD}");
        assert_eq!(dec("windows-1252", b"\xEF\xBB\xBFa"), "ï»¿a");
        assert_eq!(dec("replacement", b"anything"), "\u{FFFD}");
        assert_eq!(dec("replacement", b""), "");
        for (t, (name, _)) in SINGLE_BYTE.iter().enumerate() {
            let all: Vec<u8> = (0..=255).collect();
            let out = to_utf8(&all, Encoding::SingleByte(t as u8));
            assert!(std::str::from_utf8(&out).is_ok(), "{name}");
            assert_eq!(&out[..128], &all[..128]);
        }
    }
}
