//! Enumeration cost on a real NTFS volume: `FindFirstFileExW` (with and without
//! `FIND_FIRST_EX_LARGE_FETCH`) against the `FileIdExtdDirectoryInfo` batch at three
//! buffer sizes.
//!
//! `cargo run --release --example win_enum_perf -- <root> [runs]`
//!
//! One walk per run, single-threaded depth-first, so the number is the syscall cost of
//! the walk itself and not the work queue. Every backend fills the same `Entry` vec, so
//! the per-entry conversion is identical across rows and the difference is what the
//! kernel was asked. Cache state is the caller's business: run it twice for warm, and
//! after a cache purge for cold.

use dusage::model::ErrorCounts;
use dusage::win::enumerate::{read_plain, read_plain_ex, DirReader, Entry};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[cfg(not(windows))]
fn main() {
    eprintln!("win_enum_perf measures Windows enumeration only");
}

#[cfg(windows)]
fn main() {
    let mut args = std::env::args().skip(1);
    let root: PathBuf = args
        .next()
        .expect("usage: win_enum_perf <root> [runs]")
        .into();
    let runs: usize = args.next().map(|s| s.parse().unwrap()).unwrap_or(5);
    // Optional third argument: run only the named backend, so a driver can interleave
    // backends across processes instead of trusting the order inside one process.
    let only: Option<String> = args.next();
    if !root.is_dir() {
        eprintln!("not a directory: {}", root.display());
        std::process::exit(2);
    }
    let modes = [
        ("walk", Backend::Walk),
        ("walk-large-fetch", Backend::WalkLargeFetch),
        ("batch-64k", Backend::Batch(64 * 1024)),
        ("batch-256k", Backend::Batch(256 * 1024)),
        ("batch-1m", Backend::Batch(1024 * 1024)),
    ];
    println!(
        "root={} runs={} threads=1 (walk is sequential by construction)",
        root.display(),
        runs
    );
    println!(
        "{:<18} {:>10} {:>10} {:>12} {:>10}",
        "backend", "ms(med)", "files", "files/s", "dirs"
    );
    for (name, backend) in modes {
        if let Some(o) = &only {
            if o != name {
                continue;
            }
        }
        let mut times = Vec::with_capacity(runs);
        let mut reads = Vec::with_capacity(runs);
        let mut files = 0u64;
        let mut dirs = 0u64;
        for _ in 0..runs {
            let (dt, rd, f, d) = walk(&root, &backend);
            reads.push(rd);
            times.push(dt);
            files = f;
            dirs = d;
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med = times[times.len() / 2];
        reads.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let rmed = reads[reads.len() / 2];
        let total = files + dirs;
        println!(
            "{:<18} {:>10.1} {:>10} {:>12.0} {:>10}",
            name,
            med,
            total,
            total as f64 / (med / 1000.0),
            dirs
        );
        eprintln!(
            "  {name}: read() {rmed:.1} ms of {med:.1} ms; runs {:?}",
            times
                .iter()
                .map(|t| (t * 10.0).round() / 10.0)
                .collect::<Vec<_>>()
        );
    }
}

enum Backend {
    Walk,
    WalkLargeFetch,
    Batch(usize),
}

/// One whole-tree walk. Returns (elapsed ms, files, dirs).
fn walk(root: &Path, backend: &Backend) -> (f64, f64, u64, u64) {
    let mut errs = ErrorCounts::default();
    let mut reader = match backend {
        Backend::Batch(n) => Some(DirReader::with_buffer(*n)),
        _ => None,
    };
    let mut entries: Vec<Entry> = Vec::with_capacity(1024);
    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    let (mut files, mut dirs) = (0u64, 0u64);
    let mut read_ns = 0u128;
    let start = Instant::now();
    while let Some(dir) = stack.pop() {
        entries.clear();
        let read_start = Instant::now();
        let r = match (backend, reader.as_mut()) {
            (Backend::Walk, _) => read_plain(&dir, &mut entries, &mut errs),
            (Backend::WalkLargeFetch, _) => read_plain_ex(&dir, &mut entries, &mut errs, true),
            (Backend::Batch(_), Some(r)) => r.read(&dir, &mut entries, &mut errs),
            _ => unreachable!(),
        };
        if r.is_err() {
            continue;
        }
        read_ns += read_start.elapsed().as_nanos();
        subdirs.clear();
        for e in &entries {
            if e.is_dir() {
                if e.is_reparse() {
                    dirs += 1;
                } else {
                    dirs += 1;
                    subdirs.push(dir.join(&e.name));
                }
            } else {
                files += 1;
            }
        }
        stack.append(&mut subdirs);
    }
    (
        start.elapsed().as_secs_f64() * 1000.0,
        read_ns as f64 / 1e6,
        files,
        dirs,
    )
}
