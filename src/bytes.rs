//! Fast byte scanning: memmem-style substring search, ASCII case folding, word boundaries.
//!
//! Every search is a safe scalar scan over native-endian `u64` words: the first needle byte is
//! broadcast across a word, the candidate words are found with the classic has-zero-byte trick
//! after the XOR, and interior bytes are only compared once a word holds a candidate. Long
//! needles test their last byte before the rest, so a stray first-byte hit rarely reaches the
//! middle comparison. No `unsafe`, no dependencies, no allocations.

const LO: u64 = 0x0101_0101_0101_0101;
const HI: u64 = 0x8080_8080_8080_8080;
const M7: u64 = 0x7F7F_7F7F_7F7F_7F7F;

const fn lower_table() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = if i >= b'A' as usize && i <= b'Z' as usize {
            i as u8 + 32
        } else {
            i as u8
        };
        i += 1;
    }
    t
}

/// ASCII lowercase for every byte value; bytes outside `A-Z` map to themselves, so `0xC1` and
/// `0xE1` stay distinct. A plain `| 0x20` would fold `@`/`` ` `` and those Latin-1 letters
/// together, which is why the table exists.
const LOWER: [u8; 256] = lower_table();

#[inline]
fn broadcast(b: u8) -> u64 {
    u64::from(b) * LO
}

/// High bits mark the zero bytes of `x`. A borrow out of a zero byte also flags the byte above
/// it, so this is never used to locate a byte, only to answer "any zero here?" once OR-ed
/// across a block: the flag then still implies that some byte of that block is zero.
#[inline]
fn zero_flags(x: u64) -> u64 {
    x.wrapping_sub(LO) & !x
}

#[inline]
fn has_zero(x: u64) -> bool {
    zero_flags(x) & HI != 0
}

/// One high bit per zero byte of `x`, exactly (no borrow artefacts), for counting.
#[inline]
fn zero_bytes(x: u64) -> u64 {
    !(((x & M7).wrapping_add(M7)) | x) & HI
}

#[inline]
fn load8(hay: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(hay[at..at + 8].try_into().unwrap())
}

/// Case rule for byte comparisons: `Exact` or `Ci`.
trait Case {
    /// Broadcasts of the needle byte that a matching haystack byte may equal: one for a
    /// non-letter, both the lower and the upper form for a letter under `Ci`.
    fn targets(b: u8) -> (u64, u64);
    /// Zero flags (high bits) for the bytes of `chunk` that match. Several flags from one block
    /// can be OR-ed and tested once, which is what keeps the scan loop branch-light.
    fn flags(chunk: u64, targets: (u64, u64)) -> u64;
    fn eq(a: u8, b: u8) -> bool;
}

struct Exact;

impl Case for Exact {
    #[inline]
    fn targets(b: u8) -> (u64, u64) {
        let t = broadcast(b);
        (t, t)
    }

    #[inline]
    fn flags(chunk: u64, targets: (u64, u64)) -> u64 {
        zero_flags(chunk ^ targets.0)
    }

    #[inline]
    fn eq(a: u8, b: u8) -> bool {
        a == b
    }
}

struct Ci;

impl Case for Ci {
    #[inline]
    fn targets(b: u8) -> (u64, u64) {
        let lo = LOWER[b as usize];
        let up = if b.is_ascii_alphabetic() {
            lo & !0x20
        } else {
            lo
        };
        (broadcast(lo), broadcast(up))
    }

    #[inline]
    fn flags(chunk: u64, targets: (u64, u64)) -> u64 {
        let lo = zero_flags(chunk ^ targets.0);
        if targets.0 == targets.1 {
            lo
        } else {
            lo | zero_flags(chunk ^ targets.1)
        }
    }

    #[inline]
    fn eq(a: u8, b: u8) -> bool {
        LOWER[a as usize] == LOWER[b as usize]
    }
}

/// First index in `from..=last` holding a byte that matches `byte` under `C`.
fn find_candidate<C: Case>(hay: &[u8], byte: u8, from: usize, last: usize) -> Option<usize> {
    if from > last || last >= hay.len() {
        return None;
    }
    let targets = C::targets(byte);
    let mut i = from;
    while i + 32 <= last + 1 {
        let flags = C::flags(load8(hay, i), targets)
            | C::flags(load8(hay, i + 8), targets)
            | C::flags(load8(hay, i + 16), targets)
            | C::flags(load8(hay, i + 24), targets);
        if flags & HI != 0 {
            for (k, &h) in hay[i..i + 32].iter().enumerate() {
                if C::eq(h, byte) {
                    return Some(i + k);
                }
            }
        }
        i += 32;
    }
    while i + 8 <= last + 1 {
        if C::flags(load8(hay, i), targets) & HI != 0 {
            for (k, &h) in hay[i..i + 8].iter().enumerate() {
                if C::eq(h, byte) {
                    return Some(i + k);
                }
            }
        }
        i += 8;
    }
    while i <= last {
        if C::eq(hay[i], byte) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Whether `needle` occurs at `pos` in `hay`. The caller guarantees `pos + needle.len()` is in bounds.
#[inline]
fn verify<C: Case>(hay: &[u8], needle: &[u8], pos: usize) -> bool {
    let n = needle.len();
    match n {
        0 | 1 => true,
        2 => C::eq(hay[pos + 1], needle[1]),
        3 => C::eq(hay[pos + 2], needle[2]) && C::eq(hay[pos + 1], needle[1]),
        _ => {
            // Tail first: it rejects most false first-byte hits without touching the interior.
            if !C::eq(hay[pos + n - 1], needle[n - 1]) {
                return false;
            }
            (1..n - 1).all(|k| C::eq(hay[pos + k], needle[k]))
        }
    }
}

fn search<C: Case>(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from > hay.len() {
        return None;
    }
    let n = needle.len();
    if n == 0 {
        return Some(from);
    }
    if n > hay.len() - from {
        return None;
    }
    if n == 1 {
        return find_candidate::<C>(hay, needle[0], from, hay.len() - 1);
    }
    let last = hay.len() - n;
    let mut at = from;
    loop {
        let p = find_candidate::<C>(hay, needle[0], at, last)?;
        if verify::<C>(hay, needle, p) {
            return Some(p);
        }
        at = p + 1;
    }
}

/// First occurrence of needle in hay.
#[inline]
pub fn memmem(hay: &[u8], needle: &[u8]) -> Option<usize> {
    search::<Exact>(hay, needle, 0)
}

/// First occurrence at or after `from`.
#[inline]
pub fn memmem_from(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    search::<Exact>(hay, needle, from)
}

/// ASCII case-insensitive variant (A-Z folded, everything else exact).
#[inline]
pub fn memmem_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    search::<Ci>(hay, needle, 0)
}

/// First case-insensitive occurrence at or after `from`.
#[inline]
pub fn memmem_ci_from(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    search::<Ci>(hay, needle, from)
}

/// First occurrence of needle at or after `from` where the bytes before start and after end
/// are not word bytes (`[0-9A-Za-z_]`).
pub fn memmem_word(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let n = needle.len();
    let mut at = from;
    while let Some(p) = memmem_from(hay, needle, at) {
        if at_word_boundary(hay, p, p + n) {
            return Some(p);
        }
        at = p + 1;
    }
    None
}

/// True when both ends of hay[start..end] sit on word boundaries.
pub fn at_word_boundary(hay: &[u8], start: usize, end: usize) -> bool {
    if start > end || end > hay.len() {
        return false;
    }
    let left = start == 0 || !is_word_byte(hay[start - 1]);
    let right = end == hay.len() || !is_word_byte(hay[end]);
    left && right
}

/// A byte that can be part of a word. Every byte of a multi-byte character counts, so a
/// non-ASCII letter is a letter to `-w`: `-w caf` does not match inside `café`, matching what
/// `rg` and a UTF-8 `grep` do.
#[inline]
pub fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Number of non-overlapping occurrences of `needle` in `hay`; zero for an empty needle.
pub fn count_matches(hay: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut n = 0;
    let mut at = 0;
    while let Some(p) = memmem_from(hay, needle, at) {
        n += 1;
        at = p + needle.len();
    }
    n
}

/// First `b` in hay at or after `from`.
pub fn find_byte(hay: &[u8], b: u8, from: usize) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    find_candidate::<Exact>(hay, b, from, hay.len() - 1)
}

/// Last `b` strictly before `before` (`before` is clamped to `hay.len()`).
pub fn rfind_byte(hay: &[u8], b: u8, before: usize) -> Option<usize> {
    let end = before.min(hay.len());
    let target = broadcast(b);
    let mut i = end;
    while i >= 32 {
        let base = i - 32;
        let flags = zero_flags(load8(hay, base) ^ target)
            | zero_flags(load8(hay, base + 8) ^ target)
            | zero_flags(load8(hay, base + 16) ^ target)
            | zero_flags(load8(hay, base + 24) ^ target);
        if flags & HI != 0 {
            if let Some(k) = hay[base..i].iter().rposition(|&h| h == b) {
                return Some(base + k);
            }
        }
        i = base;
    }
    while i >= 8 {
        let base = i - 8;
        if has_zero(load8(hay, base) ^ target) {
            if let Some(k) = hay[base..i].iter().rposition(|&h| h == b) {
                return Some(base + k);
            }
        }
        i = base;
    }
    while i > 0 {
        i -= 1;
        if hay[i] == b {
            return Some(i);
        }
    }
    None
}

/// Number of `b` bytes in hay.
pub fn count_byte(hay: &[u8], b: u8) -> usize {
    let target = broadcast(b);
    let mut n = 0;
    let mut i = 0;
    while i + 8 <= hay.len() {
        n += zero_bytes(load8(hay, i) ^ target).count_ones() as usize;
        i += 8;
    }
    n + hay[i..].iter().filter(|&&h| h == b).count()
}

/// True when the buffer looks binary: a NUL byte in the first `scan` bytes (or anywhere if scan is 0).
pub fn is_binary(hay: &[u8], scan: usize) -> bool {
    let end = if scan == 0 {
        hay.len()
    } else {
        scan.min(hay.len())
    };
    find_byte(&hay[..end], 0, 0).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hint::black_box;
    use std::time::Instant;

    struct Lcg(u64);

    impl Lcg {
        fn new(seed: u64) -> Self {
            Lcg(seed)
        }

        fn next_u32(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) as u32
        }

        fn below(&mut self, n: u32) -> u32 {
            self.next_u32() % n
        }
    }

    fn naive(hay: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.len() > hay.len() {
            return None;
        }
        if needle.is_empty() {
            return Some(0);
        }
        hay.windows(needle.len()).position(|w| w == needle)
    }

    fn naive_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.len() > hay.len() {
            return None;
        }
        if needle.is_empty() {
            return Some(0);
        }
        hay.windows(needle.len())
            .position(|w| w.eq_ignore_ascii_case(needle))
    }

    fn naive_count(hay: &[u8], needle: &[u8]) -> usize {
        if needle.is_empty() {
            return 0;
        }
        let mut n = 0;
        let mut at = 0;
        while at + needle.len() <= hay.len() {
            match naive(&hay[at..], needle) {
                Some(p) => {
                    n += 1;
                    at += p + needle.len();
                }
                None => break,
            }
        }
        n
    }

    fn naive_words(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
        if needle.len() > hay.len() || from > hay.len() - needle.len() {
            return None;
        }
        let end = hay.len() - needle.len();
        (from..=end).find(|&p| {
            &hay[p..p + needle.len()] == needle && at_word_boundary(hay, p, p + needle.len())
        })
    }

    #[test]
    fn exact_match_at_start_middle_and_end() {
        assert_eq!(memmem(b"hello world", b"hello"), Some(0));
        assert_eq!(memmem(b"hello world", b" world"), Some(5));
        assert_eq!(memmem(b"hello world", b"world"), Some(6));
        assert_eq!(memmem(b"hello world", b"ld"), Some(9));
        assert_eq!(memmem(b"hello world", b"xyz"), None);
    }

    #[test]
    fn overlapping_candidates_are_rescanned() {
        assert_eq!(memmem(b"aaa", b"aa"), Some(0));
        assert_eq!(memmem(b"ababab", b"abab"), Some(0));
        assert_eq!(memmem_from(b"aaa", b"aa", 1), Some(1));
        assert_eq!(memmem_from(b"ababab", b"abab", 1), Some(2));
    }

    #[test]
    fn empty_needle_and_hay() {
        assert_eq!(memmem(b"", b""), Some(0));
        assert_eq!(memmem(b"abc", b""), Some(0));
        assert_eq!(memmem_from(b"abc", b"", 3), Some(3));
        assert_eq!(memmem_from(b"abc", b"", 4), None);
        assert_eq!(memmem(b"", b"a"), None);
        assert_eq!(memmem_from(b"", b"", 0), Some(0));
        assert_eq!(memmem_from(b"", b"", 1), None);
    }

    #[test]
    fn from_beyond_len_and_exact_fit() {
        assert_eq!(memmem_from(b"abcdef", b"def", 3), Some(3));
        assert_eq!(memmem_from(b"abcdef", b"def", 4), None);
        assert_eq!(memmem_from(b"abcdef", b"def", 99), None);
        assert_eq!(memmem(b"abcdef", b"abcdef"), Some(0));
        assert_eq!(memmem(b"abcdef", b"abcdefg"), None);
        assert_eq!(memmem(b"ab", b"abc"), None);
    }

    #[test]
    fn needle_spanning_every_offset() {
        let hay: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        for n in [2usize, 3, 4, 7, 8, 9, 17, 40] {
            for at in 0..=hay.len() - n {
                let needle = &hay[at..at + n];
                assert_eq!(memmem(&hay, needle), Some(at), "len {n} at {at}");
                assert_eq!(memmem_from(&hay, needle, at), Some(at));
                assert_eq!(memmem_from(&hay, needle, at + 1), None);
            }
        }
    }

    #[test]
    fn near_miss_needles_are_rejected() {
        assert_eq!(memmem(b"the quick brown", b"the quick bror"), None);
        assert_eq!(memmem(b"the quick brown", b"thf quick brown"), None);
        assert_eq!(memmem(b"the quick brown", b"the quick brownx"), None);
        assert_eq!(memmem(b"the quick brownX", b"the quick brown"), Some(0));
    }

    #[test]
    fn long_haystack_of_one_byte() {
        let hay = vec![b'a'; 10_000];
        assert_eq!(memmem(&hay, b"a"), Some(0));
        assert_eq!(memmem(&hay, b"aaa"), Some(0));
        assert_eq!(memmem_from(&hay, b"aaa", 9_997), Some(9_997));
        assert_eq!(memmem_from(&hay, b"aaa", 9_998), None);
        assert_eq!(memmem(&hay, b"b"), None);
        assert_eq!(count_matches(&hay, b"aa"), 5_000);
        assert_eq!(find_byte(&hay, b'a', 9_999), Some(9_999));
        assert_eq!(rfind_byte(&hay, b'a', 10_000), Some(9_999));
        assert_eq!(count_byte(&hay, b'a'), 10_000);
    }

    #[test]
    fn case_insensitive_mixed_ascii() {
        assert_eq!(memmem_ci(b"Hello World", b"hello"), Some(0));
        assert_eq!(memmem_ci(b"hello world", b"WORLD"), Some(6));
        assert_eq!(memmem_ci(b"HeLLo WoRLD", b"hEllO wOrld"), Some(0));
        assert_eq!(memmem_ci(b"abc", b"ABD"), None);
        assert_eq!(memmem_ci(b"abc", b"c"), Some(2));
        assert_eq!(memmem_ci_from(b"aXbXc", b"x", 2), Some(3));
        assert_eq!(memmem_ci_from(b"aXbXc", b"x", 4), None);
    }

    #[test]
    fn case_insensitive_length_edges() {
        assert_eq!(memmem_ci(b"abc", b""), Some(0));
        assert_eq!(memmem_ci(b"", b"a"), None);
        assert_eq!(memmem_ci(b"", b""), Some(0));
        assert_eq!(memmem_ci(b"sc", b"SC"), Some(0));
        assert_eq!(memmem_ci(b"abcdefghij", b"GH"), Some(6));
    }

    #[test]
    fn non_ascii_bytes_are_not_folded() {
        // 'Á' (0xC1) vs 'á' (0xE1): a `| 0x20` fold would wrongly equate them.
        assert_eq!(memmem_ci(&[0xC1, 0xE1], &[0xE1]), Some(1));
        assert_eq!(memmem_ci(&[0xC1], &[0xE1]), None);
        assert_eq!(memmem_ci(&[0x40], &[0x60]), None);
        assert_eq!(memmem_ci("Ärger".as_bytes(), "ärger".as_bytes()), None);
        assert_eq!(memmem_ci("ärger".as_bytes(), "Ärger".as_bytes()), None);
        assert_eq!(memmem_ci("ärger".as_bytes(), "ÄRGER".as_bytes()), None);
        assert_eq!(memmem_ci("Ärger".as_bytes(), "ÄRGER".as_bytes()), Some(0));
    }

    #[test]
    fn word_boundary_filtering() {
        assert_eq!(memmem_word(b"concatenate", b"cat", 0), None);
        assert_eq!(memmem_word(b"a cat.", b"cat", 0), Some(2));
        assert_eq!(memmem_word(b"cat", b"cat", 0), Some(0));
        assert_eq!(memmem_word(b"_cat", b"cat", 0), None);
        assert_eq!(memmem_word(b"cat_", b"cat", 0), None);
        assert_eq!(memmem_word(b"9cat", b"cat", 0), None);
        assert_eq!(memmem_word(b"-cat-", b"cat", 0), Some(1));
        assert_eq!(memmem_word(b"cat cats cat", b"cat", 1), Some(9));
        assert_eq!(memmem_word(b"cats cat", b"cat", 0), Some(5));
        assert_eq!(
            memmem_word(b"concatenate cat concatenate", b"cat", 0),
            Some(12)
        );
    }

    #[test]
    fn word_boundary_edges() {
        assert!(at_word_boundary(b"cat", 0, 3));
        assert!(at_word_boundary(b"a cat.", 2, 5));
        assert!(!at_word_boundary(b"scat", 1, 4));
        assert!(!at_word_boundary(b"cats", 0, 3));
        assert!(!at_word_boundary(b"cat_", 0, 3));
        assert!(!at_word_boundary(b"abc", 0, 0));
        assert!(at_word_boundary(b" abc", 0, 0));
        assert!(!at_word_boundary(b"abc", 4, 4));
        assert!(!at_word_boundary(b"abc", 2, 1));
        assert!(at_word_boundary(b"", 0, 0));
        assert!(!at_word_boundary(b"a_b", 1, 2));
        assert!(at_word_boundary(b"a-b", 2, 3));
    }

    #[test]
    fn word_byte_class() {
        assert!(is_word_byte(b'a'));
        assert!(is_word_byte(b'Z'));
        assert!(is_word_byte(b'0'));
        assert!(is_word_byte(b'9'));
        assert!(is_word_byte(b'_'));
        assert!(!is_word_byte(b'-'));
        assert!(!is_word_byte(b' '));
        assert!(!is_word_byte(b'.'));
        assert!(is_word_byte(0x80), "non-ASCII bytes belong to words");
        assert!(is_word_byte(0xC3));
        assert!(!is_word_byte(b','));
    }

    #[test]
    fn count_matches_non_overlapping() {
        assert_eq!(count_matches(b"aaaaa", b"aaa"), 1);
        assert_eq!(count_matches(b"aaaaaa", b"aaa"), 2);
        assert_eq!(count_matches(b"abababa", b"aba"), 2);
        assert_eq!(count_matches(b"abcabcabc", b"abc"), 3);
        assert_eq!(count_matches(b"abc", b"abcd"), 0);
        assert_eq!(count_matches(b"", b"a"), 0);
        assert_eq!(count_matches(b"abc", b""), 0);
        assert_eq!(count_matches(b"x", b"x"), 1);
    }

    #[test]
    fn find_byte_positions() {
        assert_eq!(find_byte(b"hello", b'h', 0), Some(0));
        assert_eq!(find_byte(b"hello", b'l', 0), Some(2));
        assert_eq!(find_byte(b"hello", b'l', 3), Some(3));
        assert_eq!(find_byte(b"hello", b'o', 4), Some(4));
        assert_eq!(find_byte(b"hello", b'o', 5), None);
        assert_eq!(find_byte(b"hello", b'z', 0), None);
        assert_eq!(find_byte(b"", b'a', 0), None);
        assert_eq!(find_byte(b"hello", b'o', 99), None);
    }

    #[test]
    fn find_byte_across_word_loads() {
        let hay: Vec<u8> = (0..100u32).map(|i| i as u8).collect();
        for at in 0..hay.len() {
            assert_eq!(find_byte(&hay, hay[at], 0), Some(at));
        }
        for at in 0..hay.len() {
            assert_eq!(rfind_byte(&hay, hay[at], hay.len()), Some(at));
        }
    }

    #[test]
    fn rfind_byte_positions() {
        assert_eq!(rfind_byte(b"hello", b'l', 5), Some(3));
        assert_eq!(rfind_byte(b"hello", b'l', 3), Some(2));
        assert_eq!(rfind_byte(b"hello", b'h', 1), Some(0));
        assert_eq!(rfind_byte(b"hello", b'h', 0), None);
        assert_eq!(rfind_byte(b"hello", b'o', 4), None);
        assert_eq!(rfind_byte(b"hello", b'o', 99), Some(4));
        assert_eq!(rfind_byte(b"", b'a', 0), None);
        assert_eq!(rfind_byte(b"", b'a', 10), None);
        assert_eq!(rfind_byte(b"aaaa", b'a', 3), Some(2));
    }

    #[test]
    fn rfind_byte_across_word_loads() {
        let hay: Vec<u8> = (0..100u8).map(|i| b'a' + i % 26).collect();
        for end in 1..=hay.len() {
            for &b in b"azm" {
                let want = hay[..end].iter().rposition(|&h| h == b);
                assert_eq!(rfind_byte(&hay, b, end), want, "end {end} byte {b}");
            }
        }
    }

    #[test]
    fn count_byte_edges() {
        assert_eq!(count_byte(b"", b'a'), 0);
        assert_eq!(count_byte(b"aaa", b'a'), 3);
        assert_eq!(count_byte(b"abab", b'a'), 2);
        assert_eq!(count_byte(b"abab", b'z'), 0);
        assert_eq!(count_byte(b"a\0a\0\0", 0), 3);
        let hay: Vec<u8> = (0..1000u32).map(|i| (i % 256) as u8).collect();
        for b in 0..=255u8 {
            let want = hay.iter().filter(|&&h| h == b).count();
            assert_eq!(count_byte(&hay, b), want, "byte {b}");
        }
    }

    #[test]
    fn binary_detection_scan_limits() {
        assert!(!is_binary(b"hello", 0));
        assert!(!is_binary(b"hello", 5));
        assert!(is_binary(b"hel\0lo", 0));
        assert!(is_binary(b"hel\0lo", 4));
        assert!(!is_binary(b"hel\0lo", 3));
        assert!(is_binary(b"\0", 1));
        assert!(!is_binary(b"", 0));
        assert!(!is_binary(b"", 10));
        assert!(is_binary(&[0u8; 64], 0));
        assert!(!is_binary(&[b'a'; 64], 3));
        assert!(is_binary(b"aaa\0", 100));
    }

    #[test]
    fn no_panic_on_any_small_input() {
        let hays: [&[u8]; 6] = [b"", b"a", b"ab", b"ba", b"aab", b"abab"];
        let needles: [&[u8]; 5] = [b"", b"a", b"b", b"ab", b"baa"];
        for hay in hays {
            for needle in needles {
                for from in 0..hay.len() + 3 {
                    let _ = memmem_from(hay, needle, from);
                    let _ = memmem_ci_from(hay, needle, from);
                    let _ = memmem_word(hay, needle, from);
                }
                let _ = memmem(hay, needle);
                let _ = memmem_ci(hay, needle);
                let _ = count_matches(hay, needle);
                for before in 0..hay.len() + 3 {
                    let _ = rfind_byte(hay, b'a', before);
                    let _ = find_byte(hay, b'a', before);
                }
                for scan in [0usize, 1, 2, 7, 99] {
                    let _ = is_binary(hay, scan);
                }
            }
        }
    }

    /// Compare every entry point against the naive reference on one pair.
    fn check_pair(hay: &[u8], needle: &[u8], froms: &[usize]) {
        assert_eq!(
            memmem(hay, needle),
            naive(hay, needle),
            "hay {hay:?} needle {needle:?}"
        );
        assert_eq!(
            count_matches(hay, needle),
            naive_count(hay, needle),
            "count hay {hay:?} needle {needle:?}"
        );
        assert_eq!(memmem_ci(hay, needle), naive_ci(hay, needle));
        for &from in froms {
            let want = if from > hay.len() {
                None
            } else {
                naive(&hay[from..], needle).map(|p| from + p)
            };
            assert_eq!(
                memmem_from(hay, needle, from),
                want,
                "from {from} hay {hay:?} needle {needle:?}"
            );
            assert_eq!(
                memmem_word(hay, needle, from),
                naive_words(hay, needle, from),
                "word from {from} hay {hay:?} needle {needle:?}"
            );
        }
    }

    #[test]
    fn no_panic_on_random_byte_soup() {
        let mut rng = Lcg::new(0xB16B_00B5);
        for _ in 0..500 {
            let n = rng.below(200) as usize;
            let hay: Vec<u8> = (0..n).map(|_| rng.below(256) as u8).collect();
            let m = rng.below(70) as usize;
            let needle: Vec<u8> = (0..m).map(|_| rng.below(256) as u8).collect();
            let from = rng.below(300) as usize;
            let before = rng.below(300) as usize;
            let scan = rng.below(300) as usize;
            let _ = memmem(&hay, &needle);
            let _ = memmem_from(&hay, &needle, from);
            let _ = memmem_ci(&hay, &needle);
            let _ = memmem_ci_from(&hay, &needle, from);
            let _ = memmem_word(&hay, &needle, from);
            let _ = count_matches(&hay, &needle);
            let _ = at_word_boundary(&hay, from, before);
            let _ = find_byte(&hay, rng.below(256) as u8, from);
            let _ = rfind_byte(&hay, rng.below(256) as u8, before);
            let _ = count_byte(&hay, rng.below(256) as u8);
            let _ = is_binary(&hay, scan);
        }
    }

    #[test]
    fn agrees_with_naive_reference_exhaustively() {
        let mut hays: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"a".to_vec(),
            b"b".to_vec(),
            b"ab".to_vec(),
            b"ba".to_vec(),
            b"aab".to_vec(),
            b"abab".to_vec(),
            b"babababab".to_vec(),
        ];
        let mut rng = Lcg::new(0x5EED_1234);
        for _ in 0..200 {
            let n = rng.below(49) as usize;
            hays.push(
                (0..n)
                    .map(|_| if rng.below(2) == 0 { b'a' } else { b'b' })
                    .collect(),
            );
        }
        let mut needles: Vec<Vec<u8>> = Vec::new();
        for len in 0..=8usize {
            for bits in 0u32..(1 << len) {
                needles.push(
                    (0..len)
                        .map(|k| if bits >> k & 1 == 0 { b'a' } else { b'b' })
                        .collect(),
                );
            }
        }
        for hay in &hays {
            // Every offset on short haystacks; a sample above that, because each `from`
            // re-scans the buffer and a debug build makes that the dominant cost.
            let froms: Vec<usize> = if hay.len() <= 8 {
                (0..hay.len() + 3).collect()
            } else {
                vec![
                    0,
                    1,
                    2,
                    hay.len() / 2,
                    hay.len() - 1,
                    hay.len(),
                    hay.len() + 2,
                ]
            };
            for needle in &needles {
                check_pair(hay, needle, &froms);
            }
        }

        // Repeated-byte haystacks with one pattern per length and run shape.
        let big: [Vec<u8>; 3] = [
            vec![b'a'; 10_000],
            vec![b'b'; 10_000],
            (0..5_000)
                .map(|i| if i % 3 == 0 { b'a' } else { b'b' })
                .collect(),
        ];
        let shapes: [&[u8]; 3] = [b"a", b"b", b"ab"];
        let mut big_needles: Vec<Vec<u8>> = Vec::new();
        for len in 0..=8usize {
            for shape in shapes {
                big_needles.push((0..len).map(|k| shape[k % shape.len()]).collect());
            }
        }
        for hay in &big {
            let froms = [0, 1, hay.len() / 2, hay.len() - 1];
            for needle in &big_needles {
                check_pair(hay, needle, &froms);
            }
        }
    }

    #[test]
    fn case_insensitive_agrees_with_naive_exhaustively() {
        let alphabet = *b"aAbB";
        let mut hays: Vec<Vec<u8>> = vec![Vec::new(), b"aA".to_vec(), b"Bb".to_vec()];
        let mut rng = Lcg::new(0xC0FF_EE00);
        for _ in 0..60 {
            let n = rng.below(24) as usize;
            hays.push((0..n).map(|_| alphabet[rng.below(4) as usize]).collect());
        }
        let mut needles: Vec<Vec<u8>> = Vec::new();
        for len in 0..=4usize {
            for mut bits in 0u32..(1u32 << (2 * len)) {
                let mut n = Vec::with_capacity(len);
                for _ in 0..len {
                    n.push(alphabet[(bits & 3) as usize]);
                    bits >>= 2;
                }
                needles.push(n);
            }
        }
        for hay in &hays {
            for needle in &needles {
                assert_eq!(
                    memmem_ci(hay, needle),
                    naive_ci(hay, needle),
                    "hay {hay:?} needle {needle:?}"
                );
                for from in 0..=hay.len() + 1 {
                    let want = if from > hay.len() {
                        None
                    } else {
                        naive_ci(&hay[from..], needle).map(|p| from + p)
                    };
                    assert_eq!(memmem_ci_from(hay, needle, from), want);
                }
            }
        }
    }

    const WORDS: [&str; 24] = [
        "the", "of", "and", "to", "in", "that", "she", "he", "it", "was", "for", "on", "are", "as",
        "with", "his", "they", "at", "be", "this", "have", "from", "one", "had",
    ];

    fn english_like(len: usize, seed: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 32);
        let mut rng = Lcg::new(seed);
        while out.len() < len {
            if !out.is_empty() {
                match rng.below(24) {
                    0 => out.push(b','),
                    1 => out.push(b'\n'),
                    _ => out.push(b' '),
                }
            }
            let w = WORDS[rng.below(WORDS.len() as u32) as usize];
            out.extend_from_slice(w.as_bytes());
            if rng.below(12) == 0 {
                out.push(b'.');
            }
        }
        out.truncate(len);
        out
    }

    fn best_millis(iterations: u32, mut f: impl FnMut() -> usize) -> f64 {
        let mut best = f64::INFINITY;
        for _ in 0..iterations {
            let start = Instant::now();
            black_box(f());
            let ms = start.elapsed().as_secs_f64() * 1e3;
            if ms < best {
                best = ms;
            }
        }
        best
    }

    #[test]
    fn bench_memmem_vs_naive() {
        const HAY_LEN: usize = 4 << 20;
        let hay = english_like(HAY_LEN, 0xDEAD_BEEF_CAFE);
        let long = b"q the quick brown fox jumps over the lazy dog and then";
        assert!(long.len() >= 40);
        // The word list has no 'q', so the 'q'-headed needles are absent: both sides scan the
        // whole haystack and the timing measures the sweep, not an early exit.
        assert_eq!(count_byte(&hay, b'q'), 0);
        let cases: [(&str, &[u8]); 5] = [
            ("common byte", b"e"),
            ("absent byte", b"q"),
            ("4", &long[..4]),
            ("16", &long[..16]),
            ("40", &long[..40]),
        ];
        assert!(cases[2..].iter().all(|(_, n)| memmem(&hay, n).is_none()));

        let iterations = if cfg!(debug_assertions) { 1 } else { 5 };
        eprintln!(
            "\nmemmem vs hay.windows(n).position() on {} MiB of English-like text (best of {iterations})",
            HAY_LEN >> 20
        );
        eprintln!(
            "{:<12} {:>4} {:>12} {:>12} {:>9}",
            "needle", "len", "naive us", "memmem us", "speedup"
        );

        for (label, needle) in cases {
            assert_eq!(memmem(&hay, needle), naive(&hay, needle));
            let naive_us = best_millis(iterations, || {
                naive(black_box(&hay), black_box(needle)).unwrap_or(usize::MAX)
            }) * 1e3;
            let fast_us = best_millis(iterations, || {
                memmem(black_box(&hay), black_box(needle)).unwrap_or(usize::MAX)
            }) * 1e3;
            let speedup = naive_us / fast_us;
            eprintln!(
                "{:<12} {:>4} {:>12.1} {:>12.1} {:>8.1}x",
                label,
                needle.len(),
                naive_us,
                fast_us,
                speedup
            );
            if !cfg!(debug_assertions) && needle.len() >= 4 {
                assert!(
                    speedup >= 1.5,
                    "len {} only {speedup:.2}x faster than the naive reference",
                    needle.len()
                );
            }
        }
    }
}
