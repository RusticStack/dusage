//! Subcommand entry points shared by the `dusage` and `fastdel` binaries.
//!
//! `dusage` is one binary with subcommands — `scan` (the disk-usage report), `search`
//! (grep), `find` (path search), `prune` (the fastdel engine) and `big` (largest files) —
//! and it still accepts the original `<root> [min_big] [threads] [top_n]` form, which is
//! equivalent to `scan`. `fastdel` is the `prune` subcommand under its own name.

pub mod prune;
pub mod scan;
pub mod search;

use std::process::exit;

/// Exit code and message: 2 for misuse, 1 for a failed run.
#[derive(Debug)]
pub struct Fail(pub i32, pub String);

impl From<String> for Fail {
    fn from(m: String) -> Self {
        Fail(2, m)
    }
}

const TOP_HELP: &str = "dusage - disk usage, search and cleanup in one binary

USAGE:
    dusage <root> [min_big] [threads] [top_n] [OPTIONS]   disk usage (legacy form)
    dusage scan <root> [OPTIONS]                           disk usage report
    dusage big <root> [OPTIONS]                            largest files only
    dusage search <pattern> [paths...] [OPTIONS]           file contents (grep/rg)
    dusage find <glob> [paths...] [OPTIONS]                file names and paths
    dusage prune <root> --age AGE [OPTIONS]                delete old entries (fastdel)

Each subcommand has its own help: `dusage search --help`, `dusage find --help`,
`dusage scan --help`, `dusage prune --help`.

Run 'dusage --help' for the full list, or 'dusage <subcommand> --help'.";

/// Dispatch `dusage <args>`.
pub fn dispatch(mut raw: Vec<String>) -> Result<(), Fail> {
    if raw.is_empty() {
        eprintln!("{TOP_HELP}");
        return Err(Fail(2, "missing subcommand or <root>".to_string()));
    }
    let cmd = raw[0].clone();
    match cmd.as_str() {
        "search" | "grep" | "rg" => {
            raw.remove(0);
            search::run_search(raw)
        }
        "find" | "ls" => {
            raw.remove(0);
            search::run_find(raw)
        }
        "prune" | "fastdel" | "del" => {
            raw.remove(0);
            prune::run(raw)
        }
        "scan" => {
            raw.remove(0);
            scan::run(raw, scan::Mode::Scan)
        }
        "big" => {
            raw.remove(0);
            scan::run(raw, scan::Mode::Big)
        }
        "-h" | "--help" | "help" => {
            // The full help: the subcommand map, then the options of the default command.
            println!("{TOP_HELP}\n");
            println!("{}", scan::help());
            Ok(())
        }
        "-V" | "--version" => {
            println!("dusage {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => scan::run(raw, scan::Mode::Scan),
    }
}

/// Print a failure and exit with its code: 2 for misuse, 1 for a failed run.
pub fn finish(r: Result<(), Fail>) -> ! {
    match r {
        Ok(()) => exit(0),
        Err(Fail(code, m)) => {
            if !m.is_empty() {
                eprintln!("dusage: {m}");
            }
            if code == 2 {
                eprintln!("try 'dusage --help'");
            }
            exit(code)
        }
    }
}

/// Exit without a message for subcommands that already reported one.
pub fn finish_prune(r: Result<(), Fail>) -> ! {
    match r {
        Ok(()) => exit(0),
        Err(Fail(code, m)) => {
            if !m.is_empty() {
                eprintln!("fastdel: {m}");
            }
            if code == 2 {
                eprintln!("(try fastdel --help)");
            }
            exit(code)
        }
    }
}
