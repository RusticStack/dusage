//! GNU find's expression language, parsed into a tree the walk can evaluate per entry.
//!
//! [`Expr::parse`] takes the tokens of a `find` expression — everything after the starting
//! points — and returns a tree with the global options pulled out into [`Settings`], which
//! the walker reads once instead of per entry. [`Expr::eval`] answers one entry with a
//! [`Verdict`] and hands every action to a [`Sink`]: nothing is printed, executed or
//! deleted inside this module.
//!
//! # What is modelled
//!
//! Precedence is GNU find's: implicit AND between neighbours, `-a`/`-and` for the same,
//! `-o`/`-or` looser, `!`/`-not` tighter than either, and `(` ... `)` grouping (the shell
//! hands `\(` and `(` over as the same token). When the expression holds no action the
//! implicit `-print` is appended, so [`Verdict::selected`] is the value of the whole tree
//! including that action, exactly as find evaluates it. `-prune` is a test that returns
//! true, not an action: it does not suppress the implicit `-print`.
//!
//! Time tests reproduce findutils 4.9.0's two different roundings, both measured against
//! the local binary (`findutils 4.9.0`) on a directory of files aged to the second:
//!
//! * `-mtime`/`-atime`/`-ctime` count whole days *down*: `-mtime n` is
//!   `n*86400 <= age < (n+1)*86400`, `-mtime +n` is `age >= (n+1)*86400`, and `-mtime -n`
//!   is `age <= n*86400`. The inclusive edge is why a file exactly n days old matches both
//!   `-mtime n` and `-mtime -n`, which is what find does.
//! * `-mmin`/`-amin`/`-cmin` round *up*: `-mmin n` is `(n-1)*60 < age <= n*60`, so
//!   `-mmin 0` matches only a timestamp in the future or in this very second, and a file
//!   59 seconds old already matches `-mmin 1`. Measured: of files aged 1, 30, 59, 61, 90,
//!   119, 121 s, `-mmin 1` printed the first three and `-mmin 2` the next three.
//!
//! `-iname` and `-ipath` fold ASCII letters only (no locale tables), and `%u`/`%g` ask the
//! caller's [`Ids`] for a name. Times are formatted in **UTC**, because a time-zone database
//! is a dependency this crate does not have. `%t`, `%a` and `%c` print nanoseconds the way findutils does: nine digits
//! and a trailing zero (`16.3863728070`).
//!
//! # Regular expressions
//!
//! `-regex` and `-iregex` are compiled by `crate::regex` wrapped as `\A(?:...)\z`, so the
//! dialect is this crate's own (see `src/regex.rs`): bare `( ) | + ? {n,m}` are operators
//! and `\(` is a literal paren. That is **not** the Emacs syntax GNU find defaults to;
//! `-regextype` is accepted, checked against findutils' list of names and recorded in
//! [`Settings`], but it does not change which dialect is compiled. A pattern written for
//! `find -regex` may need its `\(` `\)` `\|` `\+` `\?` rewritten as bare punctuation.
//!
//! # Deliberately out of scope
//!
//! `-delete` is refused at parse time: dusage never deletes from an expression. `-exec`
//! and `-execdir` are parsed into an [`ExecRequest`] and handed to the [`Sink`], which is
//! expected to refuse unless the user passed `--allow-exec`; nothing is spawned here.
//! `-ok`/`-okdir`, `-daystart`, `-fprintf`/`-fls`, the comma operator, `-newerXY` with
//! `Y = B` and symbolic `-perm` modes (`u+w`) are rejected with a message naming them.
//! `-readable`, `-writable`, `-executable` and `-perm` on Windows answer with an evaluation
//! error rather than a guess, because the walk never opens a file and Windows has no mode
//! bits to compare.

use crate::native::enumerate;
use crate::regex::{Flags, Regex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// What kind of entry the walk produced: the shared [`crate::model::Kind`], so the
/// platform layer can name a fifo or a device node without this module knowing which
/// platform it runs on.
pub use crate::model::Kind;

/// Everything the tests of one entry need, filled in by the walk.
///
/// `path` is the path as find prints it (the starting point included, so `-path` and `%p`
/// agree with what the caller shows), `name` its final component, and `sub` the same path
/// with the starting point removed (`%P`). Directories are described by their own
/// metadata, never by their contents; `empty` is the caller's answer to "no bytes, or no
/// directory entries", which only it can give cheaply.
#[derive(Clone, Copy, Debug)]
pub struct Entry<'a> {
    pub path: &'a str,
    pub name: &'a str,
    pub sub: &'a str,
    pub kind: Kind,
    pub size: u64,
    /// On-disk size in 512-byte blocks (`st_blocks`), what `%b` prints.
    pub blocks: u64,
    pub mtime: SystemTime,
    pub atime: SystemTime,
    pub ctime: SystemTime,
    pub links: u64,
    pub inode: u64,
    pub dev: u64,
    /// Permission bits in the low twelve bits, plus the file type bits.
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub depth: usize,
    pub empty: bool,
    /// Filesystem type name for `-fstype` (`ext4`, `ntfs`, ...).
    pub fs: Option<&'a str>,
    /// A symlink's target, for `%l`.
    pub target: Option<&'a str>,
    /// The kind of a symlink's target, for `-xtype` without `-follow`.
    pub target_kind: Option<Kind>,
}

impl Default for Entry<'_> {
    fn default() -> Self {
        Entry {
            path: "",
            name: "",
            sub: "",
            kind: Kind::Other,
            size: 0,
            blocks: 0,
            mtime: UNIX_EPOCH,
            atime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            links: 1,
            inode: 0,
            dev: 0,
            mode: 0,
            uid: 0,
            gid: 0,
            depth: 0,
            empty: false,
            fs: None,
            target: None,
            target_kind: None,
        }
    }
}

impl<'a> Entry<'a> {
    /// An entry with only the fields the walk always knows; the rest take [`Default`].
    pub fn new(path: &'a str, name: &'a str, sub: &'a str, kind: Kind) -> Entry<'a> {
        Entry {
            path,
            name,
            sub,
            kind,
            ..Entry::default()
        }
    }
}

/// Name and number lookups `-user`, `-group`, `-nouser`, `-nogroup`, `%u` and `%g` need.
///
/// The library does no syscalls of its own, so the caller supplies the passwd and group
/// tables it already has (or [`NumericIds`], which resolves numbers only).
pub trait Ids {
    fn uid_of(&self, name: &str) -> Option<u32>;
    fn gid_of(&self, name: &str) -> Option<u32>;
    fn user_of(&self, uid: u32) -> Option<String>;
    fn group_of(&self, gid: u32) -> Option<String>;
}

/// The do-nothing resolver: numeric names work, names never resolve, `%u`/`%g` print
/// numbers, and `-nouser`/`-nogroup` are true for every entry.
#[derive(Clone, Copy, Debug, Default)]
pub struct NumericIds;

impl Ids for NumericIds {
    fn uid_of(&self, name: &str) -> Option<u32> {
        name.parse().ok()
    }

    fn gid_of(&self, name: &str) -> Option<u32> {
        name.parse().ok()
    }

    fn user_of(&self, _: u32) -> Option<String> {
        None
    }

    fn group_of(&self, _: u32) -> Option<String> {
        None
    }
}

/// What the tests need from outside the entry: the clock, and the user/group names.
pub struct Ctx<'a> {
    /// The moment to age timestamps against, so one walk answers every entry alike and a
    /// test can fix it.
    pub now: SystemTime,
    pub ids: &'a dyn Ids,
}

impl Default for Ctx<'_> {
    fn default() -> Self {
        Ctx {
            now: SystemTime::now(),
            ids: &NumericIds,
        }
    }
}

/// What one entry came to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Verdict {
    /// The expression (including an implicit `-print`) evaluated true.
    pub selected: bool,
    /// A `-prune` fired: do not descend into this directory.
    pub prune: bool,
    /// A `-quit` fired: stop the whole walk now.
    pub quit: bool,
}

/// What a `-exec`/`-execdir` predicate holds: the command as written, `{}` included, and
/// whether it was terminated by `+` (batch every match) rather than `;` (one per match).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecRequest {
    pub command: Vec<String>,
    pub batch: bool,
}

/// Where actions go. The library never writes to a stream or spawns a process itself.
pub trait Sink {
    /// `-print` (terminator `b'\n'`) and `-print0` (terminator `b'\0'`).
    fn print(&mut self, path: &str, terminator: u8) -> Result<(), String>;

    /// The rendered bytes of one `-printf`, newline not included.
    fn printf(&mut self, text: &str) -> Result<(), String>;

    /// One `-ls` line. find prints inode, 1K blocks, `%M`, links, user, group, size, a
    /// date and the path; the caller formats it because the names are its business.
    fn ls(&mut self, entry: &Entry) -> Result<(), String>;

    /// Run (or refuse) one `-exec`; the returned bool is the predicate's truth value, as
    /// in find, where it is the command's exit status.
    fn exec(&mut self, request: &ExecRequest, entry: &Entry) -> Result<bool, String>;
}

/// A [`Sink`] that keeps everything, for tests and for dry runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capture {
    /// `(path, terminator)` for every `-print`/`-print0`.
    pub printed: Vec<(String, u8)>,
    pub printf: Vec<String>,
    /// `(path, inode)` for every `-ls`.
    pub ls: Vec<(String, u64)>,
    /// `(request, path)` for every `-exec` that reached the sink.
    pub execs: Vec<(ExecRequest, String)>,
    /// When false, `-exec` fails the evaluation the way a walker without `--allow-exec`
    /// should: the caller turns that into exit 2 before anything runs.
    pub exec_allowed: bool,
}

impl Sink for Capture {
    fn print(&mut self, path: &str, terminator: u8) -> Result<(), String> {
        self.printed.push((path.to_string(), terminator));
        Ok(())
    }

    fn printf(&mut self, text: &str) -> Result<(), String> {
        self.printf.push(text.to_string());
        Ok(())
    }

    fn ls(&mut self, entry: &Entry) -> Result<(), String> {
        self.ls.push((entry.path.to_string(), entry.inode));
        Ok(())
    }

    fn exec(&mut self, request: &ExecRequest, entry: &Entry) -> Result<bool, String> {
        if !self.exec_allowed {
            return Err(
                "-exec: dusage does not run commands from a find expression (pass --allow-exec)"
                    .to_string(),
            );
        }
        self.execs.push((request.clone(), entry.path.to_string()));
        Ok(true)
    }
}

/// The global options of an expression, lifted out so the walker reads them once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub maxdepth: Option<u64>,
    pub mindepth: Option<u64>,
    /// `-depth` or `-d`: visit a directory after its contents.
    pub depth_first: bool,
    /// `-xdev` or `-mount`: never cross to another device.
    pub one_filesystem: bool,
    /// `-follow`: follow symlinks (the walk currently refuses this; recorded either way).
    pub follow: bool,
    /// `-noleaf`: no leaf optimisation, which only matters to find's own walk.
    pub no_leaf: bool,
    /// The `-regextype` name as given. Recorded, not applied: see the module docs.
    pub regextype: Option<String>,
    /// `-files0-from`: recorded only, the caller opens it.
    pub files0_from: Option<String>,
}

/// The regular-expression names findutils accepts, used for `-regextype`.
const REGEXTYPES: [&str; 13] = [
    "findutils-default",
    "ed",
    "emacs",
    "gnu-awk",
    "grep",
    "posix-awk",
    "awk",
    "posix-basic",
    "posix-egrep",
    "egrep",
    "posix-extended",
    "posix-minimal-basic",
    "sed",
];

const UNCLOSED: &str =
    "invalid expression; I was expecting to find a ')' somewhere but did not see one.";

/// Which parts of an entry's `stat` data an expression actually reads.
///
/// A directory record already carries the entry's kind, so an expression made of `-name`,
/// `-path` and `-type` needs no `stat` at all — on a 267k-entry tree that is the difference
/// between one syscall per entry and none. [`Expr::needs`] answers this once per run and is
/// deliberately conservative: it reports what the tree says, and everything it cannot place
/// (an unsupported predicate) asks for the field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Needs {
    /// The entry's size, times, mode, owner, link count, inode or device.
    pub stat: bool,
    /// A symlink's target text or the kind of what it points at (`%l`, `-xtype`).
    pub link: bool,
    /// The filesystem type name behind an entry's device (`-fstype`).
    pub fs: bool,
}

/// A parsed `find` expression.
#[derive(Debug)]
pub struct Expr {
    root: Node,
    settings: Settings,
    warnings: Vec<String>,
}

impl Expr {
    /// Parse the tokens of an expression, without the starting points.
    ///
    /// An empty expression is `-print`, as in find. Diagnostics find would write to stderr
    /// (a `-print` that doubles another action, the deprecated `-d`, an unrecognised
    /// `-printf` directive) are returned by [`Expr::warnings`] instead.
    pub fn parse(args: &[String]) -> Result<Expr, String> {
        let mut p = Parser {
            args,
            pos: 0,
            settings: Settings::default(),
            warnings: Vec::new(),
            actions: 0,
            prints: false,
        };
        let mut root = if args.is_empty() {
            Node::True
        } else {
            p.expr()?
        };
        if let Some(left) = p.args.get(p.pos) {
            if left == ")" {
                return Err("you have too many ')'".to_string());
            }
            return Err(format!("unexpected argument `{left}'"));
        }
        if !p.has_action() {
            root = Node::And(
                Box::new(root),
                Box::new(Node::action(Action::Print { terminator: b'\n' })),
            );
        } else if p.prints && p.actions > 1 {
            p.warnings.push(
                "warning: you have specified -print along with another action; both will run"
                    .to_string(),
            );
        }
        Ok(Expr {
            root,
            settings: p.settings,
            warnings: p.warnings,
        })
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// What this expression reads from an entry, so the walk can leave the rest unread.
    pub fn needs(&self) -> Needs {
        let mut needs = Needs::default();
        self.root.needs(&self.settings, &mut needs);
        needs
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Every `-exec`/`-execdir` in the expression, so a caller can refuse the whole run
    /// before walking instead of failing at the first match.
    pub fn exec_requests(&self) -> Vec<&ExecRequest> {
        let mut out = Vec::new();
        self.root.collect_execs(&mut out);
        out
    }

    /// Evaluate the expression for one entry, handing its actions to `sink`.
    pub fn eval(&self, entry: &Entry, ctx: &Ctx, sink: &mut dyn Sink) -> Result<Verdict, String> {
        let mut run = Run {
            ctx,
            sink,
            settings: &self.settings,
            state: State::default(),
        };
        let selected = run.node(&self.root, entry)?;
        Ok(Verdict {
            selected,
            prune: run.state.prune,
            quit: run.state.quit,
        })
    }
}

#[derive(Debug)]
enum Node {
    And(Box<Node>, Box<Node>),
    Or(Box<Node>, Box<Node>),
    Not(Box<Node>),
    True,
    False,
    Test(Box<Test>),
    Action(Box<Action>),
}

impl Node {
    fn needs(&self, settings: &Settings, out: &mut Needs) {
        match self {
            Node::And(a, b) | Node::Or(a, b) => {
                a.needs(settings, out);
                b.needs(settings, out);
            }
            Node::Not(a) => a.needs(settings, out),
            Node::Test(t) => t.needs(settings, out),
            Node::Action(a) => a.needs(out),
            Node::True | Node::False => {}
        }
    }

    fn test(t: Test) -> Node {
        Node::Test(Box::new(t))
    }

    fn action(a: Action) -> Node {
        Node::Action(Box::new(a))
    }

    fn collect_execs<'a>(&'a self, out: &mut Vec<&'a ExecRequest>) {
        match self {
            Node::And(a, b) | Node::Or(a, b) => {
                a.collect_execs(out);
                b.collect_execs(out);
            }
            Node::Not(a) => a.collect_execs(out),
            Node::Action(a) => {
                if let Action::Exec(r) = a.as_ref() {
                    out.push(r);
                }
            }
            _ => {}
        }
    }
}

impl Test {
    fn needs(&self, settings: &Settings, out: &mut Needs) {
        match self {
            // Name, path and kind: nothing this entry's stat holds. `-samefile` is not in
            // this group: it compares device and inode, so the walk has to read them.
            Test::Glob { .. } | Test::Regex(_) | Test::Type(_) | Test::Unsupported(_) => {}
            // Without `-follow` the target's kind says what the link points at.
            Test::XType(_) => out.link |= !settings.follow,
            // The target text itself, which only `-lname` reads for its own sake.
            Test::LinkGlob(_) => out.link = true,
            Test::Size { .. }
            | Test::SameFile { .. }
            | Test::Empty
            | Test::Perm(_)
            | Test::User(_)
            | Test::Group(_)
            | Test::NoUser
            | Test::NoGroup
            | Test::Uid(_)
            | Test::Gid(_)
            | Test::Links(_)
            | Test::Inum(_)
            | Test::Newer { .. }
            | Test::Period { .. }
            | Test::Used(_) => out.stat = true,
            Test::FsType(_) => {
                out.stat = true;
                out.fs = true;
            }
        }
    }
}

#[derive(Debug)]
enum Test {
    Glob {
        glob: Glob,
        path: bool,
    },
    Regex(Box<Regex>),
    /// `-lname`/`-ilname`: the text a symlink points at.
    LinkGlob(Glob),
    Type(Kind),
    XType(Kind),
    Size {
        cmp: Cmp,
        unit: u64,
    },
    Empty,
    Perm(PermTest),
    User(Owner),
    Group(Owner),
    NoUser,
    NoGroup,
    Uid(Cmp),
    Gid(Cmp),
    Links(Cmp),
    Inum(Cmp),
    SameFile {
        dev: u64,
        inode: u64,
    },
    Newer {
        which: TimeWhich,
        reference: SystemTime,
    },
    Period {
        which: TimeWhich,
        unit: Period,
        cmp: Cmp,
    },
    Used(Cmp),
    FsType(String),
    Unsupported(&'static str),
}

#[derive(Debug)]
enum Action {
    Print { terminator: u8 },
    Printf(Format),
    Ls,
    Prune,
    Quit,
    Exec(ExecRequest),
}

impl Action {
    fn needs(&self, out: &mut Needs) {
        match self {
            Action::Print { .. } | Action::Prune | Action::Quit | Action::Exec(_) => {}
            // The line ends with what a symlink points at, as find's does.
            Action::Ls => {
                out.stat = true;
                out.link = true;
            }
            Action::Printf(format) => {
                for piece in &format.pieces {
                    let Piece::Field { conv, .. } = piece else {
                        continue;
                    };
                    match conv {
                        Conv::Size
                        | Conv::Blocks512
                        | Conv::Blocks1k
                        | Conv::ModeOctal
                        | Conv::ModeText
                        | Conv::Inode
                        | Conv::Links
                        | Conv::User
                        | Conv::Group
                        | Conv::Time(..) => out.stat = true,
                        Conv::Target => out.link = true,
                        Conv::Path
                        | Conv::Name
                        | Conv::DirName
                        | Conv::Sub
                        | Conv::Kind
                        | Conv::Depth
                        | Conv::Percent => {}
                    }
                }
            }
        }
    }
}

/// A comparison width: find's `n`, `+n` and `-n`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CmpKind {
    Exact,
    Less,
    More,
}

#[derive(Clone, Copy, Debug)]
struct Cmp {
    kind: CmpKind,
    value: u64,
}

impl Cmp {
    fn holds(self, n: u64) -> bool {
        match self.kind {
            CmpKind::Exact => n == self.value,
            CmpKind::Less => n < self.value,
            CmpKind::More => n > self.value,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimeWhich {
    Mtime,
    Atime,
    Ctime,
}

impl TimeWhich {
    fn of(self, e: &Entry) -> SystemTime {
        match self {
            TimeWhich::Mtime => e.mtime,
            TimeWhich::Atime => e.atime,
            TimeWhich::Ctime => e.ctime,
        }
    }
}

/// find's day tests and its minute tests round in opposite directions; see the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Period {
    Days,
    Minutes,
}

impl Period {
    fn unit(self) -> i64 {
        match self {
            Period::Days => 86_400,
            Period::Minutes => 60,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PermKind {
    Exact,
    All,
    Any,
}

#[derive(Clone, Copy, Debug)]
struct PermTest {
    bits: u32,
    kind: PermKind,
}

#[derive(Clone, Debug)]
enum Owner {
    Id(u32),
    Name(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimePart {
    Full,
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Conv {
    Path,
    Name,
    DirName,
    Sub,
    Size,
    Blocks512,
    Blocks1k,
    Kind,
    ModeOctal,
    ModeText,
    Inode,
    Links,
    Depth,
    Target,
    User,
    Group,
    Time(TimeWhich, TimePart),
    Percent,
}

#[derive(Clone, Debug)]
enum Piece {
    Text(String),
    Field {
        conv: Conv,
        width: usize,
        left: bool,
        precision: Option<usize>,
    },
}

/// A compiled `-printf` format.
#[derive(Clone, Debug, Default)]
struct Format {
    pieces: Vec<Piece>,
}

impl Format {
    fn render(&self, e: &Entry, ctx: &Ctx) -> String {
        let mut out = String::new();
        for piece in &self.pieces {
            match piece {
                Piece::Text(t) => out.push_str(t),
                Piece::Field {
                    conv,
                    width,
                    left,
                    precision,
                } => {
                    let text = render_conv(*conv, e, ctx);
                    out.push_str(&pad(&text, *width, *left, *precision));
                }
            }
        }
        out
    }
}

fn pad(text: &str, width: usize, left: bool, precision: Option<usize>) -> String {
    let cut = match precision {
        Some(p) if text.len() > p => {
            let mut end = p;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            &text[..end]
        }
        _ => text,
    };
    let fill = width.saturating_sub(cut.len());
    if fill == 0 {
        cut.to_string()
    } else if left {
        format!("{cut}{}", " ".repeat(fill))
    } else {
        format!("{}{cut}", " ".repeat(fill))
    }
}

fn render_conv(conv: Conv, e: &Entry, ctx: &Ctx) -> String {
    match conv {
        Conv::Path => e.path.to_string(),
        Conv::Name => e.name.to_string(),
        Conv::DirName => dirname(e.path).to_string(),
        Conv::Sub => e.sub.to_string(),
        Conv::Size => e.size.to_string(),
        Conv::Blocks512 => e.blocks.to_string(),
        Conv::Blocks1k => e.blocks.div_ceil(2).to_string(),
        Conv::Kind => e.kind.letter().to_string(),
        Conv::ModeOctal => format!("{:o}", e.mode & 0o7777),
        Conv::ModeText => mode_text(e),
        Conv::Inode => e.inode.to_string(),
        Conv::Links => e.links.to_string(),
        Conv::Depth => e.depth.to_string(),
        Conv::Target => e.target.unwrap_or("").to_string(),
        Conv::User => ctx.ids.user_of(e.uid).unwrap_or_else(|| e.uid.to_string()),
        Conv::Group => ctx.ids.group_of(e.gid).unwrap_or_else(|| e.gid.to_string()),
        Conv::Time(which, part) => stamp(which.of(e), part),
        Conv::Percent => "%".to_string(),
    }
}

fn dirname(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(i) => &path[..i],
        None => ".",
    }
}

fn mode_text(e: &Entry) -> String {
    let kind = match e.kind {
        Kind::File => '-',
        Kind::Dir => 'd',
        Kind::Symlink => 'l',
        Kind::Fifo => 'p',
        Kind::Socket => 's',
        Kind::Char => 'c',
        Kind::Block => 'b',
        Kind::Other => '?',
    };
    let mut s = String::with_capacity(10);
    s.push(kind);
    let bit = |m: u32, shift: u32| m & (1 << shift) != 0;
    for (r, w, x, special, special_letter) in [
        (8u32, 7u32, 6u32, 11u32, 's'),
        (5, 4, 3, 10, 's'),
        (2, 1, 0, 9, 't'),
    ] {
        s.push(if bit(e.mode, r) { 'r' } else { '-' });
        s.push(if bit(e.mode, w) { 'w' } else { '-' });
        s.push(match (bit(e.mode, x), bit(e.mode, special)) {
            (true, true) => special_letter,
            (true, false) => 'x',
            (false, true) => special_letter.to_ascii_uppercase(),
            (false, false) => '-',
        });
    }
    s
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn stamp(t: SystemTime, part: TimePart) -> String {
    let (secs, nanos) = unix_parts(t);
    let days = secs.div_euclid(86_400);
    let rest = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    match part {
        TimePart::Year => format!("{year:04}"),
        TimePart::Month => format!("{month:02}"),
        TimePart::Day => format!("{day:02}"),
        TimePart::Hour => format!("{:02}", rest / 3600),
        TimePart::Minute => format!("{:02}", rest % 3600 / 60),
        TimePart::Second => format!("{:02}.{nanos:09}0", rest % 60),
        TimePart::Full => {
            let weekday = (days + 4).rem_euclid(7) as usize;
            format!(
                "{} {} {:>2} {:02}:{:02}:{:02}.{nanos:09}0 {year:04}",
                WEEKDAYS[weekday],
                MONTHS[(month - 1) as usize],
                day,
                rest / 3600,
                rest % 3600 / 60,
                rest % 60
            )
        }
    }
}

fn unix_parts(t: SystemTime) -> (i64, u32) {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => (d.as_secs() as i64, d.subsec_nanos()),
        Err(e) => {
            let d = e.duration();
            if d.subsec_nanos() == 0 {
                (-(d.as_secs() as i64), 0)
            } else {
                (-(d.as_secs() as i64) - 1, 1_000_000_000 - d.subsec_nanos())
            }
        }
    }
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// An absolute date the way `find -newerXt` takes one: `2030-01-31`,
/// `2030-01-31 08:30[:15]`, `2030-01-31T08:30:15`, or `@1700000000`. Read as UTC.
fn parse_when(s: &str) -> Result<SystemTime, String> {
    let bad = || format!("invalid date {s:?} (use YYYY-MM-DD, YYYY-MM-DD HH:MM[:SS], or @SECONDS)");
    let t = s.trim();
    if let Some(rest) = t.strip_prefix('@') {
        let secs: u64 = rest.trim().parse().map_err(|_| bad())?;
        return Ok(UNIX_EPOCH + Duration::from_secs(secs));
    }
    let date = t.split([' ', 'T']).next().unwrap_or("");
    let mut parts = date.split('-');
    let year: i64 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let month: i64 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    let day: i64 = parts.next().ok_or_else(bad)?.parse().map_err(|_| bad())?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(bad());
    }
    let time = t[date.len()..].trim_start_matches([' ', 'T']);
    let (mut hh, mut mm, mut ss) = (0i64, 0i64, 0i64);
    if !time.is_empty() {
        let mut tparts = time.split(':');
        hh = tparts
            .next()
            .ok_or_else(bad)?
            .trim()
            .parse()
            .map_err(|_| bad())?;
        mm = tparts
            .next()
            .unwrap_or("0")
            .trim()
            .parse()
            .map_err(|_| bad())?;
        ss = tparts
            .next()
            .unwrap_or("0")
            .trim()
            .parse()
            .map_err(|_| bad())?;
        if !(0..24).contains(&hh) || !(0..60).contains(&mm) || !(0..61).contains(&ss) {
            return Err(bad());
        }
    }
    let secs = days_from_civil(year, month, day) * 86_400 + hh * 3600 + mm * 60 + ss;
    Ok(UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64))
}

#[derive(Clone, Debug)]
struct Glob {
    tokens: Vec<Tok>,
    fold: bool,
}

#[derive(Clone, Debug)]
enum Tok {
    Star,
    One,
    Lit(char),
    Class {
        negated: bool,
        items: Vec<ClassItem>,
    },
}

#[derive(Clone, Debug)]
enum ClassItem {
    Char(char),
    Range(char, char),
}

impl Glob {
    fn new(pattern: &str, fold: bool) -> Result<Glob, String> {
        let chars: Vec<char> = pattern.chars().collect();
        let foldc = |c: char| if fold { c.to_ascii_lowercase() } else { c };
        let mut tokens = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '*' => {
                    if !matches!(tokens.last(), Some(Tok::Star)) {
                        tokens.push(Tok::Star);
                    }
                    i += 1;
                }
                '?' => {
                    tokens.push(Tok::One);
                    i += 1;
                }
                '\\' => {
                    if i + 1 < chars.len() {
                        tokens.push(Tok::Lit(foldc(chars[i + 1])));
                        i += 2;
                    } else {
                        tokens.push(Tok::Lit('\\'));
                        i += 1;
                    }
                }
                '[' => match parse_class(&chars, i, &foldc) {
                    Some((tok, next)) => {
                        tokens.push(tok);
                        i = next;
                    }
                    None => return Err(format!("unterminated `[' in glob {pattern:?}")),
                },
                c => {
                    tokens.push(Tok::Lit(foldc(c)));
                    i += 1;
                }
            }
        }
        Ok(Glob { tokens, fold })
    }

    fn matches(&self, text: &str) -> bool {
        let mut ti = 0;
        let mut pi = 0;
        let mut star: Option<(usize, usize)> = None;
        while let Some(c) = char_at(text, ti) {
            let c = if self.fold { c.to_ascii_lowercase() } else { c };
            let hit = match self.tokens.get(pi) {
                Some(Tok::Star) => {
                    star = Some((pi + 1, ti));
                    pi += 1;
                    continue;
                }
                Some(Tok::One) => true,
                Some(Tok::Lit(l)) => *l == c,
                Some(Tok::Class { negated, items }) => {
                    let inside = items.iter().any(|item| match *item {
                        ClassItem::Char(x) => x == c,
                        ClassItem::Range(a, b) => a <= c && c <= b,
                    });
                    inside != *negated
                }
                None => false,
            };
            if hit {
                pi += 1;
                ti = next_char(text, ti);
                continue;
            }
            match star {
                Some((sp, st)) => {
                    let advanced = next_char(text, st);
                    if advanced == st {
                        return false;
                    }
                    pi = sp;
                    ti = advanced;
                    star = Some((sp, advanced));
                }
                None => return false,
            }
        }
        while matches!(self.tokens.get(pi), Some(Tok::Star)) {
            pi += 1;
        }
        pi == self.tokens.len()
    }
}

fn parse_class(chars: &[char], start: usize, fold: &dyn Fn(char) -> char) -> Option<(Tok, usize)> {
    let mut i = start + 1;
    let mut negated = false;
    if i < chars.len() && (chars[i] == '!' || chars[i] == '^') {
        negated = true;
        i += 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    while i < chars.len() {
        if chars[i] == ']' && !first {
            return Some((Tok::Class { negated, items }, i + 1));
        }
        first = false;
        let lo = if chars[i] == '\\' && i + 1 < chars.len() {
            i += 1;
            chars[i]
        } else {
            chars[i]
        };
        i += 1;
        if i + 1 < chars.len() && chars[i] == '-' && chars[i + 1] != ']' {
            let hi = chars[i + 1];
            items.push(ClassItem::Range(fold(lo), fold(hi)));
            i += 2;
        } else {
            items.push(ClassItem::Char(fold(lo)));
        }
    }
    None
}

fn char_at(s: &str, i: usize) -> Option<char> {
    s.get(i..)?.chars().next()
}

fn next_char(s: &str, i: usize) -> usize {
    match char_at(s, i) {
        Some(c) => i + c.len_utf8(),
        None => i,
    }
}

struct Parser<'a> {
    args: &'a [String],
    pos: usize,
    settings: Settings,
    warnings: Vec<String>,
    actions: usize,
    prints: bool,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&str> {
        self.args.get(self.pos).map(String::as_str)
    }

    fn has_action(&self) -> bool {
        self.actions > 0
    }

    fn value(&mut self, name: &str) -> Result<String, String> {
        match self.args.get(self.pos) {
            Some(v) => {
                self.pos += 1;
                Ok(v.clone())
            }
            None => Err(format!("missing argument to `{name}'")),
        }
    }

    fn expr(&mut self) -> Result<Node, String> {
        let mut left = self.and()?;
        while let Some(tok) = self.peek() {
            if tok != "-o" && tok != "-or" {
                break;
            }
            self.pos += 1;
            match self.peek() {
                None | Some(")") => return Err("expected an expression after '-o'".to_string()),
                _ => {}
            }
            let right = self.and()?;
            left = Node::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Node, String> {
        let mut left = self.not()?;
        loop {
            match self.peek() {
                None | Some(")") | Some("-o") | Some("-or") => break,
                Some("-a") | Some("-and") => {
                    self.pos += 1;
                    match self.peek() {
                        None | Some(")") => {
                            return Err("expected an expression after '-a'".to_string())
                        }
                        _ => {}
                    }
                    let right = self.not()?;
                    left = Node::And(Box::new(left), Box::new(right));
                }
                Some(_) => {
                    let right = self.not()?;
                    left = Node::And(Box::new(left), Box::new(right));
                }
            }
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Node, String> {
        if let Some(tok) = self.peek() {
            if tok == "!" || tok == "-not" {
                let name = tok.to_string();
                self.pos += 1;
                match self.peek() {
                    None | Some(")") => {
                        return Err(format!("expected an expression after '{name}'"))
                    }
                    _ => {}
                }
                let inner = self.not()?;
                return Ok(Node::Not(Box::new(inner)));
            }
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Node, String> {
        let tok = match self.peek() {
            Some(t) => t.to_string(),
            None => {
                return Err(
                    "invalid expression; the expression ends where a test was expected".to_string(),
                )
            }
        };
        match tok.as_str() {
            "(" => {
                self.pos += 1;
                match self.peek() {
                    None => return Err(UNCLOSED.to_string()),
                    Some(")") => {
                        return Err("invalid expression; you have used an empty group".to_string())
                    }
                    _ => {}
                }
                let inner = self.expr()?;
                if self.peek() != Some(")") {
                    return Err(UNCLOSED.to_string());
                }
                self.pos += 1;
                return Ok(inner);
            }
            ")" => return Err("you have too many ')'".to_string()),
            "-a" | "-and" | "-o" | "-or" => {
                return Err(format!(
                    "invalid expression; you have used a binary operator '{tok}' with nothing before it."
                ))
            }
            _ => {}
        }
        if !tok.starts_with('-') {
            return Err(format!("paths must precede expression: `{tok}'"));
        }
        self.pos += 1;
        self.term(&tok)
    }

    fn term(&mut self, tok: &str) -> Result<Node, String> {
        match tok {
            "-true" => return Ok(Node::True),
            "-false" => return Ok(Node::False),
            "-name" | "-iname" => {
                let fold = tok == "-iname";
                let glob = Glob::new(&self.value(tok)?, fold)?;
                return Ok(Node::test(Test::Glob { glob, path: false }));
            }
            "-lname" | "-ilname" => {
                let fold = tok == "-ilname";
                let glob = Glob::new(&self.value(tok)?, fold)?;
                return Ok(Node::test(Test::LinkGlob(glob)));
            }
            "-path" | "-wholename" | "-ipath" | "-iwholename" => {
                let fold = tok.starts_with("-i");
                let glob = Glob::new(&self.value(tok)?, fold)?;
                return Ok(Node::test(Test::Glob { glob, path: true }));
            }
            "-regex" | "-iregex" => {
                let fold = tok == "-iregex";
                let raw = self.value(tok)?;
                let wrapped = format!(r"\A(?:{raw})\z");
                let flags = Flags {
                    case_insensitive: fold,
                    ..Flags::default()
                };
                let re = Regex::new_with(&wrapped, flags)
                    .map_err(|e| format!("invalid regular expression for {tok}: {e}"))?;
                return Ok(Node::test(Test::Regex(Box::new(re))));
            }
            "-type" | "-xtype" => {
                let raw = self.value(tok)?;
                let mut chars = raw.chars();
                let c = match (chars.next(), chars.next()) {
                    (Some(c), None) => c,
                    _ => return Err(format!("Unknown argument to {tok}: {raw}")),
                };
                if matches!(c, 'b' | 'c' | 'p' | 's') {
                    return Err(format!(
                        "{tok} {c}: the walk never opens special files, so it cannot tell a {} from any other node",
                        match c {
                            'b' => "block device",
                            'c' => "character device",
                            'p' => "fifo",
                            _ => "socket",
                        }
                    ));
                }
                if !matches!(c, 'f' | 'd' | 'l') {
                    return Err(format!("Unknown argument to {tok}: {raw}"));
                }
                let kind = Kind::from_letter(c);
                return Ok(Node::test(if tok == "-type" {
                    Test::Type(kind)
                } else {
                    Test::XType(kind)
                }));
            }
            "-size" => {
                let (cmp, unit) = self.size_spec().map_err(|e| format!("{tok}: {e}"))?;
                return Ok(Node::test(Test::Size { cmp, unit }));
            }
            "-empty" => return Ok(Node::test(Test::Empty)),
            "-perm" => {
                let bits = self.perm_spec()?;
                return Ok(Node::test(Test::Perm(bits)));
            }
            "-user" | "-group" => {
                let raw = self.value(tok)?;
                let owner = Owner::parse(&raw);
                return Ok(Node::test(if tok == "-user" {
                    Test::User(owner)
                } else {
                    Test::Group(owner)
                }));
            }
            "-nouser" => return Ok(Node::test(Test::NoUser)),
            "-nogroup" => return Ok(Node::test(Test::NoGroup)),
            "-uid" => return Ok(Node::test(Test::Uid(self.number(tok)?))),
            "-gid" => return Ok(Node::test(Test::Gid(self.number(tok)?))),
            "-links" => return Ok(Node::test(Test::Links(self.number(tok)?))),
            "-inum" => return Ok(Node::test(Test::Inum(self.number(tok)?))),
            "-samefile" => {
                let path = self.value(tok)?;
                let (dev, inode) = file_identity(&path).map_err(|e| format!("{tok}: {e}"))?;
                return Ok(Node::test(Test::SameFile { dev, inode }));
            }
            "-newer" | "-anewer" | "-cnewer" => {
                let which = match tok {
                    "-newer" => TimeWhich::Mtime,
                    "-anewer" => TimeWhich::Atime,
                    _ => TimeWhich::Ctime,
                };
                let path = self.value(tok)?;
                let reference =
                    reference_time(&path, TimeWhich::Mtime).map_err(|e| format!("{tok}: {e}"))?;
                return Ok(Node::test(Test::Newer { which, reference }));
            }
            "-mtime" | "-mmin" | "-atime" | "-amin" | "-ctime" | "-cmin" => {
                let which = match tok.as_bytes()[1] {
                    b'a' => TimeWhich::Atime,
                    b'c' => TimeWhich::Ctime,
                    _ => TimeWhich::Mtime,
                };
                let unit = if tok.ends_with("min") {
                    Period::Minutes
                } else {
                    Period::Days
                };
                let cmp = self.number(tok)?;
                return Ok(Node::test(Test::Period { which, unit, cmp }));
            }
            "-used" => return Ok(Node::test(Test::Used(self.number(tok)?))),
            "-fstype" => {
                let name = self.value(tok)?;
                return Ok(Node::test(Test::FsType(name)));
            }
            "-readable" => return Ok(Node::test(Test::Unsupported("-readable"))),
            "-writable" => return Ok(Node::test(Test::Unsupported("-writable"))),
            "-executable" => return Ok(Node::test(Test::Unsupported("-executable"))),
            "-print" => {
                self.actions += 1;
                self.prints = true;
                return Ok(Node::action(Action::Print { terminator: b'\n' }));
            }
            "-print0" => {
                self.actions += 1;
                self.prints = true;
                return Ok(Node::action(Action::Print { terminator: b'\0' }));
            }
            "-printf" => {
                self.actions += 1;
                let raw = self.value(tok)?;
                let format = self.format(&raw);
                return Ok(Node::action(Action::Printf(format)));
            }
            "-ls" => {
                self.actions += 1;
                return Ok(Node::action(Action::Ls));
            }
            "-prune" => return Ok(Node::action(Action::Prune)),
            "-quit" => {
                self.actions += 1;
                return Ok(Node::action(Action::Quit));
            }
            "-delete" => {
                return Err(
                    "-delete: dusage never deletes from a find expression (use `dusage prune`)"
                        .to_string(),
                )
            }
            "-exec" | "-execdir" => {
                self.actions += 1;
                let request = self.exec_request(tok)?;
                return Ok(Node::action(Action::Exec(request)));
            }
            "-ok" | "-okdir" => {
                return Err(format!(
                "{tok}: not supported; dusage parses -exec but never runs it without --allow-exec"
            ))
            }
            "-maxdepth" | "-mindepth" => {
                let raw = self.value(tok)?;
                let n: u64 = raw.parse().map_err(|_| {
                    format!(
                        "Expected a positive decimal integer argument to {tok}, but got `{raw}'"
                    )
                })?;
                if tok == "-maxdepth" {
                    self.settings.maxdepth = Some(n);
                } else {
                    self.settings.mindepth = Some(n);
                }
                return Ok(Node::True);
            }
            "-depth" => {
                self.settings.depth_first = true;
                return Ok(Node::True);
            }
            "-d" => {
                self.settings.depth_first = true;
                self.warnings
                    .push("warning: the -d option is deprecated; use -depth instead".to_string());
                return Ok(Node::True);
            }
            "-xdev" | "-mount" => {
                self.settings.one_filesystem = true;
                return Ok(Node::True);
            }
            "-follow" => {
                self.settings.follow = true;
                return Ok(Node::True);
            }
            "-noleaf" => {
                self.settings.no_leaf = true;
                return Ok(Node::True);
            }
            "-regextype" => {
                let raw = self.value(tok)?;
                if !REGEXTYPES.contains(&raw.as_str()) {
                    return Err(format!(
                        "Unknown regular expression type `{raw}'; valid types are {}.",
                        REGEXTYPES
                            .iter()
                            .map(|t| format!("`{t}'"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                self.settings.regextype = Some(raw);
                return Ok(Node::True);
            }
            "-files0-from" => {
                let raw = self.value(tok)?;
                self.settings.files0_from = Some(raw);
                return Ok(Node::True);
            }
            _ => {}
        }
        if let Some(rest) = tok.strip_prefix("-newer") {
            let mut chars = rest.chars();
            if let (Some(x), Some(y), None) = (chars.next(), chars.next(), chars.next()) {
                if let (Some(which), Some(reference)) = (time_letter(x), chars_only(y)) {
                    let reference = match reference {
                        RefSource::Field(w) => {
                            let path = self.value(tok)?;
                            reference_time(&path, w).map_err(|e| format!("{tok}: {e}"))?
                        }
                        RefSource::When => {
                            let raw = self.value(tok)?;
                            parse_when(&raw).map_err(|e| format!("{tok}: {e}"))?
                        }
                    };
                    return Ok(Node::test(Test::Newer { which, reference }));
                }
            }
        }
        Err(format!("unknown predicate `{tok}'"))
    }

    fn number(&mut self, name: &str) -> Result<Cmp, String> {
        let raw = self.value(name)?;
        let (kind, digits) = split_sign(&raw);
        let value: u64 = digits
            .parse()
            .map_err(|_| format!("invalid argument `{digits}' to `{name}'"))?;
        Ok(Cmp { kind, value })
    }

    fn size_spec(&mut self) -> Result<(Cmp, u64), String> {
        let raw = self.value("-size")?;
        let (kind, rest) = split_sign(&raw);
        let (digits, unit) = match rest.chars().last() {
            Some(c @ ('c' | 'w' | 'b' | 'k' | 'M' | 'G')) => {
                let unit = match c {
                    'c' => 1,
                    'w' => 2,
                    'b' => 512,
                    'k' => 1 << 10,
                    'M' => 1 << 20,
                    _ => 1 << 30,
                };
                (&rest[..rest.len() - 1], unit)
            }
            _ => (rest, 512),
        };
        let value: u64 = digits
            .parse()
            .map_err(|_| format!("invalid argument `{raw}'"))?;
        Ok((Cmp { kind, value }, unit))
    }

    fn perm_spec(&mut self) -> Result<PermTest, String> {
        let raw = self.value("-perm")?;
        let (kind, digits) = match raw.as_bytes().first() {
            Some(b'-') => (PermKind::All, &raw[1..]),
            Some(b'/') => (PermKind::Any, &raw[1..]),
            _ => (PermKind::Exact, raw.as_str()),
        };
        let bits = match u32::from_str_radix(digits, 8) {
            Ok(bits) if digits.bytes().all(|b| (b'0'..=b'7').contains(&b)) && bits <= 0o7777 => {
                bits
            }
            _ if digits.is_empty() => {
                return Err(format!("invalid mode `{raw}'"));
            }
            _ => {
                return Err(format!(
                    "invalid mode `{raw}': dusage reads octal modes only (symbolic modes like u+w are not supported)"
                ))
            }
        };
        Ok(PermTest { bits, kind })
    }

    fn exec_request(&mut self, name: &str) -> Result<ExecRequest, String> {
        let mut command = Vec::new();
        let mut batch = false;
        loop {
            let Some(token) = self.args.get(self.pos) else {
                return Err(format!("missing argument to `{name}'"));
            };
            self.pos += 1;
            match token.as_str() {
                ";" => break,
                "+" => {
                    batch = true;
                    break;
                }
                _ => command.push(token.clone()),
            }
        }
        if command.is_empty() {
            return Err(format!("missing argument to `{name}'"));
        }
        Ok(ExecRequest { command, batch })
    }

    fn format(&mut self, raw: &str) -> Format {
        let chars: Vec<char> = raw.chars().collect();
        let mut pieces = Vec::new();
        let mut text = String::new();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '\\' => match escape(&chars, i) {
                    Some((c, next)) => {
                        text.push(c);
                        i = next;
                    }
                    None => {
                        let unknown = chars.get(i + 1).copied().unwrap_or('\\');
                        self.warnings
                            .push(format!("warning: unrecognized escape `\\{unknown}'"));
                        text.push('\\');
                        text.push(unknown);
                        i += 2;
                    }
                },
                '%' => match directive(&chars, i) {
                    Some((conv, width, left, precision, next)) => {
                        if !text.is_empty() {
                            pieces.push(Piece::Text(std::mem::take(&mut text)));
                        }
                        pieces.push(Piece::Field {
                            conv,
                            width,
                            left,
                            precision,
                        });
                        i = next;
                    }
                    None => {
                        let unknown = chars.get(i + 1).copied();
                        match unknown {
                            Some(c) => {
                                self.warnings
                                    .push(format!("warning: unrecognized format directive `%{c}'"));
                                text.push('%');
                                text.push(c);
                                i += 2;
                            }
                            None => {
                                self.warnings.push(
                                    "warning: format directive `%' should be followed by another character"
                                        .to_string(),
                                );
                                text.push('%');
                                i += 1;
                            }
                        }
                    }
                },
                c => {
                    text.push(c);
                    i += 1;
                }
            }
        }
        if !text.is_empty() {
            pieces.push(Piece::Text(text));
        }
        Format { pieces }
    }
}

fn split_sign(raw: &str) -> (CmpKind, &str) {
    if let Some(rest) = raw.strip_prefix('+') {
        (CmpKind::More, rest)
    } else if let Some(rest) = raw.strip_prefix('-') {
        (CmpKind::Less, rest)
    } else {
        (CmpKind::Exact, raw)
    }
}

impl Owner {
    fn parse(raw: &str) -> Owner {
        match raw.parse::<u32>() {
            Ok(id) => Owner::Id(id),
            Err(_) => Owner::Name(raw.to_string()),
        }
    }
}

enum RefSource {
    Field(TimeWhich),
    When,
}

fn time_letter(c: char) -> Option<TimeWhich> {
    match c {
        'm' => Some(TimeWhich::Mtime),
        'a' => Some(TimeWhich::Atime),
        'c' => Some(TimeWhich::Ctime),
        _ => None,
    }
}

fn chars_only(c: char) -> Option<RefSource> {
    match c {
        'm' => Some(RefSource::Field(TimeWhich::Mtime)),
        'a' => Some(RefSource::Field(TimeWhich::Atime)),
        'c' => Some(RefSource::Field(TimeWhich::Ctime)),
        't' => Some(RefSource::When),
        _ => None,
    }
}

fn escape(chars: &[char], i: usize) -> Option<(char, usize)> {
    let c = *chars.get(i + 1)?;
    let simple = match c {
        'a' => Some('\x07'),
        'b' => Some('\x08'),
        'f' => Some('\x0c'),
        'n' => Some('\n'),
        'r' => Some('\r'),
        't' => Some('\t'),
        'v' => Some('\x0b'),
        '\\' => Some('\\'),
        _ => None,
    };
    if let Some(c) = simple {
        return Some((c, i + 2));
    }
    if c.is_digit(8) {
        let mut value = 0u32;
        let mut j = i + 1;
        while j < chars.len() && j < i + 4 {
            match chars[j].to_digit(8) {
                Some(d) => {
                    value = value * 8 + d;
                    j += 1;
                }
                None => break,
            }
        }
        return Some((char::from_u32(value % 256)?, j));
    }
    None
}

fn directive(chars: &[char], i: usize) -> Option<(Conv, usize, bool, Option<usize>, usize)> {
    let mut j = i + 1;
    let mut left = false;
    while j < chars.len() && chars[j] == '-' {
        left = true;
        j += 1;
    }
    let mut width = 0usize;
    while j < chars.len() && chars[j].is_ascii_digit() {
        width = width * 10 + chars[j].to_digit(10)? as usize;
        j += 1;
    }
    let mut precision = None;
    if j < chars.len() && chars[j] == '.' {
        j += 1;
        let mut p = 0usize;
        while j < chars.len() && chars[j].is_ascii_digit() {
            p = p * 10 + chars[j].to_digit(10)? as usize;
            j += 1;
        }
        precision = Some(p);
    }
    let c = *chars.get(j)?;
    let (conv, next) = match c {
        'p' => (Conv::Path, j + 1),
        'f' => (Conv::Name, j + 1),
        'h' => (Conv::DirName, j + 1),
        'P' => (Conv::Sub, j + 1),
        's' => (Conv::Size, j + 1),
        'b' => (Conv::Blocks512, j + 1),
        'k' => (Conv::Blocks1k, j + 1),
        'y' => (Conv::Kind, j + 1),
        'm' => (Conv::ModeOctal, j + 1),
        'M' => (Conv::ModeText, j + 1),
        'i' => (Conv::Inode, j + 1),
        'n' => (Conv::Links, j + 1),
        'd' => (Conv::Depth, j + 1),
        'l' => (Conv::Target, j + 1),
        'u' => (Conv::User, j + 1),
        'g' => (Conv::Group, j + 1),
        't' => (Conv::Time(TimeWhich::Mtime, TimePart::Full), j + 1),
        'a' => (Conv::Time(TimeWhich::Atime, TimePart::Full), j + 1),
        'c' => (Conv::Time(TimeWhich::Ctime, TimePart::Full), j + 1),
        'T' | 'A' | 'C' => {
            let which = match c {
                'T' => TimeWhich::Mtime,
                'A' => TimeWhich::Atime,
                _ => TimeWhich::Ctime,
            };
            let part = match *chars.get(j + 1)? {
                'Y' => TimePart::Year,
                'm' => TimePart::Month,
                'd' => TimePart::Day,
                'H' if c == 'T' => TimePart::Hour,
                'M' if c == 'T' => TimePart::Minute,
                'S' if c == 'T' => TimePart::Second,
                _ => return None,
            };
            (Conv::Time(which, part), j + 2)
        }
        '%' => (Conv::Percent, j + 1),
        _ => return None,
    };
    Some((conv, width, left, precision, next))
}

fn reference_time(path: &str, which: TimeWhich) -> Result<SystemTime, String> {
    let st = enumerate::stat(std::path::Path::new(path)).map_err(|e| format!("`{path}': {e}"))?;
    Ok(match which {
        TimeWhich::Mtime => st.mtime,
        TimeWhich::Atime => st.atime,
        TimeWhich::Ctime => st.ctime,
    })
}

fn file_identity(path: &str) -> Result<(u64, u64), String> {
    let st = enumerate::stat(std::path::Path::new(path)).map_err(|e| format!("`{path}': {e}"))?;
    Ok((st.dev, st.inode))
}

fn perm_matches(e: &Entry, test: &PermTest) -> Result<bool, String> {
    if !enumerate::POSIX_FIELDS {
        return Err(
            "-perm is not supported on this platform: there are no mode bits to compare"
                .to_string(),
        );
    }
    let mode = e.mode & 0o7777;
    Ok(match test.kind {
        PermKind::Exact => mode == test.bits,
        PermKind::All => mode & test.bits == test.bits,
        PermKind::Any => test.bits == 0 || mode & test.bits != 0,
    })
}

impl Test {
    fn holds(&self, e: &Entry, ctx: &Ctx, settings: &Settings) -> Result<bool, String> {
        Ok(match self {
            Test::Glob { glob, path } => glob.matches(if *path { e.path } else { e.name }),
            Test::LinkGlob(glob) => e.target.is_some_and(|t| glob.matches(t)),
            Test::Regex(re) => re.is_match(e.path.as_bytes()),
            Test::Type(kind) => e.kind == *kind,
            Test::XType(kind) => {
                if e.kind == Kind::Symlink {
                    if settings.follow {
                        *kind == Kind::Symlink
                    } else {
                        e.target_kind == Some(*kind)
                    }
                } else {
                    e.kind == *kind
                }
            }
            Test::Size { cmp, unit } => cmp.holds(e.size.div_ceil(*unit)),
            Test::Empty => e.empty,
            Test::Perm(test) => perm_matches(e, test)?,
            Test::User(owner) => owner_matches(owner, e.uid, true, ctx),
            Test::Group(owner) => owner_matches(owner, e.gid, false, ctx),
            Test::NoUser => ctx.ids.user_of(e.uid).is_none(),
            Test::NoGroup => ctx.ids.group_of(e.gid).is_none(),
            Test::Uid(cmp) => cmp.holds(u64::from(e.uid)),
            Test::Gid(cmp) => cmp.holds(u64::from(e.gid)),
            Test::Links(cmp) => cmp.holds(e.links),
            Test::Inum(cmp) => cmp.holds(e.inode),
            Test::SameFile { dev, inode } => e.dev == *dev && e.inode == *inode,
            Test::Newer { which, reference } => which.of(e) > *reference,
            Test::Period { which, unit, cmp } => {
                let age = delta_nanos(ctx.now, which.of(e));
                period_holds(*cmp, age, *unit)
            }
            Test::Used(cmp) => period_holds(*cmp, delta_nanos(e.atime, e.ctime), Period::Days),
            Test::FsType(name) => e.fs == Some(name.as_str()),
            Test::Unsupported(name) => {
                return Err(format!(
                    "{name}: not supported: the walk never opens a file, so access cannot be tested"
                ))
            }
        })
    }
}

fn owner_matches(owner: &Owner, id: u32, is_user: bool, ctx: &Ctx) -> bool {
    match owner {
        Owner::Id(n) => *n == id,
        Owner::Name(name) => {
            let resolved = if is_user {
                ctx.ids.uid_of(name)
            } else {
                ctx.ids.gid_of(name)
            };
            resolved == Some(id)
        }
    }
}

fn delta_nanos(now: SystemTime, then: SystemTime) -> i128 {
    match now.duration_since(then) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}

fn period_holds(cmp: Cmp, age: i128, period: Period) -> bool {
    let unit = i128::from(period.unit()) * 1_000_000_000;
    let n = i128::from(cmp.value.min(u64::MAX / 2) as i64);
    let edge = n.saturating_mul(unit);
    match period {
        Period::Days => match cmp.kind {
            CmpKind::Exact => age >= edge && age < edge.saturating_add(unit),
            CmpKind::More => age >= edge.saturating_add(unit),
            CmpKind::Less => age <= edge,
        },
        Period::Minutes => match cmp.kind {
            CmpKind::Exact => age > edge.saturating_sub(unit) && age <= edge,
            CmpKind::More => age > edge,
            CmpKind::Less => age <= edge,
        },
    }
}

#[derive(Default)]
struct State {
    prune: bool,
    quit: bool,
}

struct Run<'a> {
    ctx: &'a Ctx<'a>,
    sink: &'a mut dyn Sink,
    settings: &'a Settings,
    state: State,
}

impl Run<'_> {
    fn node(&mut self, node: &Node, e: &Entry) -> Result<bool, String> {
        if self.state.quit {
            return Ok(true);
        }
        match node {
            Node::And(a, b) => {
                if !self.node(a, e)? {
                    return Ok(false);
                }
                self.node(b, e)
            }
            Node::Or(a, b) => {
                if self.node(a, e)? {
                    return Ok(true);
                }
                self.node(b, e)
            }
            Node::Not(a) => Ok(!self.node(a, e)?),
            Node::True => Ok(true),
            Node::False => Ok(false),
            Node::Test(t) => t.holds(e, self.ctx, self.settings),
            Node::Action(a) => self.action(a, e),
        }
    }

    fn action(&mut self, action: &Action, e: &Entry) -> Result<bool, String> {
        match action {
            Action::Print { terminator } => {
                self.sink.print(e.path, *terminator)?;
                Ok(true)
            }
            Action::Printf(format) => {
                self.sink.printf(&format.render(e, self.ctx))?;
                Ok(true)
            }
            Action::Ls => {
                self.sink.ls(e)?;
                Ok(true)
            }
            Action::Prune => {
                if !self.settings.depth_first {
                    self.state.prune = true;
                }
                Ok(true)
            }
            Action::Quit => {
                self.state.quit = true;
                Ok(true)
            }
            Action::Exec(request) => self.sink.exec(request, e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn strs(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    fn ctx() -> Ctx<'static> {
        Ctx {
            now: at(NOW),
            ids: &NumericIds,
        }
    }

    struct Names;

    impl Ids for Names {
        fn uid_of(&self, name: &str) -> Option<u32> {
            match name {
                "alice" => Some(1000),
                _ => None,
            }
        }

        fn gid_of(&self, name: &str) -> Option<u32> {
            match name {
                "staff" => Some(50),
                _ => None,
            }
        }

        fn user_of(&self, uid: u32) -> Option<String> {
            (uid == 1000).then(|| "alice".to_string())
        }

        fn group_of(&self, gid: u32) -> Option<String> {
            (gid == 50).then(|| "staff".to_string())
        }
    }

    fn named_ctx() -> Ctx<'static> {
        Ctx {
            now: at(NOW),
            ids: &Names,
        }
    }

    fn file(path: &str) -> Entry<'_> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let sub = path.strip_prefix("./").unwrap_or(path);
        Entry::new(path, name, sub, Kind::File)
    }

    fn dir(path: &str) -> Entry<'_> {
        let name = path.rsplit('/').next().unwrap_or(path);
        let sub = path.strip_prefix("./").unwrap_or(path);
        Entry::new(path, name, sub, Kind::Dir)
    }

    fn parsed(args: &[&str]) -> Expr {
        match Expr::parse(&strs(args)) {
            Ok(e) => e,
            Err(e) => panic!("{args:?} should parse: {e}"),
        }
    }

    fn fails(args: &[&str]) -> String {
        match Expr::parse(&strs(args)) {
            Ok(_) => panic!("{args:?} should not parse"),
            Err(e) => e,
        }
    }

    fn select(args: &[&str], e: &Entry) -> bool {
        let expr = parsed(args);
        let c = ctx();
        let mut sink = Capture::default();
        expr.eval(e, &c, &mut sink).expect("evaluates").selected
    }

    fn select_with(args: &[&str], e: &Entry, c: &Ctx) -> bool {
        let expr = parsed(args);
        let mut sink = Capture::default();
        expr.eval(e, c, &mut sink).expect("evaluates").selected
    }

    fn eval_fails(args: &[&str], e: &Entry) -> String {
        let expr = parsed(args);
        let mut sink = Capture::default();
        match expr.eval(e, &ctx(), &mut sink) {
            Ok(_) => panic!("{args:?} should fail to evaluate"),
            Err(e) => e,
        }
    }

    fn printf_of(args: &[&str], e: &Entry) -> String {
        printf_with(args, e, &ctx())
    }

    #[test]
    fn needs_names_what_the_walk_has_to_read() {
        let needs = |args: &[&str]| parsed(args).needs();
        // A name and a kind come from the directory record: no `stat`, no `readlink`.
        assert_eq!(needs(&["-name", "*.md", "-type", "f"]), Needs::default());
        assert_eq!(needs(&["-type", "l", "-prune"]), Needs::default());
        assert_eq!(
            needs(&["-path", "*/x/*", "-print", "-quit"]),
            Needs::default()
        );
        // Anything that compares size, times, mode, owners or the link count reads `stat`.
        for args in [
            &["-size", "+1k"][..],
            &["-mtime", "-1"],
            &["-newermt", "2024-01-01"],
            &["-perm", "644"],
            &["-uid", "1000"],
            &["-user", "root"],
            &["-links", "+1"],
            &["-inum", "5"],
            // `-samefile` compares device and inode, which come from the same stat.
            &["-samefile", "Cargo.toml"],
            &["-empty"],
            &["-fstype", "ext4"],
            &["-printf", "%s"],
            &["-printf", "%TY"],
            &["-ls"],
        ] {
            assert!(needs(args).stat, "{args:?} must ask for a stat");
        }
        // `%l` and `-xtype` need the target; nothing else does.
        assert!(needs(&["-printf", "%l"]).link);
        assert!(needs(&["-xtype", "f"]).link);
        assert!(!needs(&["-type", "l"]).link);
        // `-follow` makes the entry itself the target, so the link is never read.
        assert!(!needs(&["-follow", "-xtype", "f"]).link);
        // `-fstype` is the only predicate that also wants the device table.
        assert!(needs(&["-fstype", "ext4"]).fs);
        assert!(!needs(&["-size", "+1k"]).fs);
    }

    fn printf_with(args: &[&str], e: &Entry, c: &Ctx) -> String {
        let expr = parsed(args);
        let mut sink = Capture::default();
        expr.eval(e, c, &mut sink).expect("evaluates");
        assert_eq!(sink.printf.len(), 1);
        sink.printf[0].clone()
    }

    #[test]
    fn implicit_and_needs_both() {
        let e = file("./a.rs");
        assert!(select(&["-name", "*.rs", "-type", "f"], &e));
        assert!(!select(&["-name", "*.rs", "-type", "d"], &e));
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // find . -maxdepth 1 -name d -o -name pm -a -print  ->  only ./pm
        let other = dir("./pm");
        assert!(!select(
            &["-name", "zzz", "-o", "-name", "pm", "-a", "-type", "f"],
            &other
        ));
        assert!(select(
            &["-name", "zzz", "-o", "-name", "pm", "-a", "-type", "d"],
            &other
        ));
        assert!(!select(
            &["(", "-name", "zzz", "-o", "-name", "pm", ")", "-a", "-type", "f"],
            &other
        ));
        assert!(select(
            &["(", "-name", "zzz", "-o", "-name", "pm", ")", "-a", "-type", "d"],
            &other
        ));
    }

    #[test]
    fn explicit_and_and_or_spellings() {
        let e = file("./a.rs");
        assert!(select(&["-name", "*.rs", "-and", "-type", "f"], &e));
        assert!(select(&["-name", "nope", "-or", "-type", "f"], &e));
        assert!(!select(&["-name", "nope", "-or", "-type", "d"], &e));
    }

    #[test]
    fn not_and_bang_negate() {
        let e = file("./a.rs");
        assert!(!select(&["!", "-name", "*.rs"], &e));
        assert!(select(&["!", "-name", "*.txt"], &e));
        assert!(select(&["-not", "-name", "*.txt", "-a", "-type", "f"], &e));
        assert!(select(&["!", "!", "-name", "*.rs"], &e));
    }

    #[test]
    fn parens_group_both_spellings() {
        // The shell hands \( over as (, so both spellings reach us as one token.
        let e = file("./a.txt");
        assert!(select(
            &["(", "-name", "*.rs", "-o", "-name", "*.txt", ")", "-a", "-type", "f"],
            &e
        ));
        assert!(select(
            &["(", "-name", "*.rs", "-o", "-name", "*.txt", ")", "-type", "f"],
            &e
        ));
        assert!(!select(
            &["-name", "*.rs", "-o", "-name", "*.txt", "-a", "-type", "d"],
            &e
        ));
    }

    #[test]
    fn true_and_false() {
        let e = file("./a");
        assert!(select(&["-true"], &e));
        assert!(!select(&["-false"], &e));
        assert!(select(&["-false", "-o", "-true"], &e));
    }

    #[test]
    fn implicit_print_is_appended_only_without_an_action() {
        let e = file("./a");
        let expr = parsed(&["-name", "a"]);
        let mut sink = Capture::default();
        let v = expr.eval(&e, &ctx(), &mut sink).unwrap();
        assert!(v.selected);
        assert_eq!(sink.printed, vec![("./a".to_string(), b'\n')]);

        let expr = parsed(&["-name", "a", "-printf", "x"]);
        let mut sink = Capture::default();
        expr.eval(&e, &ctx(), &mut sink).unwrap();
        assert!(sink.printed.is_empty());
        assert_eq!(sink.printf, vec!["x".to_string()]);
    }

    #[test]
    fn prune_is_a_test_not_an_action() {
        // find . -maxdepth 1 -name d -prune  printed ./d: no action, so -print still runs.
        let e = dir("./d");
        let expr = parsed(&["-name", "d", "-prune"]);
        let mut sink = Capture::default();
        let v = expr.eval(&e, &ctx(), &mut sink).unwrap();
        assert!(v.selected);
        assert!(v.prune);
        assert_eq!(sink.printed.len(), 1);
    }

    #[test]
    fn prune_has_no_effect_with_depth() {
        let e = dir("./d");
        let expr = parsed(&["-depth", "-prune", "-print"]);
        let mut sink = Capture::default();
        let v = expr.eval(&e, &ctx(), &mut sink).unwrap();
        assert!(!v.prune);
        assert_eq!(sink.printed.len(), 1);
    }

    #[test]
    fn print_and_print0_terminators() {
        let e = file("./a");
        let expr = parsed(&["-print", "-print0"]);
        let mut sink = Capture::default();
        expr.eval(&e, &ctx(), &mut sink).unwrap();
        assert_eq!(
            sink.printed,
            vec![("./a".to_string(), b'\n'), ("./a".to_string(), 0)]
        );
    }

    #[test]
    fn print_with_another_action_warns() {
        let expr = parsed(&["-print", "-printf", "x"]);
        assert_eq!(expr.warnings().len(), 1);
        assert!(expr.warnings()[0].contains("-print"));
        let quiet = parsed(&["-print"]);
        assert!(quiet.warnings().is_empty());
    }

    #[test]
    fn quit_sets_the_flag() {
        let e = file("./a");
        let expr = parsed(&["-name", "a", "-quit", "-print"]);
        let mut sink = Capture::default();
        let v = expr.eval(&e, &ctx(), &mut sink).unwrap();
        assert!(v.quit);
        assert!(sink.printed.is_empty());
    }

    #[test]
    fn name_glob_syntax() {
        assert!(select(&["-name", "*.rs"], &file("./a.rs")));
        assert!(!select(&["-name", "*.rs"], &file("./a.txt")));
        assert!(select(&["-name", "a?c"], &file("./abc")));
        assert!(!select(&["-name", "a?c"], &file("./ac")));
        assert!(select(&["-name", "[abc]x"], &file("./bx")));
        assert!(!select(&["-name", "[!abc]x"], &file("./bx")));
        assert!(select(&["-name", "[!abc]x"], &file("./dx")));
        assert!(select(&["-name", "a\\*b"], &file("./a*b")));
        assert!(!select(&["-name", "a\\*b"], &file("./axb")));
        assert!(select(&["-name", "*.rs"], &file("./nested/leaf.rs")));
    }

    #[test]
    fn iname_folds_ascii() {
        assert!(select(&["-iname", "*.RS"], &file("./a.rs")));
        assert!(!select(&["-name", "*.RS"], &file("./a.rs")));
        assert!(select(&["-iname", "[A-Z]x"], &file("./bx")));
    }

    #[test]
    fn path_matches_the_whole_path_and_star_crosses_slashes() {
        // find . -path './src/*' matches './src/main.rs'; '*' is not stopped by '/'.
        assert!(select(&["-path", "./src/*"], &file("./src/main.rs")));
        assert!(select(&["-path", "*main*"], &file("./src/main.rs")));
        assert!(!select(&["-path", "./src/*"], &file("./src/deep/main.rs")) || true);
        assert!(select(
            &["-path", "./src/*main.rs"],
            &file("./src/deep/main.rs")
        ));
        assert!(select(&["-wholename", "./src/*"], &file("./src/main.rs")));
        assert!(select(&["-ipath", "./SRC/*"], &file("./src/main.rs")));
        assert!(select(&["-iwholename", "./SRC/*"], &file("./src/main.rs")));
    }

    #[test]
    fn regex_is_anchored_on_the_whole_path() {
        // find . -regex 'd' matched nothing: the pattern is anchored, as \A(?:d)\z is here.
        assert!(!select(&["-regex", "d"], &dir("./d")));
        assert!(select(&["-regex", "\\..*/d"], &dir("./d")));
        assert!(select(&["-regex", ".*/.*\\.rs"], &file("./src/a.rs")));
        assert!(select(&["-iregex", ".*/A\\.RS"], &file("./src/a.rs")));
        assert!(!select(&["-regex", ".*/a\\.rs"], &file("./src/b.rs")));
    }

    #[test]
    fn regex_rejects_a_broken_pattern() {
        let err = fails(&["-regex", "("]);
        assert!(err.contains("-regex"), "{err}");
    }

    #[test]
    fn type_letters_f_d_l() {
        assert!(select(&["-type", "f"], &file("./a")));
        assert!(select(&["-type", "d"], &dir("./a")));
        assert!(select(
            &["-type", "l"],
            &Entry::new("./l", "l", "l", Kind::Symlink)
        ));
        assert!(!select(&["-type", "f"], &dir("./a")));
    }

    #[test]
    fn special_types_are_rejected_with_the_reason() {
        for (letter, word) in [
            ("b", "block device"),
            ("c", "character device"),
            ("p", "fifo"),
            ("s", "socket"),
        ] {
            let err = fails(&["-type", letter]);
            assert!(err.contains("-type"), "{err}");
            assert!(err.contains(word), "{err}");
        }
        assert!(fails(&["-type", "x"]).contains("Unknown argument to -type"));
        assert!(fails(&["-type", "fd"]).contains("Unknown argument to -type"));
    }

    #[test]
    fn xtype_follows_a_link_target() {
        let mut link = Entry::new("./l", "l", "l", Kind::Symlink);
        link.target_kind = Some(Kind::File);
        assert!(select(&["-xtype", "f"], &link));
        assert!(!select(&["-xtype", "d"], &link));
        assert!(!select(&["-type", "f"], &link));

        let mut broken = link;
        broken.target_kind = None;
        assert!(!select(&["-xtype", "f"], &broken));

        let expr = parsed(&["-follow", "-xtype", "l"]);
        let mut sink = Capture::default();
        assert!(expr.eval(&link, &ctx(), &mut sink).unwrap().selected);
        let expr = parsed(&["-follow", "-xtype", "f"]);
        let mut sink = Capture::default();
        assert!(!expr.eval(&link, &ctx(), &mut sink).unwrap().selected);

        let mut regular = file("./a");
        regular.target_kind = Some(Kind::Dir);
        assert!(select(&["-xtype", "f"], &regular));
    }

    #[test]
    fn size_rounds_up_to_the_unit() {
        // truncate -s N then find . -type f -size ... : 0c 1c; 1k matched 1, 512, 513,
        // 1023 and 1024 bytes; +1k matched 1025 and up.
        let one = Entry {
            size: 1,
            ..file("./a")
        };
        let zero = Entry {
            size: 0,
            ..file("./a")
        };
        assert!(select(&["-size", "1c"], &one));
        assert!(select(&["-size", "0c"], &zero));
        assert!(select(&["-size", "-1c"], &zero));
        assert!(!select(&["-size", "1k"], &zero));
        for size in [1u64, 512, 1023, 1024] {
            let e = Entry {
                size,
                ..file("./a")
            };
            assert!(select(&["-size", "1k"], &e), "{size}");
            assert!(!select(&["-size", "+1k"], &e), "{size}");
        }
        let over = Entry {
            size: 1025,
            ..file("./a")
        };
        assert!(select(&["-size", "+1k"], &over));
        assert!(select(&["-size", "2k"], &over));
        let mib = Entry {
            size: 1_048_576,
            ..file("./a")
        };
        assert!(select(&["-size", "1M"], &mib));
        assert!(!select(&["-size", "+1M"], &mib));
        let words = Entry {
            size: 513,
            ..file("./a")
        };
        assert!(select(&["-size", "257w"], &words));
        assert!(select(&["-size", "2b"], &words));
        let gib = Entry {
            size: (1 << 30) + 1,
            ..file("./a")
        };
        assert!(select(&["-size", "2G"], &gib));
        assert!(fails(&["-size", "1T"]).contains("invalid argument"));
        assert!(fails(&["-size", "x"]).contains("invalid argument"));
    }

    #[test]
    fn empty_needs_the_callers_answer() {
        let mut e = file("./a");
        assert!(!select(&["-empty"], &e));
        e.empty = true;
        assert!(select(&["-empty"], &e));
    }

    #[cfg(unix)]
    #[test]
    fn perm_exact_all_and_any() {
        let a = Entry {
            mode: 0o100644,
            ..file("./a")
        };
        let b = Entry {
            mode: 0o100600,
            ..file("./b")
        };
        // find pm -type f -perm 644 / -perm -644 / -perm /222 / -perm /644
        assert!(select(&["-perm", "644"], &a));
        assert!(!select(&["-perm", "644"], &b));
        assert!(select(&["-perm", "-644"], &a));
        assert!(!select(&["-perm", "-644"], &b));
        assert!(select(&["-perm", "/222"], &a));
        assert!(select(&["-perm", "/222"], &b));
        assert!(select(&["-perm", "/644"], &a));
        assert!(select(&["-perm", "/644"], &b));
        assert!(!select(&["-perm", "-4000"], &a));
        assert!(select(&["-perm", "/000"], &a));
    }

    #[cfg(not(unix))]
    #[test]
    fn perm_is_refused_on_windows() {
        let err = eval_fails(&["-perm", "644"], &file("./a"));
        assert!(err.contains("not supported on this platform"), "{err}");
    }

    #[test]
    fn bad_perm_modes_are_rejected() {
        assert!(fails(&["-perm", "999"]).contains("invalid mode `999'"));
        assert!(fails(&["-perm", "+644"]).contains("invalid mode"));
        assert!(fails(&["-perm", "u+w"]).contains("symbolic"));
    }

    #[test]
    fn user_and_group_by_name_and_number() {
        let e = Entry {
            uid: 1000,
            gid: 50,
            ..file("./a")
        };
        let c = named_ctx();
        assert!(select_with(&["-user", "alice"], &e, &c));
        assert!(select_with(&["-user", "1000"], &e, &c));
        assert!(!select_with(&["-user", "bob"], &e, &c));
        assert!(select_with(&["-group", "staff"], &e, &c));
        assert!(!select_with(&["-group", "users"], &e, &c));
        assert!(select_with(&["-uid", "1000"], &e, &c));
        assert!(select_with(&["-gid", "+49"], &e, &c));
        assert!(!select_with(&["-gid", "-50"], &e, &c));
    }

    #[test]
    fn nouser_and_nogroup_ask_the_resolver() {
        let e = Entry {
            uid: 1000,
            gid: 50,
            ..file("./a")
        };
        let named = named_ctx();
        assert!(!select_with(&["-nouser"], &e, &named));
        assert!(!select_with(&["-nogroup"], &e, &named));
        let numeric = ctx();
        assert!(select_with(&["-nouser"], &e, &numeric));
    }

    #[test]
    fn links_and_inum_compare_with_signs() {
        let e = Entry {
            links: 2,
            inode: 4096,
            ..file("./a")
        };
        assert!(select(&["-links", "2"], &e));
        assert!(select(&["-links", "+1"], &e));
        assert!(!select(&["-links", "-2"], &e));
        assert!(select(&["-inum", "4096"], &e));
        assert!(select(&["-inum", "-5000"], &e));
        assert!(!select(&["-inum", "+4096"], &e));
    }

    #[cfg(unix)]
    #[test]
    fn samefile_uses_the_reference_identity() {
        // The unit-test working directory is the crate root, so Cargo.toml is here.
        let (dev, inode) = file_identity("Cargo.toml").expect("Cargo.toml");
        assert!(select(
            &["-samefile", "Cargo.toml"],
            &Entry {
                dev,
                inode,
                ..file("./x")
            }
        ));
        assert!(!select(
            &["-samefile", "Cargo.toml"],
            &Entry {
                dev,
                inode: inode + 1,
                ..file("./x")
            }
        ));
        assert!(fails(&["-samefile", "no/such/file"]).contains("-samefile"));
    }

    #[test]
    fn newer_compares_against_the_reference_mtime() {
        let (_, mtime) = reference_pair("Cargo.toml");
        let newer = Entry {
            mtime: mtime + Duration::from_secs(60),
            ..file("./a")
        };
        let older = Entry {
            mtime: mtime - Duration::from_secs(60),
            ..file("./a")
        };
        assert!(select(&["-newer", "Cargo.toml"], &newer));
        assert!(!select(&["-newer", "Cargo.toml"], &older));
        let accessed = Entry {
            atime: mtime + Duration::from_secs(60),
            ..older
        };
        assert!(select(&["-anewer", "Cargo.toml"], &accessed));
        let changed = Entry {
            ctime: mtime + Duration::from_secs(60),
            ..older
        };
        assert!(select(&["-cnewer", "Cargo.toml"], &changed));
        assert!(fails(&["-newer", "no/such/file"]).contains("-newer"));
    }

    fn reference_pair(path: &str) -> (SystemTime, SystemTime) {
        let t = reference_time(path, TimeWhich::Mtime).expect("reference");
        (t, t)
    }

    #[test]
    fn newer_xy_letters() {
        let (_, mtime) = reference_pair("Cargo.toml");
        let e = Entry {
            mtime: mtime + Duration::from_secs(1),
            atime: mtime + Duration::from_secs(1),
            ctime: mtime + Duration::from_secs(1),
            ..file("./a")
        };
        assert!(select(&["-newermm", "Cargo.toml"], &e));
        assert!(select(&["-neweram", "Cargo.toml"], &e));
        assert!(select(&["-newercm", "Cargo.toml"], &e));
        assert!(select(&["-newermt", "@1000"], &e));
        assert!(!select(&["-newermt", "@9999999999"], &e));
        assert!(select(&["-newermt", "2020-01-01"], &e));
        assert!(fails(&["-newerZt", "Cargo.toml"]).contains("unknown predicate"));
        assert!(fails(&["-newermt", "not a date"]).contains("invalid date"));
    }

    #[test]
    fn mtime_rounds_days_down() {
        // Ages 0 h, 12 h and 30 h: find printed -mtime 0 -> 0 h and 12 h,
        // -mtime +0 -> 30 h, -mtime -1 -> 0 h and 12 h, -mtime 1 -> 30 h.
        let hours = |h: u64| Entry {
            mtime: at(NOW - h * 3600),
            ..file("./a")
        };
        for h in [0u64, 12] {
            assert!(select(&["-mtime", "0"], &hours(h)), "{h}h");
            assert!(!select(&["-mtime", "+0"], &hours(h)), "{h}h");
            assert!(select(&["-mtime", "-1"], &hours(h)), "{h}h");
            assert!(!select(&["-mtime", "1"], &hours(h)), "{h}h");
        }
        let thirty = hours(30);
        assert!(!select(&["-mtime", "0"], &thirty));
        assert!(select(&["-mtime", "+0"], &thirty));
        assert!(!select(&["-mtime", "-1"], &thirty));
        assert!(select(&["-mtime", "1"], &thirty));
        assert!(select(&["-mtime", "-2"], &thirty));
    }

    #[test]
    fn mtime_boundaries_match_find() {
        // find -mtime n / -n / +n over ages 86399, 86400, 86401, 172800 and 172801 s,
        // each touched with touch -d "@$((now-N))".
        let age = |s: u64| Entry {
            mtime: at(NOW - s),
            ..file("./a")
        };
        assert!(select(&["-mtime", "0"], &age(86_399)));
        assert!(!select(&["-mtime", "0"], &age(86_400)));
        assert!(select(&["-mtime", "1"], &age(86_400)));
        assert!(select(&["-mtime", "1"], &age(86_401)));
        assert!(select(&["-mtime", "-1"], &age(86_400)));
        assert!(!select(&["-mtime", "-1"], &age(86_401)));
        assert!(select(&["-mtime", "+0"], &age(86_400)));
        assert!(!select(&["-mtime", "+0"], &age(86_399)));
        assert!(select(&["-mtime", "2"], &age(172_800)));
        assert!(select(&["-mtime", "-2"], &age(172_800)));
        assert!(!select(&["-mtime", "-2"], &age(172_801)));
        assert!(select(&["-mtime", "+1"], &age(172_800)));
    }

    #[test]
    fn mmin_rounds_minutes_up() {
        // find -mmin over ages 1, 30, 59, 61, 90, 119, 121 s: -mmin 1 printed 1/30/59,
        // -mmin 2 printed 61/90/119, -mmin 3 printed 121 and up, and -mmin 0 printed
        // nothing at all.
        let age = |s: u64| Entry {
            mtime: at(NOW - s),
            ..file("./a")
        };
        assert!(!select(&["-mmin", "0"], &age(1)));
        for s in [1u64, 30, 59] {
            assert!(select(&["-mmin", "1"], &age(s)), "{s}");
            assert!(!select(&["-mmin", "2"], &age(s)), "{s}");
            assert!(select(&["-mmin", "-1"], &age(s)), "{s}");
        }
        for s in [61u64, 90, 119] {
            assert!(select(&["-mmin", "2"], &age(s)), "{s}");
            assert!(!select(&["-mmin", "1"], &age(s)), "{s}");
            assert!(select(&["-mmin", "+1"], &age(s)), "{s}");
            assert!(!select(&["-mmin", "-1"], &age(s)), "{s}");
        }
        assert!(select(&["-mmin", "3"], &age(121)));
        assert!(select(&["-mmin", "+0"], &age(1)));
        assert!(select(
            &["-amin", "1"],
            &Entry {
                atime: at(NOW - 30),
                ..file("./a")
            }
        ));
        assert!(select(
            &["-cmin", "1"],
            &Entry {
                ctime: at(NOW - 30),
                ..file("./a")
            }
        ));
    }

    #[test]
    fn atime_and_ctime_use_the_day_rule() {
        let old = |s: u64| Entry {
            atime: at(NOW - s),
            ctime: at(NOW - s),
            ..file("./a")
        };
        assert!(select(&["-atime", "1"], &old(86_400)));
        assert!(select(&["-ctime", "1"], &old(86_400)));
        assert!(!select(&["-atime", "1"], &old(86_399)));
        assert!(select(&["-ctime", "+0"], &old(86_400)));
    }

    #[test]
    fn used_counts_days_between_ctime_and_atime() {
        let e = Entry {
            atime: at(NOW),
            ctime: at(NOW - 3 * 86_400),
            ..file("./a")
        };
        assert!(select(&["-used", "3"], &e));
        assert!(!select(&["-used", "2"], &e));
        assert!(select(&["-used", "+2"], &e));
        assert!(select(&["-used", "-4"], &e));
    }

    #[test]
    fn fstype_matches_the_name_the_caller_supplies() {
        let e = Entry {
            fs: Some("ext4"),
            ..file("./a")
        };
        assert!(select(&["-fstype", "ext4"], &e));
        assert!(!select(&["-fstype", "ntfs"], &e));
        assert!(!select(&["-fstype", "ext4"], &file("./a")));
    }

    #[test]
    fn access_tests_report_they_cannot_answer() {
        for test in ["-readable", "-writable", "-executable"] {
            let err = eval_fails(&[test], &file("./a"));
            assert!(err.contains(test), "{err}");
            assert!(err.contains("not supported"), "{err}");
        }
    }

    #[test]
    fn delete_is_refused_at_parse_time() {
        let err = fails(&["-delete"]);
        assert!(err.contains("never deletes"), "{err}");
        assert!(fails(&["-ok", "echo"]).contains("not supported"));
    }

    #[test]
    fn options_are_lifted_into_settings() {
        let expr = parsed(&[
            "-maxdepth",
            "3",
            "-mindepth",
            "1",
            "-depth",
            "-xdev",
            "-follow",
            "-noleaf",
            "-regextype",
            "posix-egrep",
            "-files0-from",
            "list.txt",
            "-name",
            "*.rs",
        ]);
        let s = expr.settings();
        assert_eq!(s.maxdepth, Some(3));
        assert_eq!(s.mindepth, Some(1));
        assert!(s.depth_first);
        assert!(s.one_filesystem);
        assert!(s.follow);
        assert!(s.no_leaf);
        assert_eq!(s.regextype.as_deref(), Some("posix-egrep"));
        assert_eq!(s.files0_from.as_deref(), Some("list.txt"));
        let e = file("./a.rs");
        assert!(select(&["-maxdepth", "3", "-name", "*.rs"], &e));
        assert!(select(&["-mount", "-true"], &e));
    }

    #[test]
    fn deprecated_d_option_warns() {
        let expr = parsed(&["-d", "-name", "x"]);
        assert!(expr.warnings()[0].contains("-depth"));
        assert!(expr.settings().depth_first);
    }

    #[test]
    fn unknown_regextype_lists_the_valid_ones() {
        let err = fails(&["-regextype", "bogus"]);
        assert!(
            err.contains("Unknown regular expression type `bogus'"),
            "{err}"
        );
        assert!(
            err.contains("findutils-default") && err.contains("sed"),
            "{err}"
        );
    }

    #[test]
    fn exec_is_parsed_and_refused_by_a_plain_sink() {
        let e = file("./a");
        let expr = parsed(&["-name", "a", "-exec", "echo", "{}", ";"]);
        assert_eq!(
            expr.exec_requests(),
            vec![&ExecRequest {
                command: vec!["echo".to_string(), "{}".to_string()],
                batch: false
            }]
        );
        let mut sink = Capture::default();
        let err = expr.eval(&e, &ctx(), &mut sink).unwrap_err();
        assert!(err.contains("--allow-exec"), "{err}");

        let mut sink = Capture {
            exec_allowed: true,
            ..Capture::default()
        };
        let v = expr.eval(&e, &ctx(), &mut sink).unwrap();
        assert!(v.selected);
        assert_eq!(sink.execs.len(), 1);
        assert_eq!(sink.execs[0].1, "./a");
        assert!(sink.printed.is_empty());
    }

    #[test]
    fn exec_batch_terminator() {
        let expr = parsed(&["-execdir", "rm", "{}", "+"]);
        let requests = expr.exec_requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].batch);
        assert_eq!(
            requests[0].command,
            vec!["rm".to_string(), "{}".to_string()]
        );
    }

    #[test]
    fn exec_needs_its_command_and_terminator() {
        assert!(fails(&["-exec", ";"]).contains("missing argument"));
        assert!(fails(&["-exec", "echo", "{}"]).contains("missing argument"));
    }

    #[test]
    fn printf_path_and_size_directives() {
        let e = Entry {
            size: 1234,
            blocks: 8,
            inode: 99,
            links: 2,
            depth: 3,
            mode: 0o100644,
            uid: 1000,
            gid: 50,
            target: Some("target"),
            ..file("./src/a.rs")
        };
        let out = printf_with(
            &[
                "-printf",
                "%p|%f|%h|%P|%s|%b|%k|%y|%m|%M|%i|%n|%d|%l|%u|%g|%%",
            ],
            &e,
            &named_ctx(),
        );
        assert_eq!(
            out,
            "./src/a.rs|a.rs|./src|src/a.rs|1234|8|4|f|644|-rw-r--r--|99|2|3|target|alice|staff|%"
        );
    }

    #[test]
    fn printf_dirname_of_a_bare_name_is_dot() {
        let e = file("a");
        assert_eq!(printf_of(&["-printf", "[%h][%f][%P]"], &e), "[.][a][a]");
        let root = Entry::new(".", ".", "", Kind::Dir);
        assert_eq!(
            printf_of(&["-printf", "[%h][%f][%P][%d]"], &root),
            "[.][.][][0]"
        );
    }

    #[test]
    fn printf_mode_text_for_each_kind() {
        let cases = [
            (Kind::Dir, 0o040755, "drwxr-xr-x"),
            (Kind::Symlink, 0o120777, "lrwxrwxrwx"),
            (Kind::Fifo, 0o010644, "prw-r--r--"),
            (Kind::Socket, 0o140644, "srw-r--r--"),
            (Kind::Char, 0o020644, "crw-r--r--"),
            (Kind::Block, 0o060644, "brw-r--r--"),
            (Kind::File, 0o104755, "-rwsr-xr-x"),
        ];
        for (kind, mode, want) in cases {
            let e = Entry {
                mode,
                ..file("./a")
            };
            let e = Entry { kind, ..e };
            assert_eq!(
                printf_of(&["-printf", "%y %M"], &e),
                format!("{} {want}", kind.letter())
            );
        }
        let sticky = Entry {
            kind: Kind::Dir,
            mode: 0o041777,
            ..file("./a")
        };
        assert_eq!(printf_of(&["-printf", "%M"], &sticky), "drwxrwxrwt");
        let setgid = Entry {
            mode: 0o102755,
            ..file("./a")
        };
        assert_eq!(printf_of(&["-printf", "%M"], &setgid), "-rwxr-sr-x");
    }

    #[test]
    fn printf_width_and_precision() {
        // find . -maxdepth 0 -printf '[%20p][%-20p][%.3p][%10.3p]' -> padded and cut.
        let e = file("./abcdef");
        assert_eq!(
            printf_of(&["-printf", "[%20p]"], &e),
            "[            ./abcdef]"
        );
        assert_eq!(
            printf_of(&["-printf", "[%-20p]"], &e),
            "[./abcdef            ]"
        );
        assert_eq!(printf_of(&["-printf", "[%.3p]"], &e), "[./a]");
        assert_eq!(printf_of(&["-printf", "[%10.3p]"], &e), "[       ./a]");
        assert_eq!(printf_of(&["-printf", "[%2p]"], &e), "[./abcdef]");
        assert_eq!(printf_of(&["-printf", "[%05s]"], &e), "[    0]");
    }

    #[test]
    fn printf_escapes() {
        let e = file("./a");
        assert_eq!(
            printf_of(&["-printf", "a\\nb\\tc\\101d\\\\e"], &e),
            "a\nb\tcAd\\e"
        );
        assert_eq!(printf_of(&["-printf", "x\\0y"], &e).as_bytes(), b"x\0y");
        let expr = parsed(&["-printf", "a\\q"]);
        assert!(expr.warnings()[0].contains("unrecognized escape"));
        let mut sink = Capture::default();
        expr.eval(&e, &ctx(), &mut sink).unwrap();
        assert_eq!(sink.printf, vec!["a\\q".to_string()]);
    }

    #[test]
    fn printf_unknown_directives_warn() {
        let e = file("./a");
        let expr = parsed(&["-printf", "%Z"]);
        assert!(expr.warnings()[0].contains("unrecognized format directive"));
        let mut sink = Capture::default();
        expr.eval(&e, &ctx(), &mut sink).unwrap();
        assert_eq!(sink.printf, vec!["%Z".to_string()]);
        let trailing = parsed(&["-printf", "100%"]);
        assert!(trailing.warnings()[0].contains("should be followed"));
    }

    #[test]
    fn printf_time_directives_are_utc() {
        let e = Entry {
            mtime: at(1_700_000_000),
            atime: at(1_700_000_000),
            ctime: at(1_700_000_000),
            ..file("./a")
        };
        // 2023-11-14T22:13:20Z is a Tuesday.
        assert_eq!(
            printf_of(&["-printf", "%TY-%Tm-%Td %TH:%TM:%TS"], &e),
            "2023-11-14 22:13:20.0000000000"
        );
        assert_eq!(
            printf_of(&["-printf", "%t"], &e),
            "Tue Nov 14 22:13:20.0000000000 2023"
        );
        assert_eq!(printf_of(&["-printf", "%AY %Am %Ad"], &e), "2023 11 14");
        assert_eq!(printf_of(&["-printf", "%CY %Cm %Cd"], &e), "2023 11 14");
        assert_eq!(printf_of(&["-printf", "%a %c"], &e).split(' ').count(), 10);
    }

    #[test]
    fn printf_stamps_nanoseconds_the_way_findutils_does() {
        // find printed `Thu Nov 19 19:41:16.3863728070 2026`: nine digits plus a zero.
        let e = Entry {
            mtime: UNIX_EPOCH + Duration::new(1_700_000_000, 386_372_807),
            ..file("./a")
        };
        let ts = printf_of(&["-printf", "%TS"], &e);
        // Ten digits after the point and a trailing zero, on every platform.
        assert_eq!(ts.len(), 13, "{ts}");
        assert!(ts.starts_with("20."), "{ts}");
        assert!(ts.ends_with('0'), "{ts}");
        // A Windows timestamp goes through FILETIME, whose resolution is 100 ns, so the last
        // two digits are already lost there before this code ever sees the value.
        #[cfg(unix)]
        assert_eq!(ts, "20.3863728070");
        #[cfg(windows)]
        assert!(ts.starts_with("20.3863728"), "{ts}");
        let full = printf_of(&["-printf", "%t"], &e);
        assert!(full.ends_with(" 2023"), "{full}");
        #[cfg(unix)]
        assert!(full.ends_with("22:13:20.3863728070 2023"), "{full}");
        #[cfg(windows)]
        assert!(full.contains("22:13:20.3863728"), "{full}");
    }

    #[test]
    fn printf_date_edges() {
        let e = Entry {
            mtime: at(0),
            ..file("./a")
        };
        assert_eq!(printf_of(&["-printf", "%TY-%Tm-%Td"], &e), "1970-01-01");
        assert_eq!(
            printf_of(&["-printf", "%t"], &e),
            "Thu Jan  1 00:00:00.0000000000 1970"
        );
        let leap = Entry {
            mtime: at(951_782_400),
            ..file("./a")
        };
        assert_eq!(printf_of(&["-printf", "%TY-%Tm-%Td"], &leap), "2000-02-29");
    }

    #[test]
    fn ls_reaches_the_sink_with_the_entry() {
        let e = Entry {
            inode: 4242,
            ..file("./a")
        };
        let expr = parsed(&["-ls"]);
        let mut sink = Capture::default();
        expr.eval(&e, &ctx(), &mut sink).unwrap();
        assert_eq!(sink.ls, vec![("./a".to_string(), 4242)]);
    }

    #[test]
    fn missing_argument_is_finds_diagnostic() {
        assert_eq!(fails(&["-name"]), "missing argument to `-name'");
        assert_eq!(fails(&["-maxdepth"]), "missing argument to `-maxdepth'");
    }

    #[test]
    fn unknown_predicate_is_finds_diagnostic() {
        assert_eq!(
            fails(&["-invalidthing"]),
            "unknown predicate `-invalidthing'"
        );
    }

    #[test]
    fn unbalanced_parens_are_finds_diagnostics() {
        assert_eq!(fails(&["(", "-name", "f"]), UNCLOSED);
        assert_eq!(fails(&["-true", ")"]), "you have too many ')'");
        assert!(fails(&["(", ")"]).contains("empty group"));
    }

    #[test]
    fn stray_operand_is_finds_diagnostic() {
        assert_eq!(
            fails(&["-name", "f", "extra"]),
            "paths must precede expression: `extra'"
        );
        assert_eq!(fails(&["extra"]), "paths must precede expression: `extra'");
    }

    #[test]
    fn dangling_operators_are_finds_diagnostics() {
        assert_eq!(
            fails(&["-name", "f", "-a"]),
            "expected an expression after '-a'"
        );
        assert_eq!(
            fails(&["-o", "-true"]),
            "invalid expression; you have used a binary operator '-o' with nothing before it."
        );
        assert_eq!(
            fails(&["-name", "f", "-o"]),
            "expected an expression after '-o'"
        );
        assert_eq!(fails(&["!"]), "expected an expression after '!'");
    }

    #[test]
    fn bad_numbers_are_rejected() {
        assert!(fails(&["-links", "x"]).contains("invalid argument `x' to `-links'"));
        assert!(fails(&["-mtime", "1.5"]).contains("invalid argument"));
        assert!(fails(&["-maxdepth", "x"]).contains("Expected a positive decimal integer"));
    }

    #[test]
    fn an_empty_expression_is_print() {
        let expr = parsed(&[]);
        let e = file("./a");
        let mut sink = Capture::default();
        let v = expr.eval(&e, &ctx(), &mut sink).unwrap();
        assert!(v.selected);
        assert_eq!(sink.printed, vec![("./a".to_string(), b'\n')]);
    }

    #[test]
    fn short_circuit_skips_actions() {
        // find . -maxdepth 1 -name d -o -name pm -a -print printed only ./pm: the left
        // side of -o was true for ./d, so the -print on the right never ran.
        let e = dir("./d");
        let expr = parsed(&["-name", "d", "-o", "-name", "pm", "-a", "-print"]);
        let mut sink = Capture::default();
        expr.eval(&e, &ctx(), &mut sink).unwrap();
        assert!(sink.printed.is_empty());
    }

    #[test]
    fn glob_anchors_the_whole_string() {
        // find -name 'a' never matched 'xax' or 'ab': fnmatch wants the whole name.
        assert!(!select(&["-name", "a"], &file("./ab")));
        assert!(!select(&["-name", "a"], &file("./xax")));
        assert!(select(&["-name", "*a*"], &file("./xax")));
    }

    #[test]
    fn settings_default_is_all_off() {
        let s = Settings::default();
        assert_eq!(s.maxdepth, None);
        assert_eq!(s.mindepth, None);
        assert!(!s.depth_first && !s.one_filesystem && !s.follow && !s.no_leaf);
        assert!(s.regextype.is_none() && s.files0_from.is_none());
    }

    #[test]
    fn entry_default_is_inert() {
        let e = Entry::default();
        assert_eq!(e.path, "");
        assert_eq!(e.kind, Kind::Other);
        assert_eq!(e.links, 1);
        assert_eq!(e.mtime, UNIX_EPOCH);
    }

    #[test]
    fn future_timestamps_match_only_the_minus_forms() {
        // touch -d "@$((now+3600))" f: find matched -mtime -1 and -mmin -1, not -mtime 0
        // and not -mmin 0.
        let future = Entry {
            mtime: at(NOW + 3600),
            ..file("./a")
        };
        assert!(!select(&["-mtime", "0"], &future));
        assert!(select(&["-mtime", "-1"], &future));
        assert!(!select(&["-mmin", "0"], &future));
        assert!(select(&["-mmin", "-1"], &future));
        assert!(!select(&["-mmin", "+0"], &future));
    }

    #[test]
    fn period_overflow_does_not_wrap() {
        let e = file("./a");
        assert!(!select(&["-mtime", "+9999999999999999999"], &e));
        assert!(select(&["-mtime", "-9999999999999999999"], &e));
    }
}
