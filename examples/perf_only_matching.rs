//! Timing harness for `search -o` and `search -c`: where the per-match and per-line work goes.
//!
//! `cargo run --release --example perf_only_matching`
//!
//! Two sections, because the cost of `-o` lives in two places:
//!
//! * **modes** — the real path, `search::search` over one file of Go-like source followed by
//!   `report::render_search`, one row per output mode, split into the scan (walk, read, match,
//!   collect hits) and the render (everything printed). `-l` and the plain print are at parity
//!   with grep, so their rows are the floor: whatever `-o` and `-c` add on top of them is the
//!   work this harness is looking for. The render goes into a counting sink rather than a real
//!   stdout, so the row also reports the write calls and bytes the mode issues.
//! * **match walk** — the same buffer searched by each way of finding matches (a block scan
//!   continuing from the previous match end, a per-line `find_all` that restarts every line,
//!   and a per-line `find_at`), in ns/byte. The gap between the first and the second is the
//!   re-scan a per-line `-o` pays for.
//!
//! The corpus prints its own statistics so a number can be compared with the CLI it stands in
//! for (`grep -rIo -F 'func ' /usr/local/go` finds one match per ~2 KB on 244 MB of Go).

use dusage::pattern::Pattern;
use dusage::report;
use dusage::search::{search, SearchOptions};
use std::io::{self, Write};
use std::time::Instant;

/// The buffer the file is made of: big enough that a pass is not all startup.
const CORPUS: usize = 8 << 20;
/// Blocks the scanner feeds the engine (`search` reads a megabyte at a time).
const BLOCK: usize = 1 << 20;
const WARMUP: usize = 1;
const RUNS: usize = 5;
/// The per-line cap `-o` and `--count-matches` apply, as `src/search.rs` holds it.
const MAX_PER_LINE: usize = 1024;
const PATTERN: &str = "func ";

fn main() {
    let buf = corpus(CORPUS);
    println!(
        "buffer {} bytes, {} lines (mean {} bytes), {} '{}' occurrences = 1 per {} bytes",
        buf.len(),
        buf.iter().filter(|b| **b == b'\n').count(),
        buf.len() / buf.iter().filter(|b| **b == b'\n').count().max(1),
        count(&buf, PATTERN.as_bytes()),
        PATTERN,
        buf.len() / count(&buf, PATTERN.as_bytes()).max(1),
    );
    let file = write_temp(&buf);

    println!(
        "\n{} warmup, {} runs, median of the runs; scan = search(), render = render_search()",
        WARMUP, RUNS
    );
    println!(
        "{:<16} {:>8} {:>8} {:>8} {:>7} {:>7} {:>9} {:>7} {:>7}",
        "mode",
        "scan ms",
        "render ms",
        "ns/byte",
        "lines",
        "hits",
        "bytes out",
        "writes",
        "w/match"
    );
    for m in MODES {
        let r = measure(&m, PATTERN, &file, buf.len());
        println!(
            "{:<16} {:>8.2} {:>8.2} {:>8.2} {:>7.0} {:>7.0} {:>9.0} {:>7.0} {:>7.2}",
            m.name, r.scan, r.render, r.ns_byte, r.lines, r.hits, r.bytes, r.writes, r.per_hit
        );
    }

    // What one match costs, split into the scan that found and stored it and the render that
    // printed it: the same buffer at three densities, each against the floor of that mode on a
    // pattern that never matches, so the slope between the rows is the per-match cost.
    println!(
        "\nper-match cost by density (floor = the same mode on a pattern that never matches):"
    );
    println!(
        "{:<8} {:<6} {:>8} {:>7} {:>9} {:>8} {:>9} {:>8}",
        "pattern", "mode", "scan ms", "floor", "ns/match", "render", "ns/match", "hits"
    );
    for m in [&MODES[3], &MODES[2]] {
        let floor = measure(m, "zzqqxxjj", &file, buf.len());
        for (src, label) in [(PATTERN, "1/2KB"), ("struct", "1/4KB"), ("e", "1/0.9B")] {
            let r = measure(m, src, &file, buf.len());
            let per = |t: f64, f: f64| match r.hits > 0.0 && t > f {
                true => format!("{:.1}", (t - f) * 1e6 / r.hits),
                false => "-".to_string(),
            };
            println!(
                "{:<8} {:<6} {:>8.2} {:>7.2} {:>9} {:>8.2} {:>9} {:>8.0}",
                label,
                m.name,
                r.scan,
                floor.scan,
                per(r.scan, floor.scan),
                r.render,
                per(r.render, floor.render),
                r.hits
            );
        }
    }

    println!(
        "\nmatch walk over the whole buffer ({} byte blocks, newline-aligned boundaries):",
        BLOCK
    );
    println!(
        "{:<16} {:>9} {:>8} {:>9}",
        "way", "ns/byte", "matches", "MB/s"
    );
    let blocks = blocks(&buf);
    let pat = Pattern::new(PATTERN, true, false, false).expect("pattern");
    for w in WALKS {
        let mut found = 0;
        for _ in 0..WARMUP {
            found = (w.run)(&pat, &blocks);
        }
        let mut t = Vec::new();
        for _ in 0..RUNS {
            let t0 = Instant::now();
            found = (w.run)(&pat, &blocks);
            t.push(t0.elapsed().as_nanos() as f64);
        }
        let med = median(&mut t);
        println!(
            "{:<16} {:>9.3} {:>8} {:>9.1}",
            w.name,
            med / buf.len() as f64,
            found,
            buf.len() as f64 / med * 1e3
        );
    }
    let _ = std::fs::remove_file(&file);
}

/// One row of the modes table.
struct Mode {
    name: &'static str,
    set: fn(&mut SearchOptions),
}

const MODES: [Mode; 5] = [
    Mode {
        name: "-l",
        set: |o| o.files_with_matches = true,
    },
    Mode {
        name: "plain",
        set: |_| {},
    },
    Mode {
        name: "-c",
        set: |o| o.count = true,
    },
    Mode {
        name: "-o",
        set: |o| o.only_matching = true,
    },
    Mode {
        name: "count-matches",
        set: |o| o.count_matches = true,
    },
];

fn options(m: &Mode, src: &str, file: &std::path::Path) -> SearchOptions {
    let pat = Pattern::new(src, true, false, false).expect("pattern");
    let mut o = SearchOptions {
        threads: threads(),
        ..SearchOptions::new(pat, vec![file.to_path_buf()])
    };
    (m.set)(&mut o);
    o
}

/// Worker threads: one by default, so a row is this machine's single-core work and nothing
/// else; THREADS=n measures what the allocator does when several of them run at once.
fn threads() -> usize {
    std::env::var("THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
}

/// One mode over one pattern: the medians of the scan and the render, and what they produced.
struct Measured {
    scan: f64,
    render: f64,
    ns_byte: f64,
    lines: f64,
    hits: f64,
    bytes: f64,
    writes: f64,
    per_hit: f64,
}

fn measure(m: &Mode, src: &str, file: &std::path::Path, size: usize) -> Measured {
    let mut scan_t = Vec::new();
    let mut render_t = Vec::new();
    let mut lines = 0;
    let mut hits = 0;
    let mut bytes = 0;
    let mut writes = 0;
    for i in 0..WARMUP + RUNS {
        let o = options(m, src, file);
        let t0 = Instant::now();
        let r = search(&o).expect("search");
        let dt = t0.elapsed().as_nanos() as f64;
        let t1 = Instant::now();
        let mut sink = Sink::default();
        report::render_search(&r, &o, false, &mut sink).expect("render");
        let rt = t1.elapsed().as_nanos() as f64;
        if i >= WARMUP {
            scan_t.push(dt);
            render_t.push(rt);
            lines += r.stats.matches;
            hits += r.files.iter().map(|f| f.hits.len() as u64).sum::<u64>();
            bytes += sink.bytes;
            writes += sink.writes;
        }
    }
    let scan = median(&mut scan_t);
    let render = median(&mut render_t);
    let hits = hits as f64 / RUNS as f64;
    let writes = writes as f64 / RUNS as f64;
    Measured {
        scan: scan / 1e6,
        render: render / 1e6,
        ns_byte: (scan + render) / size as f64,
        lines: lines as f64 / RUNS as f64,
        hits,
        bytes: bytes as f64 / RUNS as f64,
        writes,
        per_hit: if hits > 0.0 { writes / hits } else { 0.0 },
    }
}

/// A sink that counts what it is handed: one call is one write, whatever the size.
#[derive(Default)]
struct Sink {
    bytes: u64,
    writes: u64,
}

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.bytes += b.len() as u64;
        self.writes += 1;
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Walk {
    name: &'static str,
    run: fn(&Pattern, &[&[u8]]) -> usize,
}

const WALKS: [Walk; 3] = [
    Walk {
        name: "continued",
        run: walk_continued,
    },
    Walk {
        name: "line find_all",
        run: walk_line_find_all,
    },
    Walk {
        name: "line find_at",
        run: walk_line_find_at,
    },
];

/// Every match in the block found by continuing from the previous match's end: what `-o`
/// costs when the scan does not restart at each line.
fn walk_continued(pat: &Pattern, blocks: &[&[u8]]) -> usize {
    let mut n = 0;
    for body in blocks {
        let mut pos = 0;
        while let Some(m) = pat.block_match(body, pos) {
            if body[m.start..m.end].contains(&b'\n') {
                pos = m.start + 1;
                continue;
            }
            n += 1;
            pos = m.end.max(m.start + 1);
        }
    }
    n
}

/// Every match on every line found by splitting the block at each newline and starting the
/// search at the line's first byte: the per-line walk `handle_line` does for `-o`.
fn walk_line_find_all(pat: &Pattern, blocks: &[&[u8]]) -> usize {
    let mut n = 0;
    for body in blocks {
        for line in body.split(|&b| b == b'\n') {
            n += pat.find_all(line, MAX_PER_LINE).len();
        }
    }
    n
}

/// The plain mode's question only: does the line hold a match.
fn walk_line_find_at(pat: &Pattern, blocks: &[&[u8]]) -> usize {
    let mut n = 0;
    for body in blocks {
        for line in body.split(|&b| b == b'\n') {
            if pat.find_at(line, 0).is_some() {
                n += 1;
            }
        }
    }
    n
}

fn blocks(buf: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < buf.len() {
        let mut end = (start + BLOCK).min(buf.len());
        if end < buf.len() {
            if let Some(i) = dusage::bytes::rfind_byte(&buf[start..end], b'\n', end - start) {
                end = start + i;
            }
        }
        out.push(&buf[start..end]);
        start = end + 1;
    }
    out
}

fn write_temp(buf: &[u8]) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "dusage_perf_only_matching_{}.go",
        std::process::id()
    ));
    std::fs::write(&p, buf).expect("write corpus");
    p
}

fn count(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len()).filter(|w| *w == needle).count()
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    v[v.len() / 2]
}

/// Go-like source at the shape of the tree the benchmark runs on: about 45 bytes a line,
/// every ~50th line a `func ` declaration, and the returns/ifs/comments around them.
fn corpus(size: usize) -> Vec<u8> {
    const WORDS: [&str; 20] = [
        "state", "cache", "buffer", "reader", "writer", "index", "value", "slice", "header",
        "result", "context", "server", "client", "handle", "packet", "struct", "field", "offset",
        "length", "target",
    ];
    const UPPER: [&str; 12] = [
        "Config", "Reader", "Writer", "Buffer", "Server", "Client", "Handle", "Result", "Index",
        "Context", "Packet", "Header",
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
        let w = WORDS[pick(WORDS.len(), 11)];
        let w2 = WORDS[pick(WORDS.len(), 15)];
        match pick(10_000, 0) {
            0..=199 => {
                out.extend_from_slice(b"func (c *");
                out.extend_from_slice(u.as_bytes());
                out.extend_from_slice(b") ");
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(b"(p *");
                out.extend_from_slice(u2.as_bytes());
                out.extend_from_slice(b", n int) error {\n");
            }
            200..=3499 => {
                out.extend_from_slice(b"\t");
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(b" := ");
                out.extend_from_slice(u.as_bytes());
                out.extend_from_slice(b".");
                out.extend_from_slice(w2.as_bytes());
                out.extend_from_slice(b"(");
                out.extend_from_slice(u2.as_bytes());
                out.extend_from_slice(b")\n");
            }
            3500..=5399 => {
                out.extend_from_slice(b"// ");
                out.extend_from_slice(u.as_bytes());
                out.extend_from_slice(b" is the ");
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(b" held for the current ");
                out.extend_from_slice(u2.as_bytes());
                out.extend_from_slice(b".\n");
            }
            5400..=7599 => {
                out.extend_from_slice(b"\tif ");
                out.extend_from_slice(u2.as_bytes());
                out.extend_from_slice(b" != nil && ");
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(b" != nil {\n\t\treturn nil\n\t}\n");
            }
            _ => {
                out.extend_from_slice(b"\treturn ");
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(b".Check(");
                out.extend_from_slice(u.as_bytes());
                out.extend_from_slice(b", ");
                out.extend_from_slice(w2.as_bytes());
                out.extend_from_slice(b")\n");
            }
        }
    }
    out.truncate(size);
    out
}
