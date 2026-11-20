//! Fast byte scanning: memmem-style substring search, ASCII case folding, word boundaries.
//!
//! Every search is a safe scalar scan over native-endian `u64` words. The byte a needle is
//! searched by — its "anchor" — is broadcast across a word, candidates are found with the
//! classic has-zero-byte trick after the XOR, and the exact per-byte flag word pins down the
//! lane, so a 32-byte block that holds a hit costs no rescan. The anchor is the rarest byte of
//! the needle (a coarse frequency table, the same idea as memchr's `memmem`), which is what
//! keeps candidate density low enough that the only comparisons on a hit are `memcmp`-shaped
//! whole-window compares. Needles long enough for it use Horspool instead: its bad-byte table
//! advances a whole needle width per mismatch on text whose bytes are all common.
//!
//! Both decisions cost more than the scan of a 30-byte line, so a needle searched line after
//! line is prepared once into a [`Finder`] — the anchor and the skip table built with the
//! pattern, not per call. The free functions build one per call, for the callers that match a
//! single buffer.
//!
//! No `unsafe`, no dependencies, no allocations.

const LO: u64 = 0x0101_0101_0101_0101;
const HI: u64 = 0x8080_8080_8080_8080;
const M7: u64 = 0x7F7F_7F7F_7F7F_7F7F;

/// How rare each byte is in running text and source code, `0` rarest. Only the ordering
/// matters: it decides which needle byte a search scans for, so a needle of one common and
/// one rare byte is searched by the rare one, and `func ` is searched by its `f`, not its
/// space. Tiers are ordered by the usual English/source-code letter frequencies; the exact
/// numbers are irrelevant to correctness.
const RANK: [u8; 256] = rank_table();

const fn rank_table() -> [u8; 256] {
    let mut t = [3u8; 256];
    let tiers: [&[u8]; 8] = [
        b"^~`\\$@",
        b"|%#?&!zqjx",
        b"{}[]<>*;+=-_:'\"/,.()",
        b"0123456789kvbfpwyge",
        b"mcudl",
        b"hrsn",
        b"ioat",
        b" e",
    ];
    let mut tier = 0;
    while tier < tiers.len() {
        let mut i = 0;
        while i < tiers[tier].len() {
            t[tiers[tier][i] as usize] = tier as u8;
            i += 1;
        }
        tier += 1;
    }
    let mut b = 0;
    while b < 0x20 {
        t[b] = 0;
        b += 1;
    }
    t[b'\n' as usize] = 7;
    t[b'\t' as usize] = 5;
    t[b'\r' as usize] = 3;
    t[0x7F] = 0;
    let mut b = 0x80;
    while b < 0x100 {
        t[b] = 2;
        b += 1;
    }
    t
}

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
    /// True for the exact case, whose anchor byte one vector compare can look for; the folded
    /// case needs two compares per position and keeps the SWAR loop.
    const SENSITIVE: bool;
    /// Broadcast of the needle byte as the scan sees it: one target, because the haystack word
    /// is folded first.
    fn target(b: u8) -> u64;
    /// Folds all eight bytes of `chunk` the way the model folds one haystack byte: the
    /// identity for `Exact`, bit 5 OR-ed in for `Ci`. Folding only ever adds candidates, never
    /// loses one, so a false candidate costs one window compare and never an answer.
    fn fold_word(chunk: u64) -> u64;
    /// Zero flags (high bits) for the folded bytes of `chunk` that equal the target. Several
    /// flags from one block can be OR-ed and tested once, which is what keeps the scan loop
    /// branch-light. A borrow out of a zero byte also flags the byte above it, so these flags
    /// are only ever tested for "any match here?".
    #[inline]
    fn flags(chunk: u64, target: u64) -> u64 {
        zero_flags(Self::fold_word(chunk) ^ target)
    }
    /// Exactly one high bit per matching byte, no borrow artefacts: what locates the lane.
    #[inline]
    fn flags_exact(chunk: u64, target: u64) -> u64 {
        zero_bytes(Self::fold_word(chunk) ^ target)
    }
    fn eq(a: u8, b: u8) -> bool;
    /// Key for Horspool's bad-byte table. Two bytes that match under this rule always share a
    /// key; `Ci` also ties the seven pairs that differ only in bit 5 (`@`/`` ` ``, and `[\]^_`
    /// with `{|}~`), which can only make the table's shift shorter, never skip a match.
    fn skip_key(b: u8) -> u8;
    /// Whole-window compare of two equal-length slices.
    fn slice_eq(a: &[u8], b: &[u8]) -> bool;
    /// Rarity of the haystack byte that matches `b`, for anchor choice.
    fn rank(b: u8) -> u8;
}

struct Exact;

impl Case for Exact {
    const SENSITIVE: bool = true;

    #[inline]
    fn target(b: u8) -> u64 {
        broadcast(b)
    }

    #[inline]
    fn fold_word(chunk: u64) -> u64 {
        chunk
    }

    #[inline]
    fn eq(a: u8, b: u8) -> bool {
        a == b
    }

    #[inline]
    fn skip_key(b: u8) -> u8 {
        b
    }

    #[inline]
    fn slice_eq(a: &[u8], b: &[u8]) -> bool {
        a == b
    }

    #[inline]
    fn rank(b: u8) -> u8 {
        RANK[b as usize]
    }
}

struct Ci;

/// Bit 5 of every byte: `A-Z` become `a-z`, every other byte keeps its value. Seven byte pairs
/// share a folded value without being the same character — `@`/`` ` ``, and `[\]^_` with
/// `{|}~` — so a folded hit there is a candidate `eq` then rejects. Letters, digits and every
/// other pair are exact. Latin-1 is deliberately not folded: `LOWER` decides what matches.
const OR20: u64 = 0x2020_2020_2020_2020;

impl Case for Ci {
    const SENSITIVE: bool = false;

    #[inline]
    fn target(b: u8) -> u64 {
        broadcast(b | 0x20)
    }

    #[inline]
    fn fold_word(chunk: u64) -> u64 {
        chunk | OR20
    }

    #[inline]
    fn eq(a: u8, b: u8) -> bool {
        LOWER[a as usize] == LOWER[b as usize]
    }

    #[inline]
    fn skip_key(b: u8) -> u8 {
        b | 0x20
    }

    #[inline]
    fn slice_eq(a: &[u8], b: &[u8]) -> bool {
        a.eq_ignore_ascii_case(b)
    }

    #[inline]
    fn rank(b: u8) -> u8 {
        RANK[LOWER[b as usize] as usize]
    }
}

/// First index in `from..=last` holding a byte that matches `byte` under `C`.
///
/// Blocks of 32, then 16, then 8 bytes are tested with one branch each; whatever is left over
/// is a single masked word load anchored at `end`, so a short line — the common shape, since
/// this runs once per line — never pays for a byte-at-a-time loop.
#[inline]
fn find_candidate<C: Case>(hay: &[u8], byte: u8, from: usize, last: usize) -> Option<usize> {
    if from > last || last >= hay.len() {
        return None;
    }
    // The folded scan is one vector compare on the lowered bytes, the same equivalence the SWAR
    // loop below implements a word at a time.
    if !C::SENSITIVE && hay.len() - from >= VECTOR_MIN {
        return crate::simd::memchr_ci(hay, byte, from).filter(|&p| p <= last);
    }
    let target = C::target(byte);
    let end = last + 1;
    let mut i = from;
    while i + 32 <= end {
        let w0 = load8(hay, i);
        let w1 = load8(hay, i + 8);
        let w2 = load8(hay, i + 16);
        let w3 = load8(hay, i + 24);
        let flags = C::flags(w0, target)
            | C::flags(w1, target)
            | C::flags(w2, target)
            | C::flags(w3, target);
        if flags & HI != 0 {
            let m = C::flags_exact(w0, target);
            if m != 0 {
                return Some(i + (m.trailing_zeros() as usize >> 3));
            }
            let m = C::flags_exact(w1, target);
            if m != 0 {
                return Some(i + 8 + (m.trailing_zeros() as usize >> 3));
            }
            let m = C::flags_exact(w2, target);
            if m != 0 {
                return Some(i + 16 + (m.trailing_zeros() as usize >> 3));
            }
            let m = C::flags_exact(w3, target);
            if m != 0 {
                return Some(i + 24 + (m.trailing_zeros() as usize >> 3));
            }
        }
        i += 32;
    }
    while i + 16 <= end {
        let a = load8(hay, i);
        let b = load8(hay, i + 8);
        if (C::flags(a, target) | C::flags(b, target)) & HI != 0 {
            let m = C::flags_exact(a, target);
            if m != 0 {
                return Some(i + (m.trailing_zeros() as usize >> 3));
            }
            let m = C::flags_exact(b, target);
            if m != 0 {
                return Some(i + 8 + (m.trailing_zeros() as usize >> 3));
            }
        }
        i += 16;
    }
    while i + 8 <= end {
        let word = load8(hay, i);
        if C::flags(word, target) & HI != 0 {
            let m = C::flags_exact(word, target);
            if m != 0 {
                return Some(i + (m.trailing_zeros() as usize >> 3));
            }
        }
        i += 8;
    }
    if i == end {
        return None;
    }
    if end < 8 {
        return hay[i..end]
            .iter()
            .position(|&h| C::eq(h, byte))
            .map(|k| i + k);
    }
    // The last word ends exactly at `end`; the lanes below `i` are already known to be clean.
    let base = end - 8;
    let lanes = (i - base) * 8;
    let m = C::flags_exact(load8(hay, base), target) & (u64::MAX << lanes);
    (m != 0).then(|| base + (m.trailing_zeros() as usize >> 3))
}

/// First index in `from..=last` holding a byte that *matches* `byte` under `C`.
///
/// [`find_candidate`] answers "could a match be here" from the folded flags, so under `Ci` it
/// also reports the seven bytes that fold onto another character (`@` for `` ` ``); a
/// single-byte needle has no window to compare, so it confirms each candidate with `eq`.
#[inline]
fn find_match<C: Case>(hay: &[u8], byte: u8, from: usize, last: usize) -> Option<usize> {
    let mut at = from;
    loop {
        let p = find_candidate::<C>(hay, byte, at, last)?;
        if C::eq(hay[p], byte) {
            return Some(p);
        }
        at = p + 1;
    }
}

/// Whether `hay[pos..pos + needle.len()]` equals `needle` under `C`.
#[inline]
fn verify<C: Case>(hay: &[u8], needle: &[u8], pos: usize) -> bool {
    let Some(end) = pos.checked_add(needle.len()) else {
        return false;
    };
    match hay.get(pos..end) {
        Some(window) => C::slice_eq(window, needle),
        None => false,
    }
}

/// Index of the needle byte whose matches are rarest in text: the cheapest one to scan for.
///
/// The lookahead is capped so that a long needle costs the same handful of table reads as a
/// short one, and the needle's last byte is always weighed because "common prefix, distinctive
/// tail" (`the `, `// `, `);`) is the usual shape of a real pattern.
fn anchor<C: Case>(needle: &[u8]) -> usize {
    let n = needle.len();
    if n == 0 {
        return 0;
    }
    let mut best = 0;
    let mut best_rank = C::rank(needle[0]);
    let head = n.min(6);
    for (k, &b) in needle[..head].iter().enumerate().skip(1) {
        let r = C::rank(b);
        if r < best_rank {
            best = k;
            best_rank = r;
        }
    }
    if n > head && C::rank(needle[n - 1]) < best_rank {
        best = n - 1;
    }
    best
}

/// The partner offset when the needle is short enough for the pair filter to be the plan.
#[inline]
fn pair_plan<C: Case>(needle: &[u8], anchor: usize) -> Option<usize> {
    if needle.len() < PAIR_MAX_LEN {
        partner::<C>(needle, anchor)
    } else {
        None
    }
}

/// The second needle byte a candidate has to hold, next to the anchor: the rarest of the
/// needle's other bytes, which one more compare per block turns into a filter.
///
/// The anchor alone is the needle's rarest byte in text, which in Go source is still every few
/// hundred bytes for a letter like `f`; the pair turns a candidate into "the rarest byte *and*
/// the rarest of its neighbours", and the window compare then runs on a few thousand positions
/// per tree instead of a million.
fn partner<C: Case>(needle: &[u8], anchor: usize) -> Option<usize> {
    let n = needle.len();
    if n < 2 {
        return None;
    }
    let mut best: Option<(u8, usize, usize)> = None;
    for (k, &b) in needle.iter().enumerate() {
        if k == anchor {
            continue;
        }
        let r = C::rank(b);
        let dist = k.abs_diff(anchor);
        // Rarest wins; between equal ranks the nearer byte keeps the block step wide.
        let better = match best {
            None => true,
            Some((br, bd, _)) => r < br || (r == br && dist < bd),
        };
        if better {
            best = Some((r, dist, k));
        }
    }
    best.map(|(_, _, k)| k)
}

/// Scan for the anchor and the partner byte together, and compare the window only where both
/// line up. One extra vector compare per block turns a candidate from "the rarest byte of the
/// needle" into "the rarest byte *and* the rarest byte near it", which is where the million
/// window compares of a tree-wide search go.
#[inline]
fn search_pair<C: Case>(
    hay: &[u8],
    needle: &[u8],
    from: usize,
    a: usize,
    b: usize,
) -> Option<usize> {
    let last = hay.len() - needle.len();
    let vectors = C::SENSITIVE && crate::simd::vectorized();
    let mut at = from;
    while at <= last {
        let p = match vectors && hay.len() - at >= VECTOR_MIN {
            true => crate::simd::find_pair(hay, needle[a], a, needle[b], b, at)?,
            false => pair_word::<C>(hay, needle, at, a, b)?,
        };
        if p > last {
            return None;
        }
        if verify::<C>(hay, needle, p) {
            return Some(p);
        }
        at = p + 1;
    }
    None
}

/// The pair filter a word at a time, for a haystack too short to fill a vector block: the
/// anchor comes from the same SWAR loop the anchor scan uses, and the candidate it finds is
/// kept only when the partner byte is there too.
fn pair_word<C: Case>(hay: &[u8], needle: &[u8], from: usize, a: usize, b: usize) -> Option<usize> {
    let last = hay.len() - needle.len();
    let mut at = from;
    loop {
        let q = find_candidate::<C>(hay, needle[a], at + a, last + a)?;
        let p = q - a;
        if C::eq(hay[p + b], needle[b]) {
            return Some(p);
        }
        at = p + 1;
    }
}

/// Scan for a chosen needle byte and compare the whole window on each candidate. A mismatch
/// advances the anchor by one byte, which is all that is left to do: the anchor is the needle's
/// rarest byte, so candidates are few and the window compare (`memcmp` under `Exact`) is rare.
#[inline]
fn search_anchor<C: Case>(hay: &[u8], needle: &[u8], from: usize, k: usize) -> Option<usize> {
    let n = needle.len();
    let last = hay.len() - n + k;
    let mut at = from + k;
    while at <= last {
        // The case-sensitive anchor byte is what a vector scan can find; a folded one is two
        // bytes per position, which only the SWAR loop handles in one pass.
        let p = match C::SENSITIVE && hay.len() - at >= VECTOR_MIN {
            true => {
                let hit = crate::simd::memchr(hay, needle[k], at)?;
                if hit > last {
                    return None;
                }
                hit
            }
            false => find_candidate::<C>(hay, needle[k], at, last)?,
        };
        let start = p - k;
        if verify::<C>(hay, needle, start) {
            return Some(start);
        }
        at = p + 1;
    }
    None
}

/// True when the pair filter can run for this case at all: the exact case compares raw bytes and
/// needs the vector loop, the folded one shares the SWAR word loop the anchor scan uses.
#[inline]
fn pairs<C: Case>() -> bool {
    !C::SENSITIVE || crate::simd::vectorized()
}

/// The plan a prepared needle carries: the pair scan when there is a second byte to filter on
/// and a vector unit to run it, Horspool for a long needle on a machine that has neither, the
/// anchor scan otherwise.
///
/// The order matters and it was measured: the pair filter costs one compare per block whatever
/// the needle length, so it beats Horspool's bad-byte skip on every shape tried — 18, 21, 22, 37
/// and 50 bytes, over Go source and over the binaries beside it — because the skip shrinks to a
/// byte a position wherever the haystack is not text. Horspool keeps its place for the machine
/// that has no vector unit, where the pair has no block to run in.
#[inline]
fn search_planned<C: Case>(
    hay: &[u8],
    needle: &[u8],
    from: usize,
    anchor: usize,
    partner: Option<usize>,
    skips: Option<&Skip>,
) -> Option<usize> {
    if pairs::<C>() {
        if let Some(b) = partner {
            return search_pair::<C>(hay, needle, from, anchor, b);
        }
    }
    match skips {
        Some(skips) => search_skip::<C>(hay, needle, from, skips),
        None => search_anchor::<C>(hay, needle, from, anchor),
    }
}

/// Horspool's bad-byte table: 256 entries packed eight to a word, so building one costs 32
/// stores plus one per needle byte instead of a 256-byte initialisation.
#[derive(Debug, Clone)]
struct Skip([u64; 32]);

impl Skip {
    /// `skip[b]` is how far the window may advance when its last byte is `b`: the distance to
    /// that byte's last occurrence before the needle's end, or the needle width when it never
    /// recurs. Offsets above 255 clamp, which only makes the skip smaller than allowed.
    fn build<C: Case>(needle: &[u8]) -> Skip {
        let n = needle.len();
        let mut table = Skip([broadcast((n - 1).min(255) as u8); 32]);
        for (k, &b) in needle[..n - 1].iter().enumerate() {
            let key = C::skip_key(b) as usize;
            let shift = (key & 7) * 8;
            let lane = !(0xFFu64 << shift);
            let step = ((n - 1 - k).min(255) as u64) << shift;
            table.0[key >> 3] = (table.0[key >> 3] & lane) | step;
        }
        table
    }

    #[inline]
    fn get(&self, b: u8) -> usize {
        ((self.0[b as usize >> 3] >> ((b as usize & 7) * 8)) & 0xFF) as usize
    }
}

/// Horspool: test the window's last byte, compare the rest only when it fits, and on a mismatch
/// advance by the bad-byte table — a whole needle width per step once the haystack's bytes all
/// happen to be common, which is where an anchor scan crawls.
#[inline]
fn search_skip<C: Case>(hay: &[u8], needle: &[u8], from: usize, skips: &Skip) -> Option<usize> {
    let n = needle.len();
    let tail = n - 1;
    let last = hay.len() - n;
    // The needle's tail key cannot change, so under `Ci` the step pays one table read for the
    // haystack byte instead of two.
    let key = C::skip_key(needle[tail]);
    let mut at = from;
    while at <= last {
        let k = C::skip_key(hay[at + tail]);
        if k == key && C::slice_eq(&hay[at..at + tail], &needle[..tail]) {
            return Some(at);
        }
        at += skips.get(k);
    }
    None
}

/// Needles at least this long get a Horspool table, which is the fallback for a machine with no
/// vector unit and the plan for a needle longer than [`PAIR_MAX_LEN`]. Measured: below 16 bytes
/// the skip is too narrow to beat scanning for the rare anchor byte. Numbers in
/// `bench_literal_matcher`.
const SKIP_MIN_LEN: usize = 16;

/// Needle length up to which the pair filter is the plan. Measured against the Horspool skip on
/// the two shapes the searcher has — Go source and the binaries beside it — the pair is ahead by
/// 36% at 21 bytes, 26% at 37, 13% at 50, 5% on the binaries and 9% on the source at 64, level at
/// 80, and behind by 5% at 96, where a skip of nearly a needle width per position takes over.
/// The skip shrinks to one byte a position wherever the haystack is not text, which is why the
/// pair holds for so long.
const PAIR_MAX_LEN: usize = 64;

/// A needle prepared once and searched many times: which byte to scan for, and Horspool's
/// bad-byte table when the needle is long enough to earn one.
///
/// `dusage search` runs the matcher once per line, and a 40-byte line is over before a plan
/// paid for itself, so the plan is built with the pattern and reused. The public free functions
/// below build one per call for the callers that match a single big buffer.
#[derive(Debug, Clone)]
pub struct Finder {
    needle: Vec<u8>,
    anchor: usize,
    partner: Option<usize>,
    skips: Option<Skip>,
    ci: bool,
}

impl Finder {
    /// Prepare `needle`; `ci` selects the ASCII case-insensitive fold.
    pub fn new(needle: &[u8], ci: bool) -> Finder {
        let anchor = if ci {
            anchor::<Ci>(needle)
        } else {
            anchor::<Exact>(needle)
        };
        Finder::build(needle, ci, anchor)
    }

    /// The bytes this finder looks for.
    pub fn needle(&self) -> &[u8] {
        &self.needle
    }

    /// The offset in the needle of the byte a candidate scan looks for, which a caller can
    /// check against its own rarity order to see that the scan is not hunting a common byte.
    pub fn anchor(&self) -> usize {
        self.anchor
    }

    /// Prepare `needle` with the byte to scan for chosen by the caller, for a matcher that
    /// already knows which run of its pattern it is searching for and where that run sits in it
    /// — a regex with one mandatory literal per alternative, say. `anchor` is an index into
    /// `needle`; one at or past its end means "no preference" and the rarest byte is used, the
    /// same byte [`Finder::new`] would pick.
    ///
    /// Everything else about the plan — the partner byte that filters candidates, Horspool's
    /// table for a long needle, the fold when `ci` is set — follows from the needle exactly as
    /// it does for [`Finder::new`], so the answers are the same and only the scan differs.
    pub fn with_anchor(needle: &[u8], ci: bool, anchor: usize) -> Finder {
        let chosen = match anchor < needle.len() {
            true => anchor,
            false if ci => self::anchor::<Ci>(needle),
            false => self::anchor::<Exact>(needle),
        };
        Finder::build(needle, ci, chosen)
    }

    /// The plan both constructors share: the anchor is already chosen, and the partner and the
    /// skip table follow from the needle.
    fn build(needle: &[u8], ci: bool, anchor: usize) -> Finder {
        let (partner, skips) = if ci {
            (
                pair_plan::<Ci>(needle, anchor),
                (needle.len() >= SKIP_MIN_LEN).then(|| Skip::build::<Ci>(needle)),
            )
        } else {
            (
                pair_plan::<Exact>(needle, anchor),
                (needle.len() >= SKIP_MIN_LEN).then(|| Skip::build::<Exact>(needle)),
            )
        };
        Finder {
            needle: needle.to_vec(),
            anchor,
            partner,
            skips,
            ci,
        }
    }

    /// First occurrence at or after `from`.
    #[inline]
    pub fn find_from(&self, hay: &[u8], from: usize) -> Option<usize> {
        if self.ci {
            self.search_in::<Ci>(hay, from)
        } else {
            self.search_in::<Exact>(hay, from)
        }
    }

    /// First occurrence in `hay`.
    #[inline]
    /// The needle length, so a caller can turn a found offset into a span.
    pub fn len(&self) -> usize {
        self.needle.len()
    }

    /// A needle is never empty, but the pair keeps the lint quiet.
    pub fn is_empty(&self) -> bool {
        self.needle.is_empty()
    }

    pub fn find(&self, hay: &[u8]) -> Option<usize> {
        self.find_from(hay, 0)
    }

    #[inline]
    fn search_in<C: Case>(&self, hay: &[u8], from: usize) -> Option<usize> {
        let needle = &self.needle[..];
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
            return find_match::<C>(hay, needle[0], from, hay.len() - 1);
        }
        search_planned::<C>(
            hay,
            needle,
            from,
            self.anchor,
            self.partner,
            self.skips.as_ref(),
        )
    }
}

/// The free functions build their plan per call, so Horspool needs the buffer to be long
/// enough to pay for the table: a 64 KiB read block or a `count_matches` sweep clears it, a
/// 30-byte line does not.
const SKIP_MIN_HAY: usize = 512;

#[cfg(test)]
impl Finder {
    /// The same plan with the skip table forced off (`false`) or on (`true`), so the
    /// benchmark compares the two strategies through one call path.
    fn forced(&self, skips: bool) -> Finder {
        Finder {
            needle: self.needle.clone(),
            anchor: self.anchor,
            partner: self.partner,
            skips: if skips && self.needle.len() > 1 {
                Some(if self.ci {
                    Skip::build::<Ci>(&self.needle)
                } else {
                    Skip::build::<Exact>(&self.needle)
                })
            } else {
                None
            },
            ci: self.ci,
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
        return find_match::<C>(hay, needle[0], from, hay.len() - 1);
    }
    let anchor = anchor::<C>(needle);
    let skips =
        (n >= SKIP_MIN_LEN && hay.len() - from >= SKIP_MIN_HAY).then(|| Skip::build::<C>(needle));
    let partner = pair_plan::<C>(needle, anchor);
    search_planned::<C>(hay, needle, from, anchor, partner, skips.as_ref())
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
    // A block is worth a vector load per 32 bytes; a line is a few words and the SWAR loop
    // has less setup to pay.
    if hay.len() - from >= VECTOR_MIN {
        return crate::simd::memchr(hay, b, from);
    }
    find_candidate::<Exact>(hay, b, from, hay.len() - 1)
}

/// The haystack length from which a vector scan beats the SWAR loop, measured on the two
/// shapes that call this: 64 KiB blocks and 40-byte lines.
const VECTOR_MIN: usize = 128;

/// First byte at or after `from` inside one of `ranges`, or None. Every range must lie
/// below 0x7f: the word test compares the low seven bits of each byte, which is exact only
/// there. One masked word answers all the ranges at once, so a caller that skips the bytes
/// which cannot start a match pays a word per eight instead of a byte each.
pub fn find_in_ranges(hay: &[u8], ranges: &[(u8, u8)], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 8 <= hay.len() {
        if in_ranges(load8(hay, i), ranges) & HI != 0 {
            break;
        }
        i += 8;
    }
    hay.get(i..)?
        .iter()
        .position(|b| ranges.iter().any(|&(lo, hi)| *b >= lo && *b <= hi))
        .map(|k| i + k)
}

/// High bit set per byte of `w` that falls in one of `ranges`. Borrows can flag a byte
/// above a match, so this only ever answers "is there one in this word?".
#[inline]
fn in_ranges(w: u64, ranges: &[(u8, u8)]) -> u64 {
    let ascii = HI & !w;
    let mut flags = 0;
    for &(lo, hi) in ranges {
        flags |= at_least(w, lo) & HI & !at_least(w, hi + 1) & ascii;
    }
    flags
}

/// High bit set per byte of `w` that is at least `t`, for `t` below 0x80.
#[inline]
fn at_least(w: u64, t: u8) -> u64 {
    ((w & M7) | HI).wrapping_sub(u64::from(t) * LO) & HI
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
    if hay.len() >= VECTOR_MIN {
        return crate::simd::count(hay, b);
    }
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

    /// The scan this module replaced, kept verbatim as the benchmark's baseline: first byte
    /// only, cheap borrow-artefact flags, and an inline byte-loop verify.
    mod legacy {
        use super::{HI, LO};

        fn broadcast(b: u8) -> u64 {
            u64::from(b) * LO
        }

        fn zero_flags(x: u64) -> u64 {
            x.wrapping_sub(LO) & !x
        }

        fn load8(hay: &[u8], at: usize) -> u64 {
            u64::from_ne_bytes(hay[at..at + 8].try_into().unwrap())
        }

        fn find_candidate(hay: &[u8], byte: u8, from: usize, last: usize) -> Option<usize> {
            if from > last || last >= hay.len() {
                return None;
            }
            let target = broadcast(byte);
            let mut i = from;
            while i + 32 <= last + 1 {
                let flags = zero_flags(load8(hay, i) ^ target)
                    | zero_flags(load8(hay, i + 8) ^ target)
                    | zero_flags(load8(hay, i + 16) ^ target)
                    | zero_flags(load8(hay, i + 24) ^ target);
                if flags & HI != 0 {
                    for (k, &h) in hay[i..i + 32].iter().enumerate() {
                        if h == byte {
                            return Some(i + k);
                        }
                    }
                }
                i += 32;
            }
            while i + 8 <= last + 1 {
                if zero_flags(load8(hay, i) ^ target) & HI != 0 {
                    for (k, &h) in hay[i..i + 8].iter().enumerate() {
                        if h == byte {
                            return Some(i + k);
                        }
                    }
                }
                i += 8;
            }
            while i <= last {
                if hay[i] == byte {
                    return Some(i);
                }
                i += 1;
            }
            None
        }

        fn verify(hay: &[u8], needle: &[u8], pos: usize) -> bool {
            let n = needle.len();
            match n {
                0 | 1 => true,
                2 => hay[pos + 1] == needle[1],
                3 => hay[pos + 2] == needle[2] && hay[pos + 1] == needle[1],
                _ => {
                    if hay[pos + n - 1] != needle[n - 1] {
                        return false;
                    }
                    (1..n - 1).all(|k| hay[pos + k] == needle[k])
                }
            }
        }

        pub fn find_byte(hay: &[u8], b: u8, from: usize) -> Option<usize> {
            if from >= hay.len() {
                return None;
            }
            find_candidate(hay, b, from, hay.len() - 1)
        }

        pub fn memmem_from(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
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
                return find_candidate(hay, needle[0], from, hay.len() - 1);
            }
            let last = hay.len() - n;
            let mut at = from;
            loop {
                let p = find_candidate(hay, needle[0], at, last)?;
                if verify(hay, needle, p) {
                    return Some(p);
                }
                at = p + 1;
            }
        }
    }

    /// Common-byte needles of every length, absent from the corpus so the whole sweep runs:
    /// this is the shape that decides where Horspool starts to pay.
    const CROSSOVER: &[u8] = b"and the function returned struct error string package";

    /// Needles spanning every strategy boundary the matcher has.
    const BENCH_NEEDLES: [&[u8]; 12] = [
        b"e",
        b"q",
        b"->",
        b") {",
        b"func ",
        b"return ",
        b"fmt.Println",
        b"func (t *T) ",
        b"if err != nil {",
        b"package main\n\nimport",
        b"\x01no such byte in go source\x01",
        b"func (s *Server) ServeHTTP(w http.ResponseWriter, r *http.Req",
    ];

    /// A real corpus split into lines: one flat buffer plus `(start, end)` per line, so the
    /// benchmark measures the matcher and nothing else.
    fn corpus(root: &str, cap: usize) -> (Vec<u8>, Vec<(usize, usize)>) {
        let mut buf = Vec::new();
        let mut lines = Vec::new();
        let mut dirs = vec![std::path::PathBuf::from(root)];
        while let Some(dir) = dirs.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(meta) = entry.metadata() else { continue };
                if meta.is_dir() {
                    dirs.push(path);
                    continue;
                }
                if !meta.is_file() || meta.len() == 0 || meta.len() > (1 << 20) {
                    continue;
                }
                let Ok(data) = std::fs::read(&path) else {
                    continue;
                };
                if data.iter().take(1024).any(|&b| b == 0) {
                    continue;
                }
                let start = buf.len();
                buf.extend_from_slice(&data);
                let mut line = start;
                for (i, &b) in buf.iter().enumerate().skip(start) {
                    if b == b'\n' {
                        lines.push((line, i));
                        line = i + 1;
                    }
                }
                if line < buf.len() {
                    lines.push((line, buf.len()));
                }
                if buf.len() >= cap {
                    break;
                }
            }
            if buf.len() >= cap {
                break;
            }
        }
        (buf, lines)
    }

    /// Hits over every line, the shape the `-c` path takes: the old scan against the plan the
    /// searcher really builds, once per pattern, then reused per line.
    fn count_lines(buf: &[u8], lines: &[(usize, usize)], needle: &[u8], legacy: bool) -> usize {
        let finder = (!legacy).then(|| Finder::new(needle, false));
        let mut hits = 0;
        for &(start, end) in lines {
            let line = &buf[start..end];
            let found = match &finder {
                Some(f) => f.find_from(line, 0),
                None => legacy::memmem_from(line, needle, 0),
            };
            hits += found.is_some() as usize;
        }
        hits
    }

    /// `find_byte` until it runs out, one byte past each hit: the line splitter's shape.
    fn find_sweep(hay: &[u8], b: u8, legacy: bool) -> usize {
        let mut at = 0;
        let mut hits = 0;
        loop {
            let hit = if legacy {
                legacy::find_byte(hay, b, at)
            } else {
                find_byte(hay, b, at)
            };
            match hit {
                Some(p) => {
                    hits += 1;
                    at = p + 1;
                }
                None => return hits,
            }
        }
    }

    fn best_secs(mut f: impl FnMut() -> usize) -> f64 {
        let mut best = f64::INFINITY;
        for _ in 0..3 {
            let start = Instant::now();
            black_box(f());
            best = best.min(start.elapsed().as_secs_f64());
        }
        best
    }

    /// Best-of-three seconds for one full sweep over the corpus.
    fn sweep(buf: &[u8], lines: &[(usize, usize)], needle: &[u8], legacy: bool) -> f64 {
        let iterations = if cfg!(debug_assertions) { 1 } else { 3 };
        let mut best = f64::INFINITY;
        for _ in 0..iterations {
            let start = Instant::now();
            black_box(count_lines(black_box(buf), lines, needle, legacy));
            best = best.min(start.elapsed().as_secs_f64());
        }
        best
    }

    /// One strategy only, table prebuilt, so the columns isolate the scan from its setup.
    #[derive(Clone, Copy)]
    enum Strat {
        Anchor,
        Skip,
    }

    /// The same two strategies on one whole-buffer call, which is the shape a long needle
    /// meets in `memmem`/`count_matches` and inside the regex literal prefilter.
    fn whole_sweep(buf: &[u8], needle: &[u8], strat: Strat) -> f64 {
        if needle.len() < 2 {
            return f64::INFINITY;
        }
        let finder = Finder::new(needle, false).forced(matches!(strat, Strat::Skip));
        let mut best = f64::INFINITY;
        for _ in 0..3 {
            let start = Instant::now();
            black_box(finder.find_from(buf, 0));
            best = best.min(start.elapsed().as_secs_f64());
        }
        best
    }

    fn strategy_sweep(buf: &[u8], lines: &[(usize, usize)], needle: &[u8], strat: Strat) -> f64 {
        let finder = Finder::new(needle, false).forced(matches!(strat, Strat::Skip));
        let mut best = f64::INFINITY;
        for _ in 0..3 {
            let start = Instant::now();
            let mut hits = 0;
            for &(start, end) in lines {
                hits += finder.find_from(&buf[start..end], 0).is_some() as usize;
            }
            black_box(hits);
            best = best.min(start.elapsed().as_secs_f64());
        }
        best
    }

    /// Old against new on one real corpus, one line at a time, the way `search` drives it.
    ///
    /// `DUSAGE_BENCH_CORPUS` picks the tree (default `/usr/local/go`); `DUSAGE_BENCH_MB` the
    /// sample size. Release only: `cargo test --release bench_literal_matcher -- --ignored`.
    #[test]
    #[ignore = "benchmark: run with --release --ignored"]
    fn bench_literal_matcher() {
        let root = std::env::var("DUSAGE_BENCH_CORPUS").unwrap_or_else(|_| "/usr/local/go".into());
        let mb: usize = std::env::var("DUSAGE_BENCH_MB")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(64);
        let Ok(meta) = std::fs::metadata(&root) else {
            eprintln!("no corpus at {root}, nothing to measure");
            return;
        };
        assert!(meta.is_dir(), "corpus must be a directory");
        let (buf, lines) = corpus(&root, mb << 20);
        let bytes: usize = lines.iter().map(|&(s, e)| e - s).sum();
        let line_mb = bytes as f64 / (1u64 << 20) as f64;
        eprintln!(
            "\n{root}: {} lines, {line_mb:.1} MiB, avg {} bytes/line",
            lines.len(),
            bytes / lines.len().max(1)
        );
        eprintln!(
            "{:<42} {:>5} {:>9} {:>9} {:>8} {:>9} {:>9} {:>7}",
            "needle", "len", "old MB/s", "new MB/s", "speedup", "anchor", "horspool", "hits"
        );
        let whole_mb = buf.len() as f64 / (1u64 << 20) as f64;

        eprintln!(
            "\n{:<42} {:>5} {:>9} {:>9} {:>8} {:>8} {:>7}",
            "crossover needle", "len", "line-anch", "line-skip", "buf-anch", "buf-skip", "hits"
        );
        let crossover: Vec<&[u8]> = [8usize, 12, 16, 20, 24, 28, 32, 40, 48]
            .iter()
            .map(|&n| &CROSSOVER[..n.min(CROSSOVER.len())])
            .collect();
        for needle in crossover.into_iter().chain([CROSSOVER]) {
            let anchor_mbs = line_mb / strategy_sweep(&buf, &lines, needle, Strat::Anchor);
            let skip_mbs = line_mb / strategy_sweep(&buf, &lines, needle, Strat::Skip);
            eprintln!(
                "{:<42} {:>5} {:>9.0} {:>9.0} {:>8.0} {:>8.0} {:>7}",
                String::from_utf8_lossy(needle),
                needle.len(),
                anchor_mbs,
                skip_mbs,
                whole_mb / whole_sweep(&buf, needle, Strat::Anchor),
                whole_mb / whole_sweep(&buf, needle, Strat::Skip),
                count_lines(&buf, &lines, needle, false)
            );
        }

        // A whole-buffer sweep at one byte per call, which is exactly how the line splitter
        // drives `find_byte`; absent bytes make it a single full pass.
        eprintln!("\nwhole-buffer single-byte sweep of {whole_mb:.1} MiB, one call per hit");
        eprintln!(
            "{:<12} {:>7} {:>10} {:>10} {:>8}",
            "byte", "hits", "old GB/s", "new GB/s", "speedup"
        );
        for (label, b) in [
            ("newline", b'\n'),
            ("e", b'e'),
            ("q", b'q'),
            ("absent", 0x01),
        ] {
            let old = best_secs(|| find_sweep(black_box(&buf), b, true));
            let new = best_secs(|| find_sweep(black_box(&buf), b, false));
            eprintln!(
                "{label:<12} {:>7} {:>10.2} {:>10.2} {:>7.2}x",
                count_byte(&buf, b),
                whole_mb / 1024.0 / old,
                whole_mb / 1024.0 / new,
                old / new
            );
        }

        eprintln!();
        for needle in BENCH_NEEDLES {
            let label = String::from_utf8_lossy(needle).escape_debug().to_string();
            let label: String = label.chars().take(40).collect();
            let old = sweep(&buf, &lines, needle, true);
            let new = sweep(&buf, &lines, needle, false);
            let hits = count_lines(&buf, &lines, needle, false);
            assert_eq!(
                hits,
                count_lines(&buf, &lines, needle, true),
                "hit count differs for {label}"
            );
            let anchor_mbs = line_mb / strategy_sweep(&buf, &lines, needle, Strat::Anchor);
            let skip_mbs = if needle.len() >= 2 {
                line_mb / strategy_sweep(&buf, &lines, needle, Strat::Skip)
            } else {
                0.0
            };
            eprintln!(
                "{label:<42} {:>5} {:>9.0} {:>9.0} {:>7.2}x {:>9.0} {:>9.0} {hits:>7}",
                needle.len(),
                line_mb / old,
                line_mb / new,
                old / new,
                anchor_mbs,
                skip_mbs
            );
        }
    }

    #[test]
    fn every_needle_length_agrees_with_naive() {
        // A distinct run per offset keeps accidental repeats out of the picture for needles up
        // to 40 bytes, which is where an off-by-one in the block/tail split would show up.
        let hay: Vec<u8> = (0..=250u16)
            .map(|i| (i % 251) as u8)
            .cycle()
            .take(600)
            .collect();
        for n in 1..=40usize {
            let starts = [
                0,
                1,
                7,
                8,
                9,
                15,
                16,
                17,
                31,
                32,
                33,
                63,
                64,
                65,
                200,
                600 - n,
            ];
            for at in starts {
                let needle = &hay[at..at + n];
                assert_eq!(memmem(&hay, needle), naive(&hay, needle), "len {n} at {at}");
                assert_eq!(
                    Finder::new(needle, false).find(&hay),
                    naive(&hay, needle),
                    "finder len {n} at {at}"
                );
                for from in [at.saturating_sub(1), at, at + 1, hay.len() - n] {
                    let want = naive(&hay[from..], needle).map(|p| from + p);
                    assert_eq!(memmem_from(&hay, needle, from), want, "from {from}");
                    assert_eq!(Finder::new(needle, false).find_from(&hay, from), want);
                }
                // A needle that differs in one interior byte must miss.
                if n > 2 {
                    let mut miss = needle.to_vec();
                    miss[1] ^= 0x80;
                    assert_eq!(memmem(&hay, &miss), naive(&hay, &miss), "miss len {n}");
                }
            }
        }
    }

    #[test]
    fn repeated_byte_needles() {
        let runs = [b"a".as_slice(), b"\\", &[0u8], &[0xFF]];
        for run in runs {
            for len in 1..=40usize {
                let needle = vec![run[0]; len];
                for hay_len in [0, len.saturating_sub(1), len, len + 1, 64, 200] {
                    let hay = vec![run[0]; hay_len];
                    assert_eq!(
                        memmem(&hay, &needle),
                        naive(&hay, &needle),
                        "{hay_len}/{len}"
                    );
                    assert_eq!(Finder::new(&needle, false).find(&hay), naive(&hay, &needle));
                    assert_eq!(count_matches(&hay, &needle), naive_count(&hay, &needle));
                }
                // One byte short at the end: the needle must not match.
                let mut hay = vec![run[0]; len + 8];
                hay[len + 4] = b'x';
                assert_eq!(memmem(&hay, &needle), naive(&hay, &needle));
            }
        }
        for (hay, needle) in [
            (&b"abababababab"[..], &b"abab"[..]),
            (&b"abababababab"[..], &b"babab"[..]),
            (&b"aaaaaaaaab"[..], &b"aaaab"[..]),
            (&b"abcabcabcabc"[..], &b"abcabc"[..]),
        ] {
            assert_eq!(memmem(hay, needle), naive(hay, needle));
            for from in 0..=hay.len() {
                assert_eq!(
                    memmem_from(hay, needle, from),
                    naive(&hay[from..], needle).map(|p| from + p)
                );
            }
        }
    }

    /// The search from `from` a byte at a time, which is the reference every plan this module
    /// can choose has to answer to.
    fn brute_from(hay: &[u8], needle: &[u8], from: usize, ci: bool) -> Option<usize> {
        if from > hay.len() {
            return None;
        }
        let n = needle.len();
        if n == 0 {
            return Some(from);
        }
        let last = hay.len().checked_sub(n)?;
        (from..=last).find(|&p| match ci {
            true => hay[p..p + n].eq_ignore_ascii_case(needle),
            false => &hay[p..p + n] == needle,
        })
    }

    /// Every entry point of the prepared [`Finder`], and the free functions that share its plan,
    /// against [`brute_from`], over haystacks built to cross the boundaries the plan divides on:
    /// the 32-byte block the pair filter steps by, the 128-byte haystack length from which its
    /// vector loop runs, the 64-byte needle length at which Horspool takes over, and the ends of
    /// the buffer. Needles are ASCII, non-ASCII and longer than the haystack; matches are
    /// planted at the start, at a block edge and at the end.
    #[test]
    fn finder_entry_points_agree_with_the_brute_force_scan() {
        // Enough for accidental repeats, and two bytes above 0x80 so the fold never reaches a
        // byte it does not own.
        const POOL: &[u8] = b"abAB cC.-_\xc3\xa9\x00";
        let mut rng = Lcg::new(0x51ED_0A77);
        let mut cases = 0usize;
        for n in [1usize, 2, 3, 5, 8, 15, 16, 17, 32, 33, 40, 63, 64, 65, 127] {
            for len in [0usize, 3, 33, 64, 127, 128, 129, 200, 512, 513] {
                let needle: Vec<u8> = (0..n)
                    .map(|_| POOL[rng.below(POOL.len() as u32) as usize])
                    .collect();
                let mut hay: Vec<u8> = (0..len)
                    .map(|_| POOL[rng.below(POOL.len() as u32) as usize])
                    .collect();
                // One planted copy, at an offset that puts a block edge inside the needle when
                // the haystack has room for it.
                if n <= len {
                    let at = (31 + n % 3).min(len - n);
                    hay[at..at + n].copy_from_slice(&needle);
                }
                for ci in [false, true] {
                    let finder = Finder::new(&needle, ci);
                    assert_eq!(finder.len(), n);
                    assert_eq!(finder.is_empty(), n == 0);
                    let mut froms = vec![
                        0,
                        1,
                        15,
                        16,
                        31,
                        32,
                        33,
                        63,
                        64,
                        65,
                        126,
                        127,
                        128,
                        129,
                        len.saturating_sub(1),
                        len,
                        len + 1,
                    ];
                    if len <= 160 {
                        froms.extend(0..=len);
                    }
                    for from in froms {
                        let want = brute_from(&hay, &needle, from, ci);
                        let label = format!("n {n} len {len} from {from} ci {ci}");
                        assert_eq!(finder.find_from(&hay, from), want, "{label}");
                        if from == 0 {
                            assert_eq!(finder.find(&hay), want, "{label}");
                        }
                        if ci {
                            assert_eq!(memmem_ci_from(&hay, &needle, from), want, "{label}");
                            if from == 0 {
                                assert_eq!(memmem_ci(&hay, &needle), want, "{label}");
                            }
                        } else {
                            assert_eq!(memmem_from(&hay, &needle, from), want, "{label}");
                            if from == 0 {
                                assert_eq!(memmem(&hay, &needle), want, "{label}");
                                assert_eq!(
                                    memmem_word(&hay, &needle, from),
                                    naive_words(&hay, &needle, from),
                                    "{label}"
                                );
                            }
                        }
                        cases += 1;
                    }
                }
            }
        }
        assert!(cases > 10_000, "only {cases} cases ran");
    }

    /// A caller-chosen anchor changes which byte the scan looks for and nothing else: every
    /// offset answers what the brute-force scan answers, an offset past the needle's end is the
    /// preference-free case, and an empty needle stays empty.
    #[test]
    fn a_chosen_anchor_answers_what_the_brute_force_scan_answers() {
        const POOL: &[u8] = b"abAB c.{}\xc3";
        let mut rng = Lcg::new(0xA0C_4017);
        for n in [0usize, 1, 2, 5, 9, 16, 40, 70] {
            let needle: Vec<u8> = (0..n)
                .map(|_| POOL[rng.below(POOL.len() as u32) as usize])
                .collect();
            for len in [0usize, 7, 64, 130, 513] {
                let mut hay: Vec<u8> = (0..len)
                    .map(|_| POOL[rng.below(POOL.len() as u32) as usize])
                    .collect();
                if n > 0 && n < len {
                    let at = (63).min(len - n);
                    hay[at..at + n].copy_from_slice(&needle);
                }
                for ci in [false, true] {
                    for anchor in 0..n + 2 {
                        let finder = Finder::with_anchor(&needle, ci, anchor);
                        assert_eq!(finder.len(), n);
                        for from in [0, 1, 31, 64, 127, 128, len] {
                            assert_eq!(
                                finder.find_from(&hay, from),
                                brute_from(&hay, &needle, from, ci),
                                "n {n} len {len} ci {ci} anchor {anchor} from {from}"
                            );
                        }
                    }
                    assert_eq!(
                        Finder::with_anchor(&needle, ci, usize::MAX).find(&hay),
                        Finder::new(&needle, ci).find(&hay),
                        "out of range is no preference"
                    );
                }
            }
        }
    }

    /// Folded matches planted so a block edge falls inside them: the pair filter's second byte
    /// can land on either side of the vector load, and the fold has to hold across it.
    #[test]
    fn folded_matches_straddle_every_block_edge() {
        for n in 2..=8usize {
            let upper: Vec<u8> = (0..n)
                .map(|k| if k % 2 == 0 { b'A' } else { b'B' })
                .collect();
            let lower: Vec<u8> = (0..n)
                .map(|k| if k % 2 == 0 { b'a' } else { b'b' })
                .collect();
            // A mix of the two, so the match is neither wholly upper nor wholly lower case.
            let folded: Vec<u8> = (0..n)
                .map(|k| if k % 2 == 0 { b'A' } else { b'b' })
                .collect();
            for at in [31usize, 32, 33, 63, 64, 65, 126, 127, 128, 129, 200] {
                let mut hay = vec![b'-'; at + n + 64];
                hay[at..at + n].copy_from_slice(&lower);
                let finder = Finder::new(&upper, true);
                for from in 0..=at + 1 {
                    let want = Some(at).filter(|&p| p >= from);
                    assert_eq!(
                        finder.find_from(&hay, from),
                        want,
                        "n {n} at {at} from {from}"
                    );
                    assert_eq!(memmem_ci_from(&hay, &upper, from), want);
                    assert_eq!(brute_from(&hay, &upper, from, true), want);
                }
                assert_eq!(Finder::new(&folded, true).find(&hay), Some(at));
            }
        }
    }

    #[test]
    fn needles_at_buffer_boundaries() {
        let mut rng = Lcg::new(0xB0A7_DA7A);
        for n in 1..=40usize {
            for extra in [0usize, 1, 7, 8, 9, 16, 31, 32, 33, 64, 65] {
                let hay_len = n + extra;
                let hay: Vec<u8> = (0..hay_len).map(|_| b'a' + rng.below(3) as u8).collect();
                for at in [0, hay_len - n] {
                    let needle = hay[at..at + n].to_vec();
                    assert_eq!(memmem(&hay, &needle), naive(&hay, &needle), "{n}/{extra}");
                    assert_eq!(Finder::new(&needle, false).find(&hay), naive(&hay, &needle));
                    assert_eq!(memmem_from(&hay, &needle, at), Some(at));
                    for from in 0..=hay_len {
                        assert_eq!(
                            memmem_from(&hay, &needle, from),
                            naive(&hay[from.min(hay_len)..], &needle)
                                .map(|p| p + from.min(hay_len))
                        );
                    }
                }
            }
        }
        // Every byte position of a buffer that ends mid-word is reachable by `find_byte`.
        for len in 1..=70usize {
            let mut hay = vec![b'.'; len];
            hay[len - 1] = b'X';
            assert_eq!(find_byte(&hay, b'X', 0), Some(len - 1), "len {len}");
            assert_eq!(find_byte(&hay, b'X', len - 1), Some(len - 1));
            assert_eq!(find_byte(&hay, b'X', len), None);
            let mut front = vec![b'.'; len];
            front[0] = b'X';
            assert_eq!(find_byte(&front, b'X', 0), Some(0), "len {len}");
            assert_eq!(find_byte(&front, b'X', 1), None);
        }
    }

    #[test]
    fn overlapping_matches_advance_one_byte() {
        let hay = b"aaaaabaaaaabaaaaa";
        for needle in [&b"aab"[..], &b"aaab"[..], &b"aaaaab"[..], &b"ba"[..]] {
            for from in 0..=hay.len() {
                assert_eq!(
                    memmem_from(hay, needle, from),
                    naive(&hay[from..], needle).map(|p| from + p),
                    "needle {needle:?} from {from}"
                );
            }
            assert_eq!(count_matches(hay, needle), naive_count(hay, needle));
        }
        assert_eq!(memmem(b"aaa", b"aa"), Some(0));
        assert_eq!(memmem_from(b"aaa", b"aa", 1), Some(1));
        assert_eq!(count_matches(b"aaaa", b"aa"), 2);
    }

    #[test]
    fn case_insensitive_mixed_case_lengths() {
        let mut rng = Lcg::new(0x1CE_5EED);
        let alpha = *b"abABcC-_";
        for n in 1..=40usize {
            for _ in 0..8 {
                let hay_len = 20 + rng.below(120) as usize;
                let hay: Vec<u8> = (0..hay_len)
                    .map(|_| alpha[rng.below(alpha.len() as u32) as usize])
                    .collect();
                let mut needle: Vec<u8> = (0..n)
                    .map(|_| alpha[rng.below(alpha.len() as u32) as usize])
                    .collect();
                if rng.below(2) == 0 && n <= hay_len {
                    let at = rng.below((hay_len - n + 1) as u32) as usize;
                    needle.copy_from_slice(&hay[at..at + n]);
                    // Flip the case of every other letter, so only folding can match.
                    for b in needle.iter_mut().step_by(2) {
                        b.make_ascii_uppercase();
                    }
                }
                assert_eq!(
                    memmem_ci(&hay, &needle),
                    naive_ci(&hay, &needle),
                    "len {n} hay {hay:?} needle {needle:?}"
                );
                assert_eq!(
                    Finder::new(&needle, true).find(&hay),
                    naive_ci(&hay, &needle),
                    "finder len {n}"
                );
                for from in 0..=hay_len + 1 {
                    let want = if from > hay_len {
                        None
                    } else {
                        naive_ci(&hay[from..], &needle).map(|p| from + p)
                    };
                    assert_eq!(memmem_ci_from(&hay, &needle, from), want);
                    assert_eq!(Finder::new(&needle, true).find_from(&hay, from), want);
                }
            }
        }
    }

    /// Thousands of deterministic random cases, both case rules, both the free functions and
    /// the prepared `Finder`, all against the naive reference.
    #[test]
    fn cross_check_thousands_of_random_cases() {
        let mut rng = Lcg::new(0xD15C_0DE5);
        let alpha = *b"abAB\n().,;:";
        let mut cases = 0;
        for _ in 0..2500 {
            let hay_len = rng.below(200) as usize;
            let hay: Vec<u8> = (0..hay_len)
                .map(|_| alpha[rng.below(alpha.len() as u32) as usize])
                .collect();
            let n = 1 + rng.below(45) as usize;
            let needle: Vec<u8> = if rng.below(2) == 0 && n <= hay_len {
                let at = rng.below((hay_len - n + 1) as u32) as usize;
                hay[at..at + n].to_vec()
            } else {
                (0..n)
                    .map(|_| alpha[rng.below(alpha.len() as u32) as usize])
                    .collect()
            };
            let exact = Finder::new(&needle, false);
            let folded = Finder::new(&needle, true);
            assert_eq!(memmem(&hay, &needle), naive(&hay, &needle));
            assert_eq!(exact.find(&hay), naive(&hay, &needle));
            assert_eq!(memmem_ci(&hay, &needle), naive_ci(&hay, &needle));
            assert_eq!(folded.find(&hay), naive_ci(&hay, &needle));
            assert_eq!(count_matches(&hay, &needle), naive_count(&hay, &needle));
            let from = rng.below((hay_len + 2) as u32) as usize;
            let want = if from > hay_len {
                None
            } else {
                naive(&hay[from..], &needle).map(|p| from + p)
            };
            assert_eq!(memmem_from(&hay, &needle, from), want);
            assert_eq!(exact.find_from(&hay, from), want);
            let _ = find_byte(&hay, needle[0], from);
            cases += 1;
        }
        assert_eq!(cases, 2500);
    }

    #[test]
    fn finder_keeps_the_free_function_semantics() {
        let cases: [(&[u8], &[u8]); 8] = [
            (b"", b""),
            (b"abc", b""),
            (b"", b"a"),
            (b"hello world", b"world"),
            (b"ab", b"abc"),
            (b"Hello World", b"hello"),
            (b"\xC1\xE1", b"\xE1"),
            (b"aaaa", b"aa"),
        ];
        for (hay, needle) in cases {
            for ci in [false, true] {
                let finder = Finder::new(needle, ci);
                let (want, want_ci) = (naive(hay, needle), naive_ci(hay, needle));
                assert_eq!(finder.find(hay), if ci { want_ci } else { want });
                for from in 0..=hay.len() + 1 {
                    let want = if from > hay.len() {
                        None
                    } else if ci {
                        naive_ci(&hay[from..], needle).map(|p| from + p)
                    } else {
                        naive(&hay[from..], needle).map(|p| from + p)
                    };
                    assert_eq!(finder.find_from(hay, from), want, "ci {ci} from {from}");
                }
            }
        }
    }

    #[test]
    fn case_insensitive_mixed_case_needles() {
        let hay = b"the Memmove ctor and MEMMOVE again";
        for needle in [&b"memmove"[..], b"MeMmOvE", b"MEMMOVE", b"mEmMoVe"] {
            assert_eq!(memmem_ci(hay, needle), Some(4), "{needle:?}");
            assert_eq!(Finder::new(needle, true).find(hay), Some(4), "{needle:?}");
        }
        // A mixed-case needle past the first word of a longer buffer, so the block loop and
        // the tail word are both exercised.
        let long: Vec<u8> = (0..41).map(|_| b'.').chain("MemMove".bytes()).collect();
        for needle in [&b"mEmMoVe"[..], b"MEMmove", b"memmove"] {
            assert_eq!(memmem_ci(&long, needle), Some(41), "{needle:?}");
            assert_eq!(Finder::new(needle, true).find(&long), Some(41));
        }
        assert_eq!(memmem_ci_from(&long, b"mEmmove", 42), None);
        assert_eq!(memmem_ci_from(&long, b"mEmmove", 41), Some(41));
    }

    #[test]
    fn case_insensitive_needle_at_every_offset_of_33_bytes() {
        // 33 bytes whose folded values are all distinct, so a needle cut from offset `at`
        // occurs exactly once and the expected answer is `at`, whichever side of a word
        // boundary it starts and ends on.
        let base: Vec<u8> = (0..33u8)
            .map(|i| if i < 26 { b'a' + i } else { b'0' + (i - 26) })
            .collect();
        for mode in 0..4 {
            let hay: Vec<u8> = base
                .iter()
                .enumerate()
                .map(|(i, &c)| match (mode, i % 2) {
                    (1, _) | (2, 0) | (3, 1) if c.is_ascii_alphabetic() => c - 32,
                    _ => c,
                })
                .collect();
            assert_eq!(hay.len(), 33);
            for at in 0..hay.len() {
                for len in 1..=hay.len() - at {
                    let needle = &hay[at..at + len];
                    assert_eq!(
                        memmem_ci(&hay, needle),
                        Some(at),
                        "mode {mode} at {at} len {len}"
                    );
                    assert_eq!(Finder::new(needle, true).find(&hay), Some(at));
                    assert_eq!(memmem_ci_from(&hay, needle, at), Some(at));
                    assert_eq!(memmem_ci_from(&hay, needle, at + 1), None);
                    assert_eq!(memmem(&hay, needle), Some(at), "exact at {at} len {len}");
                }
            }
        }
    }

    #[test]
    fn case_insensitive_bytes_around_the_case_bit() {
        // `@` (0x40) and `` ` `` (0x60) differ only in bit 5, which folding forces to 1, so a
        // folded hit on one is only a candidate for the other: nothing may match it outright.
        assert_eq!(memmem_ci(b"\x40", b"\x60"), None);
        assert_eq!(memmem_ci(b"\x60", b"\x40"), None);
        assert_eq!(Finder::new(b"\x60", true).find(b"\x40"), None);
        assert_eq!(Finder::new(b"\x40", true).find(b"\x60"), None);
        assert_eq!(memmem_ci(&[b'@'; 40], b"`"), None);
        assert_eq!(memmem_ci(&[b'`'; 40], b"@"), None);
        assert_eq!(memmem_ci(b"@`@`@`@`@`", b"@"), Some(0));
        assert_eq!(memmem_ci(b"@`@`@`@`@`", b"`"), Some(1));
        assert_eq!(memmem_ci(&[b'@'; 40], b"``"), None);
        assert_eq!(memmem_ci(b"\x40\x40", b"``"), None);
        for (lo, hi) in [
            (b'[', b'{'),
            (b'\\', b'|'),
            (b']', b'}'),
            (b'^', b'~'),
            (b'_', 0x7F),
        ] {
            let left = [lo; 40];
            let right = [hi; 40];
            assert_eq!(memmem_ci(&left, &[hi]), None, "{lo:#x} vs {hi:#x}");
            assert_eq!(memmem_ci(&right, &[lo]), None, "{hi:#x} vs {lo:#x}");
            assert_eq!(memmem_ci(&left, &[lo]), Some(0));
            assert_eq!(memmem_ci(&right, &[hi]), Some(0));
            let mut mixed = left.to_vec();
            mixed.extend_from_slice(&right);
            assert_eq!(memmem_ci(&mixed, &[lo, lo]), Some(0));
            assert_eq!(memmem_ci(&mixed, &[hi, hi]), Some(40));
            assert_eq!(memmem_ci(&mixed, &[lo, hi]), Some(39));
            assert_eq!(memmem_ci(&mixed, &[hi, lo]), None);
            assert_eq!(Finder::new(&[hi], true).find(&left), None);
            assert_eq!(Finder::new(&[lo], true).find(&right), None);
        }
        // Letters still fold across those neighbours, and a letter needle ignores them.
        assert_eq!(memmem_ci(b"A@B`C", b"a@b`c"), Some(0));
        assert_eq!(Finder::new(b"a@b`c", true).find(b"A@B`C"), Some(0));
        assert_eq!(memmem_ci(&[b'@'; 40], b"a"), None);
        assert_eq!(Finder::new(b"@", true).find(&[b'`'; 40]), None);
    }

    /// Cross-check against the naive folded search on input weighted towards the bytes the fold
    /// has to be careful about, through both the free functions and the prepared `Finder`.
    #[test]
    fn case_insensitive_agrees_with_naive_on_awkward_bytes() {
        const ALPHABET: &[u8] = b"aAbB@`[{\\^_]}~ zZ09.,-\x00\x7f\xc1\xe1";
        let mut rng = Lcg::new(0x00C0_FFEE_1234);
        let mut checked = 0;
        for _ in 0..4000 {
            let hay_len = rng.below(140) as usize;
            let hay: Vec<u8> = (0..hay_len)
                .map(|_| ALPHABET[rng.below(ALPHABET.len() as u32) as usize])
                .collect();
            // Half the needles are cut out of the haystack, so a fair share of them occur.
            let n = 1 + rng.below(12) as usize;
            let needle: Vec<u8> = if n <= hay_len && rng.below(2) == 0 {
                let at = rng.below((hay_len - n + 1) as u32) as usize;
                hay[at..at + n].to_vec()
            } else {
                (0..n)
                    .map(|_| ALPHABET[rng.below(ALPHABET.len() as u32) as usize])
                    .collect()
            };
            let finder = Finder::new(&needle, true);
            assert_eq!(
                memmem_ci(&hay, &needle),
                naive_ci(&hay, &needle),
                "hay {hay:?} needle {needle:?}"
            );
            assert_eq!(finder.find(&hay), naive_ci(&hay, &needle));
            for from in [0, rng.below(hay_len as u32 + 2) as usize, hay_len] {
                let want = if from > hay_len {
                    None
                } else {
                    naive_ci(&hay[from..], &needle).map(|p| from + p)
                };
                assert_eq!(
                    memmem_ci_from(&hay, &needle, from),
                    want,
                    "from {from} hay {hay:?} needle {needle:?}"
                );
                assert_eq!(finder.find_from(&hay, from), want);
            }
            checked += 1;
        }
        assert_eq!(checked, 4000);
    }

    /// The `-i` scan must cost what the case-sensitive scan costs per byte, on a whole buffer
    /// and on the one-call-per-line sweep `dusage search` drives. `ratio` is `-i` over
    /// case-sensitive, so 1.00 is parity.
    #[test]
    fn bench_ci_vs_exact() {
        const HAY_LEN: usize = 4 << 20;
        let hay = english_like(HAY_LEN, 0x1234_5678_9ABC);
        let lines: Vec<&[u8]> = hay
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        let bytes = lines.iter().map(|l| l.len()).sum::<usize>();
        // The word list has no 'z': these are absent, so the sweep measures scanning rather
        // than verification while still folding every word it touches. The last needle is long
        // enough to be searched by the Horspool table, which scans for no single byte at all.
        let needles: [(&str, &[u8]); 3] = [
            ("absent 2", b"zz"),
            ("absent 8", b"zzwrtxcv"),
            ("absent 20 (skip)", b"zzxw kvbx mnbx vcxx qz"),
        ];
        let iterations = if cfg!(debug_assertions) { 1 } else { 5 };
        let mbs = |ms: f64, n: usize| n as f64 / 1e6 / (ms / 1e3);

        eprintln!(
            "\ncase-sensitive vs -i on {} MiB of English-like text, {} lines of {:.0} bytes \
             (best of {iterations})",
            HAY_LEN >> 20,
            lines.len(),
            bytes as f64 / lines.len() as f64
        );
        eprintln!(
            "{:<20} {:>10} {:>10} {:>7} {:>10} {:>10} {:>7}",
            "needle", "buffer cs", "buffer -i", "ratio", "lines cs", "lines -i", "ratio"
        );

        for (label, needle) in needles {
            let exact = Finder::new(needle, false);
            let folded = Finder::new(needle, true);
            assert_eq!(exact.find(&hay), None);
            assert_eq!(folded.find(&hay), None);
            let hit_lines = |f: &Finder| {
                lines
                    .iter()
                    .filter(|l| f.find(black_box(*l)).is_some())
                    .count()
            };
            let buf_cs = best_millis(iterations, || {
                exact.find(black_box(&hay)).unwrap_or(usize::MAX)
            });
            let buf_ci = best_millis(iterations, || {
                folded.find(black_box(&hay)).unwrap_or(usize::MAX)
            });
            let line_cs = best_millis(iterations, || hit_lines(&exact));
            let line_ci = best_millis(iterations, || hit_lines(&folded));
            eprintln!(
                "{:<20} {:>10.0} {:>10.0} {:>6.2}x {:>10.0} {:>10.0} {:>6.2}x",
                label,
                mbs(buf_cs, HAY_LEN),
                mbs(buf_ci, HAY_LEN),
                buf_ci / buf_cs,
                mbs(line_cs, bytes),
                mbs(line_ci, bytes),
                line_ci / line_cs
            );
        }
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
