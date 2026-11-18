// fastdel: parallel age/prefix filtered pruner (std-only, DESTRUCTIVE, no recycle bin).
// There is deliberately no confirmation prompt and no --yes flag; use --dry to preview.
use dusage::cli::{self, Args};
use dusage::prune::{self, PruneOptions};
use std::path::PathBuf;
use std::process::exit;

const HELP: &str = "fastdel - parallel age/prefix filtered pruner (PERMANENT, no recycle bin)

USAGE:
    fastdel <root> <min_age> [prefixes] [threads] [--dry]
    fastdel <root> --age AGE [--prefix a,b] [--threads N] [--dry] [--log FILE] [--limit N]

Filters apply to the direct children of <root>; matching entries are deleted recursively.
Links (junctions, symlinks) are unlinked, never followed, and a tree is never followed into
another filesystem (a mount point is left in place, like a link).
A root that is itself a link is refused; pass its target.

OPTIONS:
    --age AGE       delete entries last modified more than AGE ago: 30s 15m 2h 7d 1w, or a
                    bare number of days; a negative number means no age filter. Required.
    --prefix a,b    only names starting with a prefix (case-sensitive); repeatable
    --threads N     worker threads (default 4x CPU cores)
    --dry           print the plan, delete nothing
    --log FILE      append one line per candidate (DELETED/FAILED/DRY) plus a summary
    --limit N       rows listed by --dry (default 50)
    -h, --help      this text
    -V, --version   print the version

Protected roots are always refused:
  Windows  volume roots, Windows, Program Files, ProgramData, user profile and ancestors
  Linux    /, /etc, /usr, /bin, /sbin, /lib*, /boot, /proc, /sys, /dev, /run, /var, /opt,
           /srv, /root, /home, $HOME and ancestors";

fn die(msg: &str) -> ! {
    eprintln!("fastdel: {msg}\n(try fastdel --help)");
    exit(2)
}

fn main() {
    let a = Args::parse(
        std::env::args().skip(1),
        &["--age", "--prefix", "--threads", "--log", "--limit"],
        &["--dry", "-h", "--help", "-V", "--version"],
    )
    .unwrap_or_else(|e| die(&e));
    if a.flag("-h") || a.flag("--help") {
        println!("{HELP}");
        return;
    }
    if a.flag("-V") || a.flag("--version") {
        println!("fastdel {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    // Positional back-compat: <root> <min_age> [prefixes] [threads].
    let pos = a.positionals();
    let age_pos = a.value("--age").is_none();
    let skip = 1 + usize::from(age_pos);
    let root = pos.first().unwrap_or_else(|| die("missing <root>"));
    let age_s = match a.value("--age") {
        Some(s) => s,
        None => pos
            .get(1)
            .map(String::as_str)
            .unwrap_or_else(|| die("an age is required (positional or --age)")),
    };
    let min_age = cli::parse_age(age_s).unwrap_or_else(|e| die(&e));

    let split = |s: &str| -> Vec<String> {
        s.split(',')
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect()
    };
    // Positional "" keeps its original meaning (no prefix filter) so
    // `fastdel <root> <age> "" <threads>` still works; an explicit but empty
    // --prefix is almost always an unset variable, so it is refused.
    let mut prefixes: Vec<String> = pos.get(skip).map_or_else(Vec::new, |s| split(s));
    if let Some(s) = a.value("--prefix") {
        let p = split(s);
        if p.is_empty() {
            die("--prefix is empty; omit it to match every entry");
        }
        prefixes.extend(p);
    }
    let threads_s = a
        .value("--threads")
        .or(pos.get(skip + 1).map(String::as_str));
    let threads = threads_s.map_or_else(|| Ok(cli::default_threads()), cli::parse_threads);
    let threads = threads.unwrap_or_else(|e| die(&e));
    let list_limit = a.value("--limit").map_or(50, |s| {
        s.parse()
            .unwrap_or_else(|_| die(&format!("invalid --limit {s:?}")))
    });
    if pos.len() > skip + 2 {
        die("too many positional arguments");
    }

    let o = PruneOptions {
        root: PathBuf::from(root),
        min_age,
        prefixes,
        threads,
        dry: a.flag("--dry"),
        log: a.value("--log").map(PathBuf::from),
        list_limit,
    };
    let (cands, kept) = prune::plan(&o).unwrap_or_else(|e| {
        eprintln!("fastdel: {e}");
        exit(2)
    });
    let total = cands.len();
    eprintln!("fastdel: {total} candidates ({kept} kept), dry={}", o.dry);
    if o.dry {
        for c in cands.iter().take(o.list_limit) {
            println!(
                "  {}{}",
                c.path.display(),
                if c.is_reparse { "  [link]" } else { "" }
            );
        }
        if total > o.list_limit {
            println!("  ... and {} more", total - o.list_limit);
        }
    }
    if total == 0 && !o.dry {
        return;
    }
    let rep = prune::execute(&o, cands, &|done, failed| {
        eprintln!("progress: {}/{total} ({failed} failed)", done + failed);
    });
    if o.dry {
        return;
    }
    eprintln!(
        "fastdel done: {}/{total} deleted ({} entries), {} failed, {:.2?}",
        rep.candidates_done, rep.deleted_entries, rep.candidates_failed, rep.elapsed
    );
    if rep.errors.total() > 0 {
        let e = &rep.errors;
        eprintln!(
            "errors: access_denied={} sharing_violation={} not_found={} dir_not_empty={} other={}",
            e.access_denied, e.sharing_violation, e.not_found, e.dir_not_empty, e.other
        );
        for (k, p) in &rep.first_failures {
            eprintln!("  failed [{k}] {}", p.display());
        }
    }
    if rep.candidates_failed > 0 {
        exit(1);
    }
}
