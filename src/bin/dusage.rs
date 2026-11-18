// dusage: parallel disk usage scanner (std-only, read-only)
use dusage::cli::{default_threads, parse_size, parse_threads, Args};
use dusage::model::ScanResult;
use dusage::report::{render_json, render_text, ReportOptions};
use dusage::scan::{scan, watch, Backend, ScanOptions};
use std::io::Write;
use std::path::Path;
use std::process::exit;
use std::time::Duration;

const HELP: &str = "dusage - parallel disk usage scanner (read-only)

USAGE:
    dusage <root> [min_big] [threads] [top_n] [OPTIONS]

POSITIONAL (legacy, overridden by the matching option):
    root              directory to scan
    min_big           report files >= this size (bytes, or 300M / 1.5G / 512KiB; default 1G)
    threads           worker threads (default 4x CPU cores)
    top_n             rows to print per section (default 60)

OPTIONS:
    --min-big SIZE    report files >= SIZE
    --threads N       worker threads
    --top N           rows per section
    --json            machine-readable output on stdout instead of the text report
    --by-ext          add a per-extension breakdown
    --on-disk         also show allocated (on-disk) size
    --dedup-hardlinks count each hardlinked file once (a link whose other name lies outside
                      the root is still counted once)
    --streams         add named alternate data streams (NTFS only; 0 bytes elsewhere)
    -x, --one-filesystem
                      stay on the root's filesystem: mount points are counted as skipped,
                      not scanned (Linux; Windows mount points are links and always skipped)
    --backend NAME    auto (default) | readdir | batched | mft
    --mft             same as --backend mft (Windows/NTFS only)
    --watch SECS      rescan every SECS seconds and print each update (Ctrl+C to stop);
                      the platform change feed (NTFS USN journal on Windows, inotify on
                      Linux) limits rescans to the entries that changed. A partial rescan
                      keeps other top-level entries as last scanned, so a hardlink spanning
                      a rescanned and a kept entry is counted per name; with
                      --dedup-hardlinks every rescan is a full one
    -h, --help        show this help
    -V, --version     print the version

BACKENDS:
    auto      batched, falling back to readdir if the filesystem rejects it
    batched   Windows: GetFileInformationByHandleEx returns many entries per syscall, with
              allocation size and 128-bit file id. Linux: one statx per entry, so every row
              still carries its on-disk size and inode; the default and fastest path
    readdir   std::fs::read_dir only; on-disk size unknown (shown as 0), no hardlink dedup
    mft       Windows only: raw NTFS $MFT read of a local drive letter (needs an elevated
              shell); reads the whole volume's table, so it pays off on big trees

Symlinks, junctions and other reparse points are never followed.
A root that is itself a link is refused; pass its target.
Errors on individual files or directories are counted, not fatal.
The progress line goes to stderr; the report goes to stdout.
";

/// Exit code and message: 2 for misuse, 1 for a failed scan.
struct Fail(i32, String);

impl From<String> for Fail {
    fn from(m: String) -> Self {
        Fail(2, m)
    }
}

fn parse_backend(s: &str) -> Result<Backend, String> {
    match s.to_ascii_lowercase().as_str() {
        "auto" => Ok(Backend::Auto),
        "readdir" => Ok(Backend::ReadDir),
        "batched" => Ok(Backend::Batched),
        "mft" => Ok(Backend::Mft),
        _ => Err(format!(
            "unknown backend {s:?} (use auto, readdir, batched or mft)"
        )),
    }
}

fn parse_top(s: &str) -> Result<usize, String> {
    s.trim()
        .parse()
        .map_err(|_| format!("invalid row count {s:?}"))
}

fn parse_secs(s: &str) -> Result<Duration, String> {
    match s.trim().parse::<f64>() {
        Ok(v) if v > 0.0 && v.is_finite() => {
            Duration::try_from_secs_f64(v).map_err(|_| format!("interval too large: {s:?}"))
        }
        _ => Err(format!("invalid interval {s:?} (seconds > 0)")),
    }
}

fn run(raw: Vec<String>) -> Result<(), Fail> {
    let a = Args::parse(
        raw,
        &["--min-big", "--threads", "--top", "--backend", "--watch"],
        &[
            "--json",
            "--by-ext",
            "--on-disk",
            "--dedup-hardlinks",
            "--streams",
            "--one-filesystem",
            "--mft",
            "-x",
            "-h",
            "--help",
            "-V",
            "--version",
        ],
    )?;
    if a.flag("-h") || a.flag("--help") {
        print!("{HELP}");
        return Ok(());
    }
    if a.flag("-V") || a.flag("--version") {
        println!("dusage {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let Some(root) = a.positional(0) else {
        return Err("missing <root>".to_string().into());
    };
    if a.positionals().len() > 4 {
        return Err("too many positional arguments".to_string().into());
    }
    let mut o = ScanOptions::new(root);
    o.threads = default_threads();
    if let Some(v) = a.value("--min-big").or(a.positional(1)) {
        o.min_big = parse_size(v)?;
    }
    if let Some(v) = a.value("--threads").or(a.positional(2)) {
        o.threads = parse_threads(v)?;
    }
    if let Some(v) = a.value("--top").or(a.positional(3)) {
        o.top_n = parse_top(v)?;
    }
    if let Some(v) = a.value("--backend") {
        o.backend = parse_backend(v)?;
    }
    if a.flag("--mft") {
        o.backend = Backend::Mft;
    }
    o.dedup_hardlinks = a.flag("--dedup-hardlinks");
    o.streams = a.flag("--streams");
    if o.streams && !cfg!(windows) {
        eprintln!(
            "dusage: note: named alternate data streams are an NTFS feature; \
             --streams reports 0 bytes on this platform"
        );
    }
    o.by_ext = a.flag("--by-ext");
    o.one_filesystem = a.flag("--one-filesystem") || a.flag("-x");
    let interval = a.value("--watch").map(parse_secs).transpose()?;
    let json = a.flag("--json");
    let ro = ReportOptions {
        top_n: o.top_n,
        min_big: o.min_big,
        show_alloc: a.flag("--on-disk"),
        by_ext: o.by_ext,
    };
    if !Path::new(root).is_dir() {
        return Err(format!("{root}: not a directory (missing or not readable)").into());
    }

    eprintln!(
        "dusage: scanning {root} ({} threads, backend {:?})",
        o.threads, o.backend
    );
    let emit = |r: &ScanResult| -> std::io::Result<()> {
        let mut out = std::io::stdout().lock();
        if json {
            render_json(r, &mut out)?;
        } else {
            render_text(r, &ro, &mut out)?;
        }
        out.flush()
    };
    let fail = |e: std::io::Error| Fail(1, format!("{root}: {e}"));
    match interval {
        None => emit(&scan(&o).map_err(fail)?)
            .or_else(ignore_broken_pipe)
            .map_err(fail),
        Some(iv) => watch(&o, iv, &mut |r, what| {
            eprintln!("dusage: {what}");
            emit(r).is_ok()
        })
        .map_err(fail),
    }
}

fn ignore_broken_pipe(e: std::io::Error) -> std::io::Result<()> {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        Ok(())
    } else {
        Err(e)
    }
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() {
        eprint!("{HELP}");
        exit(2);
    }
    if let Err(Fail(code, m)) = run(raw) {
        eprintln!("dusage: {m}");
        if code == 2 {
            eprintln!("try 'dusage --help'");
        }
        exit(code);
    }
}
