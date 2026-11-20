//! One-byte search over the machine's vector registers.
//!
//! The SWAR loops in [`crate::bytes`] run at roughly two cycles a byte; a 32-byte AVX2
//! compare runs at about a tenth of that, which on a tree of files that never match is the
//! difference between reading at 1.7 GB/s and at ripgrep's 6 GB/s. Everything here answers
//! exactly what `hay[from..].iter().position(|&b| b == needle)` answers, only faster.
//!
//! [`memchr`], [`memchr2`], [`memchr_ci`] and [`count`] answer for one byte (or two), and
//! [`Prefilter`] for a small set of needles: the positions a search jumps between when it has a
//! literal every match must contain, one per candidate run a regex can offer.
//!
//! This is the one module outside `src/unix` and `src/win` allowed to use `unsafe`, and it
//! uses it for two reasons only: the x86 intrinsics, which are `unsafe` because they read past
//! the end of a vector the CPU does not bounds-check, and the vector loads themselves, which
//! are guarded by an explicit length check on every iteration. The feature level is detected
//! once per process with `is_x86_feature_detected!`, so no instruction is executed on a CPU
//! that does not advertise it, and every path falls back to the portable loop.

use std::sync::OnceLock;

/// The widest loop this CPU can run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Portable,
    #[cfg(target_arch = "x86_64")]
    Sse2,
    #[cfg(target_arch = "x86_64")]
    Avx2,
}

fn level() -> Level {
    static LEVEL: OnceLock<Level> = OnceLock::new();
    *LEVEL.get_or_init(detect)
}

#[cfg(target_arch = "x86_64")]
fn detect() -> Level {
    if std::is_x86_feature_detected!("avx2") {
        Level::Avx2
    } else if std::is_x86_feature_detected!("sse2") {
        Level::Sse2
    } else {
        Level::Portable
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn detect() -> Level {
    Level::Portable
}

/// The first `needle` in `hay` at or after `from`.
#[inline]
pub fn memchr(hay: &[u8], needle: u8, from: usize) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    #[cfg(target_arch = "x86_64")]
    match level() {
        // SAFETY: both take the haystack length and check every 32- or 16-byte load against
        // it before issuing the load, and the feature they need was detected at startup.
        Level::Avx2 => return unsafe { memchr_avx2(hay, needle, from) },
        Level::Sse2 => return unsafe { memchr_sse2(hay, needle, from) },
        Level::Portable => {}
    }
    portable(hay, needle, from)
}

/// Needles one [`Prefilter`] looks for at once. A pattern with more alternation branches than
/// this keeps the single mandatory run instead, since every branch's run would cost a vector
/// compare per block for a candidate set that is no longer selective.
pub const MAX_PREFILTER: usize = 4;

/// Needle length from which the scan may also compare the second byte. A two-byte needle is
/// already cheap to verify, so the extra compare would cost more than the candidates it removes.
const PAIR_MIN: usize = 3;

/// Bytes sampled from the start of a haystack to decide which needles earn a second-byte
/// compare. One candidate away costs more than the compare that finds it, so a leading byte
/// that shows up in this prefix is worth pairing.
///
/// The decision is made per haystack and never kept. A decision held for the process would be
/// taken from whichever file the walk read first — one 4 KB window of a binary, say, while the
/// tree behind it is source — so the same search would pick a different strategy depending on
/// the order the tree was walked in. Both answers are exact, because the verify decides, so a
/// wrong call here only costs or saves instructions.
const SAMPLE: usize = 64;

/// Occurrences of a needle's leading byte in that sample that make pairing worth it. One is
/// noise — a binary file holds any single byte value somewhere — while two is a byte every
/// [`SAMPLE`] bytes, dense enough that the candidates it finds would each pay a verify.
const PAIR_HITS: usize = 2;

/// A small set of needles scanned in one pass, for a search that jumps between the positions
/// where a match could begin instead of stepping through every byte in between.
///
/// This is the multi-needle form of [`memchr`]: a block is compared against every needle's
/// leading byte and, in the same mask, against its second byte one lane over, so a candidate
/// that fails on its second byte never reaches a verify. Every run a match must contain is a
/// needle here, which is what makes "no needle, no match" true.
#[derive(Debug)]
pub struct Prefilter {
    /// The needles, longest first: at one offset the longest needle says most about the match
    /// that contains it.
    needles: Vec<Vec<u8>>,
}

impl Prefilter {
    /// Prepare `needles`. `None` when there are none, or more than [`MAX_PREFILTER`].
    pub fn new(mut needles: Vec<Vec<u8>>) -> Option<Prefilter> {
        needles.retain(|n| !n.is_empty());
        if needles.is_empty() || needles.len() > MAX_PREFILTER {
            return None;
        }
        needles.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        needles.dedup();
        Some(Prefilter { needles })
    }

    /// Which needles are worth a second-byte compare in this haystack, from its first
    /// [`SAMPLE`] bytes. Bit `i` means needle `i` is compared on both bytes; every form answers
    /// a superset of the needle positions, since the verify step decides.
    ///
    /// A leading byte that shows up once in the sample is not yet worth pairing: the noise of a
    /// binary file holds any single byte value somewhere, and pairing on it costs a compare per
    /// block for candidates that never come. Two hits, a byte every [`SAMPLE`] bytes, is.
    fn decide(&self, hay: &[u8]) -> u8 {
        let sample = &hay[..hay.len().min(SAMPLE)];
        let mut bits = 0u8;
        for (i, n) in self.needles.iter().enumerate() {
            if n.len() >= PAIR_MIN && count(sample, n[0]) >= PAIR_HITS {
                bits |= 1 << i;
            }
        }
        bits
    }

    /// The earliest needle at or after `from`, as its offset and its index in [`Self::needles`].
    #[inline]
    pub fn find_any(&self, hay: &[u8], from: usize) -> Option<(usize, usize)> {
        if from > hay.len() {
            return None;
        }
        let state = self.decide(hay);
        // One needle whose leading byte is rare needs no mask at all: [`memchr`] is the same
        // scan with three fewer operations per block.
        if state & 1 == 0 && self.needles.len() == 1 {
            let mut at = from;
            while let Some(p) = memchr(hay, self.needles[0][0], at) {
                if hay[p..].starts_with(&self.needles[0]) {
                    return Some((p, 0));
                }
                at = p + 1;
            }
            return None;
        }
        #[cfg(target_arch = "x86_64")]
        match level() {
            // SAFETY: both check every load against the haystack length before issuing it, and
            // the feature they need was detected at startup.
            Level::Avx2 => return unsafe { self.find_any_avx2(hay, from, state) },
            Level::Sse2 => return unsafe { self.find_any_sse2(hay, from, state) },
            Level::Portable => {}
        }
        self.portable(hay, from)
    }

    /// The offset of the earliest needle at or after `from`.
    #[inline]
    pub fn find_from(&self, hay: &[u8], from: usize) -> Option<usize> {
        self.find_any(hay, from).map(|(at, _)| at)
    }

    /// The needles, longest first.
    pub fn needles(&self) -> &[Vec<u8>] {
        &self.needles
    }

    /// The first candidate among the byte positions `mask` flags, counting from `base`.
    #[inline]
    fn first_in_mask(&self, hay: &[u8], base: usize, mut mask: u32) -> Option<(usize, usize)> {
        while mask != 0 {
            let at = base + mask.trailing_zeros() as usize;
            if let Some(i) = self.verify(hay, at) {
                return Some((at, i));
            }
            mask &= mask - 1;
        }
        None
    }

    /// The first candidate at or after `from`, a byte per step.
    fn portable(&self, hay: &[u8], from: usize) -> Option<(usize, usize)> {
        (from..hay.len()).find_map(|at| self.verify(hay, at).map(|i| (at, i)))
    }

    /// The index of the longest needle that starts at `at`. The bytes after the leading one are
    /// checked from the back, which rejects a near miss on the first compare.
    #[inline]
    fn verify(&self, hay: &[u8], at: usize) -> Option<usize> {
        self.needles.iter().position(|n| {
            n.len() <= hay.len() - at
                && n.iter()
                    .rev()
                    .zip(hay[at..at + n.len()].iter().rev())
                    .all(|(a, b)| a == b)
        })
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn find_any_avx2(&self, hay: &[u8], from: usize, mode: u8) -> Option<(usize, usize)> {
        use std::arch::x86_64::*;
        let len = hay.len();
        let ptr = hay.as_ptr();
        let mut heads = [_mm256_set1_epi8(0); MAX_PREFILTER];
        let mut nexts = [_mm256_set1_epi8(0); MAX_PREFILTER];
        for (i, n) in self.needles.iter().enumerate() {
            heads[i] = _mm256_set1_epi8(n[0] as i8);
            nexts[i] = _mm256_set1_epi8(n.get(1).copied().unwrap_or(n[0]) as i8);
        }
        let paired: [bool; MAX_PREFILTER] = std::array::from_fn(|k| mode & (1 << k) != 0);
        // A paired mask looks one byte past the block for the second byte, so the next block
        // has to start on that byte rather than after it.
        let step = match paired[..self.needles.len()].iter().any(|&p| p) {
            true => 31,
            false => 32,
        };
        let mut i = from;
        // SAFETY: `i + 32 <= len`, so the 32-byte load stays inside the slice.
        while i + 32 <= len {
            let block = unsafe { _mm256_loadu_si256(ptr.add(i) as *const __m256i) };
            let mut any = 0u32;
            for ((head, next), &pair) in heads
                .iter()
                .zip(nexts.iter())
                .zip(paired.iter())
                .take(self.needles.len())
            {
                let first = _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, *head)) as u32;
                any |= match pair {
                    // The second byte of a candidate at `i + k` is the block's byte at `k + 1`,
                    // so its mask lines up with the leading mask one lane to the left.
                    true => {
                        let second = _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, *next)) as u32;
                        first & (second >> 1)
                    }
                    false => first,
                };
            }
            if any != 0 {
                if let Some(hit) = self.first_in_mask(hay, i, any) {
                    return Some(hit);
                }
            }
            i += step;
        }
        self.portable(hay, i)
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "sse2")]
    unsafe fn find_any_sse2(&self, hay: &[u8], from: usize, mode: u8) -> Option<(usize, usize)> {
        use std::arch::x86_64::*;
        let len = hay.len();
        let ptr = hay.as_ptr();
        let mut heads = [_mm_set1_epi8(0); MAX_PREFILTER];
        let mut nexts = [_mm_set1_epi8(0); MAX_PREFILTER];
        for (i, n) in self.needles.iter().enumerate() {
            heads[i] = _mm_set1_epi8(n[0] as i8);
            nexts[i] = _mm_set1_epi8(n.get(1).copied().unwrap_or(n[0]) as i8);
        }
        let paired: [bool; MAX_PREFILTER] = std::array::from_fn(|k| mode & (1 << k) != 0);
        let step = match paired[..self.needles.len()].iter().any(|&p| p) {
            true => 15,
            false => 16,
        };
        let mut i = from;
        // SAFETY: `i + 16 <= len`, so the 16-byte load stays inside the slice.
        while i + 16 <= len {
            let block = unsafe { _mm_loadu_si128(ptr.add(i) as *const __m128i) };
            let mut any = 0u32;
            for ((head, next), &pair) in heads
                .iter()
                .zip(nexts.iter())
                .zip(paired.iter())
                .take(self.needles.len())
            {
                let first = _mm_movemask_epi8(_mm_cmpeq_epi8(block, *head)) as u32;
                any |= match pair {
                    true => {
                        let second = _mm_movemask_epi8(_mm_cmpeq_epi8(block, *next)) as u32;
                        first & (second >> 1)
                    }
                    false => first,
                };
            }
            if any != 0 {
                if let Some(hit) = self.first_in_mask(hay, i, any) {
                    return Some(hit);
                }
            }
            i += step;
        }
        self.portable(hay, i)
    }
}

/// The first byte equal to `needle` under ASCII case folding, at or after `from`.
///
/// Both sides are folded the same way, so the result is exactly the set of bytes the fold
/// treats as the same letter: the low five bits of `@[\\]^_` move with the low five bits of
/// `` `{|}~ `` exactly as `a` moves with `A`, which is the equivalence the SWAR path already
/// uses and the one `C::eq` answers.
#[inline]
pub fn memchr_ci(hay: &[u8], needle: u8, from: usize) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    #[cfg(target_arch = "x86_64")]
    match level() {
        // SAFETY: as in `memchr`; every load is checked against the length.
        Level::Avx2 => return unsafe { memchr_ci_avx2(hay, needle, from) },
        Level::Sse2 => return unsafe { memchr_ci_sse2(hay, needle, from) },
        Level::Portable => {}
    }
    hay.get(from..)?
        .iter()
        .position(|&b| b | 0x20 == needle | 0x20)
        .map(|i| from + i)
}

/// The first of two `needles` at or after `from`, with the byte found.
#[inline]
pub fn memchr2(hay: &[u8], a: u8, b: u8, from: usize) -> Option<(usize, u8)> {
    if from >= hay.len() || a == b {
        return memchr(hay, a, from).map(|i| (i, a));
    }
    #[cfg(target_arch = "x86_64")]
    match level() {
        // SAFETY: as in `memchr`.
        Level::Avx2 => return unsafe { memchr2_avx2(hay, a, b, from) },
        Level::Sse2 => return unsafe { memchr2_sse2(hay, a, b, from) },
        Level::Portable => {}
    }
    portable2(hay, a, b, from)
}

/// How many `needle` bytes `hay` holds.
#[inline]
pub fn count(hay: &[u8], needle: u8) -> usize {
    #[cfg(target_arch = "x86_64")]
    match level() {
        // SAFETY: as in `memchr`; every load is checked against the length.
        Level::Avx2 => return unsafe { count_avx2(hay, needle) },
        Level::Sse2 => return unsafe { count_sse2(hay, needle) },
        Level::Portable => {}
    }
    hay.iter().filter(|&&b| b == needle).count()
}

fn portable(hay: &[u8], needle: u8, from: usize) -> Option<usize> {
    hay.get(from..)?
        .iter()
        .position(|&b| b == needle)
        .map(|i| from + i)
}

/// True when the loops here run on vector registers on this machine. The portable fallbacks
/// answer the same questions a byte at a time, which for a scan that is only a filter is slower
/// than the word-at-a-time loop the caller already has, so a caller asks before choosing.
pub fn vectorized() -> bool {
    #[cfg(target_arch = "x86_64")]
    return level() != Level::Portable;
    #[cfg(not(target_arch = "x86_64"))]
    false
}

/// The first position at or after `from` where `hay[p + a_off] == a` and `hay[p + b_off] == b`.
///
/// This is the candidate filter of a literal search: one vector load answers both compares, so
/// a position that survives it is one where two needle bytes already line up. Only positions
/// whose bytes both lie inside `hay` are reported, in increasing order and with none skipped,
/// so the caller can compare the window each one implies and reject it without losing a match.
pub fn find_pair(
    hay: &[u8],
    a: u8,
    a_off: usize,
    b: u8,
    b_off: usize,
    from: usize,
) -> Option<usize> {
    let wide = a_off.max(b_off);
    // The last start whose two bytes both lie inside the haystack.
    if from > hay.len().checked_sub(wide + 1)? {
        return None;
    }
    #[cfg(target_arch = "x86_64")]
    match level() {
        // SAFETY: both take the haystack length and check every load against it before issuing
        // it, and the feature they need was detected at startup.
        Level::Avx2 => return unsafe { find_pair_avx2(hay, a, a_off, b, b_off, from) },
        Level::Sse2 => return unsafe { find_pair_sse2(hay, a, a_off, b, b_off, from) },
        Level::Portable => {}
    }
    pair_from(hay, a, a_off, b, b_off, from)
}

/// The pair scan a byte at a time, for a machine with no vector unit and for the block a vector
/// loop leaves at the end of a haystack.
fn pair_from(hay: &[u8], a: u8, a_off: usize, b: u8, b_off: usize, from: usize) -> Option<usize> {
    let end = hay.len().checked_sub(a_off.max(b_off) + 1)?;
    (from..=end).find(|&at| hay[at + a_off] == a && hay[at + b_off] == b)
}

/// The pair scan over 32-byte blocks. Each block loads at the two offsets rather than at the
/// window start, so lane `L` of the block is window start `i + L` for both compares and the
/// block advances by its full width:
///
/// ```text
///   hay:   ... | i + a_off .. i + a_off + 31 |        compare with `a`
///              | i + b_off .. i + b_off + 31 |        compare with `b`
///   lanes:       0 ......... 31                  = window starts i ..= i + 31
/// ```
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn find_pair_avx2(
    hay: &[u8],
    a: u8,
    a_off: usize,
    b: u8,
    b_off: usize,
    from: usize,
) -> Option<usize> {
    use std::arch::x86_64::*;
    let len = hay.len();
    let ptr = hay.as_ptr();
    let wa = _mm256_set1_epi8(a as i8);
    let wb = _mm256_set1_epi8(b as i8);
    let wide = a_off.max(b_off);
    let mut i = from;
    // SAFETY: `i + wide + 32 <= len`, so both 32-byte loads, the later one ending at
    // `i + wide + 32`, stay inside the slice.
    while i + wide + 32 <= len {
        let pa = unsafe { _mm256_loadu_si256(ptr.add(i + a_off) as *const __m256i) };
        let pb = unsafe { _mm256_loadu_si256(ptr.add(i + b_off) as *const __m256i) };
        let cand = _mm256_movemask_epi8(_mm256_and_si256(
            _mm256_cmpeq_epi8(pa, wa),
            _mm256_cmpeq_epi8(pb, wb),
        )) as u32;
        if cand != 0 {
            return Some(i + cand.trailing_zeros() as usize);
        }
        i += 32;
    }
    pair_from(hay, a, a_off, b, b_off, i)
}

/// The pair scan over 16-byte blocks, for the same machine that has no AVX2.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn find_pair_sse2(
    hay: &[u8],
    a: u8,
    a_off: usize,
    b: u8,
    b_off: usize,
    from: usize,
) -> Option<usize> {
    use std::arch::x86_64::*;
    let len = hay.len();
    let ptr = hay.as_ptr();
    let wa = _mm_set1_epi8(a as i8);
    let wb = _mm_set1_epi8(b as i8);
    let wide = a_off.max(b_off);
    let mut i = from;
    // SAFETY: `i + wide + 16 <= len`, so both 16-byte loads stay inside the slice.
    while i + wide + 16 <= len {
        let pa = unsafe { _mm_loadu_si128(ptr.add(i + a_off) as *const __m128i) };
        let pb = unsafe { _mm_loadu_si128(ptr.add(i + b_off) as *const __m128i) };
        let cand = _mm_movemask_epi8(_mm_and_si128(
            _mm_cmpeq_epi8(pa, wa),
            _mm_cmpeq_epi8(pb, wb),
        )) as u32;
        if cand != 0 {
            return Some(i + cand.trailing_zeros() as usize);
        }
        i += 16;
    }
    pair_from(hay, a, a_off, b, b_off, i)
}

fn portable2(hay: &[u8], a: u8, b: u8, from: usize) -> Option<(usize, u8)> {
    hay.get(from..)?
        .iter()
        .position(|&c| c == a || c == b)
        .map(|i| (from + i, hay[from + i]))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn memchr_avx2(hay: &[u8], needle: u8, from: usize) -> Option<usize> {
    use std::arch::x86_64::*;
    let len = hay.len();
    let ptr = hay.as_ptr();
    let want = _mm256_set1_epi8(needle as i8);
    let mut i = from;
    // SAFETY: `i + 32 <= len`, so the 32-byte load stays inside the slice.
    while i + 32 <= len {
        let block = unsafe { _mm256_loadu_si256(ptr.add(i) as *const __m256i) };
        let hits = _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, want)) as u32;
        if hits != 0 {
            return Some(i + hits.trailing_zeros() as usize);
        }
        i += 32;
    }
    portable(hay, needle, i)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn memchr_ci_avx2(hay: &[u8], needle: u8, from: usize) -> Option<usize> {
    use std::arch::x86_64::*;
    let len = hay.len();
    let ptr = hay.as_ptr();
    let want = _mm256_set1_epi8((needle | 0x20) as i8);
    let lower = _mm256_set1_epi8(0x20);
    let mut i = from;
    // SAFETY: `i + 32 <= len`, so the 32-byte load stays inside the slice.
    while i + 32 <= len {
        let block = unsafe { _mm256_loadu_si256(ptr.add(i) as *const __m256i) };
        let folded = _mm256_or_si256(block, lower);
        let hits = _mm256_movemask_epi8(_mm256_cmpeq_epi8(folded, want)) as u32;
        if hits != 0 {
            return Some(i + hits.trailing_zeros() as usize);
        }
        i += 32;
    }
    hay[i..]
        .iter()
        .position(|&b| b | 0x20 == needle | 0x20)
        .map(|k| i + k)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn memchr_ci_sse2(hay: &[u8], needle: u8, from: usize) -> Option<usize> {
    use std::arch::x86_64::*;
    let len = hay.len();
    let ptr = hay.as_ptr();
    let want = _mm_set1_epi8((needle | 0x20) as i8);
    let lower = _mm_set1_epi8(0x20);
    let mut i = from;
    // SAFETY: `i + 16 <= len`, so the 16-byte load stays inside the slice.
    while i + 16 <= len {
        let block = unsafe { _mm_loadu_si128(ptr.add(i) as *const __m128i) };
        let folded = _mm_or_si128(block, lower);
        let hits = _mm_movemask_epi8(_mm_cmpeq_epi8(folded, want)) as u32;
        if hits != 0 {
            return Some(i + hits.trailing_zeros() as usize);
        }
        i += 16;
    }
    hay[i..]
        .iter()
        .position(|&b| b | 0x20 == needle | 0x20)
        .map(|k| i + k)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn count_avx2(hay: &[u8], needle: u8) -> usize {
    use std::arch::x86_64::*;
    let len = hay.len();
    let ptr = hay.as_ptr();
    let want = _mm256_set1_epi8(needle as i8);
    let mut i = 0;
    let mut n = 0;
    // SAFETY: `i + 32 <= len`, so the 32-byte load stays inside the slice.
    while i + 32 <= len {
        let block = unsafe { _mm256_loadu_si256(ptr.add(i) as *const __m256i) };
        let hits = _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, want)) as u32;
        n += hits.count_ones() as usize;
        i += 32;
    }
    n + hay[i..].iter().filter(|&&b| b == needle).count()
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn count_sse2(hay: &[u8], needle: u8) -> usize {
    use std::arch::x86_64::*;
    let len = hay.len();
    let ptr = hay.as_ptr();
    let want = _mm_set1_epi8(needle as i8);
    let mut i = 0;
    let mut n = 0;
    // SAFETY: `i + 16 <= len`, so the 16-byte load stays inside the slice.
    while i + 16 <= len {
        let block = unsafe { _mm_loadu_si128(ptr.add(i) as *const __m128i) };
        let hits = _mm_movemask_epi8(_mm_cmpeq_epi8(block, want)) as u32;
        n += hits.count_ones() as usize;
        i += 16;
    }
    n + hay[i..].iter().filter(|&&b| b == needle).count()
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn memchr2_avx2(hay: &[u8], a: u8, b: u8, from: usize) -> Option<(usize, u8)> {
    use std::arch::x86_64::*;
    let len = hay.len();
    let ptr = hay.as_ptr();
    let wa = _mm256_set1_epi8(a as i8);
    let wb = _mm256_set1_epi8(b as i8);
    let mut i = from;
    // SAFETY: as in `memchr_avx2`.
    while i + 32 <= len {
        let block = unsafe { _mm256_loadu_si256(ptr.add(i) as *const __m256i) };
        let hit_a = _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, wa)) as u32;
        let hit_b = _mm256_movemask_epi8(_mm256_cmpeq_epi8(block, wb)) as u32;
        let any = hit_a | hit_b;
        if any != 0 {
            let k = any.trailing_zeros() as usize;
            return Some((i + k, if hit_a & (1 << k) != 0 { a } else { b }));
        }
        i += 32;
    }
    portable2(hay, a, b, i)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn memchr_sse2(hay: &[u8], needle: u8, from: usize) -> Option<usize> {
    use std::arch::x86_64::*;
    let len = hay.len();
    let ptr = hay.as_ptr();
    let want = _mm_set1_epi8(needle as i8);
    let mut i = from;
    // SAFETY: `i + 16 <= len`, so the 16-byte load stays inside the slice.
    while i + 16 <= len {
        let block = unsafe { _mm_loadu_si128(ptr.add(i) as *const __m128i) };
        let hits = _mm_movemask_epi8(_mm_cmpeq_epi8(block, want)) as u32;
        if hits != 0 {
            return Some(i + hits.trailing_zeros() as usize);
        }
        i += 16;
    }
    portable(hay, needle, i)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn memchr2_sse2(hay: &[u8], a: u8, b: u8, from: usize) -> Option<(usize, u8)> {
    use std::arch::x86_64::*;
    let len = hay.len();
    let ptr = hay.as_ptr();
    let wa = _mm_set1_epi8(a as i8);
    let wb = _mm_set1_epi8(b as i8);
    let mut i = from;
    // SAFETY: as in `memchr_sse2`.
    while i + 16 <= len {
        let block = unsafe { _mm_loadu_si128(ptr.add(i) as *const __m128i) };
        let hit_a = _mm_movemask_epi8(_mm_cmpeq_epi8(block, wa)) as u32;
        let hit_b = _mm_movemask_epi8(_mm_cmpeq_epi8(block, wb)) as u32;
        let any = hit_a | hit_b;
        if any != 0 {
            let k = any.trailing_zeros() as usize;
            return Some((i + k, if hit_a & (1 << k) != 0 { a } else { b }));
        }
        i += 16;
    }
    portable2(hay, a, b, i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_length_agrees_with_the_portable_scan() {
        let mut hay = vec![b'x'; 200];
        for needle in [b'x', b'a', 0u8, 0xFF] {
            for at in [0usize, 1, 7, 31, 32, 33, 63, 64, 100, 199] {
                hay[at] = needle;
                for from in [0usize, 1, 17, 31, 32, 64, 150, 199] {
                    let want = hay[from..]
                        .iter()
                        .position(|&b| b == needle)
                        .map(|i| from + i);
                    assert_eq!(memchr(&hay, needle, from), want, "needle {needle} at {at}");
                }
                hay[at] = b'x';
            }
        }
    }

    #[test]
    fn case_folded_search_agrees_with_the_portable_scan() {
        let mut hay = vec![b'x'; 200];
        hay[70] = b'M';
        hay[140] = b'm';
        for from in [0usize, 70, 71, 141] {
            let want = hay[from..]
                .iter()
                .position(|&b| b | 0x20 == b'm')
                .map(|i| from + i);
            assert_eq!(memchr_ci(&hay, b'm', from), want);
            assert_eq!(memchr_ci(&hay, b'M', from), want);
        }
    }

    #[test]
    fn counting_agrees_with_the_portable_count() {
        let mut hay = vec![b'\n'; 300];
        for at in [0usize, 15, 16, 31, 32, 33, 299] {
            hay[at] = b'x';
        }
        let want = hay.iter().filter(|&&b| b == b'\n').count();
        assert_eq!(count(&hay, b'\n'), want);
        assert_eq!(count(&hay, b'q'), 0);
        assert_eq!(count(&[], b'\n'), 0);
    }

    #[test]
    fn two_bytes_pick_the_earlier_one() {
        let hay = b"................a......b....";
        assert_eq!(memchr2(hay, b'a', b'b', 0), Some((16, b'a')));
        assert_eq!(memchr2(hay, b'a', b'b', 17), Some((23, b'b')));
        assert_eq!(memchr2(hay, b'z', b'b', 0), Some((23, b'b')));
        assert_eq!(memchr2(hay, b'z', b'z', 0), None);
    }

    #[test]
    fn a_prefilter_finds_the_earliest_needle() {
        let hay = b"..ab..cd..abcd..";
        let pf = Prefilter::new(vec![b"cd".to_vec(), b"ab".to_vec()]).unwrap();
        assert_eq!(
            pf.find_any(hay, 0)
                .map(|(at, i)| (at, &pf.needles()[i][..])),
            Some((2, &b"ab"[..]))
        );
        assert_eq!(pf.find_any(hay, 3).map(|(at, _)| at), Some(6));
        assert_eq!(
            pf.find_any(hay, 7).map(|(at, _)| at),
            Some(10),
            "the longest needle wins one offset"
        );
        assert_eq!(pf.find_any(hay, 14), None);
        assert_eq!(pf.find_from(hay, 0), Some(2));
    }

    #[test]
    fn a_prefilter_agrees_with_the_brute_force_scan() {
        // Every length across the vector and tail paths, with needles that share a first byte
        // and needles that do not, so the mask and the verify step are both exercised.
        let mut hay = vec![b'.'; 200];
        for at in [0usize, 1, 15, 16, 17, 31, 32, 33, 63, 64, 65, 100, 199] {
            hay[at] = b'a';
            hay[(at + 3) % 200] = b'c';
        }
        hay[120..124].copy_from_slice(b"abcd");
        // Candidates straddling the vector edges, where the second byte comes from the next
        // block or from the tail loop.
        for at in [14usize, 15, 30, 31, 32, 46, 47, 62, 63, 64] {
            hay[at..at + 2].copy_from_slice(b"ab");
        }
        let sets: [Vec<Vec<u8>>; 3] = [
            vec![b"abc".to_vec(), b"cd".to_vec()],
            vec![b"aaa".to_vec()],
            vec![
                b"ac".to_vec(),
                b"aa".to_vec(),
                b"ab".to_vec(),
                b"abc".to_vec(),
            ],
        ];
        for set in sets {
            let pf = Prefilter::new(set.clone()).unwrap();
            for from in [0usize, 1, 15, 16, 31, 32, 100, 190, 198, 199, 200] {
                let want = (from..hay.len()).find_map(|at| {
                    pf.needles()
                        .iter()
                        .position(|n| hay[at..].starts_with(n))
                        .map(|i| (at, i))
                });
                assert_eq!(pf.find_any(&hay, from), want, "needles {set:?} from {from}");
            }
        }
        assert!(Prefilter::new(Vec::new()).is_none());
        assert!(Prefilter::new(vec![Vec::new()]).is_none());
        assert!(Prefilter::new((0..5).map(|_| vec![b'x']).collect()).is_none());
    }

    /// The pair filter against the same answer a byte at a time, over every length and every
    /// offset pair a needle can carry. Offsets far apart are included because the two loads are
    /// independent, and lengths around one block and one block plus an offset, where the vector
    /// loop hands over to the tail.
    #[test]
    fn the_pair_filter_agrees_with_a_byte_at_a_time_scan() {
        let mut big = vec![b'.'; 300];
        for at in [
            0usize, 1, 30, 31, 32, 33, 62, 63, 64, 65, 126, 127, 128, 200, 255, 299,
        ] {
            big[at] = b'a';
        }
        for at in [3usize, 32, 64, 100, 200] {
            if at + 41 < big.len() {
                big[at + 41] = b'z';
            }
        }
        for len in [
            0usize, 1, 2, 3, 16, 17, 31, 33, 64, 65, 100, 130, 200, 299, 300,
        ] {
            let hay = &big[..len];
            for (a_off, b_off) in [
                (0usize, 1usize),
                (1, 0),
                (0, 0),
                (2, 5),
                (0, 31),
                (31, 0),
                (41, 3),
                (3, 41),
            ] {
                let wide = a_off.max(b_off);
                for (a, b) in [(b'a', b'z'), (b'a', b'a'), (b'z', b'a')] {
                    for from in 0..=len + 1 {
                        let want = (from..len).find(|&p| {
                            p + wide < len && hay[p + a_off] == a && hay[p + b_off] == b
                        });
                        assert_eq!(
                            find_pair(hay, a, a_off, b, b_off, from),
                            want,
                            "len {len} offsets {a_off}/{b_off} bytes {a}/{b} from {from}"
                        );
                    }
                }
            }
        }
    }

    /// The pairing decision is taken from the haystack in hand and never kept, so the same
    /// needle set over two corpora with different byte distributions decides differently for
    /// each — and answers the same matches either way.
    #[test]
    fn the_prefilter_decides_per_haystack_and_answers_the_same() {
        let needles = vec![b"abc".to_vec(), b"ac".to_vec()];
        let pf = Prefilter::new(needles.clone()).unwrap();
        // One corpus has both leading bytes on every other byte; the other has neither until
        // the needles it holds, well past the sampled prefix.
        let dense = b"aabbcc..".repeat(40);
        let mut sparse = b"xyzw..".repeat(40);
        sparse.extend_from_slice(b"..abc..ac..");
        assert_ne!(
            pf.decide(&dense),
            0,
            "a haystack full of the leading bytes pairs"
        );
        assert_eq!(pf.decide(&sparse), 0, "a haystack without them does not");
        for hay in [&dense[..], &sparse[..]] {
            for from in 0..=hay.len() {
                let want = (from..hay.len()).find_map(|at| {
                    needles
                        .iter()
                        .position(|n| hay[at..].starts_with(n))
                        .map(|i| (at, i))
                });
                assert_eq!(pf.find_any(hay, from), want, "from {from}");
            }
        }
    }

    #[test]
    fn nothing_is_found_past_the_end() {
        let hay = b"abc";
        assert_eq!(memchr(hay, b'a', 3), None);
        assert_eq!(memchr(hay, b'a', 9), None);
        assert_eq!(memchr(&[], b'a', 0), None);
        assert_eq!(memchr(hay, b'c', 2), Some(2));
    }

    /// Every length from 1 to 70 against a scalar reference. Below one vector (32 bytes AVX2,
    /// 16 SSE2) the loop body never runs and the whole slice is the tail; between two and three
    /// vectors it runs once and leaves a partial vector behind. That partial vector is the case
    /// the `i + 32 <= len` guard exists for, so it is the one worth pinning: the tail loop reads
    /// it, never a vector load.
    #[test]
    fn every_length_from_one_to_seventy_agrees_with_the_scalar_scan() {
        for len in 1..=70usize {
            // The haystack is a prefix of a bigger buffer whose tail is full of the needle, so a
            // vector load one lane past the slice would show up as a match at or past `len`
            // instead of only being undefined behaviour nobody notices.
            let mut big = vec![b'a'; len + 64];
            for b in &mut big[..len] {
                *b = b'.';
            }
            for (at, byte) in [(0, b'b'), (len / 2, b'b'), (len - 1, b'c')] {
                big[at] = byte;
            }
            big[len / 3] = b'M';
            let hay = &big[..len];
            for from in 0..=len {
                let rest = &hay[from..];
                for needle in [b'a', b'b', b'c', b'M', b'm', b'.', b'z', 0u8] {
                    let want = rest.iter().position(|&b| b == needle).map(|i| from + i);
                    assert_eq!(
                        memchr(hay, needle, from),
                        want,
                        "memchr len {len} from {from} needle {needle}"
                    );
                }
                let want_ci = rest
                    .iter()
                    .position(|&b| b | 0x20 == b'm')
                    .map(|i| from + i);
                assert_eq!(
                    memchr_ci(hay, b'm', from),
                    want_ci,
                    "memchr_ci len {len} from {from}"
                );
                assert_eq!(memchr_ci(hay, b'M', from), want_ci);
                let want2 = rest
                    .iter()
                    .enumerate()
                    .find(|(_, &b)| b == b'a' || b == b'c')
                    .map(|(i, &b)| (from + i, b));
                assert_eq!(
                    memchr2(hay, b'a', b'c', from),
                    want2,
                    "memchr2 len {len} from {from}"
                );
            }
            assert_eq!(count(hay, b'.'), hay.iter().filter(|&&b| b == b'.').count());
            assert_eq!(count(hay, b'z'), 0);
            assert_eq!(count(hay, b'a'), hay.iter().filter(|&&b| b == b'a').count());
        }
    }

    /// The same length sweep through the prefilter, whose paired mask looks one lane past a
    /// candidate: the masks have to agree with a brute-force scan at every length, including
    /// the ones that end in a partial vector. The buffer after the slice repeats every needle,
    /// so a load past the end becomes a visible wrong answer.
    #[test]
    fn every_length_from_one_to_seventy_agrees_with_the_brute_force_prefilter() {
        let sets: [&[&[u8]]; 2] = [&[b"ab", b"cd"], &[b"abc", b"aa", b"ab"]];
        for len in 1..=70usize {
            let mut big = b"abcdabcdabcdabcdabcd".repeat((len + 64) / 20 + 2);
            for b in &mut big[..len] {
                *b = b'.';
            }
            let mut at = 0;
            while at + 2 <= len {
                big[at..at + 2].copy_from_slice(b"ab");
                at += 7;
            }
            let hay = &big[..len];
            for set in sets {
                let needles: Vec<Vec<u8>> = set.iter().map(|n| n.to_vec()).collect();
                let pf = Prefilter::new(needles.clone()).unwrap();
                for from in 0..=len {
                    let want = (from..len).find_map(|at| {
                        needles
                            .iter()
                            .position(|n| hay[at..].starts_with(n))
                            .map(|i| (at, i))
                    });
                    assert_eq!(
                        pf.find_any(hay, from),
                        want,
                        "needles {set:?} len {len} from {from}"
                    );
                }
            }
        }
    }
}
