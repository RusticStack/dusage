//! `dusage prune` / `fastdel`: the parallel age- and prefix-filtered pruner.

use super::Fail;
use crate::cli::{self, Args};
use crate::prune::{self, PruneOptions};
use std::path::PathBuf;

const PRUNE_HELP: &str = "fastdel / dusage prune - parallel age/prefix filtered pruner
(PERMANENT, no recycle bin)

USAGE:
    fastdel <root> <min_age> [prefixes] [threads] [--dry]
    fastdel <root> --age AGE [--prefix a,b] [--threads N] [--dry] [--log FILE] [--limit N]
    dusage prune <root> --age AGE [OPTIONS]

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

/// Entry point for `fastdel <args>` and `dusage prune <args>`.
pub fn run(raw: Vec<String>) -> Result<(), Fail> {
    let a = Args::parse(
        raw,
        &["--age", "--prefix", "--threads", "--log", "--limit"],
        &["--dry", "-h", "--help", "-V", "--version"],
    )?;
    if a.flag("-h") || a.flag("--help") {
        println!("{PRUNE_HELP}");
        return Ok(());
    }
    if a.flag("-V") || a.flag("--version") {
        println!("fastdel {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    // Positional back-compat: <root> <min_age> [prefixes] [threads].
    let pos = a.positionals();
    let age_pos = a.value("--age").is_none();
    let skip = 1 + usize::from(age_pos);
    let Some(root) = pos.first() else {
        return Err("missing <root>".to_string().into());
    };
    let age_s = match a.value("--age") {
        Some(s) => s,
        None => match pos.get(1) {
            Some(s) => s.as_str(),
            None => return Err("an age is required (positional or --age)".to_string().into()),
        },
    };
    let min_age = cli::parse_age(age_s)?;

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
            return Err("--prefix is empty; omit it to match every entry".to_string().into());
        }
        prefixes.extend(p);
    }
    let threads_s = a
        .value("--threads")
        .or(pos.get(skip + 1).map(String::as_str));
    let threads = match threads_s {
        Some(s) => cli::parse_threads(s)?,
        None => cli::default_threads(),
    };
    let list_limit = match a.value("--limit") {
        Some(s) => s
            .parse()
            .map_err(|_| Fail(2, format!("invalid --limit {s:?}")))?,
        None => 50,
    };
    if pos.len() > skip + 2 {
        return Err("too many positional arguments".to_string().into());
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
    let (cands, kept) = prune::plan(&o).map_err(|e| Fail(2, e.to_string()))?;
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
        return Ok(());
    }
    let rep = prune::execute(&o, cands, &|done, failed| {
        eprintln!("progress: {}/{total} ({failed} failed)", done + failed);
    });
    if o.dry {
        return Ok(());
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
        return Err(Fail(1, String::new()));
    }
    Ok(())
}
