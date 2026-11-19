//! Time a whole regex search, not just a matcher call: the searcher's own buffer path
//! (`search::search_buffer`) over deterministic Go-like text, so a change to prefilters,
//! automata or block scanning shows up as the wall time a user would see.
//!
//! `cargo run --release --example perf_regex`, or with a count of runs
//! `cargo run --release --example perf_regex -- 11`. Each pattern is searched
//! `RUNS` times and the median is reported, so one noisy run cannot decide a comparison.

use std::time::Instant;

use dusage::pattern::Pattern;
use dusage::search::{search_buffer, SearchOptions};

/// Text one search sees, and the read block the scanner feeds itself. Several blocks, so the
/// cost is per byte rather than per file.
const TEXT_BYTES: usize = 8 << 20;

/// Runs per pattern; the median is reported.
const RUNS: usize = 9;

const PATTERNS: [&str; 4] = [
    // The Go identifier case: a mandatory uppercase first byte, a long tail, a literal suffix.
    r"[A-Z][a-zA-Z0-9_]*\.go",
    // Two runs joined by a mandatory underscore.
    r"[a-z]+_[a-z]+",
    // A rare literal inside a regex: one mandatory run, very few lines.
    r"GOEXPERIMENT_[a-z]+\(",
    // Nothing in the text matches, so this measures the prefilter and nothing else.
    r"Xq[a-z]*Zzy",
];

fn main() {
    let runs: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(RUNS);
    let text = go_like_text(TEXT_BYTES);
    eprintln!(
        "buffer {} bytes, median of {} runs, {} lines",
        text.len(),
        runs,
        text.iter().filter(|&&b| b == b'\n').count()
    );
    println!(
        "{:<26} {:>6} {:>9} {:>10} {:>9} {:>8}",
        "pattern", "block", "matches", "median ms", "ns/byte", "MB/s"
    );
    for pat in PATTERNS {
        let opts = options(pat);
        let matches = search_buffer(&opts, "buffer", &text)
            .map(|r| r.stats.matches)
            .unwrap();
        let mut samples = Vec::with_capacity(runs);
        // One untimed pass first: the DFA cache and any lazy state are built on it.
        search_buffer(&opts, "buffer", &text).unwrap();
        for _ in 0..runs {
            let t0 = Instant::now();
            let report = search_buffer(&opts, "buffer", &text).unwrap();
            debug_assert_eq!(report.stats.matches, matches);
            samples.push(t0.elapsed().as_secs_f64() * 1e3);
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = samples[samples.len() / 2];
        let bytes = text.len() as f64;
        println!(
            "{:<26} {:>6} {:>9} {:>10.2} {:>9.3} {:>8.0}",
            pat,
            if opts.pattern.block_safe() {
                "yes"
            } else {
                "no"
            },
            matches,
            median,
            median * 1e6 / bytes,
            bytes / (median * 1e3)
        );
    }
}

fn options(pat: &str) -> SearchOptions {
    let pattern = Pattern::new(pat, false, false, false).expect("pattern compiles");
    let mut opts = SearchOptions::new(pattern, Vec::new());
    opts.count = true;
    opts.threads = 1;
    opts
}

/// Deterministic Go-like source: package lines, imports, doc comments, `func` declarations
/// with CamelCase names, string literals naming `.go` files, struct literals and the odd
/// `GOEXPERIMENT_*` line. No randomness beyond a fixed linear congruential generator, so
/// every run of the harness measures the same bytes.
fn go_like_text(target: usize) -> Vec<u8> {
    const WORDS: [&str; 24] = [
        "parser",
        "lexer",
        "token",
        "ast",
        "node",
        "walker",
        "escape",
        "inline",
        "prove",
        "range",
        "assign",
        "closure",
        "struct",
        "field",
        "method",
        "interface",
        "channel",
        "select",
        "defer",
        "panic",
        "recover",
        "layout",
        "align",
        "offset",
    ];
    const NAMES: [&str; 18] = [
        "Parser",
        "Lexer",
        "Node",
        "Walker",
        "Escape",
        "Inline",
        "Prove",
        "RangeStmt",
        "Assign",
        "Closure",
        "StructType",
        "Field",
        "Method",
        "Iface",
        "ChanType",
        "SelectStmt",
        "DeferStack",
        "Layout",
    ];
    const FILES: [&str; 12] = [
        "parser.go",
        "lex.go",
        "node.go",
        "walk.go",
        "escape.go",
        "inline.go",
        "prove.go",
        "range.go",
        "assign.go",
        "closure.go",
        "types.go",
        "layout.go",
    ];
    let mut out = Vec::with_capacity(target + 4096);
    let mut rng = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    out.extend_from_slice(b"package compiler\n\nimport (\n\t\"fmt\"\n\t\"os\"\n)\n\n");
    let mut pkg = 0;
    while out.len() < target {
        let r = next();
        match r % 16 {
            0 | 1 => {
                pkg += 1;
                out.extend_from_slice(
                    format!("// Package pass{pkg} rewrites the tree the parser built.\n\n")
                        .as_bytes(),
                );
            }
            2 => out.extend_from_slice(b"type visitor struct {\n\tseen map[string]bool\n}\n\n"),
            3..=5 => {
                let name = NAMES[(r >> 8) as usize % NAMES.len()];
                let word = WORDS[(r >> 16) as usize % WORDS.len()];
                out.extend_from_slice(
                    format!("func (v *visitor) visit{name}(n *node) error {{\n").as_bytes(),
                );
                out.extend_from_slice(
                    format!("\tif n == nil || v.seen[\"{word}\"] {{\n\t\treturn nil\n\t}}\n")
                        .as_bytes(),
                );
                out.extend_from_slice(
                    format!(
                        "\treturn fmt.Errorf(\"{}: %w\", os.ErrInvalid)\n}}\n\n",
                        word
                    )
                    .as_bytes(),
                );
            }
            6 => {
                let file = FILES[(r >> 8) as usize % FILES.len()];
                out.extend_from_slice(
                    format!("\t// see internal/{file} for the shape of this value\n").as_bytes(),
                );
            }
            7 => {
                // A CamelCase identifier naming a file, which is what the Go case matches.
                let name = NAMES[(r >> 8) as usize % NAMES.len()];
                out.extend_from_slice(
                    format!("\t// mirrors {name}.go in the pass below\n").as_bytes(),
                );
            }
            8..=10 => {
                let a = WORDS[(r >> 8) as usize % WORDS.len()];
                let b = WORDS[(r >> 16) as usize % WORDS.len()];
                out.extend_from_slice(
                    format!("\t{a}_{b} := {}\n\t_{a}_{b} = _{a}_{b}\n", (r >> 24) % 32).as_bytes(),
                );
            }
            11 => {
                let name = NAMES[(r >> 8) as usize % NAMES.len()];
                out.extend_from_slice(
                    format!(
                        "\tif GOEXPERIMENT_fieldtrace({name}) {{\n\t\tv.emit(\"{}\")\n\t}}\n",
                        WORDS[(r >> 20) as usize % WORDS.len()]
                    )
                    .as_bytes(),
                );
            }
            12 => out.extend_from_slice(
                b"\tfor i := 0; i < len(list); i++ {\n\t\tlist[i].index = i\n\t}\n",
            ),
            13 => out.extend_from_slice(
                b"\tswitch n.op {\n\tcase opAdd, opSub:\n\t\treturn v.fold(n)\n\t}\n",
            ),
            14 => {
                out.extend_from_slice(b"\t// The escape analysis is quadratic in the worst case.\n")
            }
            _ => {
                let name = NAMES[(r >> 8) as usize % NAMES.len()];
                out.extend_from_slice(format!("\t_ = {name}{{}}\n").as_bytes());
            }
        }
    }
    out.truncate(target);
    if out.last() != Some(&b'\n') {
        out.push(b'\n');
    }
    out
}
