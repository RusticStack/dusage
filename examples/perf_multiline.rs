//! Timing harness for the `-U` path: where a whole-file multiline search spends its time.
//!
//! `cargo run --release --example perf_multiline`
//!
//! `-U` reads each file whole and searches the buffer with `Pattern::find_all`, so a run of
//! the CLI is three things in sequence: the read, the automaton walk, and the records built
//! from the matches. This harness measures them separately over one deterministic buffer of
//! Go-like source written to a file, so a number here can be compared with the CLI it stands
//! in for:
//!
//! - `read` is `File::read_to_end` into a fresh `Vec`, exactly what `scan_multiline` does.
//! - `find` is `find_all(buffer, usize::MAX)`, the search alone, no read and no records.
//! - `path` is `search()` over the file with `multiline` set: the whole `-U` path, read
//!   included, as the CLI runs it.
//!
//! Every column is ns per byte of the buffer and the same figure in MB/s, a pass being one
//! whole search of the buffer. The four patterns are the shapes the mode has to handle: a
//! possibly-multiline expression that matches, one that never matches, one whose matches
//! never cross a line, and a pathological one whose match spans nearly the whole buffer.

use dusage::pattern::Pattern;
use dusage::search::{search, SearchOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// The buffer every pattern is timed over: one file, sized like a large `.go` source file.
const CORPUS: usize = 4 << 20;
const WARMUP: usize = 2;
const RUNS: usize = 5;
const REPEATS: usize = 3;

/// The patterns reported, named by what the mode has to do with them.
const SEARCHED: [(&str, &str); 4] = [
    ("across lines", r"func .{0,40}Error"),
    ("no match", r"[Z]{6}[a-z]{9}"),
    ("same line", r"type [A-Z][A-Za-z0-9_]* struct"),
    ("pathological", r"(?s)a.*b"),
];

fn main() {
    let buf = corpus(CORPUS);
    let path = write_temp(&buf);
    println!(
        "buffer {} bytes, {} newlines; {} warmup passes, {} runs per repeat, {} repeats; \
         median reported",
        buf.len(),
        buf.iter().filter(|&&b| b == b'\n').count(),
        WARMUP,
        RUNS,
        REPEATS
    );
    println!(
        "{:<13} {:<32} {:>7} {:>8} {:>8} {:>8} {:>8} {:>8} {:>7}",
        "case",
        "pattern",
        "matches",
        "read ns/B",
        "find ns/B",
        "path ns/B",
        "find MB/s",
        "path MB/s",
        "pre"
    );
    for (case, src) in SEARCHED {
        let pat = Pattern::new(src, false, false, false).expect("pattern");
        let pre = pat
            .block_re()
            .and_then(|re| re.required_literal())
            .map(|run| String::from_utf8_lossy(run).into_owned())
            .unwrap_or_else(|| "-".to_string());
        let mut matches = 0;
        let mut read = Timings::new();
        let mut find = Timings::new();
        let mut whole = Timings::new();
        for i in 0..WARMUP + RUNS * REPEATS {
            let t0 = Instant::now();
            let data = read_file(&path);
            let read_ns = t0.elapsed().as_nanos() as f64;
            let t1 = Instant::now();
            let all = pat.find_all(&data, usize::MAX);
            let find_ns = t1.elapsed().as_nanos() as f64;
            let t2 = Instant::now();
            let report = search(&options(src, &path)).expect("search");
            matches = all.len();
            assert!(report.stats.files_scanned > 0);
            let whole_ns = t2.elapsed().as_nanos() as f64;
            if i >= WARMUP {
                read.push(read_ns);
                find.push(find_ns);
                whole.push(whole_ns);
            }
        }
        let n = buf.len() as f64;
        println!(
            "{:<13} {:<32} {:>7} {:>8.3} {:>8.3} {:>8.3} {:>8.1} {:>8.1} {:>7}",
            case,
            src,
            matches,
            read.median() / n,
            find.median() / n,
            whole.median() / n,
            n / find.median() * 1e3,
            n / whole.median() * 1e3,
            pre
        );
    }
    let _ = std::fs::remove_file(&path);
}

/// `-U` over one file, the way `dusage search` is asked for it.
fn options(src: &str, path: &Path) -> SearchOptions {
    let pat = Pattern::new(src, false, false, false).expect("pattern");
    let mut o = SearchOptions::new(pat, vec![path.to_path_buf()]);
    o.multiline = true;
    o
}

/// One fresh `Vec` per file, as `scan_multiline` allocates it.
fn read_file(path: &Path) -> Vec<u8> {
    let mut f = std::fs::File::open(path).expect("open");
    let mut data = Vec::new();
    f.read_to_end(&mut data).expect("read");
    data
}

fn write_temp(buf: &[u8]) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("dusage-perf-multiline-{}.txt", std::process::id()));
    std::fs::write(&path, buf).expect("write");
    path
}

struct Timings(Vec<f64>);

impl Timings {
    fn new() -> Timings {
        Timings(Vec::with_capacity(RUNS * REPEATS))
    }

    fn push(&mut self, ns: f64) {
        self.0.push(ns);
    }

    fn median(&self) -> f64 {
        let mut v = self.0.clone();
        v.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
        v[v.len() / 2]
    }
}

/// Go-like source, in the proportions real Go source has: `func ` starts a declaration about
/// once per 140 bytes and a capitalised return type follows it, which is the shape
/// `func .{0,40}Error` matches. `a` and `b` are scattered through the words, so the
/// pathological `(?s)a.*b` has plenty to chew on.
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
                out.extend_from_slice(if pick(3, 5) == 0 {
                    b"Error) {\n".as_slice()
                } else {
                    b", error) {\n"
                });
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
            9100..=9132 => {
                out.extend_from_slice(b"// see src/runtime/");
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(b".go and the test beside it\n");
            }
            _ => {
                out.extend_from_slice(b"\treturn ");
                out.extend_from_slice(w3.as_bytes());
                out.extend_from_slice(b", ");
                out.extend_from_slice(w.as_bytes());
                out.extend_from_slice(b"\n");
            }
        }
    }
    out.truncate(size);
    out
}
