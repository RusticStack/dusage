//! `dusage tree`: the shape of tree(1), with the sizes tree cannot print.
//!
//! One deterministic depth-first walk, listed with the batched enumerator so each entry
//! arrives with its apparent and allocated bytes from one syscall, then two pre-order passes
//! over the collected records: one to measure the columns, one to print. Two size columns go
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
//! One compact record per entry plus its name, so a quarter-million-entry tree costs a few
//! tens of megabytes: a name is appended to one byte arena and referred to by offset,
//! children are `u32` indexes into the record arena, and each record keeps four numbers (its
//! own two sizes and its subtree's two). Nothing is printed during the walk, which is what
//! makes `--sort size` and the aligned columns possible at all.

use super::Fail;
use crate::cli::Args;
use crate::ignore::IgnoreRules;
use crate::model::ErrorCounts;
use crate::native::enumerate::{self, DirReader, Entry};
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

const IS_DIR: u8 = 1;
const IS_LINK: u8 = 2;

struct Node {
    name: Name,
    /// The link's target, when the platform names it; links are rare, so the extra range
    /// costs nothing on the entries that are files.
    target: Option<Name>,
    total_disk: u64,
    total_apparent: u64,
    kids: Vec<u32>,
    parent: u32,
    depth: u32,
    flags: u8,
}

impl Node {
    fn is_dir(&self) -> bool {
        self.flags & IS_DIR != 0
    }

    fn is_link(&self) -> bool {
        self.flags & IS_LINK != 0
    }
}

struct Tree<'a> {
    o: &'a Options,
    rules: IgnoreRules,
    nodes: Vec<Node>,
    names: Vec<u8>,
    reader: DirReader,
    plain: bool,
    root_dev: u64,
    errs: ErrorCounts,
    messages: Vec<String>,
    dirs: u64,
    files: u64,
    links: u64,
    skipped_mounts: u64,
}

impl<'a> Tree<'a> {
    fn new(o: &'a Options, rules: IgnoreRules) -> Self {
        Tree {
            o,
            rules,
            nodes: Vec::new(),
            names: Vec::new(),
            reader: DirReader::new(),
            plain: false,
            root_dev: 0,
            errs: ErrorCounts::default(),
            messages: Vec::new(),
            dirs: 0,
            files: 0,
            links: 0,
            skipped_mounts: 0,
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

    /// List `dir` into the arena under `node`, descending while the level limit allows.
    fn list(&mut self, dir: &Path, rel: &str, depth: usize, node: usize) {
        let mark = if self.o.no_ignore {
            self.rules.mark()
        } else {
            self.rules.push_dir(dir, rel)
        };
        let mut ents = Vec::new();
        if let Err(e) = self.read(dir, &mut ents) {
            self.messages.push(e.to_string());
        }
        let dir_hit = self.rules.builtin_under(rel);
        for e in ents {
            let text = e.name.to_string_lossy().into_owned();
            let child_rel = if rel.is_empty() {
                text.clone()
            } else {
                format!("{rel}/{text}")
            };
            if !allow(&self.rules, self.o, &child_rel, &text, e.is_dir(), dir_hit) {
                continue;
            }
            if e.is_reparse() {
                self.links += 1;
                self.files += 1;
                let name = self.intern(&text);
                let link = dir.join(&e.name);
                let target = fs::read_link(&link)
                    .ok()
                    .map(|t| t.to_string_lossy().into_owned())
                    .map(|t| self.intern(&t));
                let (own_disk, own_apparent) = own_sizes(&e, self.plain);
                self.add(
                    node,
                    Node {
                        name,
                        target,
                        total_disk: own_disk,
                        total_apparent: own_apparent,
                        kids: Vec::new(),
                        parent: node as u32,
                        depth: (depth + 1) as u32,
                        flags: IS_LINK,
                    },
                );
                continue;
            }
            if depth + 1 > self.o.level {
                continue;
            }
            let path = dir.join(&e.name);
            let is_dir = e.is_dir();
            let (own_disk, own_apparent) = own_sizes(&e, self.plain);
            let name = self.intern(&text);
            if is_dir {
                self.dirs += 1;
            } else {
                self.files += 1;
            }
            let idx = self.add(
                node,
                Node {
                    name,
                    target: None,
                    total_disk: own_disk,
                    total_apparent: own_apparent,
                    kids: Vec::new(),
                    parent: node as u32,
                    depth: (depth + 1) as u32,
                    flags: if is_dir { IS_DIR } else { 0 },
                },
            );
            if !is_dir {
                continue;
            }
            if self.o.one_filesystem && enumerate::other_filesystem(&e, self.root_dev) {
                self.skipped_mounts += 1;
                continue;
            }
            self.list(&path, &child_rel, depth + 1, idx);
        }
        self.rules.pop_to(mark);
    }

    fn add(&mut self, parent: usize, n: Node) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(n);
        self.nodes[parent].kids.push(idx as u32);
        idx
    }

    /// Roll every child's subtree total into its parent, deepest first: records are pushed
    /// parents-before-children, so one reverse pass leaves every total final.
    fn totals(&mut self) {
        for i in (1..self.nodes.len()).rev() {
            let (disk, apparent, parent) = {
                let n = &self.nodes[i];
                (n.total_disk, n.total_apparent, n.parent as usize)
            };
            let p = &mut self.nodes[parent];
            p.total_disk += disk;
            p.total_apparent += apparent;
        }
    }

    /// Order every directory's children once, so both render passes walk one sequence.
    fn sort_kids(&mut self) {
        for i in 0..self.nodes.len() {
            if self.nodes[i].kids.len() < 2 {
                continue;
            }
            let mut kids = std::mem::take(&mut self.nodes[i].kids);
            if self.o.by_size {
                kids.sort_by(|a, b| {
                    let (x, y) = (&self.nodes[*a as usize], &self.nodes[*b as usize]);
                    y.total_disk
                        .cmp(&x.total_disk)
                        .then_with(|| self.text(x.name).cmp(self.text(y.name)))
                });
            } else {
                kids.sort_by(|a, b| {
                    self.text(self.nodes[*a as usize].name)
                        .cmp(self.text(self.nodes[*b as usize].name))
                });
            }
            self.nodes[i].kids = kids;
        }
    }

    fn visible(&self, node: usize) -> impl Iterator<Item = usize> + '_ {
        self.nodes[node]
            .kids
            .iter()
            .map(|k| *k as usize)
            .filter(|k| !self.o.dirs_only || self.nodes[*k].is_dir())
    }

    /// Column widths over exactly the rows the listing prints.
    fn widths(&self, node: usize, wd: &mut usize, wa: &mut usize) {
        for kid in self.visible(node) {
            let n = &self.nodes[kid];
            *wd = (*wd).max(size_text(n.total_disk, self.o.human).len());
            *wa = (*wa).max(size_text(n.total_apparent, self.o.human).len());
            self.widths(kid, wd, wa);
        }
    }

    fn print_rows(
        &self,
        node: usize,
        prefix: &mut String,
        wd: usize,
        wa: usize,
        out: &mut dyn Write,
    ) -> io::Result<()> {
        let kids: Vec<usize> = self.visible(node).collect();
        let base = prefix.len();
        for (i, kid) in kids.iter().enumerate() {
            let last = i + 1 == kids.len();
            prefix.push_str(if last { "└── " } else { "├── " });
            let n = &self.nodes[*kid];
            let disk = size_text(n.total_disk, self.o.human);
            let apparent = size_text(n.total_apparent, self.o.human);
            let mut line = format!(
                "{prefix}[ {disk:>wd$} {apparent:>wa$}]  {}",
                self.text(n.name)
            );
            if let Some(t) = n.target {
                line.push_str(" -> ");
                line.push_str(self.text(t));
            }
            line.push('\n');
            out.write_all(line.as_bytes())?;
            prefix.truncate(base);
            prefix.push_str(if last { "    " } else { "│   " });
            self.print_rows(*kid, prefix, wd, wa, out)?;
            prefix.truncate(base);
        }
        Ok(())
    }

    fn json_rows(&self, node: usize, rel: &mut String, first: &mut bool, out: &mut Vec<u8>) {
        let kids: Vec<usize> = self.visible(node).collect();
        let base = rel.len();
        for kid in kids {
            let n = &self.nodes[kid];
            if !rel.is_empty() {
                rel.push('/');
            }
            rel.push_str(self.text(n.name));
            if !*first {
                out.push(b',');
            }
            *first = false;
            write!(out, "{{\"depth\":").ok();
            write!(out, "{}", n.depth).ok();
            out.extend_from_slice(b",\"name\":");
            push_json_str(self.text(n.name), out);
            out.extend_from_slice(b",\"rel\":");
            push_json_str(rel, out);
            out.extend_from_slice(if n.is_dir() {
                b",\"is_dir\":true"
            } else {
                b",\"is_dir\":false"
            });
            out.extend_from_slice(if n.is_link() {
                b",\"is_link\":true"
            } else {
                b",\"is_link\":false"
            });
            if let Some(t) = n.target {
                out.extend_from_slice(b",\"target\":");
                push_json_str(self.text(t), out);
            }
            write!(
                out,
                ",\"on_disk\":{},\"apparent\":{}}}",
                n.total_disk, n.total_apparent
            )
            .ok();
            self.json_rows(kid, rel, first, out);
            rel.truncate(base);
        }
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
        tree.nodes.push(Node {
            name,
            target: None,
            total_disk: own_disk,
            total_apparent: own_apparent,
            kids: Vec::new(),
            parent: 0,
            depth: 0,
            flags: if root_dir { IS_DIR } else { 0 },
        });
        if root_dir && o.level > 0 {
            tree.list(path, "", 0, 0);
        }
        tree.totals();
        tree.sort_kids();
        for m in &tree.messages {
            eprintln!("dusage tree: {m}");
        }
        if tree.errs.total() > 0 {
            self.failed = true;
        }
        if !root_dir {
            tree.files = 1;
        }
        if o.level == 0 && root_dir {
            return Ok(());
        }
        if o.json {
            json_render(&tree, out)
        } else {
            text_render(&tree, out)
        }
    }
}

fn text_render(tree: &Tree<'_>, out: &mut dyn Write) -> io::Result<()> {
    let o = tree.o;
    let root = &tree.nodes[0];
    let mut prefix = String::new();
    let disk = size_text(root.total_disk, o.human);
    let apparent = size_text(root.total_apparent, o.human);
    let line = format!("[ {disk} {apparent}]  {}", tree.text(root.name));
    writeln!(out, "{line}")?;
    let (mut wd, mut wa) = (1, 1);
    tree.widths(0, &mut wd, &mut wa);
    tree.print_rows(0, &mut prefix, wd, wa, out)?;
    let counts = if o.dirs_only {
        format!("{} directories", tree.dirs)
    } else {
        format!("{} directories, {} files", tree.dirs, tree.files)
    };
    let footer = format!(
        "\n{counts} ({} on disk, {} apparent{})\n",
        size_text(root.total_disk, o.human),
        size_text(root.total_apparent, o.human),
        if tree.skipped_mounts > 0 {
            format!(", {} mount points not entered", tree.skipped_mounts)
        } else {
            String::new()
        }
    );
    out.write_all(footer.as_bytes())
}

fn json_render(tree: &Tree<'_>, out_stream: &mut dyn Write) -> io::Result<()> {
    let root = &tree.nodes[0];
    let name = tree.text(root.name).to_string();
    let mut out = Vec::new();
    out.extend_from_slice(b"{\"root\":");
    push_json_str(&name, &mut out);
    out.extend_from_slice(b",\"entries\":[");
    write!(
        out,
        "{{\"depth\":0,\"name\":{},\"rel\":\"\",\"is_dir\":{},\"is_link\":false,\
             \"on_disk\":{},\"apparent\":{}}}",
        quote(&name),
        root.is_dir(),
        root.total_disk,
        root.total_apparent
    )
    .ok();
    let mut first = false;
    let mut rel = String::new();
    tree.json_rows(0, &mut rel, &mut first, &mut out);
    write!(
        out,
        "],\"dirs\":{},\"files\":{},\"links\":{},\"on_disk\":{},\"apparent\":{},\
             \"skipped_mounts\":{}}}",
        tree.dirs,
        tree.files,
        tree.links,
        root.total_disk,
        root.total_apparent,
        tree.skipped_mounts
    )
    .ok();
    out.push(b'\n');
    out_stream.write_all(&out)
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
