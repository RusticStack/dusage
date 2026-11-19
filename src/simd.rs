//! One-byte search over the machine's vector registers.
//!
//! The SWAR loops in [`crate::bytes`] run at roughly two cycles a byte; a 32-byte AVX2
//! compare runs at about a tenth of that, which on a tree of files that never match is the
//! difference between reading at 1.7 GB/s and at ripgrep's 6 GB/s. Everything here answers
//! exactly what `hay[from..].iter().position(|&b| b == needle)` answers, only faster.
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
    fn nothing_is_found_past_the_end() {
        let hay = b"abc";
        assert_eq!(memchr(hay, b'a', 3), None);
        assert_eq!(memchr(hay, b'a', 9), None);
        assert_eq!(memchr(&[], b'a', 0), None);
        assert_eq!(memchr(hay, b'c', 2), Some(2));
    }
}
