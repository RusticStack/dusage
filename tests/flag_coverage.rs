//! Help text versus parser: a flag one of them knows and the other does not is a bug, and
//! this is the test that notices.
//!
//! For every front end (`dusage --help`, `scan`, `du`, `search`, `find`, `prune`) the test
//! reads the flags the help text advertises, then runs each one against a throwaway fixture
//! and fails when the front end answers "unknown option" — or, in `find`, "unknown
//! predicate", which is how its argument splitter reports a token it does not know. No list of flags is written down
//! here: it comes out of the help text on every run, so the moment a help entry and a parser
//! table drift apart the test fails, whichever side was edited. Every existing test only
//! covers the flags its author remembered, which is why a help text can advertise a dozen
//! flags its own front end rejects and no test goes red.
//!
//! The other direction runs the same way: `cmd::accepted_flags` hands over each front end's
//! own `(value_flags, bool_flags)` tables, and every flag in them has to appear in that front
//! end's help. A flag the parser takes but no help text mentions is a flag nobody can
//! discover, and it fails here too.
//!
//! What the tables cannot see is the handful of flags a front end peels off *before* the
//! shared parser runs — `-u`/`--unrestricted` in both scopes, search's `--files`, find's
//! `-0`, and `--heading`'s last-one-wins — so those are checked in the help-to-parser
//! direction only. And a spelling that two front ends read differently cannot be checked at
//! all from a help text: `find -d` is find's own deprecated `-depth`, which the audit can
//! only observe as "accepted".

use dusage::cmd::{accepted_flags, FrontEnd as Tables};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_dusage");

/// One command line whose `--help` output is audited.
struct Front {
    /// The subcommand word, or `None` for the top-level form (`dusage <root> ...`).
    command: Option<&'static str>,
    /// A minimal valid invocation that the flag under test is appended to. `"."` is the
    /// fixture root, the working directory of every run.
    base: &'static [&'static str],
    /// Whose parser tables the help text is compared against.
    tables: Tables,
}

const FRONT_ENDS: &[Front] = &[
    Front {
        command: None,
        base: &["."],
        tables: Tables::Scan,
    },
    Front {
        command: Some("scan"),
        base: &["."],
        tables: Tables::Scan,
    },
    Front {
        command: Some("du"),
        base: &["."],
        tables: Tables::Du,
    },
    Front {
        command: Some("search"),
        base: &["x", "."],
        tables: Tables::Search,
    },
    Front {
        command: Some("find"),
        base: &["*.txt", "."],
        tables: Tables::Find,
    },
    Front {
        command: Some("prune"),
        base: &[".", "--age", "1s", "--dry"],
        tables: Tables::Prune,
    },
];

/// The flags whose help text does not spell the value out, so the test has to: an example
/// baked into the spelling (`--size +10M`), a mode word from a fixed vocabulary, a path that
/// has to exist, or a flag that must not be allowed to run.
enum Probe {
    /// A switch: appended on its own.
    Switch,
    /// Takes a value, appended as the next argument.
    Value(&'static str),
    /// Takes its value attached with `=` (`--color=WHEN`).
    Inline(&'static str),
    /// Takes a value, but would run forever with it (`--watch` rescans on an interval), so
    /// the run is cut short by `--help`, which every front end answers after parsing.
    Help(&'static str),
}

const NEEDS_VALUE: &[(&str, Probe)] = &[
    ("--color", Probe::Inline("never")),
    ("--encoding", Probe::Value("auto")),
    ("--ext", Probe::Value("txt")),
    ("--group-separator", Probe::Value("|")),
    ("--log", Probe::Value("plan.log")),
    ("--newer", Probe::Value("7d")),
    ("--older", Probe::Value("7d")),
    ("--pre", Probe::Value("dusage-no-such-command")),
    ("--preset", Probe::Value("cargo")),
    ("--prefix", Probe::Value("x")),
    ("--size", Probe::Value("1M")),
    ("--type-add", Probe::Value("rust2:*.rs2")),
    ("--watch", Probe::Help("1")),
];

/// Flags whose value must be swapped out because the base invocation already carries the
/// positional they replace: with `-e` or `-f` the pattern comes from the flag, and a
/// positional pattern would be read as a path to search.
const REPLACES_PATTERN: &[&str] = &["-e", "--regexp", "-f", "--file"];

fn help_of(fe: &Front) -> Option<String> {
    let mut c = Command::new(BIN);
    if let Some(cmd) = fe.command {
        c.arg(cmd);
    }
    let out = c
        .arg("--help")
        .stdin(Stdio::null())
        .current_dir(std::env::temp_dir())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    // A front end that is gone answers "unknown subcommand" with exit 2 and no USAGE block.
    (out.status.success() && text.contains("USAGE:")).then_some(text)
}

/// The body of the `OPTIONS:` section: an unindented header, then everything up to the next
/// unindented header (`BACKENDS:`, `SIZES:`, ...). Unindented prose inside the section — the
/// search help's "A path of `-` is standard input" — is still part of it.
fn option_lines(help: &str) -> Vec<&str> {
    let mut body = Vec::new();
    let mut inside = false;
    for line in help.lines() {
        if !line.starts_with(char::is_whitespace) {
            if inside {
                if line.trim_end().ends_with(':') {
                    break;
                }
            } else if line.trim_end() == "OPTIONS:" {
                inside = true;
                continue;
            }
        }
        if inside {
            body.push(line);
        }
    }
    body
}

/// A word of a help line read as a flag spelling: `--name` or a one-letter `-x`, with the
/// surrounding punctuation dropped (`-e,` is `-e`, `(-0)` is `-0`) and `--color[=WHEN]` read
/// as `--color` with an attached placeholder. Anything else — a placeholder, a prose word,
/// `utf-8`, `-10M`, the `-uu` in a repeatable flag's description — is not a flag.
fn flag_shape(word: &str) -> Option<(String, Option<String>)> {
    let s = word
        .trim_start_matches(['(', '`', '\''])
        .trim_end_matches([',', ';', '.', ')', '`', '\'']);
    if let Some(rest) = s.strip_prefix("--") {
        let (name, attached) = match rest.split_once('[') {
            Some((n, tail)) => (n, tail.trim_end_matches(']').split_once('=')),
            None => (rest, None),
        };
        let shaped = name.starts_with(|c: char| c.is_ascii_lowercase())
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        return shaped.then(|| {
            let attached = attached.map(|(_, v)| v.to_string());
            (format!("--{name}"), attached)
        });
    }
    let rest = s.strip_prefix('-')?;
    let mut chars = rest.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphanumeric() => Some((format!("-{c}"), None)),
        _ => None,
    }
}

/// A plausible value for the placeholder a help text shows after a value flag.
fn placeholder_value(word: &str) -> Option<&'static str> {
    let caps = word.starts_with(|c: char| c.is_ascii_uppercase())
        && word
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    caps.then_some(match word {
        "SIZE" => "1K",
        "N" => "1",
        "SECS" => "1",
        "NAME" => "rust",
        "PATH" | "FILE" => "patterns.txt",
        "COMMAND" => "dusage-no-such-command",
        "PATTERN" => "x",
        "KEY" => "path",
        "T" => "text",
        "A" => "recurse",
        "S" => "long-iso",
        "DATE" => "2000-01-01",
        "ENGINE" => "auto",
        "SEP" => "|",
        "TEXT" => "Y",
        "AGE" => "1s",
        "WHEN" => "never",
        _ => "x",
    })
}

/// One flag a help text advertises, and the value the audit will hand it.
struct Advertised {
    flag: String,
    probe: Probe,
}

/// Every flag the help text advertises, in the order it appears.
///
/// A definition line starts its head at the margin (`    -x, --one-filesystem`,
/// `        --vimgrep   ...`): the flags there, then at most one placeholder naming the value
/// they take. A continuation line is prose, but it can carry a second spelling
/// (`--group-separator is grep's spelling of it, --no-context-separator prints none`), so
/// every flag-shaped word in it counts too.
fn advertised(help: &str) -> Vec<Advertised> {
    let mut out: Vec<Advertised> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut push = |flag: String, probe: Probe, out: &mut Vec<Advertised>| {
        if seen.insert(flag.clone()) {
            out.push(Advertised { flag, probe });
        }
    };
    for line in option_lines(help) {
        let words: Vec<&str> = line.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        if flag_shape(words[0]).is_none() {
            for w in &words {
                if let Some((flag, attached)) = flag_shape(w) {
                    let probe = match attached {
                        Some(v) => Probe::Inline(placeholder_value(&v).unwrap_or("x")),
                        None => Probe::Switch,
                    };
                    push(flag, probe, &mut out);
                }
            }
            continue;
        }
        let mut head: Vec<String> = Vec::new();
        let mut attached = None;
        for w in &words {
            match flag_shape(w) {
                Some((flag, value)) => {
                    head.push(flag);
                    attached = attached.or(value);
                }
                None => break,
            }
        }
        let shown = words.get(head.len()).and_then(|w| placeholder_value(w));
        for flag in head {
            let probe = match attached.as_deref() {
                Some(v) => Probe::Inline(placeholder_value(v).unwrap_or("x")),
                None => match shown {
                    Some(v) => Probe::Value(v),
                    None => Probe::Switch,
                },
            };
            push(flag, probe, &mut out);
        }
    }
    // The table is authoritative over what the help text implies, so a flag whose example
    // value is baked into its spelling still gets a runnable one.
    for a in &mut out {
        if let Some((_, probe)) = NEEDS_VALUE.iter().find(|(f, _)| *f == a.flag) {
            a.probe = match probe {
                Probe::Switch => Probe::Switch,
                Probe::Value(v) => Probe::Value(v),
                Probe::Inline(v) => Probe::Inline(v),
                Probe::Help(v) => Probe::Help(v),
            };
        }
    }
    out
}

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Fixture {
        let base =
            std::env::temp_dir().join(format!("dusage_flagcov_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("root");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("alpha.txt"), "needle one\nneedle two\n").unwrap();
        fs::write(root.join("sub/beta.rs"), "fn beta() {}\n").unwrap();
        fs::write(root.join("patterns.txt"), "needle\n").unwrap();
        Fixture { root }
    }

    /// `dusage <argv>` in the fixture, with no stdin so `-f -` reads an empty pattern file.
    fn run(&self, argv: &[String]) -> Output {
        Command::new(BIN)
            .args(argv)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("cannot run {}: {e}", argv.join(" ")))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.root.parent().unwrap());
    }
}

/// The command line a flag is audited with.
fn case(fe: &Front, a: &Advertised) -> Vec<String> {
    let mut argv: Vec<String> = fe.command.map(str::to_string).into_iter().collect();
    if !(fe.command == Some("search") && REPLACES_PATTERN.contains(&a.flag.as_str())) {
        argv.extend(fe.base.iter().map(|s| s.to_string()));
    } else {
        // The pattern comes from -e/-f, so only the root stays behind it.
        argv.push(".".to_string());
    }
    match a.probe {
        Probe::Switch => argv.push(a.flag.clone()),
        Probe::Value(v) => {
            argv.push(a.flag.clone());
            argv.push(v.to_string());
        }
        Probe::Inline(v) => argv.push(format!("{}={v}", a.flag)),
        Probe::Help(v) => {
            argv.push(a.flag.clone());
            argv.push(v.to_string());
            argv.push("--help".to_string());
        }
    }
    argv
}

/// A spelling the front end refused. `--include` reaches the shared parser and comes back as
/// "unknown option"; the find front end splits its arguments at the first token it does not
/// know and hands it to the expression language, so there the same mistake comes back as
/// "unknown predicate". Both mean the flag was not accepted.
fn rejection(err: &str) -> Option<String> {
    if !err.contains("unknown option") && !err.contains("unknown predicate") {
        return None;
    }
    Some(err.lines().next().unwrap_or("").trim().to_string())
}

fn describe(fe: &Front) -> String {
    match fe.command {
        Some(c) => format!("dusage {c}"),
        None => "dusage".to_string(),
    }
}

fn wrap(flags: &[String]) -> String {
    let mut out = String::new();
    let mut col = 0;
    for f in flags {
        if col + f.len() + 1 > 88 && col > 0 {
            out.push_str("\n    ");
            col = 0;
        }
        out.push(' ');
        out.push_str(f);
        col += f.len() + 1;
    }
    out
}

#[test]
fn help_and_parser_agree_on_flags() {
    let fixture = Fixture::new("flags");
    let mut report = String::new();
    let mut failures: Vec<String> = Vec::new();
    let mut audited = 0;

    for fe in FRONT_ENDS {
        let Some(help) = help_of(fe) else {
            eprintln!(
                "flag coverage: skipping {}, its --help did not answer",
                describe(fe)
            );
            continue;
        };
        audited += 1;
        let flags = advertised(&help);
        assert!(
            flags.len() >= 10 && flags.iter().any(|a| a.flag == "--help"),
            "{}: read only {} flags from its help text, so the extraction is broken",
            describe(fe),
            flags.len()
        );
        let mut rejected = Vec::new();
        let mut refused_value = Vec::new();
        for a in &flags {
            let argv = case(fe, a);
            let out = fixture.run(&argv);
            let err = String::from_utf8_lossy(&out.stderr).into_owned();
            let code = out.status.code().unwrap_or(-1);
            let cmd = format!("dusage {}", argv.join(" "));
            if let Some(msg) = rejection(&err).filter(|_| code == 2) {
                rejected.push((cmd, code, msg));
            } else if code == 2 && err.contains("needs a value") {
                // The audit, not the parser, is wrong: this flag takes a value the tables
                // above do not know about.
                refused_value.push((cmd, a.flag.clone()));
            }
        }

        let names: Vec<String> = flags.iter().map(|a| a.flag.clone()).collect();
        let known: HashSet<&str> = names.iter().map(String::as_str).collect();
        let (values, bools) = accepted_flags(fe.tables);
        let undocumented: Vec<String> = values
            .iter()
            .chain(bools)
            .filter(|f| !known.contains(**f))
            .map(|f| f.to_string())
            .collect();
        report.push_str(&format!(
            "\n{} --help advertises {} flags:{}\n",
            describe(fe),
            names.len(),
            wrap(&names)
        ));
        if rejected.is_empty() && refused_value.is_empty() && undocumented.is_empty() {
            report.push_str(&format!(
                "  and the parser's own {} flags are the ones it takes, every one advertised\n",
                values.len() + bools.len()
            ));
            continue;
        }
        if !undocumented.is_empty() {
            report.push_str(&format!(
                "  but the parser also takes {} the help never mentions:{}\n",
                undocumented.len(),
                wrap(&undocumented)
            ));
            failures.push(format!(
                "{}: {} accepted flag(s) undocumented",
                describe(fe),
                undocumented.len()
            ));
        }
        if !refused_value.is_empty() {
            for (cmd, flag) in &refused_value {
                report.push_str(&format!(
                    "  the audit gives {flag} no value; add it to NEEDS_VALUE\n    {cmd}\n"
                ));
            }
        }
        if !rejected.is_empty() {
            report.push_str(&format!(
                "  but the {} front end rejects {} of them:\n",
                describe(fe).trim_start_matches("dusage "),
                rejected.len()
            ));
            for (cmd, code, msg) in &rejected {
                report.push_str(&format!("    {cmd}\n      exit {code}: {msg}\n"));
            }
            failures.push(format!(
                "{}: {} advertised flag(s) rejected",
                describe(fe),
                rejected.len()
            ));
        }
        if !refused_value.is_empty() {
            failures.push(format!(
                "{}: {} flag(s) missing a value in the audit table",
                describe(fe),
                refused_value.len()
            ));
        }
    }

    assert!(
        audited > 0,
        "no front end answered --help; nothing was audited"
    );
    if !failures.is_empty() {
        eprintln!("flag coverage: {}", failures.join("; "));
        eprintln!("{report}");
        panic!("help text and parser disagree: {}", failures.join("; "));
    }
    eprintln!("flag coverage: {report}");
}
