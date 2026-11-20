//! `dusage df`: the mounted filesystems, the way `df` reports them.
//!
//! The numbers and the mount list come from the platform layer
//! ([`crate::native::df`]): `statvfs` plus `/proc/self/mounts` on Linux,
//! `GetDiskFreeSpaceExW` plus the volume list on Windows. Everything here — which rows to
//! keep, how the columns are sized, how a unit is printed — is the same on both, so this
//! module holds no platform branch at all.
//!
//! # Which rows
//!
//! With no arguments, every mounted filesystem, filtered the way GNU df filters: an entry
//! with no size (a `proc`, a `sysfs`, a signed-out share) is left out, and a filesystem
//! mounted twice is one row at its first mount point. `-a` keeps both. With arguments, one
//! row per distinct filesystem holding a path, in argument order — the `df PATH` question
//! ("which disk is this on, and how full"). `-x` narrows either form to the filesystem
//! holding the first path, which is the scanner's `--one-file-system` rule: count one
//! filesystem, not the ones mounted inside it.
//!
//! # Units
//!
//! No flag prints 1 KiB blocks, `df`'s default column. `-h` scales by 1024, `--si` by 1000,
//! and both round a value up rather than down, so a filesystem with one byte used never
//! reads as empty. `-i` swaps the byte columns for inode counts, where the platform reports
//! them (Linux does; Windows has no such number and says so).

use super::Fail;
use crate::cli::Args;
use crate::native::df::{filesystems, usage_of, FsUsage};
use std::ffi::OsString;
use std::io::{self, BufWriter, Write};
use std::path::Path;

const DF_HELP: &str = "dusage df - mounted filesystems, the way df reports them

USAGE:
    dusage df [PATH...] [OPTIONS]

With no PATH, every mounted filesystem is listed. With a PATH, one row is printed per
filesystem holding a path, in argument order. Sizes are 1 KiB blocks unless -h or --si
asks for a unit.

OPTIONS:
    -h, --human-readable  powers of 1024 (64G, 3.2G), rounded up
        --si              powers of 1000
    -i, --inodes          inode columns instead of byte columns (Linux; Windows says the
                          counts are unavailable)
    -T, --print-type      a Type column after Filesystem
    -a, --all             include filesystems with no size and duplicates of one device
    -x, --one-filesystem  report only the filesystem holding the first PATH, or the current
                          directory, never a second device mounted inside it
        --json            one object: every row with its byte and inode counts
        --help            show this help (there is no short help flag: -h is human-readable)
    -V, --version         print the version

COLUMNS:
    Filesystem, Size, Used, Avail, Use%, Mounted on — plus Type with -T, or Inodes, IUsed,
    IFree and IUse% instead of the byte columns with -i. Use% is used / (used + available),
    which is how df accounts for reserved blocks: a full filesystem is 100%, not 95%.

EXIT:
    0 when rows were printed, 1 when a PATH could not be read (the other rows still print),
    2 for a bad argument.
";

/// The flags `df` takes, for [`super::accepted_flags`].
pub(super) const VALUE_FLAGS: &[&str] = &[];

pub(super) const BOOL_FLAGS: &[&str] = &[
    "-h",
    "--human-readable",
    "--si",
    "-i",
    "--inodes",
    "-T",
    "--print-type",
    "-a",
    "--all",
    "-x",
    "--one-filesystem",
    "--json",
    "--help",
    "-V",
    "--version",
];

/// The df help text, for `dusage --help`.
pub fn help() -> &'static str {
    DF_HELP
}

#[derive(Clone, Copy)]
enum Unit {
    Blocks,
    Human(u64),
}

struct Options {
    unit: Unit,
    inodes: bool,
    types: bool,
    all: bool,
    one_filesystem: bool,
    json: bool,
}

fn options(a: &Args) -> Options {
    let unit = if a.flag("-h") || a.flag("--human-readable") {
        Unit::Human(1024)
    } else if a.flag("--si") {
        Unit::Human(1000)
    } else {
        Unit::Blocks
    };
    Options {
        unit,
        inodes: a.flag("-i") || a.flag("--inodes"),
        types: a.flag("-T") || a.flag("--print-type"),
        all: a.flag("-a") || a.flag("--all"),
        one_filesystem: a.flag("-x") || a.flag("--one-filesystem"),
        json: a.flag("--json"),
    }
}

/// One printed row: the filesystem's names, then the figures in the chosen unit.
struct Row {
    fs: String,
    ty: String,
    size: String,
    used: String,
    avail: String,
    use_pct: String,
    mount: String,
    /// The raw numbers, for `--json`.
    raw: FsUsage,
}

/// A figure in the row's unit: 1 KiB blocks by default (rounded up, as df rounds), powers of
/// 1024 or 1000 under `-h`/`--si`.
fn figure(bytes: u64, unit: Unit) -> String {
    match unit {
        Unit::Blocks => bytes.div_ceil(1024).to_string(),
        Unit::Human(base) => human(bytes, base),
    }
}

/// Powers of `base`, rounded up at the printed precision the way `du -h` prints: `4.0K`,
/// `13K`, `4.1k` for `--si`. The same rule as du's private `human`, kept next to the command
/// that prints it for the same reason du keeps its copy there.
fn human(bytes: u64, base: u64) -> String {
    const BIN: [&str; 7] = ["", "K", "M", "G", "T", "P", "E"];
    const SI: [&str; 7] = ["", "k", "M", "G", "T", "P", "E"];
    let units = if base == 1000 { SI } else { BIN };
    if bytes < base {
        return bytes.to_string();
    }
    let mut idx = 0;
    let mut v = bytes as f64;
    while v >= base as f64 && idx < units.len() - 1 {
        v /= base as f64;
        idx += 1;
    }
    let mut left = if v < 10.0 {
        (v * 10.0).ceil() / 10.0
    } else {
        v.ceil()
    };
    if left >= base as f64 && idx < units.len() - 1 {
        left /= base as f64;
        idx += 1;
    }
    if left < 10.0 {
        format!("{left:.1}{}", units[idx])
    } else {
        format!("{left:.0}{}", units[idx])
    }
}

/// used / (used + available), the share df shows: the denominator is what the filesystem
/// could hold once its free space is added to what is already there, so reserved blocks do
/// not read as usage.
fn use_percent(used: u64, available: u64) -> u64 {
    let total = used.saturating_add(available);
    if total == 0 {
        return 0;
    }
    // df rounds the share up, so a filesystem with one byte used never reads as 0%.
    used.saturating_mul(100).div_ceil(total)
}

/// The pseudo filesystems that exist only to carry kernel state. GNU df leaves them out
/// without `-a` even when the kernel reports a size for one (devtmpfs does), because "how
/// full is /proc" is not a question anyone asks.
fn is_dummy(fs_type: Option<&str>) -> bool {
    const DUMMY: &[&str] = &[
        "proc",
        "sysfs",
        "devtmpfs",
        "devpts",
        "rootfs",
        "securityfs",
        "selinuxfs",
        "cgroup",
        "cgroup2",
        "pstore",
        "debugfs",
        "tracefs",
        "configfs",
        "fusectl",
        "mqueue",
        "hugetlbfs",
        "binfmts",
        "binfmt_misc",
        "bpf",
        "autofs",
        "nsfs",
        "rpc_pipefs",
        "efivarfs",
    ];
    fs_type.is_some_and(|t| DUMMY.contains(&t))
}

/// A stable key for "one filesystem": the device number when the platform has one, else the
/// name. Two mounts of one filesystem collapse; two tmpfs mounts stay two rows.
fn identity(f: &FsUsage) -> String {
    if f.device != 0 {
        format!("dev:{}", f.device)
    } else {
        format!("name:{}", f.filesystem)
    }
}

fn row_of(f: FsUsage, o: &Options) -> Row {
    let pct = use_percent(f.used, f.available);
    let ty = f.fs_type.clone().unwrap_or_else(|| "-".to_string());
    if o.inodes {
        match (f.inodes, f.inodes_used, f.inodes_free) {
            (Some(total), Some(used), Some(free)) => Row {
                fs: f.filesystem.clone(),
                ty,
                size: total.to_string(),
                used: used.to_string(),
                avail: free.to_string(),
                use_pct: format!("{}%", use_percent(used, free)),
                mount: display(&f.mount),
                raw: f,
            },
            _ => Row {
                fs: f.filesystem.clone(),
                ty,
                size: "-".to_string(),
                used: "-".to_string(),
                avail: "-".to_string(),
                use_pct: "-".to_string(),
                mount: display(&f.mount),
                raw: f,
            },
        }
    } else {
        Row {
            fs: f.filesystem.clone(),
            ty,
            size: figure(f.size, o.unit),
            used: figure(f.used, o.unit),
            avail: figure(f.available, o.unit),
            use_pct: format!("{pct}%"),
            mount: display(&f.mount),
            raw: f,
        }
    }
}

fn display(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The rows one invocation prints.
fn collect(o: &Options, paths: &[OsString]) -> (Vec<FsUsage>, usize) {
    let mut failures = 0;
    let mut picked: Vec<FsUsage> = Vec::new();
    if paths.is_empty() {
        match filesystems() {
            Ok(all) => picked = all,
            Err(e) => {
                eprintln!("dusage df: {e}");
                return (Vec::new(), 1);
            }
        }
    } else {
        for p in paths {
            if !Path::new(p).exists() {
                eprintln!(
                    "dusage df: {}: no such file or directory",
                    p.to_string_lossy()
                );
                failures += 1;
                continue;
            }
            match usage_of(Path::new(p)) {
                Ok(f) => {
                    if !picked
                        .iter()
                        .any(|r| r.filesystem == f.filesystem && r.mount == f.mount)
                    {
                        picked.push(f);
                    }
                }
                Err(e) => {
                    eprintln!("dusage df: {}: {e}", p.to_string_lossy());
                    failures += 1;
                }
            }
        }
    }
    if o.one_filesystem {
        if let Some(first) = picked.first().cloned() {
            picked.retain(|r| r.filesystem == first.filesystem && r.mount == first.mount);
        }
    }
    if !o.all {
        picked.retain(|r| r.size > 0 && !is_dummy(r.fs_type.as_deref()));
        let mut seen: Vec<String> = Vec::new();
        picked.retain(|r| {
            let key = identity(r);
            if seen.contains(&key) {
                false
            } else {
                seen.push(key);
                true
            }
        });
    }
    (picked, failures)
}

fn render(rows: &[Row], o: &Options) -> String {
    let mut heads: Vec<&str> = vec!["Filesystem"];
    if o.types {
        heads.push("Type");
    }
    if o.inodes {
        heads.extend(["Inodes", "IUsed", "IFree", "IUse%", "Mounted on"]);
    } else if matches!(o.unit, Unit::Blocks) {
        // df's own headings for the block columns, so a script that greps the header finds
        // the unit it is reading.
        heads.extend(["1K-blocks", "Used", "Available", "Use%", "Mounted on"]);
    } else {
        heads.extend(["Size", "Used", "Avail", "Use%", "Mounted on"]);
    }
    let mut body: Vec<Vec<&str>> = Vec::with_capacity(rows.len());
    for r in rows {
        let mut v = vec![r.fs.as_str()];
        if o.types {
            v.push(r.ty.as_str());
        }
        v.extend([
            r.size.as_str(),
            r.used.as_str(),
            r.avail.as_str(),
            r.use_pct.as_str(),
            r.mount.as_str(),
        ]);
        body.push(v);
    }
    // The last column is a path and grows to the right; every column before it is a figure
    // and is right-aligned, which is what makes df's block of numbers readable.
    let mut widths: Vec<usize> = heads.iter().map(|h| h.chars().count()).collect();
    for row in &body {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let fmt = |cells: &[&str], widths: &[usize], out: &mut String| {
        for (i, cell) in cells.iter().enumerate() {
            if i == 0 {
                out.push_str(&format!("{cell:<w$}", w = widths[0]));
            } else if i + 1 == cells.len() {
                out.push(' ');
                out.push_str(cell);
            } else {
                out.push_str(&format!(" {cell:>w$}", w = widths[i]));
            }
        }
        out.push('\n');
    };
    fmt(&heads, &widths, &mut out);
    for row in &body {
        let cells: Vec<&str> = row.clone();
        fmt(&cells, &widths, &mut out);
    }
    out
}

fn json(rows: &[Row], out: &mut dyn Write) -> io::Result<()> {
    let mut s = String::from("{\"filesystems\":[");
    for (i, r) in rows.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let f = &r.raw;
        s.push_str("{\"filesystem\":");
        push_json_str(&f.filesystem, &mut s);
        s.push_str(",\"type\":");
        match &f.fs_type {
            Some(t) => push_json_str(t, &mut s),
            None => s.push_str("null"),
        }
        s.push_str(",\"mount\":");
        push_json_str(&display(&f.mount), &mut s);
        s.push_str(&format!(
            ",\"size\":{},\"used\":{},\"available\":{},\"use_percent\":{}",
            f.size,
            f.used,
            f.available,
            use_percent(f.used, f.available)
        ));
        s.push_str(&format!(
            ",\"inodes\":{},\"inodes_used\":{},\"inodes_free\":{}}}",
            json_opt(f.inodes),
            json_opt(f.inodes_used),
            json_opt(f.inodes_free)
        ));
    }
    s.push_str("]}\n");
    out.write_all(s.as_bytes())
}

fn json_opt(v: Option<u64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    }
}

fn push_json_str(v: &str, out: &mut String) {
    out.push('"');
    crate::report::json_escape(v, out);
    out.push('"');
}

pub fn run(raw: Vec<String>) -> Result<(), Fail> {
    let a = Args::parse(raw, VALUE_FLAGS, BOOL_FLAGS)?;
    if a.flag("--help") {
        print!("{DF_HELP}");
        return Ok(());
    }
    if a.flag("-V") || a.flag("--version") {
        println!("dusage {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let o = options(&a);
    let paths: Vec<OsString> = a.positionals().iter().map(OsString::from).collect();
    let (picked, failures) = collect(&o, &paths);
    let rows: Vec<Row> = picked.into_iter().map(|f| row_of(f, &o)).collect();
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    if o.json {
        json(&rows, &mut out).map_err(Fail::output)?;
    } else {
        out.write_all(render(&rows, &o).as_bytes())
            .map_err(Fail::output)?;
    }
    out.flush().map_err(Fail::output)?;
    if failures > 0 || rows.is_empty() {
        return Err(Fail(1, String::new()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_units_follow_du() {
        assert_eq!(human(4096, 1024), "4.0K");
        assert_eq!(human(13000, 1024), "13K");
        assert_eq!(human(4100, 1000), "4.1k");
        assert_eq!(human(0, 1024), "0");
    }

    #[test]
    fn blocks_round_up_like_df() {
        assert_eq!(figure(0, Unit::Blocks), "0");
        assert_eq!(figure(1, Unit::Blocks), "1");
        assert_eq!(figure(1024, Unit::Blocks), "1");
        assert_eq!(figure(1025, Unit::Blocks), "2");
    }

    #[test]
    fn use_percent_ignores_reserved_blocks() {
        assert_eq!(use_percent(0, 0), 0);
        assert_eq!(use_percent(50, 50), 50);
        // 95 GiB used, 5 GiB available, a 10 GiB reserve inside it: 100%, not 95%.
        assert_eq!(use_percent(9500, 500), 95);
    }
}
