//! Timing harness for the regex engine: nanoseconds per byte for the patterns a `search`
//! spends its time on.
//!
//! `cargo run --release --example perf_dfa`
//!
//! Each pattern is run through [`Pattern`] exactly the way `src/search.rs` runs it — a
//! block-safe pattern through the block scan (`block_accept_end`, then `block_match` on the
//! line it lands on), everything else line by line through `find_at` — over two deterministic
//! buffers. A run is one whole pass over a buffer, so ns/byte covers the automaton walk, the
//! skips and the matched lines, and nothing that is not the search.
//!
//! The first buffer is Go-like source. The second is what a search spends its time on in a
//! tree that is not all source: the tool binaries beside it, whose bytes have no line
//! structure worth the name, an uppercase letter or two in ten, and a `.go` path every twenty
//! kilobytes — so the automaton cannot jump over much and every position has to be walked.
//! Both `corpus` and `binary_corpus` print their own statistics, and those are the proportions
//! of the trees they stand in for, so a number here can be compared with the CLI it is
//! standing in for.

use dusage::bytes;
use dusage::pattern::Pattern;
use dusage::regex::Regex;
use std::time::Instant;

/// The buffer every pattern is timed over.
const CORPUS: usize = 4 << 20;
/// Blocks the scanner feeds the engine: `search` reads a file a megabyte at a time once it
/// has proved larger than one.
const BLOCK: usize = 1 << 20;
const WARMUP: usize = 2;
const RUNS: usize = 5;
const REPEATS: usize = 3;

fn main() {
    let text = corpus(CORPUS);
    report("Go-like source", &text, walk, None);
    // A binary is searched block by block with a plain `find_at`, since it has no lines to
    // scan for, so every pattern in this table is on that one path.
    let binary = binary_corpus(CORPUS);
    report("binary content", &binary, walk_binary, Some("binary"));
}

/// One table over one buffer, driven the way the CLI drives it for that kind of file.
fn report(
    label: &str,
    buf: &[u8],
    walk: fn(&Pattern, &[&[u8]]) -> usize,
    fixed_path: Option<&str>,
) {
    let upper = buf.iter().filter(|b| b.is_ascii_uppercase()).count();
    let newlines = buf.iter().filter(|&&b| b == b'\n').count();
    println!(
        "{}: {} bytes ({:.1}% A-Z, one newline per {} bytes, {} .go), block {} bytes, \
         {} warmup passes, {} runs per repeat, {} repeats; ns/byte* is the fastest run",
        label,
        buf.len(),
        upper as f64 * 100.0 / buf.len() as f64,
        buf.len() / newlines.max(1),
        buf.windows(3).filter(|w| w == b".go").count(),
        BLOCK,
        WARMUP,
        RUNS,
        REPEATS
    );
    println!(
        "{:<24} {:>7} {:>6} {:>8} {:>8} {:>8} {:>6}",
        "pattern", "matches", "path", "ns/byte", "ns/byte*", "MB/s", "pre"
    );
    for src in SEARCHED {
        let pat = Pattern::new(src, false, false, false).expect("pattern");
        let blocks = blocks(buf);
        let path = fixed_path.unwrap_or(if pat.block_safe() { "block" } else { "line" });
        let re = Regex::new(src).expect("regex");
        let pre = match re.required_literal() {
            Some(run) => String::from_utf8_lossy(run).into_owned(),
            None => "-".to_string(),
        };
        let mut hits = 0;
        for _ in 0..WARMUP {
            hits = walk(&pat, &blocks);
        }
        let mut times = Vec::with_capacity(RUNS * REPEATS);
        for _ in 0..REPEATS {
            for _ in 0..RUNS {
                let t0 = Instant::now();
                hits = walk(&pat, &blocks);
                times.push(t0.elapsed().as_nanos() as f64);
            }
        }
        times.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
        let median = times[times.len() / 2] / buf.len() as f64;
        let min = times[0] / buf.len() as f64;
        println!(
            "{:<24} {:>7} {:>6} {:>8.3} {:>8.3} {:>8.1} {:>6}",
            src,
            hits,
            path,
            median,
            min,
            buf.len() as f64 / times[times.len() / 2] * 1e3,
            pre
        );
    }
    println!();
}

/// The patterns reported: the one the CLI comparison uses, a common class expression, a word
/// boundary (which runs the line path), and a rare and a common start byte to show what
/// skipping is worth. `[Z]{6}[a-z]{9}` is the CLI's never-matching row, whose every byte has to
/// be walked because it has no run to jump between.
const SEARCHED: [&str; 6] = [
    r"[A-Z][a-zA-Z0-9_]*\.go",
    r"[a-z]+_[a-z]+",
    r"\bword\b",
    r"[QZ][a-z]+",
    r"[A-Z][a-z]+",
    r"[Z]{6}[a-z]{9}",
];

/// The buffer cut into the blocks a scanner feeds the engine. Each boundary is moved back to
/// a newline so no line is split: the last newline of a block ends it, as `block_scan` does
/// with `rfind_byte`.
fn blocks(buf: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < buf.len() {
        let mut end = (start + BLOCK).min(buf.len());
        if end < buf.len() {
            if let Some(i) = bytes::rfind_byte(&buf[start..end], b'\n', end - start) {
                end = start + i;
            }
        }
        out.push(&buf[start..end]);
        start = end + 1;
    }
    out
}

/// One pass over every block, the way `Scanner::block_scan` drives a block-safe pattern and
/// `Scanner::feed_chunk` drives the rest. Returns the number of matched lines so the work
/// cannot be optimised away.
fn walk(pat: &Pattern, blocks: &[&[u8]]) -> usize {
    let mut lines = 0;
    if pat.block_safe() {
        for body in blocks {
            let mut pos = 0;
            while let Some(end) = pat.block_accept_end(body, pos) {
                let hit = end.saturating_sub(1).max(pos);
                let le = bytes::find_byte(body, b'\n', hit).unwrap_or(body.len());
                let ls = bytes::rfind_byte(body, b'\n', hit).map_or(0, |i| i + 1);
                lines += 1;
                // The line is looked at again for its exact span, as `block_scan` does.
                if pat.block_match(&body[ls..le], 0).is_none() {
                    break;
                }
                pos = le + 1;
            }
        }
    } else {
        for body in blocks {
            for line in body.split(|&b| b == b'\n') {
                let line = if line.last() == Some(&b'\r') {
                    &line[..line.len() - 1]
                } else {
                    line
                };
                if pat.find_at(line, 0).is_some() {
                    lines += 1;
                }
            }
        }
    }
    lines
}

/// One pass over a binary the way `Scanner` runs one: a file reported by name is scanned block
/// by block with a plain search, there being no line structure to keep. That search stops at the
/// first match, which would make ns/byte depend on where in the buffer the match sat, so this
/// keeps going past it and covers every byte — the same work, with a denominator that means the
/// same thing for a pattern that matches and one that does not. A binary that does not match is
/// the common case and the expensive one, since nothing ends its scan early.
fn walk_binary(pat: &Pattern, blocks: &[&[u8]]) -> usize {
    let mut hits = 0;
    for body in blocks {
        let mut at = 0;
        while at < body.len() {
            let Some(m) = pat.find_at(body, at) else {
                break;
            };
            hits += 1;
            at = m.end.max(m.start + 1);
        }
    }
    hits
}

/// Go-like source, in the proportions real Go source has: about 9% of its bytes uppercase,
/// `.go` in a comment about 77 times per megabyte, `word` about 30 times, and a capitalised
/// file name — the thing `[A-Z][a-zA-Z0-9_]*\.go` matches — about twice.
fn corpus(size: usize) -> Vec<u8> {
    const WORDS: [&str; 24] = [
        "state", "cache", "buffer", "reader", "writer", "index", "value", "slice", "header",
        "result", "context", "server", "client", "handle", "packet", "struct", "field", "offset",
        "length", "source", "target", "worker", "signal", "record",
    ];
    const UPPER: [&str; 18] = [
        "Config",
        "Reader",
        "Writer",
        "Buffer",
        "Server",
        "Client",
        "Handle",
        "Result",
        "Index",
        "Context",
        "Packet",
        "Header",
        "Runtime",
        "Scheduler",
        "Allocator",
        "Span",
        "Page",
        "Mutex",
    ];
    let mut out = Vec::with_capacity(size + 256);
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut pick = |n: usize, shift: u32| (next() >> shift) as usize % n;
    out.extend_from_slice(b"// Package runtime implements the Go runtime.\npackage runtime\n\n");
    while out.len() < size {
        let u = UPPER[pick(UPPER.len(), 3)];
        let u2 = UPPER[pick(UPPER.len(), 7)];
        let u3 = UPPER[pick(UPPER.len(), 19)];
        let w = WORDS[pick(WORDS.len(), 11)];
        let w2 = WORDS[pick(WORDS.len(), 15)];
        let w3 = WORDS[pick(WORDS.len(), 23)];
        match pick(10_000, 0) {
            0..=2999 => {
                out.extend_from_slice(b"\t");
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(b" := ");
                out.extend_from_slice(u.as_bytes());
                out.extend_from_slice(b".");
                out.extend_from_slice(w2.as_bytes());
                out.extend_from_slice(b"(");
                out.extend_from_slice(u2.as_bytes());
                out.extend_from_slice(b", ");
                out.extend_from_slice(u3.as_bytes());
                out.extend_from_slice(b")\n");
            }
            3000..=4599 => {
                out.extend_from_slice(b"func (c *");
                out.extend_from_slice(u.as_bytes());
                out.extend_from_slice(b") ");
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(b"(p *");
                out.extend_from_slice(u2.as_bytes());
                out.extend_from_slice(b") (");
                out.extend_from_slice(u3.as_bytes());
                out.extend_from_slice(b", error) {\n");
            }
            4600..=6899 => {
                out.extend_from_slice(b"// ");
                out.extend_from_slice(u.as_bytes());
                out.extend_from_slice(b" is the ");
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(b" held for the current ");
                out.extend_from_slice(u2.as_bytes());
                out.extend_from_slice(b" and is not reused across\n// the transition.\n");
            }
            6900..=7599 => {
                out.extend_from_slice(b"\tif ");
                out.extend_from_slice(u3.as_bytes());
                out.extend_from_slice(b" != nil && ");
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(
                    b" != nil && len(buf) > 0 {\n\t\treturn errors.New(\"short buffer\")\n\t}\n",
                );
            }
            7600..=8399 => {
                out.extend_from_slice(b"type ");
                out.extend_from_slice(u.as_bytes());
                out.extend_from_slice(b" struct {\n\t");
                out.extend_from_slice(u2.as_bytes());
                out.extend_from_slice(b" []byte\n\t");
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(b" uintptr\n}\n");
            }
            8400..=9099 => {
                out.extend_from_slice(b"\tfor i := 0; i < len(");
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(b"); i++ {\n\t\tsum += uint64(buf[i]) * 31\n\t}\n");
            }
            // A path in a comment: `.go` shows up far more often than a file whose name
            // starts with a capital does.
            9100..=9132 => {
                out.extend_from_slice(b"// see src/runtime/");
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(b".go and the test beside it\n");
            }
            9300..=9301 => {
                out.extend_from_slice(b"//go:linkname ");
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(b" runtime.");
                out.extend_from_slice(u.as_bytes());
                out.extend_from_slice(b".go\n");
            }
            9320..=9335 => {
                out.extend_from_slice(b"\t");
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(b"_");
                out.extend_from_slice(w2.as_bytes());
                out.extend_from_slice(b" := word(\"");
                out.extend_from_slice(w3.as_bytes());
                out.extend_from_slice(b"\")\n");
            }
            _ => {
                out.extend_from_slice(b"\t");
                out.extend_from_slice(u.as_bytes());
                out.extend_from_slice(b" = &");
                out.extend_from_slice(u2.as_bytes());
                out.extend_from_slice(b"{");
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(b": ");
                out.extend_from_slice(u3.as_bytes());
                out.extend_from_slice(b"{}}\n");
            }
        }
    }
    out.truncate(size);
    // The buffer ends on a line boundary so the last block is a whole one.
    while out.last() != Some(&b'\n') {
        out.pop();
    }
    out
}

/// Machine code, in the proportions a Go tool binary has: 23% of its bytes above 0x7f, one
/// uppercase letter in ten, one lowercase letter in ten, one digit in fifty, one newline every
/// 212 bytes, and a `.go` path every 22 KB. One in twenty-four of those paths sits in a
/// capitalised file name, which is the thing `[A-Z][a-zA-Z0-9_]*\.go` matches.
fn binary_corpus(size: usize) -> Vec<u8> {
    const PUNCT: &[u8] = b" \t:;,.[]{}()<>-_=+*/&|!?\"'#$%@^~";
    const WORDS: [&str; 12] = [
        "runtime", "reflect", "internal", "sync", "atomic", "errors", "context", "unicode",
        "strconv", "hash", "sort", "time",
    ];
    const UPPER: [&str; 8] = [
        "Config", "Reader", "Server", "Client", "Handle", "Result", "Context", "Packet",
    ];
    let mut out = Vec::with_capacity(size + 256);
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    // One drawing per byte decides which kind of byte it is, so the shares below are the
    // shares in the buffer.
    let byte = |r: u64| -> u8 {
        let kind = r % 100;
        match kind {
            0..=22 => ((r >> 8) as u8) | 0x80,
            23..=33 => b'A' + ((r >> 16) as u8 % 26),
            34..=44 => b'a' + ((r >> 24) as u8 % 26),
            45..=46 => b'0' + ((r >> 32) as u8 % 10),
            _ => PUNCT[(r >> 40) as usize % PUNCT.len()],
        }
    };
    let mut paths = 0usize;
    while out.len() < size {
        let len = 20 + (next() >> 5) as usize % 384;
        for _ in 0..len {
            let b = byte(next());
            out.push(b);
        }
        out.push(b'\n');
        // A `.go` path turns up at the rate one does in the real binary.
        if next() % 104 == 0 {
            out.extend_from_slice(b"src/");
            out.extend_from_slice(WORDS[(next() >> 11) as usize % WORDS.len()].as_bytes());
            out.extend_from_slice(b"/");
            let name = match paths % 24 {
                0 => UPPER[(next() >> 17) as usize % UPPER.len()],
                _ => WORDS[(next() >> 23) as usize % WORDS.len()],
            };
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b".go");
            paths += 1;
        }
    }
    out.truncate(size);
    // The buffer ends on a line boundary so the last block is a whole one.
    while out.last() != Some(&b'\n') {
        out.pop();
    }
    out
}
