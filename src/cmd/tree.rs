//! `dusage tree`: the shape of tree(1), with the sizes tree cannot print.
//!
//! Two deterministic depth-first passes, listed with the batched enumerator so each entry
//! arrives with its apparent and allocated bytes from one syscall: the first sums the
//! subtrees and measures the columns, the second lists again and prints. Two size columns go
//! in front of every name — the bytes the entry occupies on disk (`st_blocks * 512` on Linux,
//! the allocation size the Windows enumerator returns) and the length it claims (`st_size`).
//! A directory's row carries its subtree, the way `tree --du` shows directory usage, so the
//! columns answer "how big is this branch" and "where did the bytes go" in one listing.
//!
//! # Links
//!
//! A symlink — on Windows any reparse point — is printed as the entry it is (`name ->
//! target`) and never followed, for the reason every other command refuses to: a link to an
//! ancestor is a cycle and a link to `/` is another device's bytes. Its own blocks are
//! counted, which is what `du` charges it.
//!
//! # Filters
//!
//! The ignore layers are find's: the builtin caches, `.gitignore`, `.ignore`, the parent
//! directories' files, and the same hidden rule, so `dusage tree` on a checkout shows what
//! `dusage find` would return. `-a` turns the hidden rule off, `--no-ignore` turns the files
//! off, and `-g` filters with a gitignore-style glob exactly as find's does (a positive
//! pattern switches the ignore layers off for the paths it names).
//!
//! # Memory
//!
//! One record per directory — its name in one byte arena, its subtree's two totals, and
//! the links to its first child and next sibling — so memory follows the directory count,
//! not the entry count. Files and links are summed and measured as the first pass meets
//! them and then dropped; the second pass lists a directory again when it prints it and
//! holds that one directory's rows while it sorts them. Totals and column widths have to be
//! known before the first row, which is why nothing is printed during the first pass.
//!
//! The price is a second listing of every directory that has a file or link row (none under
//! `-d`), and a tree that changes between the two passes: a row the second pass sees is
//! printed with the sizes it has then, under totals and a footer from the first.

use super::Fail;
use crate::cli::Args;
use crate::ignore::IgnoreRules;
use crate::model::ErrorCounts;
use crate::native::enumerate::{self, DirReader, Entry};
use std::ffi::OsString;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::Path;

const TREE_HELP: &str = "dusage tree - a directory tree with on-disk and apparent sizes

USAGE:
    dusage tree [PATH...] [OPTIONS]

Each line is one entry, indented the way tree indents it, with two sizes in brackets
before the name: the bytes it occupies on disk first, then the length it claims. A
directory's row is its whole subtree (as `tree --du` reports directory usage), a file's is
its own, and a link's is the link itself, because links are never followed.

OPTIONS:
    -L, --level N         descend at most N levels below the start point (1 = its children)
    -a, --all             include dot entries; hidden entries are skipped without it
    -d, --dirs-only       print directories only
    -h, --human-readable  powers of 1024 (4.0K, 13K), the same rule as `du -h`
        --si              powers of 1000
        --sort KEY        `name` (default, byte order) or `size` (largest on-disk subtree
                          first, ties by name)
        --no-ignore       do not read .gitignore, .ignore or the builtin caches
    -g, --glob PATTERN    skip entries a gitignore-style glob excludes, or with a positive
                          pattern show only what it names and ignore the ignore files
    -x, --one-file-system do not descend into a directory on another filesystem
        --json            one JSON object: the entries in print order, with both sizes and
                          the run's totals
        --help            show this help (there is no short help flag: -h is human-readable)
    -V, --version         print the version

OUTPUT:
    /tmp/tt
    ├── [   40   3]  a.txt
    ├── [    0   4]  link -> /tmp
    └── [   60   2]  sub
        └── [ 4096   2]  b

    3 directories, 2 files (4136 bytes on disk, 11 apparent)

    Both columns right-align on the widest value in the tree; -h and --si scale them.
    The footer keeps tree's directory and file counts and adds the run's totals.

LINKS AND ERRORS:
    A symlink (on Windows, any reparse point) prints its target and is never followed. An
    unreadable directory is reported once on stderr, counted as empty, and makes the run
    exit 1; a missing start point is the same. Bad arguments exit 2.
";

/// The flags `tree` takes, for [`super::accepted_flags`].
pub(super) const VALUE_FLAGS: &[&str] = &["-L", "--level", "--sort", "-g", "--glob"];

pub(super) const BOOL_FLAGS: &[&str] = &[
    "-a",
    "--all",
    "-d",
    "--dirs-only",
    "-h",
    "--human-readable",
    "--si",
    "--no-ignore",
    "-x",
    "--one-file-system",
    "--json",
    "--help",
    "-V",
    "--version",
];

/// The tree help text, for `dusage --help`.
pub fn help() -> &'static str {
    TREE_HELP
}

struct Options {
    all: bool,
    dirs_only: bool,
    /// `usize::MAX` when no limit was given; level 1 is the start point's children.
    level: usize,
    /// `Some(base)` under `-h`/`--si`, `None` for plain byte counts.
    human: Option<u64>,
    by_size: bool,
    json: bool,
    no_ignore: bool,
    globs: Vec<String>,
    one_filesystem: bool,
}

/// A name in the tree's one byte arena: a range, not an owned string.
#[derive(Clone, Copy)]
struct Name {
    off: u32,
    len: u32,
}

/// The walk listed this directory; one it only counted (a mount point under `-x`) is not set.
const ENTERED: u8 = 1;
/// Listing it yields at least one row that is not a directory, so printing must read it again.
const HAS_ROWS: u8 = 2;
/// Its ignore files contributed a layer, so printing must stack them again.
const HAS_RULES: u8 = 4;
/// It was read by the portable reader, which decides what its entries' on-disk size means.
const PLAIN: u8 = 8;
/// Its listing failed and was reported; the second read is not reported again.
const FAILED: u8 = 16;

/// What the walk keeps: one record per directory. Files and links are folded into their
/// directory's totals and the column widths as they go by, and read again when printed.
struct Dir {
    name: Name,
    total_disk: u64,
    total_apparent: u64,
    /// The first child directory and the next sibling, in the order the walk met them;
    /// 0 is "none", because the start point is record 0 and nothing's child or sibling.
    first_kid: u32,
    next: u32,
    flags: u8,
}

/// One printed row, alive only while its directory is being printed.
struct Row {
    name: String,
    target: Option<String>,
    disk: u64,
    apparent: u64,
    /// The directory record behind the row, or 0 for a file or a link.
    dir: u32,
    link: bool,
}

struct Tree<'a> {
    o: &'a Options,
    rules: IgnoreRules,
    nodes: Vec<Dir>,
    names: Vec<u8>,
    /// The real names of the directories whose text is a lossy rendering, by record, in
    /// record order: the second pass opens a directory by the name the filesystem gave.
    raw: Vec<(u32, OsString)>,
    reader: DirReader,
    plain: bool,
    root_dev: u64,
    errs: ErrorCounts,
    messages: Vec<String>,
    dirs: u64,
    files: u64,
    links: u64,
    skipped_mounts: u64,
    /// A directory the walk read could not be read again to print its rows.
    late_failure: bool,
    /// Column widths over exactly the rows the listing prints.
    wd: usize,
    wa: usize,
}

impl<'a> Tree<'a> {
    fn new(o: &'a Options, rules: IgnoreRules) -> Self {
        Tree {
            o,
            rules,
            nodes: Vec::new(),
            names: Vec::new(),
            raw: Vec::new(),
            reader: DirReader::new(),
            plain: false,
            root_dev: 0,
            errs: ErrorCounts::default(),
            messages: Vec::new(),
            dirs: 0,
            files: 0,
            links: 0,
            skipped_mounts: 0,
            late_failure: false,
            wd: 1,
            wa: 1,
        }
    }

    fn intern(&mut self, s: &str) -> Name {
        let off = self.names.len() as u32;
        self.names.extend_from_slice(s.as_bytes());
        Name {
            off,
            len: s.len() as u32,
        }
    }

    /// The text of a name in the arena. Names are interned from `to_string_lossy` output,
    /// so every range is valid UTF-8 by construction.
    fn text(&self, n: Name) -> &str {
        std::str::from_utf8(&self.names[n.off as usize..(n.off + n.len) as usize]).unwrap_or("")
    }

    /// Read one directory, falling back to the portable reader for the rest of the run when
    /// the filesystem rejects the batched query (the scan engine's rule).
    fn read(&mut self, dir: &Path, out: &mut Vec<Entry>) -> io::Result<()> {
        if self.plain {
            return enumerate::read_plain(dir, out, &mut self.errs);
        }
        match self.reader.read(dir, out, &mut self.errs) {
            Err(e) if enumerate::is_unsupported(&e) => {
                self.plain = true;
                out.clear();
                enumerate::read_plain(dir, out, &mut self.errs)
            }
            r => r,
        }
    }

    /// A row the listing prints: widen the columns to hold it.
    fn measure(&mut self, disk: u64, apparent: u64) {
        self.wd = self.wd.max(size_width(disk, self.o.human));
        self.wa = self.wa.max(size_width(apparent, self.o.human));
    }

    /// The first pass: list `dir` under record `node`, descending while the level limit
    /// allows. Directories get a record; everything else is counted, summed into `node` and
    /// measured for the columns, and then forgotten.
    fn walk(&mut self, dir: &Path, rel: &str, depth: usize, node: usize) {
        let mark = if self.o.no_ignore {
            self.rules.mark()
        } else {
            self.rules.push_dir(dir, rel)
        };
        let mut flags = ENTERED;
        if self.rules.mark() > mark {
            flags |= HAS_RULES;
        }
        let mut ents = Vec::new();
        if let Err(e) = self.read(dir, &mut ents) {
            self.messages.push(e.to_string());
            flags |= FAILED;
        }
        let plain = self.plain;
        if plain {
            flags |= PLAIN;
        }
        let dir_hit = self.rules.builtin_under(rel);
        let (mut disk, mut apparent) = (0u64, 0u64);
        let mut last_kid = 0usize;
        for e in ents {
            let text = e.name.to_string_lossy();
            let child_rel = if rel.is_empty() {
                text.to_string()
            } else {
                format!("{rel}/{text}")
            };
            if !allow(&self.rules, self.o, &child_rel, &text, e.is_dir(), dir_hit) {
                continue;
            }
            let (own_disk, own_apparent) = own_sizes(&e, plain);
            if e.is_reparse() {
                self.links += 1;
                self.files += 1;
            } else if depth + 1 > self.o.level {
                continue;
            } else if e.is_dir() {
                self.dirs += 1;
                let idx = self.nodes.len();
                if let std::borrow::Cow::Owned(_) = text {
                    self.raw.push((idx as u32, e.name.clone()));
                }
                let name = self.intern(&text);
                self.nodes.push(Dir {
                    name,
                    total_disk: own_disk,
                    total_apparent: own_apparent,
                    first_kid: 0,
                    next: 0,
                    flags: 0,
                });
                if last_kid == 0 {
                    self.nodes[node].first_kid = idx as u32;
                } else {
                    self.nodes[last_kid].next = idx as u32;
                }
                last_kid = idx;
                if self.o.one_filesystem && enumerate::other_filesystem(&e, self.root_dev) {
                    self.skipped_mounts += 1;
                } else {
                    self.walk(&dir.join(&e.name), &child_rel, depth + 1, idx);
                }
                let (d, a) = (self.nodes[idx].total_disk, self.nodes[idx].total_apparent);
                disk += d;
                apparent += a;
                self.measure(d, a);
                continue;
            } else {
                self.files += 1;
            }
            flags |= HAS_ROWS;
            disk += own_disk;
            apparent += own_apparent;
            if !self.o.dirs_only {
                self.measure(own_disk, own_apparent);
            }
        }
        let n = &mut self.nodes[node];
        n.total_disk += disk;
        n.total_apparent += apparent;
        n.flags = flags;
        self.rules.pop_to(mark);
    }

    /// The rows under record `node`, in print order: its child directories from their
    /// records, and — unless only directories are printed — its files and links from a
    /// second listing of `dir`, filtered exactly as the walk filtered them.
    fn rows(&mut self, dir: &Path, rel: &str, depth: usize, node: usize) -> Vec<Row> {
        let mut rows = Vec::new();
        let mut kid = self.nodes[node].first_kid;
        while kid != 0 {
            let n = &self.nodes[kid as usize];
            rows.push(Row {
                name: self.text(n.name).to_string(),
                target: None,
                disk: n.total_disk,
                apparent: n.total_apparent,
                dir: kid,
                link: false,
            });
            kid = n.next;
        }
        let flags = self.nodes[node].flags;
        if !self.o.dirs_only && flags & HAS_ROWS != 0 {
            let plain = flags & PLAIN != 0;
            let mut ents = Vec::new();
            let read = if plain {
                enumerate::read_plain(dir, &mut ents, &mut self.errs)
            } else {
                self.reader.read(dir, &mut ents, &mut self.errs)
            };
            if let Err(e) = read {
                if flags & FAILED == 0 {
                    eprintln!("dusage tree: {e}");
                    self.late_failure = true;
                }
            }
            let dir_hit = self.rules.builtin_under(rel);
            for e in ents {
                let link = e.is_reparse();
                if !link && (e.is_dir() || depth + 1 > self.o.level) {
                    continue;
                }
                let text = e.name.to_string_lossy();
                let child_rel = if rel.is_empty() {
                    text.to_string()
                } else {
                    format!("{rel}/{text}")
                };
                if !allow(&self.rules, self.o, &child_rel, &text, e.is_dir(), dir_hit) {
                    continue;
                }
                let (disk, apparent) = own_sizes(&e, plain);
                let target = if link {
                    fs::read_link(dir.join(&e.name))
                        .ok()
                        .map(|t| t.to_string_lossy().into_owned())
                } else {
                    None
                };
                rows.push(Row {
                    name: text.into_owned(),
                    target,
                    disk,
                    apparent,
                    dir: 0,
                    link,
                });
            }
        }
        if self.o.by_size {
            rows.sort_by(|a, b| b.disk.cmp(&a.disk).then_with(|| a.name.cmp(&b.name)));
        } else {
            rows.sort_by(|a, b| a.name.cmp(&b.name));
        }
        rows
    }

    /// The second pass: print everything under record `node`, one directory's rows in
    /// memory at a time per level of depth. `prefix` is the text listing's indentation and
    /// is unused by the JSON one.
    fn print(
        &mut self,
        dir: &Path,
        rel: &str,
        depth: usize,
        node: usize,
        prefix: &mut String,
        out: &mut dyn Write,
    ) -> io::Result<()> {
        let flags = self.nodes[node].flags;
        let mark = if flags & HAS_RULES != 0 {
            self.rules.push_dir(dir, rel)
        } else {
            self.rules.mark()
        };
        let rows = self.rows(dir, rel, depth, node);
        let base = prefix.len();
        let mut line = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            let child_rel = if rel.is_empty() {
                row.name.clone()
            } else {
                format!("{rel}/{}", row.name)
            };
            let last = i + 1 == rows.len();
            line.clear();
            if self.o.json {
                write!(line, ",{{\"depth\":{},\"name\":", depth + 1).ok();
                push_json_str(&row.name, &mut line);
                line.extend_from_slice(b",\"rel\":");
                push_json_str(&child_rel, &mut line);
                write!(
                    line,
                    ",\"is_dir\":{},\"is_link\":{}",
                    row.dir != 0,
                    row.link
                )
                .ok();
                if let Some(t) = &row.target {
                    line.extend_from_slice(b",\"target\":");
                    push_json_str(t, &mut line);
                }
                write!(
                    line,
                    ",\"on_disk\":{},\"apparent\":{}}}",
                    row.disk, row.apparent
                )
                .ok();
            } else {
                let disk = size_text(row.disk, self.o.human);
                let apparent = size_text(row.apparent, self.o.human);
                let (wd, wa) = (self.wd, self.wa);
                let branch = if last { "└── " } else { "├── " };
                write!(
                    line,
                    "{prefix}{branch}[ {disk:>wd$} {apparent:>wa$}]  {}",
                    row.name
                )
                .ok();
                if let Some(t) = &row.target {
                    write!(line, " -> {t}").ok();
                }
                line.push(b'\n');
            }
            out.write_all(&line)?;
            let kid = row.dir as usize;
            if kid == 0 || !self.descends(kid) {
                continue;
            }
            let path = match self.raw.binary_search_by_key(&row.dir, |r| r.0) {
                Ok(at) => dir.join(&self.raw[at].1),
                Err(_) => dir.join(&row.name),
            };
            prefix.push_str(if last { "    " } else { "│   " });
            self.print(&path, &child_rel, depth + 1, kid, prefix, out)?;
            prefix.truncate(base);
        }
        self.rules.pop_to(mark);
        Ok(())
    }

    /// Whether a directory has any row under it to print.
    fn descends(&self, node: usize) -> bool {
        let n = &self.nodes[node];
        n.first_kid != 0 || (!self.o.dirs_only && n.flags & HAS_ROWS != 0)
    }
}

/// Whether the walk may report one entry: the hidden rule, then the ignore layers, then the
/// `-g` globs. The same three steps, in the same order, as the `find` walk.
fn allow(
    rules: &IgnoreRules,
    o: &Options,
    rel: &str,
    name: &str,
    is_dir: bool,
    dir_hit: bool,
) -> bool {
    if !o.all && name.starts_with('.') {
        // A positive glob re-includes a dotfile it names; a dot directory still needs -a, so
        // the walk never wanders into `.git`.
        if is_dir || !rules.glob_named(rel) {
            return false;
        }
    }
    let globs_win = rules.whitelisted() || o.no_ignore;
    if !globs_win && rules.is_ignored_under(dir_hit, name, rel, is_dir) {
        return false;
    }
    rules.glob_allows(rel, is_dir)
}

/// An entry's own two sizes: the bytes it occupies and the bytes it claims. A backend that
/// cannot report allocations (`readdir` on Windows) charges apparent bytes instead, the way
/// `du` does there; a reported 0 — a hole, an empty file — stays 0.
fn own_sizes(e: &Entry, plain: bool) -> (u64, u64) {
    let disk = if allocs_known(plain) { e.alloc } else { e.size };
    // A directory contributes no apparent bytes of its own: GNU's rule for `-b`.
    let apparent = if e.is_dir() { 0 } else { e.size };
    (disk, apparent)
}

/// Whether this backend reports allocated sizes at all. The portable `readdir` reader does
/// not on Windows; on Linux every backend reads `st_blocks`.
fn allocs_known(plain: bool) -> bool {
    !plain || cfg!(target_os = "linux")
}

fn parse_level(s: &str) -> Result<usize, Fail> {
    s.trim()
        .parse::<usize>()
        .map_err(|_| Fail(2, format!("--level {s:?} is not a whole number")))
}

fn options(a: &Args) -> Result<Options, Fail> {
    let by_size = match a.value("--sort").unwrap_or("name") {
        "name" => false,
        "size" => true,
        other => {
            return Err(Fail(
                2,
                format!("--sort {other:?} is not one of: name, size"),
            ))
        }
    };
    let human = if a.flag("-h") || a.flag("--human-readable") {
        Some(1024u64)
    } else if a.flag("--si") {
        Some(1000u64)
    } else {
        None
    };
    let mut globs: Vec<String> = a.values("--glob").to_vec();
    globs.extend(a.values("-g").iter().cloned());
    Ok(Options {
        all: a.flag("-a") || a.flag("--all"),
        dirs_only: a.flag("-d") || a.flag("--dirs-only"),
        level: match a.value("--level").or(a.value("-L")) {
            Some(v) => parse_level(v)?,
            None => usize::MAX,
        },
        human,
        by_size,
        json: a.flag("--json"),
        no_ignore: a.flag("--no-ignore"),
        globs,
        one_filesystem: a.flag("-x") || a.flag("--one-file-system"),
    })
}

/// The rules one start point begins with: the shared layers plus its ancestors' ignore
/// files, exactly as `find` builds them.
fn root_rules(o: &Options, root: &Path) -> Result<IgnoreRules, Fail> {
    let mut rules = IgnoreRules::new(!o.no_ignore);
    if !o.no_ignore {
        rules.push_global();
        rules.push_ancestors(root);
    }
    rules
        .add_globs(&o.globs)
        .map_err(|e| Fail(2, format!("--glob: {e}")))?;
    Ok(rules)
}

/// One JSON string, quotes included: `report::json_escape` writes the contents only.
fn push_json_str(v: &str, out: &mut Vec<u8>) {
    let mut buf = String::with_capacity(v.len() + 2);
    buf.push('"');
    crate::report::json_escape(v, &mut buf);
    buf.push('"');
    out.extend_from_slice(buf.as_bytes());
}

/// The printed width of a size, without building the text for a plain byte count.
fn size_width(v: u64, human: Option<u64>) -> usize {
    match human {
        Some(base) => human_bytes(v, base).len(),
        None => v.checked_ilog10().map_or(1, |d| d as usize + 1),
    }
}

fn size_text(v: u64, human: Option<u64>) -> String {
    match human {
        Some(base) => human_bytes(v, base),
        None => v.to_string(),
    }
}

/// Powers of `base`, rounded up at the printed precision the way `du -h` prints: `4.0K`,
/// `13K`, `4.1k` for `--si`. The same rule as du's private `human`, kept next to the command
/// that prints it for the same reason du keeps its copy there.
fn human_bytes(bytes: u64, base: u64) -> String {
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

/// Allocated bytes of a path whose metadata is already in hand: `st_blocks * 512` on Linux
/// and macOS. Windows' `std` metadata carries the EndOfFile size and not AllocationSize, so a
/// start point's own blocks are `None` there and count 0 — the limitation `du` documents too.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn path_alloc(md: &fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(md.blocks() * 512)
}

#[cfg(windows)]
fn path_alloc(_md: &fs::Metadata) -> Option<u64> {
    None
}

/// The exit status of the whole command, and the stream every start point writes to.
struct Run {
    failed: bool,
}

impl Run {
    fn one(&mut self, spec: &str, o: &Options, out: &mut dyn Write) -> io::Result<()> {
        let path = Path::new(spec);
        let md = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("dusage tree: cannot access '{spec}': {e}");
                self.failed = true;
                return Ok(());
            }
        };
        if let Err(e) = enumerate::refuse_reparse_root(path) {
            eprintln!("dusage tree: {e}");
            self.failed = true;
            return Ok(());
        }
        let rules = match root_rules(o, path) {
            Ok(r) => r,
            Err(f) => {
                eprintln!("dusage tree: {}", f.1);
                self.failed = true;
                return Ok(());
            }
        };
        let mut tree = Tree::new(o, rules);
        if o.one_filesystem {
            tree.root_dev = enumerate::dev_of(path).unwrap_or(0);
        }
        let root_dir = md.is_dir();
        let own_disk = path_alloc(&md).unwrap_or(0);
        let own_apparent = if root_dir { 0 } else { md.len() };
        let name = tree.intern(spec);
        tree.nodes.push(Dir {
            name,
            total_disk: own_disk,
            total_apparent: own_apparent,
            first_kid: 0,
            next: 0,
            flags: 0,
        });
        if root_dir && o.level > 0 {
            tree.walk(path, "", 0, 0);
        }
        for m in &tree.messages {
            eprintln!("dusage tree: {m}");
        }
        if !root_dir {
            tree.files = 1;
        }
        let rendered = if o.level == 0 && root_dir {
            Ok(())
        } else if o.json {
            json_render(&mut tree, path, root_dir, out)
        } else {
            text_render(&mut tree, path, out)
        };
        if tree.errs.total() > 0 || tree.late_failure {
            self.failed = true;
        }
        rendered
    }
}

fn text_render(tree: &mut Tree<'_>, path: &Path, out: &mut dyn Write) -> io::Result<()> {
    let o = tree.o;
    let disk = size_text(tree.nodes[0].total_disk, o.human);
    let apparent = size_text(tree.nodes[0].total_apparent, o.human);
    let line = format!("[ {disk} {apparent}]  {}", tree.text(tree.nodes[0].name));
    writeln!(out, "{line}")?;
    tree.print(path, "", 0, 0, &mut String::new(), out)?;
    let counts = if o.dirs_only {
        format!("{} directories", tree.dirs)
    } else {
        format!("{} directories, {} files", tree.dirs, tree.files)
    };
    let footer = format!(
        "\n{counts} ({disk} on disk, {apparent} apparent{})\n",
        if tree.skipped_mounts > 0 {
            format!(", {} mount points not entered", tree.skipped_mounts)
        } else {
            String::new()
        }
    );
    out.write_all(footer.as_bytes())
}

fn json_render(
    tree: &mut Tree<'_>,
    path: &Path,
    root_dir: bool,
    out: &mut dyn Write,
) -> io::Result<()> {
    let (total_disk, total_apparent) = (tree.nodes[0].total_disk, tree.nodes[0].total_apparent);
    let name = quote(tree.text(tree.nodes[0].name));
    write!(
        out,
        "{{\"root\":{name},\"entries\":[{{\"depth\":0,\"name\":{name},\"rel\":\"\",\
             \"is_dir\":{root_dir},\"is_link\":false,\"on_disk\":{total_disk},\
             \"apparent\":{total_apparent}}}"
    )?;
    tree.print(path, "", 0, 0, &mut String::new(), out)?;
    writeln!(
        out,
        "],\"dirs\":{},\"files\":{},\"links\":{},\"on_disk\":{total_disk},\
             \"apparent\":{total_apparent},\"skipped_mounts\":{}}}",
        tree.dirs, tree.files, tree.links, tree.skipped_mounts
    )
}

fn quote(v: &str) -> String {
    let mut s = Vec::new();
    push_json_str(v, &mut s);
    String::from_utf8(s).unwrap_or_default()
}

pub fn run(raw: Vec<String>) -> Result<(), Fail> {
    let a = Args::parse(raw, VALUE_FLAGS, BOOL_FLAGS)?;
    if a.flag("--help") {
        print!("{TREE_HELP}");
        return Ok(());
    }
    if a.flag("-V") || a.flag("--version") {
        println!("dusage {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let o = options(&a)?;
    let roots: Vec<String> = if a.positionals().is_empty() {
        vec![".".to_string()]
    } else {
        a.positionals().to_vec()
    };
    let mut run = Run { failed: false };
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    for spec in &roots {
        run.one(spec, &o, &mut out).map_err(Fail::output)?;
    }
    out.flush().map_err(Fail::output)?;
    if run.failed {
        return Err(Fail(1, String::new()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_units_follow_du() {
        assert_eq!(human_bytes(4096, 1024), "4.0K");
        assert_eq!(human_bytes(13000, 1024), "13K");
        assert_eq!(human_bytes(7095, 1024), "7.0K");
        assert_eq!(human_bytes(4100, 1000), "4.1k");
        assert_eq!(human_bytes(0, 1024), "0");
        assert_eq!(human_bytes(1048575, 1024), "1.0M");
    }

    #[test]
    fn flags_parse_and_misuse_is_exit_2() {
        let a = Args::parse(
            vec!["--level".into(), "2".into(), "--sort".into(), "size".into()],
            VALUE_FLAGS,
            BOOL_FLAGS,
        )
        .unwrap();
        let o = options(&a).unwrap();
        assert_eq!(o.level, 2);
        assert!(o.by_size);
        let bad = Args::parse(
            vec!["--sort".into(), "mtime".into()],
            VALUE_FLAGS,
            BOOL_FLAGS,
        )
        .unwrap();
        assert_eq!(options(&bad).err().map(|f| f.0), Some(2));
    }

    #[test]
    fn json_strings_are_quoted_and_escaped() {
        assert_eq!(quote("plain"), "\"plain\"");
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
    }
}
