//! Subcommand entry points shared by the `dusage` and `fastdel` binaries.
//!
//! `dusage` is one binary with subcommands — `scan` (the disk-usage report), `search`
//! (grep), `find` (path search), `prune` (the fastdel engine), `big` (largest files) and
//! `du` (per-directory totals) — and it still accepts the original
//! `<root> [min_big] [threads] [top_n]` form, which is equivalent to `scan`. `fastdel` is
//! the `prune` subcommand under its own name.

pub mod df;
pub mod du;
pub mod prune;
pub mod scan;
pub mod search;
pub mod tree;

use std::process::exit;

/// A command line whose flag tables [`accepted_flags`] returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontEnd {
    /// `dusage scan`, `dusage big`, and the top-level `<root> ...` form, which is `scan`.
    Scan,
    Du,
    Search,
    Find,
    Prune,
    Tree,
}

/// The flags a front end's parser accepts: the ones that take a value, then the switches.
/// The tables are the parser's own, so a test can compare them with what the help text
/// advertises and catch either kind of drift — a documented flag the parser rejects, or an
/// accepted flag nobody can discover.
pub fn accepted_flags(fe: FrontEnd) -> (&'static [&'static str], &'static [&'static str]) {
    match fe {
        FrontEnd::Scan => (scan::VALUE_FLAGS, scan::BOOL_FLAGS),
        FrontEnd::Du => (du::VALUE_FLAGS, du::BOOL_FLAGS),
        FrontEnd::Search => (search::SEARCH_VALUE_FLAGS, search::SEARCH_BOOL_FLAGS),
        FrontEnd::Find => (search::FIND_VALUE_FLAGS, search::FIND_BOOL_FLAGS),
        FrontEnd::Prune => (prune::VALUE_FLAGS, prune::BOOL_FLAGS),
        FrontEnd::Tree => (tree::VALUE_FLAGS, tree::BOOL_FLAGS),
    }
}

/// Exit code and message: 2 for misuse, 1 for a failed run.
#[derive(Debug)]
pub struct Fail(pub i32, pub String);

impl From<String> for Fail {
    fn from(m: String) -> Self {
        Fail(2, m)
    }
}

impl Fail {
    /// A write that fails because the reader went away is not a failure: it is the pipeline
    /// saying it has seen enough. Exit 0 with nothing on stderr, the way ripgrep does.
    pub fn output(e: std::io::Error) -> Fail {
        match e.kind() {
            std::io::ErrorKind::BrokenPipe => Fail(0, String::new()),
            _ => Fail(1, e.to_string()),
        }
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
    dusage du [PATH...] [OPTIONS]                          du-compatible per-directory totals
    dusage df [PATH...] [OPTIONS]                          mounted filesystems and their usage
    dusage tree [PATH...] [OPTIONS]                        the tree, with on-disk and apparent sizes

Each subcommand has its own help: `dusage search --help`, `dusage find --help`,
`dusage scan --help`, `dusage prune --help`, `dusage du --help`, `dusage tree --help`.

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
        "du" => {
            raw.remove(0);
            du::run(raw)
        }
        "tree" => {
            raw.remove(0);
            tree::run(raw)
        }
        "df" => {
            raw.remove(0);
            df::run(raw)
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
