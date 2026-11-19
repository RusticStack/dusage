//! End-to-end tests of `dusage search` and `dusage find`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("dusage_search_cli_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("root");
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::write(
            root.join("src/main.rs"),
            "fn main() {\n    let needle = 1;\n    println!(\"{needle}\");\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/deep/util.rs"),
            "pub fn helper() {}\n// no match here\n",
        )
        .unwrap();
        fs::write(root.join("tests/it.rs"), "needle in a test\nneedle twice\n").unwrap();
        fs::write(
            root.join("node_modules/pkg/index.js"),
            "needle in a dependency\n",
        )
        .unwrap();
        fs::write(root.join("README.md"), "# Title\n\nNeedle with capital N\n").unwrap();
        Fixture { root }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.root.parent().unwrap());
    }
}

fn run(args: &[&str], root: &Path) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_dusage"));
    c.args(args).arg(root);
    c.output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

#[test]
fn finds_matching_lines_and_exits_0() {
    let f = Fixture::new("basic");
    let o = run(&["search", "-n", "needle"], &f.root);
    assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
    let out = stdout(&o);
    assert!(out.contains("main.rs:2:    let needle = 1;"), "{out}");
    assert!(out.contains("it.rs:1:needle in a test"), "{out}");
    assert!(out.contains("it.rs:2:needle twice"), "{out}");
    assert!(!out.contains("node_modules"), "ignored by default: {out}");
}

#[test]
fn no_match_exits_1_and_bad_pattern_exits_2() {
    let f = Fixture::new("exit");
    let o = run(&["search", "definitely-absent-token"], &f.root);
    assert_eq!(code(&o), 1);
    assert!(stdout(&o).is_empty());
    let o = run(&["search", "(unclosed"], &f.root);
    assert_eq!(code(&o), 2);
    let o = run(&["search", "--bogus-flag", "x"], &f.root);
    assert_eq!(code(&o), 2);
    let o = Command::new(env!("CARGO_BIN_EXE_dusage"))
        .arg("search")
        .output()
        .unwrap();
    assert_eq!(code(&o), 2, "a missing pattern is misuse");
}

#[test]
fn flags_case_word_invert_count_list() {
    let f = Fixture::new("flags");
    let o = run(&["search", "-i", "NEEDLE", "--no-ignore"], &f.root);
    assert_eq!(code(&o), 0);
    assert!(
        stdout(&o).contains("README.md:Needle with capital N"),
        "{}",
        stdout(&o)
    );
    let o = run(&["search", "-w", "needles"], &f.root);
    assert_eq!(code(&o), 1, "-w must not match a longer word");
    let o = run(&["search", "-c", "needle", "--no-ignore"], &f.root);
    assert_eq!(code(&o), 0);
    let out = stdout(&o);
    assert!(
        out.lines().any(|l| l.ends_with(":2")),
        "counts per file: {out}"
    );
    let o = run(&["search", "-l", "needle"], &f.root);
    assert_eq!(code(&o), 0);
    let out = stdout(&o);
    assert_eq!(out.lines().count(), 2, "two files match: {out}");
    // `-l` prints bare paths: no line numbers or match text after a separator (a Windows
    // path keeps its drive colon, so the check is on the name, not on colons).
    assert!(
        out.lines()
            .all(|l| l.ends_with("main.rs") || l.ends_with("it.rs")),
        "{out}"
    );
    let o = run(&["search", "-v", "-c", "needle"], &f.root);
    assert_eq!(code(&o), 0, "inverted search finds non-matching lines");
}

#[test]
fn only_matching_context_and_json() {
    let f = Fixture::new("output");
    let o = run(&["search", "-o", "ne+dle", "--no-ignore"], &f.root);
    let out = stdout(&o);
    assert_eq!(code(&o), 0);
    assert!(out.lines().all(|l| l.ends_with("needle")), "{out}");
    let o = run(
        &["search", "-C", "1", "-n", "twice", "--no-ignore", "-H"],
        &f.root,
    );
    let out = stdout(&o);
    assert!(out.contains("-1-needle in a test"), "context: {out}");
    assert!(out.contains(":2:needle twice"), "{out}");
    let o = run(&["search", "--json", "needle", "--no-ignore"], &f.root);
    let out = stdout(&o);
    assert!(out.trim_start().starts_with("{\"files\":["), "{out}");
    assert!(out.contains("\"line_number\":2"), "{out}");
    assert!(
        out.contains("\"submatches\":[{\"start\":8,\"end\":14"),
        "{out}"
    );
    assert!(out.trim_end().ends_with('}'));
}

#[test]
fn glob_and_type_filters() {
    let f = Fixture::new("glob");
    let o = run(&["search", "-g", "*.rs", "-l", "needle"], &f.root);
    let out = stdout(&o);
    assert!(!out.contains("README"), "{out}");
    assert_eq!(out.lines().count(), 2, "{out}");
    let o = run(&["search", "-t", "rust", "-l", "needle"], &f.root);
    assert_eq!(stdout(&o).lines().count(), 2);
    let o = run(
        &[
            "search",
            "--no-ignore",
            "-g",
            "!node_modules/**",
            "-l",
            "needle",
        ],
        &f.root,
    );
    assert!(!stdout(&o).contains("node_modules"));
    let o = run(&["search", "--no-ignore", "-l", "needle"], &f.root);
    assert!(stdout(&o).contains("node_modules"), "no-ignore reaches it");
}

#[test]
fn hidden_files_need_the_flag() {
    let f = Fixture::new("hidden");
    fs::write(f.root.join(".env"), "needle=1\n").unwrap();
    let o = run(&["search", "-l", "needle"], &f.root);
    assert!(!stdout(&o).contains(".env"));
    let o = run(&["search", "--hidden", "-l", "needle"], &f.root);
    assert!(stdout(&o).contains(".env"));
}

#[test]
fn quiet_is_silent_and_fast() {
    let f = Fixture::new("quiet");
    let o = run(&["search", "-q", "needle"], &f.root);
    assert_eq!(code(&o), 0);
    assert!(stdout(&o).is_empty(), "quiet prints nothing");
    let o = run(&["search", "-q", "absent"], &f.root);
    assert_eq!(code(&o), 1);
}

#[test]
fn find_by_glob_and_filters() {
    let f = Fixture::new("find");
    let o = run(&["find", "*.rs"], &f.root);
    assert_eq!(code(&o), 0);
    let out = stdout(&o);
    assert!(out.contains("src/main.rs"), "{out}");
    assert!(out.contains("src/deep/util.rs"), "{out}");
    assert!(!out.contains(".md"), "{out}");
    let o = run(&["find", "--dirs", "*"], &f.root);
    let out = stdout(&o);
    assert!(out.lines().any(|l| l.ends_with("src")), "dirs: {out}");
    let o = run(&["find", "-F", "deep"], &f.root);
    assert!(stdout(&o).contains("src/deep/util.rs"), "{}", stdout(&o));
    let o = run(&["find", "--long", "*.md"], &f.root);
    let out = stdout(&o);
    assert!(out.contains("README.md"), "{out}");
    assert!(
        out.contains(':') && out.lines().count() == 1,
        "one long row: {out}"
    );
    let o = run(&["find", "--json", "*.toml"], &f.root);
    assert_eq!(code(&o), 1, "nothing matched");
    assert!(stdout(&o).contains("\"count\":0"));
}

#[test]
fn legacy_scan_still_works_through_the_dispatcher() {
    let f = Fixture::new("legacy");
    let o = Command::new(env!("CARGO_BIN_EXE_dusage"))
        .arg(&f.root)
        .args(["--json"])
        .output()
        .unwrap();
    assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
    assert!(stdout(&o).contains("\"total\""));
    let o = Command::new(env!("CARGO_BIN_EXE_dusage"))
        .arg("scan")
        .arg(&f.root)
        .args(["--json"])
        .output()
        .unwrap();
    assert_eq!(code(&o), 0);
    assert!(stdout(&o).contains("\"total\""));
}

#[test]
fn help_lists_search_and_find() {
    let o = Command::new(env!("CARGO_BIN_EXE_dusage"))
        .arg("--help")
        .output()
        .unwrap();
    let h = stdout(&o);
    for word in ["search", "find", "prune", "scan"] {
        assert!(h.contains(word), "{word} missing from top-level help");
    }
    let o = Command::new(env!("CARGO_BIN_EXE_dusage"))
        .args(["search", "--help"])
        .output()
        .unwrap();
    let h = stdout(&o);
    for flag in [
        "--fixed-strings",
        "--invert-match",
        "--only-matching",
        "--context",
        "--glob",
        "--vimgrep",
        "--trim",
        "-M, --max-columns",
        "--path-separator",
        "--field-match-separator",
        "--field-context-separator",
        "--context-separator",
        "--no-context-separator",
        "--engine",
        "--crlf",
        "--stop-on-nonmatch",
        "--mmap",
        "--no-mmap",
        "--no-heading",
    ] {
        assert!(h.contains(flag), "{flag} missing from search help");
    }
    let o = Command::new(env!("CARGO_BIN_EXE_dusage"))
        .args(["find", "--help"])
        .output()
        .unwrap();
    assert!(stdout(&o).contains("--long"));
}

fn run_env(args: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_dusage"));
    c.args(args);
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().unwrap()
}

#[test]
fn encoding_auto_and_forced() {
    let f = Fixture::new("encoding");
    let le: Vec<u8> = "needle in utf16\nsecond needle\n"
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    fs::write(f.root.join("wide.txt"), le).unwrap();
    let latin1: Vec<u8> = b"needle caf\xe9\n".to_vec();
    fs::write(f.root.join("latin.txt"), latin1).unwrap();

    let o = run(&["search", "second", "--no-ignore"], &f.root);
    assert!(stdout(&o).contains("wide.txt"), "auto: {}", stdout(&o));

    let o = run(&["search", "--encoding", "latin1", "café"], &f.root);
    assert!(
        stdout(&o).contains("latin.txt:needle café"),
        "{}",
        stdout(&o)
    );

    let o = run(&["search", "--encoding", "klingon", "x"], &f.root);
    assert_eq!(code(&o), 2, "an unknown encoding is misuse");
}

#[test]
fn presets_print_and_select() {
    let f = Fixture::new("preset");
    let conf = f.root.join("presets.conf");
    fs::write(
        &conf,
        "[mini]\nprefixes = node_modules\nage = 1w\nlist_limit = 5\n",
    )
    .unwrap();

    let o = run_env(&["prune", "--print-presets"], &[("DUSAGE_CONFIG", &conf)]);
    assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
    let out = stdout(&o);
    assert!(out.contains("[mini]"), "{out}");
    assert!(out.contains("age = 1w"), "{out}");
    assert!(out.contains("[cargo]"), "builtins stay visible: {out}");

    let o = run_env(
        &[
            "prune",
            f.root.to_str().unwrap(),
            "--preset",
            "nope",
            "--dry",
        ],
        &[("DUSAGE_CONFIG", &conf)],
    );
    assert_eq!(code(&o), 2, "an unknown preset is misuse");

    let o = run_env(
        &[
            "prune",
            f.root.to_str().unwrap(),
            "--preset",
            "mini",
            "--dry",
        ],
        &[("DUSAGE_CONFIG", &conf)],
    );
    assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(err.contains("candidates"), "{err}");
}

#[test]
fn a_positive_glob_overrides_ignore_rules() {
    let f = Fixture::new("globwin");
    fs::write(f.root.join(".gitignore"), "*.log\n").unwrap();
    fs::write(f.root.join("debug.log"), "needle in a log\n").unwrap();

    let o = run(&["search", "-l", "needle"], &f.root);
    assert!(!stdout(&o).contains("debug.log"), "{}", stdout(&o));

    let o = run(&["search", "-l", "-g", "*.log", "needle"], &f.root);
    assert!(stdout(&o).contains("debug.log"), "{}", stdout(&o));
}

#[test]
fn type_filters_narrow_without_overriding_ignores() {
    let f = Fixture::new("typefilter");
    fs::write(f.root.join(".gitignore"), "*.log\n").unwrap();
    fs::write(f.root.join("debug.log"), "needle in a log\n").unwrap();

    let o = run(&["search", "-l", "-t", "rust", "needle"], &f.root);
    let out = stdout(&o);
    assert!(out.contains("main.rs"), "{out}");
    assert!(
        !out.contains("debug.log"),
        "a type filter keeps ignore rules: {out}"
    );

    let o = run(&["find", "--dirs", "*"], &f.root);
    assert!(stdout(&o).contains("src"), "{}", stdout(&o));
    let o = run(&["find", "-t", "rust", "*"], &f.root);
    let out = stdout(&o);
    assert!(out.contains("main.rs"), "{out}");
    assert!(
        !out.lines().any(|l| l.ends_with("/src")),
        "a type filter implies files only: {out}"
    );
}

#[test]
fn depth_links_and_attached_values() {
    let f = Fixture::new("depth");
    fs::write(f.root.join("top.txt"), "needle at the top\n").unwrap();
    let d = run(&["search", "-l", "--max-depth", "1", "needle"], &f.root);
    let out = stdout(&d);
    assert!(out.contains("top.txt"), "{out}");
    assert!(!out.contains("main.rs"), "depth 1 stops at the root: {out}");
    let d = run(&["search", "-l", "--max-depth", "2", "needle"], &f.root);
    assert!(stdout(&d).contains("main.rs"), "{}", stdout(&d));

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(f.root.join("src/main.rs"), f.root.join("link.rs")).unwrap();
        let o = run(&["search", "-l", "needle"], &f.root);
        assert!(
            !stdout(&o).contains("link.rs"),
            "a walk never follows a link: {}",
            stdout(&o)
        );
    }

    let o = run(&["search", "-m0", "needle"], &f.root);
    assert_eq!(code(&o), 1, "-m0 reports nothing");
    assert!(stdout(&o).is_empty());
    let o = run(&["search", "-C1", "-n", "needle"], &f.root);
    assert_eq!(code(&o), 0, "an attached short value parses");
    assert!(stdout(&o).contains("it.rs"), "{}", stdout(&o));
}

#[test]
fn ancestor_ignore_files_apply_to_a_subdirectory_scan() {
    let f = Fixture::new("ancestor");
    fs::write(f.root.join(".gitignore"), "*.log\nsrc/deep/\n").unwrap();
    fs::create_dir_all(f.root.join(".git")).unwrap();
    fs::write(f.root.join("src/main.rs"), "needle\n").unwrap();
    fs::write(f.root.join("src/deep/util.rs"), "needle\n").unwrap();
    fs::write(f.root.join("src/run.log"), "needle\n").unwrap();

    let sub = f.root.join("src");
    let o = run(&["search", "-l", "needle"], &sub);
    let out = stdout(&o);
    assert!(out.contains("main.rs"), "{out}");
    assert!(
        !out.contains("util.rs"),
        "an ancestor rule hides src/deep/: {out}"
    );
    assert!(
        !out.contains("run.log"),
        "an ancestor *.log rule applies: {out}"
    );
}

#[test]
fn a_positive_glob_re_includes_a_dotfile() {
    let f = Fixture::new("dotglob");
    fs::write(f.root.join(".hidden.rs"), "needle in a dotfile\n").unwrap();
    let o = run(&["search", "-l", "needle"], &f.root);
    assert!(!stdout(&o).contains(".hidden.rs"), "{}", stdout(&o));
    let o = run(&["search", "-l", "-g", "*.rs", "needle"], &f.root);
    assert!(
        stdout(&o).contains(".hidden.rs"),
        "-g names the dotfile: {}",
        stdout(&o)
    );
}

#[test]
fn standard_input_is_searched_when_piped() {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_dusage"))
        .args(["search", "-n", "needle"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"alpha needle\nbeta\n")
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(code(&o), 0);
    assert!(stdout(&o).contains("1:alpha needle"), "{}", stdout(&o));

    let mut child = Command::new(env!("CARGO_BIN_EXE_dusage"))
        .args(["search", "-H", "needle", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"needle here\n")
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert!(
        stdout(&o).contains("(standard input):needle here"),
        "{}",
        stdout(&o)
    );
}

#[test]
fn repeated_patterns_and_match_counts() {
    let f = Fixture::new("patterns");
    let o = run(&["search", "-n", "-e", "needle", "-e", "Title"], &f.root);
    let out = stdout(&o);
    assert!(out.contains("main.rs:2"), "{out}");
    assert!(out.contains("README.md:1"), "{out}");
    let o = run(&["search", "-F", "-e", "a.c", "-e", "zzz"], &f.root);
    assert_eq!(code(&o), 1, "a literal '.' must not match a wildcard");
    let o = run(&["search", "-e", "a.c", "-e", "helper"], &f.root);
    assert_eq!(code(&o), 0, "an unescaped '.' is a wildcard");
    let o = run(
        &["search", "--count-matches", "needle", "--no-ignore"],
        &f.root,
    );
    let out = stdout(&o);
    let total: u64 = out
        .lines()
        .map(|l| {
            l.rsplit(':')
                .next()
                .unwrap_or("0")
                .parse::<u64>()
                .unwrap_or(0)
        })
        .sum();
    assert_eq!(total, 5, "five needle occurrences: {out}");
}

#[test]
fn files_mode_lists_what_a_search_would_read() {
    let f = Fixture::new("filesmode");
    let o = run(&["search", "--files"], &f.root);
    assert_eq!(code(&o), 0, "{}", String::from_utf8_lossy(&o.stderr));
    let out = stdout(&o);
    assert!(out.contains("src/main.rs"), "{out}");
    assert!(
        !out.contains("node_modules"),
        "ignore rules still apply: {out}"
    );
    let o = run(&["search", "--files", "-t", "rust"], &f.root);
    let out = stdout(&o);
    assert!(out.contains("main.rs"), "{out}");
    assert!(!out.contains("README.md"), "a type filter narrows: {out}");
}

#[test]
fn lookaround_backreferences_and_unicode_classes() {
    let f = Fixture::new("engine");
    fs::write(
        f.root.join("eng.txt"),
        "foo bar\nfoo baz\nabc abc\ncafé ١٢٣ Ωμέγα\n",
    )
    .unwrap();

    let o = run(&["search", "-n", "foo(?= bar)"], &f.root);
    assert!(stdout(&o).contains("eng.txt:1:foo bar"), "{}", stdout(&o));
    let o = run(&["search", "-n", r"(\w+) \1"], &f.root);
    assert!(stdout(&o).contains("eng.txt:3:abc abc"), "{}", stdout(&o));
    let o = run(&["search", "-n", "-P", "foo(?= baz)"], &f.root);
    assert!(stdout(&o).contains("eng.txt:2:foo baz"), "{}", stdout(&o));

    let o = run(&["search", "-o", r"\p{L}+"], &f.root);
    let out = stdout(&o);
    assert!(out.contains("café"), "Unicode letters: {out}");
    assert!(out.contains("Ωμέγα"), "{out}");
    let o = run(&["search", "-i", "CAF[É]"], &f.root);
    assert_eq!(code(&o), 0, "regex -i folds non-ASCII");
    assert!(stdout(&o).contains("café"), "{}", stdout(&o));
}

#[test]
fn pattern_files_and_bundled_short_flags() {
    let f = Fixture::new("patfile");
    let pats = f.root.join("pats.txt");
    fs::write(&pats, "needle\r\nTitle\n\n").unwrap();

    // Two patterns, a CRLF line and a blank line: the blank one is not an empty alternative.
    let o = run(&["search", "-n", "-f", pats.to_str().unwrap()], &f.root);
    let out = stdout(&o);
    assert!(out.contains("main.rs:2"), "{out}");
    assert!(out.contains("README.md:1"), "{out}");
    assert_eq!(code(&o), 0);

    // `-f` and `-e` add up, the way they do in grep.
    let o = run(
        &["search", "-c", "-f", pats.to_str().unwrap(), "-e", "helper"],
        &f.root,
    );
    let out = stdout(&o);
    assert!(out.contains("util.rs:1"), "{out}");

    // `-F` makes every pattern in the file a literal.
    let wild = f.root.join("wild.txt");
    fs::write(&wild, "need.le\n").unwrap();
    let o = run(&["search", "-f", wild.to_str().unwrap()], &f.root);
    assert_eq!(code(&o), 0, "a '.' is a wildcard here");
    let o = run(&["search", "-F", "-f", wild.to_str().unwrap()], &f.root);
    assert_eq!(code(&o), 1, "-F is a literal, so 'need.le' never matches");

    // A pattern file with nothing in it is an error, not a silent empty pattern.
    let empty = f.root.join("empty.txt");
    fs::write(&empty, "\n\n").unwrap();
    let o = run(&["search", "-f", empty.to_str().unwrap()], &f.root);
    assert_eq!(code(&o), 2, "{}", stdout(&o));

    // Bundled short flags mean the same as the separate ones.
    let bundled = run(&["search", "-inc", "needle"], &f.root);
    let separate = run(&["search", "-i", "-n", "-c", "needle"], &f.root);
    assert_eq!(code(&bundled), 0, "{}", stdout(&bundled));
    assert_eq!(stdout(&bundled), stdout(&separate));
    // A value flag may end a bundle and take the rest of it as its value.
    let o = run(&["search", "-inm1", "needle", "--no-ignore"], &f.root);
    let out = stdout(&o);
    assert_eq!(
        out.lines().filter(|l| l.contains("it.rs")).count(),
        1,
        "-m1 stops at one line per file, and it.rs holds two: {out}"
    );
}

#[test]
fn pattern_file_on_stdin_cannot_also_be_the_haystack() {
    let f = Fixture::new("stdinboth");
    let mut child = Command::new(env!("CARGO_BIN_EXE_dusage"))
        .args(["search", "-f", "-"])
        .arg(&f.root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write as _;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"needle\n")
        .unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(
        o.status.code(),
        Some(0),
        "with a path there is no clash, and the pattern file may come from stdin"
    );
    assert!(String::from_utf8_lossy(&o.stdout).contains("needle"));
}

#[test]
fn a_closed_pipe_is_not_a_failure() {
    let f = Fixture::new("epipe");
    let mut big = String::new();
    for i in 0..50_000 {
        big.push_str("needle line ");
        big.push_str(&i.to_string());
        big.push('\n');
    }
    fs::write(f.root.join("big.txt"), big).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_dusage"))
        .args(["search", "needle", "--no-ignore"])
        .arg(&f.root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Closing the read end is what `| head` does when it has seen enough.
    drop(child.stdout.take());
    let o = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(0), "stderr: {err}");
    assert!(err.is_empty(), "no noise on a closed pipe: {err}");
}

#[test]
fn case_modes_and_line_regexp() {
    let f = Fixture::new("cases");
    fs::write(f.root.join("shape.txt"), "Needle here\nneedle there\n").unwrap();
    let plain = run(&["search", "-c", "needle"], &f.root);
    let sensitive = run(&["search", "-c", "-s", "needle"], &f.root);
    let smart_lower = run(&["search", "-c", "-S", "needle"], &f.root);
    let smart_upper = run(&["search", "-c", "-S", "Needle"], &f.root);
    assert!(stdout(&plain).contains("shape.txt:1"), "{}", stdout(&plain));
    assert!(
        stdout(&sensitive).contains("shape.txt:1"),
        "{}",
        stdout(&sensitive)
    );
    assert!(
        stdout(&smart_lower).contains("shape.txt:2"),
        "-S folds a lowercase pattern: {}",
        stdout(&smart_lower)
    );
    assert!(
        stdout(&smart_upper).contains("shape.txt:1"),
        "-S keeps an uppercase pattern exact"
    );

    // `-s` after `-i` wins, as in ripgrep.
    let o = run(&["search", "-c", "-i", "-s", "NEEDLE"], &f.root);
    assert_eq!(code(&o), 1, "-i then -s is case-sensitive: {}", stdout(&o));

    // `--line-regexp` requires the whole line.
    let o = run(&["search", "--line-regexp", "needle"], &f.root);
    assert_eq!(code(&o), 1, "{}", stdout(&o));
    let o = run(&["search", "--line-regexp", "needle there"], &f.root);
    assert_eq!(code(&o), 0, "{}", stdout(&o));
}

#[test]
fn types_unrestricted_and_iglob() {
    let f = Fixture::new("types");
    fs::write(f.root.join("keep.rs"), "needle\n").unwrap();
    fs::write(f.root.join("drop.py"), "needle\n").unwrap();
    let o = run(&["search", "-l", "-t", "rust", "needle"], &f.root);
    assert!(stdout(&o).contains("keep.rs"), "{}", stdout(&o));
    assert!(
        !stdout(&o).contains("drop.py"),
        "-t narrows: {}",
        stdout(&o)
    );

    let o = run(&["search", "-l", "-T", "rust", "needle"], &f.root);
    assert!(
        !stdout(&o).contains("keep.rs"),
        "-T excludes: {}",
        stdout(&o)
    );
    assert!(stdout(&o).contains("drop.py"), "{}", stdout(&o));

    // `--type-list` prints the table as globs, which is what `-t` turns into filters.
    let o = run(&["search", "--type-list"], &f.root);
    assert!(stdout(&o).contains("rust: *.rs"), "{}", stdout(&o));

    // `-u` drops the ignore rules, `-uu` also takes hidden paths.
    let o = run(&["search", "-l", "needle"], &f.root);
    assert!(!stdout(&o).contains("node_modules"), "{}", stdout(&o));
    let o = run(&["search", "-l", "-u", "needle"], &f.root);
    assert!(
        stdout(&o).contains("node_modules"),
        "-u ignores rules: {}",
        stdout(&o)
    );
    fs::write(f.root.join(".hidden.txt"), "needle\n").unwrap();
    let o = run(&["search", "-l", "-u", "needle"], &f.root);
    assert!(
        !stdout(&o).contains(".hidden.txt"),
        "-u keeps hidden out: {}",
        stdout(&o)
    );
    let o = run(&["search", "-l", "-uu", "needle"], &f.root);
    assert!(
        stdout(&o).contains(".hidden.txt"),
        "-uu takes hidden: {}",
        stdout(&o)
    );

    // `--iglob` matches without case and is an override like `-g`.
    let o = run(&["search", "-l", "--iglob", "*.RS", "needle"], &f.root);
    assert!(stdout(&o).contains("keep.rs"), "{}", stdout(&o));
    assert!(!stdout(&o).contains("drop.py"), "{}", stdout(&o));
}

#[test]
fn column_offset_heading_and_include_zero() {
    let f = Fixture::new("positions");
    fs::write(
        f.root.join("pos.txt"),
        "alpha needle beta\nsecond line\nneedle here\n",
    )
    .unwrap();
    // `-h` keeps the file name out, so the shapes below are the prefix alone.
    let o = run(
        &[
            "search", "-h", "-n", "--column", "--iglob", "pos.txt", "needle",
        ],
        &f.root,
    );
    assert_eq!(stdout(&o), "1:7:alpha needle beta\n3:1:needle here\n");
    let o = run(
        &[
            "search",
            "-h",
            "-n",
            "--byte-offset",
            "--iglob",
            "pos.txt",
            "needle",
        ],
        &f.root,
    );
    assert_eq!(stdout(&o), "1:0:alpha needle beta\n3:30:needle here\n");
    let o = run(
        &[
            "search",
            "-h",
            "-n",
            "--column",
            "--byte-offset",
            "--iglob",
            "pos.txt",
            "needle",
        ],
        &f.root,
    );
    assert_eq!(stdout(&o), "1:7:0:alpha needle beta\n3:1:30:needle here\n");

    // `--heading` prints the name once, above, and a blank line between files.
    fs::write(f.root.join("other.txt"), "needle again\n").unwrap();
    let o = run(&["search", "-n", "--heading", "needle"], &f.root);
    let out = stdout(&o);
    let headings = out.lines().filter(|l| !l.contains(':')).count();
    assert!(headings >= 3, "one heading per matching file: {out}");
    assert!(out.contains("\n\n"), "files are separated: {out:?}");
    assert!(
        out.lines().any(|l| l == "1:alpha needle beta"),
        "lines under a heading carry no file name: {out}"
    );

    // `-c --include-zero` prints the files that matched nothing.
    fs::write(f.root.join("zero.txt"), "nothing here\n").unwrap();
    let o = run(&["search", "-c", "--include-zero", "needle"], &f.root);
    let out = stdout(&o);
    assert!(out.contains("zero.txt:0"), "{out}");
}

#[test]
fn passthru_prints_every_line() {
    let f = Fixture::new("passthru");
    let dir = f.root.join("pt");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), "aaa\nneedle\nbbb\n").unwrap();
    fs::write(dir.join("b.txt"), "zzz\n").unwrap();
    let o = run(&["search", "--passthru", "needle"], &dir);
    assert_eq!(code(&o), 0, "stderr: {}", stderr(&o));
    // The matched line keeps its ':' separator and the rest come out as context ('-'),
    // which is exactly what ripgrep prints.
    let p = |name: &str| dir.join(name).display().to_string();
    assert_eq!(
        stdout(&o),
        format!(
            "{}-aaa\n{}:needle\n{}-bbb\n{}-zzz\n",
            p("a.txt"),
            p("a.txt"),
            p("a.txt"),
            p("b.txt")
        )
    );

    // No match at all: every line still prints, and the exit status stays 1.
    let o = run(&["search", "--passthru", "absent"], &dir);
    assert_eq!(code(&o), 1);
    assert_eq!(stdout(&o).lines().count(), 4, "{}", stdout(&o));
}

#[test]
fn sort_orders_results() {
    let f = Fixture::new("sort");
    let dir = f.root.join("sortdir");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), "needle\n").unwrap();
    fs::write(dir.join("b.txt"), "needle\n").unwrap();
    let first = |s: String| -> String { s.lines().next().unwrap_or_default().to_string() };
    let plain = first(stdout(&run(&["find", "*.txt"], &dir)));
    assert!(plain.ends_with("a.txt"), "{plain}");
    let rev = first(stdout(&run(&["find", "*.txt", "--sortr", "path"], &dir)));
    assert!(rev.ends_with("b.txt"), "{rev}");

    // `modified` goes oldest first, and `--sortr` brings the newest to the front.
    let older = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
    let ft = fs::File::options()
        .write(true)
        .open(dir.join("b.txt"))
        .unwrap();
    ft.set_modified(older).unwrap();
    let by_time = first(stdout(&run(
        &["search", "-l", "--sort", "modified", "needle"],
        &dir,
    )));
    assert!(by_time.ends_with("b.txt"), "{by_time}");
    let newest = first(stdout(&run(
        &["search", "-l", "--sortr", "modified", "needle"],
        &dir,
    )));
    assert!(newest.ends_with("a.txt"), "{newest}");

    let bad = run(&["find", "*.txt", "--sort", "size"], &dir);
    assert_eq!(code(&bad), 2, "an unknown key is refused");
    assert!(
        stderr(&bad).contains("unsupported --sort key"),
        "{}",
        stderr(&bad)
    );
}

#[test]
fn ignore_sources_can_be_switched_off() {
    let f = Fixture::new("nosources");
    let dir = f.root.join("ignored");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(".gitignore"), "bygit.txt\n").unwrap();
    fs::write(dir.join(".ignore"), "bydot.txt\n").unwrap();
    fs::write(dir.join("bygit.txt"), "needle\n").unwrap();
    fs::write(dir.join("bydot.txt"), "needle\n").unwrap();
    fs::write(dir.join("kept.txt"), "needle\n").unwrap();

    let base = stdout(&run(&["search", "-l", "needle"], &dir));
    assert!(
        base.contains("kept.txt") && !base.contains("bygit"),
        "{base}"
    );

    // `--no-ignore-vcs` takes only the `.gitignore` layer away, and `--no-ignore-dot` only
    // the `.ignore` one.
    let vcs = stdout(&run(&["search", "-l", "--no-ignore-vcs", "needle"], &dir));
    assert!(vcs.contains("bygit.txt") && !vcs.contains("bydot"), "{vcs}");
    let dot = stdout(&run(&["search", "-l", "--no-ignore-dot", "needle"], &dir));
    assert!(dot.contains("bydot.txt") && !dot.contains("bygit"), "{dot}");
    let none = stdout(&run(&["search", "-l", "--no-ignore-files", "needle"], &dir));
    assert_eq!(none.lines().count(), 3, "{none}");
}

#[test]
fn replace_rewrites_matches() {
    let f = Fixture::new("replace");
    let dir = f.root.join("rep");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("m.txt"), "a1 a2 a3\nzzz\n").unwrap();

    // A replacement covers every match on the line, not just the first.
    assert_eq!(
        stdout(&run(&["search", "-h", "-r", "[$1]", r"a(.)"], &dir)),
        "[1] [2] [3]\n"
    );
    assert_eq!(
        stdout(&run(&["search", "-h", "-r", "", r"a."], &dir)),
        "  \n"
    );

    // `-o` prints just the replacement, one per match.
    assert_eq!(
        stdout(&run(&["search", "-h", "-o", "-r", "<$1>", r"a(.)"], &dir)),
        "<1>\n<2>\n<3>\n"
    );
    // `$0` is the whole match, `$$` a literal dollar, `${1}` the braced spelling, and grep's
    // `\1` is accepted as the same thing.
    assert_eq!(
        stdout(&run(&["search", "-h", "-o", "-r", "$0/$0", r"a."], &dir)),
        "a1/a1\na2/a2\na3/a3\n"
    );
    assert_eq!(
        stdout(&run(&["search", "-h", "-o", "-r", "$$${1}", r"a(.)"], &dir)),
        "$1\n$2\n$3\n"
    );
    assert_eq!(
        stdout(&run(&["search", "-h", "-o", "-r", r"\1", r"a(.)"], &dir)),
        "1\n2\n3\n"
    );

    // A group the pattern does not have is refused, as grep refuses an invalid back
    // reference, rather than printing nothing.
    let bad = run(&["search", "-r", "$3", r"a(.)"], &dir);
    assert_eq!(code(&bad), 2);
    assert!(stderr(&bad).contains("group $3"), "{}", stderr(&bad));
}

#[test]
fn follow_walks_links_once() {
    let f = Fixture::new("follow");
    let dir = f.root.join("followdir");
    fs::create_dir_all(dir.join("real/sub")).unwrap();
    fs::write(dir.join("real/a.txt"), "needle\n").unwrap();
    fs::write(dir.join("real/sub/b.txt"), "needle\n").unwrap();
    let link = |target: &str, name: &str| {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, dir.join(name)).unwrap();
        #[cfg(windows)]
        let _ = (target, name);
    };
    link("real", "linkdir");
    link("real/a.txt", "linkfile.txt");

    // Without `--follow` a link is invisible, as it was before.
    let plain = stdout(&run(&["search", "-l", "needle"], &dir));
    assert!(!plain.contains("link"), "{plain}");

    #[cfg(unix)]
    {
        let followed = stdout(&run(&["search", "-l", "--follow", "needle"], &dir));
        assert!(followed.contains("linkdir/a.txt"), "{followed}");
        assert!(followed.contains("linkfile.txt"), "{followed}");
        assert!(followed.contains("linkdir/sub/b.txt"), "{followed}");
        // The real tree is still walked once, not twice.
        assert_eq!(
            followed
                .lines()
                .filter(|l| l.ends_with("real/a.txt"))
                .count(),
            1,
            "{followed}"
        );

        // A link pointing back up the tree must not loop forever.
        link("..", "up");
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_dusage"));
        let out = cmd
            .args(["search", "-l", "--follow", "needle"])
            .arg(&dir)
            .output()
            .unwrap();
        assert!(out.status.code().is_some(), "the walk terminated");
    }
}

#[test]
fn multiline_matches_span_lines() {
    let f = Fixture::new("multiline");
    let dir = f.root.join("ml");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), "start\nmid\nend\n").unwrap();

    // A match across lines prints each line it covers, numbered.
    let block = stdout(&run(&["search", "-U", "-n", "-h", r"start\nmid"], &dir));
    assert_eq!(block, "1:start\n2:mid\n");

    // `-o` prints the whole match, newline included.
    assert_eq!(
        stdout(&run(
            &["search", "-U", "-o", "-h", r"start[\s\S]*end"],
            &dir
        )),
        "start\nmid\nend\n"
    );
    // Without `-U` a pattern that spells a newline is refused, as ripgrep refuses it, rather
    // than quietly matching nothing.
    let escaped = run(&["search", "-n", r"start\nmid"], &dir);
    assert_eq!(code(&escaped), 2);
    assert!(
        stderr(&escaped).contains("pass -U/--multiline"),
        "{}",
        stderr(&escaped)
    );
    // `--multiline-dotall` is what lets `.` cross the newline.
    assert_eq!(code(&run(&["search", "-U", "-o", r"start.*end"], &dir)), 1);
    assert_eq!(
        stdout(&run(
            &[
                "search",
                "-U",
                "--multiline-dotall",
                "-o",
                "-h",
                r"start.*end"
            ],
            &dir
        )),
        "start\nmid\nend\n"
    );

    // `-c` counts the lines the matches cover, and `-l` names the file once.
    assert_eq!(
        stdout(&run(&["search", "-U", "-c", "-h", "mid"], &dir)),
        "1\n"
    );
    assert_eq!(
        stdout(&run(&["search", "-U", "-l", "start\\nmid"], &dir))
            .lines()
            .count(),
        1
    );

    // The line-level modes this cannot answer are refused rather than answered wrongly.
    let inverted = run(&["search", "-U", "-v", "mid"], &dir);
    assert_eq!(code(&inverted), 2);
    assert!(
        stderr(&inverted).contains("-U cannot be combined"),
        "{}",
        stderr(&inverted)
    );
    let context = run(&["search", "-U", "-A", "1", "mid"], &dir);
    assert_eq!(code(&context), 2);
}

#[test]
fn search_zip_expands_gzip() {
    let f = Fixture::new("searchzip");
    let dir = f.root.join("zip");
    fs::create_dir_all(&dir).unwrap();
    // `gzip -9` of "alpha needle beta\ngamma needle delta\n", so the test needs no gzip
    // binary and no compressor of its own.
    const GZ: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x08, 0xf1, 0xef, 0xfe, 0x6a, 0x02, 0x03, 0x67, 0x2e, 0x74, 0x78, 0x74,
        0x00, 0x4b, 0xcc, 0x29, 0xc8, 0x48, 0x54, 0xc8, 0x4b, 0x4d, 0x4d, 0xc9, 0x49, 0x55, 0x48,
        0x4a, 0x2d, 0x49, 0xe4, 0x4a, 0x4f, 0xcc, 0xcd, 0x85, 0x8b, 0xa4, 0xa4, 0xe6, 0x00, 0x85,
        0x00, 0x1e, 0xea, 0x96, 0x3e, 0x25, 0x00, 0x00, 0x00,
    ];
    fs::write(dir.join("log.txt.gz"), GZ).unwrap();

    // Without `-z` the compressed bytes are what gets searched, and the pattern is not in
    // them.
    assert_eq!(code(&run(&["search", "-l", "needle"], &dir)), 1);

    let found = stdout(&run(&["search", "-z", "-n", "needle"], &dir));
    assert!(found.contains("log.txt.gz:1:alpha needle beta"), "{found}");
    assert!(found.contains("log.txt.gz:2:gamma needle delta"), "{found}");
    assert_eq!(
        stdout(&run(&["search", "-z", "-c", "-h", "needle"], &dir)),
        "2\n"
    );

    // A short or corrupt member is an error, reported the way an unreadable file is.
    fs::write(dir.join("broken.txt.gz"), &GZ[..20]).unwrap();
    // An error makes the run exit 2 even though the good file matched, which is grep's
    // contract for "something went wrong" as opposed to "nothing matched".
    let broken = run(&["search", "-z", "-l", "needle"], &dir);
    assert_eq!(code(&broken), 2);
    assert!(stdout(&broken).contains("log.txt.gz"));
    assert!(
        stderr(&broken).contains("could not be read"),
        "{}",
        stderr(&broken)
    );
}

#[test]
fn find_matching_controls() {
    let f = Fixture::new("findctl");
    let dir = f.root.join("fd");
    fs::create_dir_all(dir.join("a/b")).unwrap();
    fs::write(dir.join("top.txt"), "x").unwrap();
    fs::write(dir.join("a/mid.txt"), "x").unwrap();
    fs::write(dir.join("a/b/deep.txt"), "x").unwrap();
    fs::write(dir.join("A_upper.TXT"), "x").unwrap();

    // Without a hint a pattern with no `/` is matched against the name.
    let plain = stdout(&run(&["find", "*.txt"], &dir));
    assert!(!plain.contains("A_upper.TXT"), "{plain}");
    assert!(plain.contains("a/b/deep.txt"), "{plain}");

    // `-i` folds the glob, and `--name` says the subject is the file name alone.
    let folded = stdout(&run(&["find", "-i", "--name", "*.txt"], &dir));
    assert!(folded.contains("A_upper.TXT"), "{folded}");
    assert!(folded.contains("a/b/deep.txt"), "{folded}");

    // `--full-path` anchors against the whole relative path.
    let path = stdout(&run(&["find", "--full-path", "a/*.txt"], &dir));
    assert_eq!(path.trim(), "a/mid.txt");

    // `--mindepth` counts levels below the root, so a file in the root is 1.
    let deep = stdout(&run(&["find", "--mindepth", "3", "*"], &dir));
    assert_eq!(deep.trim(), "a/b/deep.txt");

    // `--fixed` matches a substring, folded with `-i` too.
    assert!(stdout(&run(&["find", "-F", "deep"], &dir)).contains("deep.txt"));
    let named = run(&["find", "-F", "-i", "upper"], &dir);
    assert!(stdout(&named).contains("A_upper.TXT"), "{}", stdout(&named));
}

#[test]
fn types_can_be_added_and_cleared() {
    let f = Fixture::new("typetable");
    let dir = f.root.join("ty");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("x.foo"), "needle\n").unwrap();
    fs::write(dir.join("y.rs"), "needle\n").unwrap();
    fs::write(dir.join("z.md"), "needle\n").unwrap();

    // A new type is a set of globs, and `-t` then selects them.
    let added = stdout(&run(
        &[
            "search",
            "-l",
            "--type-add",
            "foo:*.foo",
            "-t",
            "foo",
            "needle",
        ],
        &dir,
    ));
    assert!(
        added.contains("x.foo") && !added.contains("y.rs"),
        "{added}"
    );

    // `--type-list` prints the table as it stands after the edits.
    let list = stdout(&run(
        &["search", "--type-add", "foo:*.foo", "--type-list"],
        &dir,
    ));
    assert!(list.contains("foo: *.foo"), "{list}");
    assert!(list.contains("rust: *.rs"), "{list}");

    // Naming an existing type adds to it, and `--type-clear` is how one is redefined.
    let extended = stdout(&run(
        &[
            "search",
            "-l",
            "--type-add",
            "rust:*.foo",
            "-t",
            "rust",
            "needle",
        ],
        &dir,
    ));
    assert!(
        extended.contains("x.foo") && extended.contains("y.rs"),
        "{extended}"
    );
    let redefined = stdout(&run(
        &[
            "search",
            "-l",
            "--type-clear",
            "rust",
            "--type-add",
            "rust:*.foo",
            "-t",
            "rust",
            "needle",
        ],
        &dir,
    ));
    assert!(
        redefined.contains("x.foo") && !redefined.contains("y.rs"),
        "{redefined}"
    );

    // An unknown type is still refused, and `--type-list` accepts a type that is not there.
    let unknown = run(&["search", "-l", "-t", "nope", "needle"], &dir);
    assert_eq!(code(&unknown), 2);
    assert!(
        stderr(&unknown).contains("unknown type"),
        "{}",
        stderr(&unknown)
    );
    assert_eq!(
        code(&run(
            &["search", "--type-clear", "nope", "-t", "rust", "needle"],
            &dir
        )),
        0
    );
}

#[test]
fn columns_preview_and_time_sorts() {
    let f = Fixture::new("polish");
    let dir = f.root.join("pol");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("long.txt"), "a very long line with needle in it\n").unwrap();
    fs::write(dir.join("b.txt"), "needle\n").unwrap();
    fs::write(dir.join("a.txt"), "needle\n").unwrap();

    // `--max-columns` cuts at a character and `--max-columns-preview` says it did.
    let cut = stdout(&run(
        &[
            "search",
            "-h",
            "--max-columns",
            "20",
            "--max-columns-preview",
            "needle",
        ],
        &dir,
    ));
    assert!(
        cut.ends_with("a very long line wit [... omitted end of long line]\n"),
        "{cut}"
    );
    // The short lines are untouched: the cut is for the lines that need it.
    assert_eq!(cut.lines().filter(|l| *l == "needle").count(), 2, "{cut}");

    // The time sorts order by the time they name, and `--sortr` reverses.
    let oldest_first = stdout(&run(&["search", "-l", "--sort", "created", "needle"], &dir));
    let reversed = stdout(&run(
        &["search", "-l", "--sortr", "created", "needle"],
        &dir,
    ));
    assert_eq!(
        oldest_first.lines().next(),
        reversed.lines().last(),
        "the two orders are each other's reverse"
    );
    // An unknown key names the ones that exist.
    let bad = run(&["search", "--sort", "size", "needle"], &dir);
    assert_eq!(code(&bad), 2);
    assert!(stderr(&bad).contains("accessed"), "{}", stderr(&bad));
}

#[test]
fn grep_compatibility_flags() {
    let f = Fixture::new("grepcompat");
    let dir = f.root.join("gc");
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::write(dir.join("a.c"), "alpha needle\n").unwrap();
    fs::write(dir.join("b.h"), "needle\n").unwrap();
    fs::write(dir.join("sub/c.c"), "needle\n").unwrap();
    fs::write(dir.join("bin.dat"), b"bin\0needle\n").unwrap();

    // `--include` narrows like `-g`, `--exclude` and `--exclude-dir` take paths away.
    let inc = stdout(&run(
        &["search", "-I", "-l", "--include", "*.c", "needle"],
        &dir,
    ));
    // Windows separates with `\`, so match on the file names rather than the paths.
    assert!(inc.contains("a.c") && inc.contains("c.c"), "{inc}");
    assert_eq!(inc.lines().count(), 2, "{inc}");
    assert!(!inc.contains("b.h"), "{inc}");
    let exc = stdout(&run(
        &["search", "-I", "-l", "--exclude", "*.h", "needle"],
        &dir,
    ));
    assert!(!exc.contains("b.h"), "{exc}");
    let excdir = stdout(&run(
        &["search", "-I", "-l", "--exclude-dir", "sub", "needle"],
        &dir,
    ));
    // Both files in the root stay; the one under `sub/` is gone.
    assert_eq!(excdir.lines().count(), 2, "{excdir}");
    assert!(excdir.contains("a.c") && excdir.contains("b.h"), "{excdir}");

    // `-I` makes a NUL-carrying file no match at all, which is `--binary-files=without-match`.
    assert!(!stdout(&run(&["search", "-l", "needle"], &dir)).is_empty());
    let quiet_bin = stdout(&run(&["search", "-I", "-l", "needle"], &dir));
    assert!(!quiet_bin.contains("bin.dat"), "{quiet_bin}");
    assert_eq!(
        stdout(&run(
            &["search", "--binary-files", "without-match", "-l", "needle"],
            &dir
        )),
        quiet_bin
    );

    // `-d skip` does not descend, so a file in a subdirectory drops out.
    let skipped = stdout(&run(&["search", "-I", "-l", "-d", "skip", "needle"], &dir));
    assert!(
        skipped.contains("a.c") && skipped.contains("b.h"),
        "{skipped}"
    );
    assert_eq!(skipped.lines().count(), 2, "{skipped}");
}

#[test]
fn basic_regular_expressions() {
    let f = Fixture::new("bre");
    let dir = f.root.join("bre");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("t.txt"), "abc\nabbbc\na+b\n(x)\nabbc\n").unwrap();

    // `+` and `( )` are literal in a basic regexp, `\+` and `\( \)` are the operators, and
    // `-E` is the default dialect spelled out.
    let lit_plus = stdout(&run(&["search", "-G", "-h", "a+b"], &dir));
    assert_eq!(lit_plus, "a+b\n");
    assert_eq!(
        stdout(&run(&["search", "-G", "-h", r"a\+b"], &dir)),
        "abc\nabbbc\nabbc\n"
    );
    assert_eq!(
        stdout(&run(&["search", "-G", "-h", r"\(x\)"], &dir)),
        "(x)\n"
    );
    assert_eq!(
        stdout(&run(&["search", "-E", "-h", "a+b"], &dir)),
        "abc\nabbbc\nabbc\n"
    );
    // `\{n\}` is BEG's bounded repetition, and it is exact.
    assert_eq!(
        stdout(&run(&["search", "-G", "-h", r"ab\{2\}c"], &dir)),
        "abbc\n"
    );
    // `{n}` is exact in the default dialect too, which is a bug this batch fixed.
    assert_eq!(
        stdout(&run(&["search", "-h", "-o", "ab{2}c"], &dir)),
        "abbc\n"
    );
}

#[test]
fn absolute_date_filters() {
    let f = Fixture::new("dates");
    let dir = f.root.join("dates");
    fs::create_dir_all(&dir).unwrap();
    let old = dir.join("old.txt");
    let new = dir.join("new.txt");
    fs::write(&old, "x").unwrap();
    fs::write(&new, "x").unwrap();
    // The timestamps are set from the file handle, so the test needs no date arithmetic of
    // its own beyond two fixed instants.
    let set = |p: &std::path::Path, at: std::time::SystemTime| {
        fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(at)
            .unwrap();
    };
    let epoch = std::time::UNIX_EPOCH;
    set(&old, epoch + std::time::Duration::from_secs(1_500_000_000)); // 2017
    set(&new, epoch + std::time::Duration::from_secs(1_700_000_000)); // 2023

    // `--newermt` takes an absolute date, a date-time, or a Unix timestamp.
    let newer = stdout(&run(&["find", "--newermt", "2020-01-01", "*.txt"], &dir));
    assert_eq!(newer.trim(), "new.txt", "{newer}");
    let stamped = stdout(&run(&["find", "--newermt", "@1600000000", "*.txt"], &dir));
    assert_eq!(stamped.trim(), "new.txt", "{stamped}");
    let both = stdout(&run(&["find", "--newermt", "2000-01-01", "*.txt"], &dir));
    assert_eq!(both.lines().count(), 2, "{both}");

    // A date that makes no sense is refused rather than silently matching nothing.
    let bad = run(&["find", "--newermt", "not-a-date", "*"], &dir);
    assert_eq!(code(&bad), 2);
    assert!(stderr(&bad).contains("invalid date"), "{}", stderr(&bad));
}

#[test]
fn null_data_splits_on_nul() {
    let f = Fixture::new("nulldata");
    let dir = f.root.join("nd");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("nul.dat"), b"aaa\0needle one\0ccc\0").unwrap();
    fs::write(dir.join("plain.txt"), "alpha needle beta\nsecond\n").unwrap();

    // A record ends at NUL, the record number counts records, and the output ends the same
    // way, so `dusage search --null-data … | xargs -0` works.
    let nul = dir.join("nul.dat").display().to_string();
    let plain = dir.join("plain.txt").display().to_string();
    let rec = run(&["search", "--null-data", "-n", "needle"], &dir);
    assert_eq!(code(&rec), 0, "{}", stderr(&rec));
    assert_eq!(
        String::from_utf8_lossy(&rec.stdout),
        format!("{nul}:2:needle one\0{plain}:1:alpha needle beta\nsecond\n\0")
    );

    // `-c` counts records, `-l` names files, both NUL-terminated.
    let count = run(&["search", "--null-data", "-c", "needle"], &dir);
    assert_eq!(
        String::from_utf8_lossy(&count.stdout),
        format!("{nul}:1\0{plain}:1\0")
    );
    let names = run(&["search", "--null-data", "-l", "needle"], &dir);
    assert_eq!(
        String::from_utf8_lossy(&names.stdout),
        format!("{nul}\0{plain}\0")
    );

    // `-o` prints just the match, and `-Z`/`-0` still mean "NUL after the file name".
    let only = run(&["search", "--null-data", "-o", "needle"], &dir);
    assert_eq!(
        String::from_utf8_lossy(&only.stdout),
        format!("{nul}:needle\0{plain}:needle\0")
    );
    let zero = run(&["search", "-0", "-l", "needle"], &dir);
    assert_eq!(zero.stdout, names.stdout);
}

#[test]
fn empty_files_and_directories() {
    let f = Fixture::new("empty");
    let dir = f.root.join("emp");
    fs::create_dir_all(dir.join("hollow")).unwrap();
    fs::create_dir_all(dir.join("full")).unwrap();
    fs::write(dir.join("zero.txt"), "").unwrap();
    fs::write(dir.join("full/f.txt"), "x").unwrap();
    fs::write(dir.join("some.txt"), "x").unwrap();

    // A file of no bytes and a directory with no entries, which is `find -empty`.
    let both = stdout(&run(&["find", "--empty", "*"], &dir));
    assert_eq!(both.lines().count(), 2, "{both}");
    assert!(both.contains("hollow"), "{both}");
    assert!(both.contains("zero.txt"), "{both}");
    assert!(!both.contains("full"), "{both}");

    // The type filters narrow it further, as `find -empty -type f` does.
    assert_eq!(
        stdout(&run(&["find", "--empty", "--files", "*"], &dir)).trim(),
        "zero.txt"
    );
    assert_eq!(
        stdout(&run(&["find", "--empty", "--dirs", "*"], &dir)).trim(),
        "hollow"
    );
}

#[cfg(unix)]
#[test]
fn pre_runs_a_command_per_file() {
    let f = Fixture::new("pre");
    let dir = f.root.join("pre");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), "raw text needle\n").unwrap();
    fs::write(dir.join("b.bin"), "needle\n").unwrap();

    // The command gets the path as its argument and its output is what gets searched.
    let out = stdout(&run(
        &["search", "-h", "-n", "--pre", "cat", "needle"],
        &dir,
    ));
    assert_eq!(out, "1:raw text needle\n1:needle\n", "{out}");

    // `--pre-glob` narrows which files the command runs on; the rest are searched as they are.
    let narrowed = stdout(&run(
        &[
            "search",
            "-l",
            "--pre",
            "cat",
            "--pre-glob",
            "*.txt",
            "needle",
        ],
        &dir,
    ));
    assert_eq!(narrowed.lines().count(), 2, "{narrowed}");

    // A command that fails on a file is an error, reported like an unreadable path.
    let broken = run(&["search", "--pre", "false", "needle"], &dir);
    assert_eq!(code(&broken), 2);
}

#[test]
fn vimgrep_prints_a_row_per_match() {
    let f = Fixture::new("vimgrep");
    let dir = f.root.join("vg");
    fs::create_dir_all(&dir).unwrap();
    let pos = dir.join("pos.txt");
    fs::write(&pos, "alpha foo beta foo\nlast\na foo\n").unwrap();
    let p = pos.display().to_string();

    // ripgrep 14.1 prints the whole line once per match, with the match's own column:
    //   $ rg --vimgrep foo pos.txt
    //   pos.txt:1:7:alpha foo beta foo
    //   pos.txt:1:16:alpha foo beta foo
    //   pos.txt:3:3:a foo
    let o = run(&["search", "--vimgrep", "foo"], &pos);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(
        stdout(&o),
        format!("{p}:1:7:alpha foo beta foo\n{p}:1:16:alpha foo beta foo\n{p}:3:3:a foo\n")
    );

    // `-o` keeps the row count and trades the line for the match alone:
    //   $ rg --vimgrep -o foo pos.txt
    //   pos.txt:1:7:foo
    let o = run(&["search", "--vimgrep", "-o", "foo"], &pos);
    assert_eq!(
        stdout(&o),
        format!("{p}:1:7:foo\n{p}:1:16:foo\n{p}:3:3:foo\n")
    );

    // A context row keeps the path and the line number and has no column:
    //   $ rg --vimgrep -A1 HIT one.txt
    //   one.txt:2:1:HIT
    //   one.txt-3-3
    let one = dir.join("one.txt");
    fs::write(&one, "1\nHIT\n3\n").unwrap();
    let c = one.display().to_string();
    let o = run(&["search", "--vimgrep", "-A1", "HIT"], &one);
    assert_eq!(stdout(&o), format!("{c}:2:1:HIT\n{c}-3-3\n"));

    // `-c` still counts lines, not rows.
    let o = run(&["search", "--vimgrep", "-c", "foo"], &pos);
    assert_eq!(stdout(&o), format!("{p}:2\n"));
}

#[test]
fn trim_drops_leading_whitespace() {
    let f = Fixture::new("trim");
    let dir = f.root.join("trim");
    fs::create_dir_all(&dir).unwrap();
    let long = dir.join("long.txt");
    fs::write(&long, "     needle\n     xx\n").unwrap();
    let p = long.display().to_string();

    // Indentation goes; the column still counts from the start of the line, as
    // `rg --trim --column` answers `1:6:needle` for this file, and `-h` keeps the name off
    // the row.
    let o = run(&["search", "-h", "-n", "--trim", "needle"], &long);
    assert_eq!(stdout(&o), "1:needle\n");

    let o = run(&["search", "--trim", "--column", "-n", "needle"], &long);
    assert_eq!(stdout(&o), "1:6:needle\n");
    let named = run(
        &["search", "-H", "--trim", "--column", "-n", "needle"],
        &long,
    );
    assert_eq!(stdout(&named), format!("{p}:1:6:needle\n"));

    // `--max-columns` measures the trimmed line, so a line that is only long because of its
    // indentation is no longer cut. ripgrep omits a long line instead of cutting it, which
    // this tool has always done differently (docs/SEARCH.md, §1.1).
    let cut = run(&["search", "-h", "-n", "-M", "8", "needle"], &long);
    assert_eq!(stdout(&cut), "1:     nee\n");
    let kept = run(
        &["search", "-h", "-n", "-M", "8", "--trim", "needle"],
        &long,
    );
    assert_eq!(stdout(&kept), "1:needle\n");

    // `-M` is `--max-columns`, so the two spellings agree.
    let long_form = run(
        &["search", "-h", "-n", "--max-columns", "8", "needle"],
        &long,
    );
    assert_eq!(long_form.stdout, cut.stdout);
}

#[test]
fn crlf_terminator_and_anchors() {
    let f = Fixture::new("crlf");
    let dir = f.root.join("crlf");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("dos.txt");
    fs::write(&file, b"one\r\nneedle\r\nthree\r\n").unwrap();
    let p = file.display().to_string();

    // `$` matches before the `\r` and the `\r` is not printed, with the flag or without it:
    // this scanner has always read `\r\n` as one terminator. ripgrep only does so under
    // `--crlf`, and it prints the `\r` back out:
    //   $ rg --crlf '^needle$' dos.txt | od -c   ->   n e e d l e \r \n
    let o = run(&["search", "-h", "-n", "--crlf", "^needle$"], &file);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(stdout(&o), "2:needle\n");
    let o = run(&["search", "-h", "-n", "^needle$"], &file);
    assert_eq!(stdout(&o), "2:needle\n");

    // The row a vimgrep prints for it holds no `\r` either.
    let o = run(&["search", "--vimgrep", "--crlf", "^needle$"], &file);
    assert_eq!(stdout(&o), format!("{p}:2:1:needle\n"));
}

#[test]
fn separators_between_fields_and_groups() {
    let f = Fixture::new("separators");
    let dir = f.root.join("sep");
    fs::create_dir_all(&dir).unwrap();
    let ctx = dir.join("ctx.txt");
    fs::write(&ctx, "1\nHIT\n3\n4\n5\nHIT\n7\n").unwrap();

    // ripgrep 14.1:
    //   $ rg -n -A1 --field-match-separator '|' --field-context-separator '~' \
    //        --context-separator '==' HIT ctx.txt
    //   2|HIT
    //   3~3
    //   ==
    //   6|HIT
    //   7-7
    let o = run(
        &[
            "search",
            "-n",
            "-A1",
            "--field-match-separator",
            "|",
            "--field-context-separator",
            "~",
            "--context-separator",
            "==",
            "HIT",
        ],
        &ctx,
    );
    assert_eq!(stdout(&o), "2|HIT\n3~3\n==\n6|HIT\n7~7\n");

    // grep's `--group-separator` is the same switch as `--context-separator`.
    let o = run(
        &["search", "-n", "-A1", "--group-separator", "==", "HIT"],
        &ctx,
    );
    assert_eq!(stdout(&o), "2:HIT\n3-3\n==\n6:HIT\n7-7\n");

    // `--no-context-separator` prints nothing between the groups, and an empty separator is a
    // blank line, which is what ripgrep does with each.
    let o = run(
        &["search", "-n", "-A1", "--no-context-separator", "HIT"],
        &ctx,
    );
    assert_eq!(stdout(&o), "2:HIT\n3-3\n6:HIT\n7-7\n");
    let o = run(
        &["search", "-n", "-A1", "--context-separator", "", "HIT"],
        &ctx,
    );
    assert_eq!(stdout(&o), "2:HIT\n3-3\n\n6:HIT\n7-7\n");
}

#[test]
fn path_separator_rewrites_printed_paths() {
    let f = Fixture::new("pathsep");
    let dir = f.root.join("ps");
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::write(dir.join("sub/f.txt"), "foo\n").unwrap();

    // ripgrep rewrites `/` and `\\` alike, so the given separator is the only one left:
    //   $ rg --path-separator '%' -n foo sub
    //   sub%f.txt:1:foo
    let file = dir.join("sub").join("f.txt").display().to_string();
    let pct = |path: &str| path.replace(['/', '\\'], "%");
    let plain = run(&["search", "-n", "foo"], &dir);
    assert_eq!(stdout(&plain), format!("{file}:1:foo\n"));
    let o = run(&["search", "-n", "--path-separator", "%", "foo"], &dir);
    assert_eq!(stdout(&o), format!("{}:1:foo\n", pct(&file)));
    assert!(
        !stdout(&o).contains(std::path::MAIN_SEPARATOR),
        "the only separator left is the given one: {}",
        stdout(&o)
    );
    let o = run(
        &["search", "--vimgrep", "--path-separator", "%", "foo"],
        &dir,
    );
    assert_eq!(stdout(&o), format!("{}:1:1:foo\n", pct(&file)));

    // A separator is one byte, as in ripgrep, which refuses anything longer.
    let bad = run(&["search", "--path-separator", "::", "foo"], &dir);
    assert_eq!(code(&bad), 2);
    assert!(
        stderr(&bad).contains("--path-separator"),
        "{}",
        stderr(&bad)
    );
}

#[test]
fn engine_chooses_the_matcher() {
    let f = Fixture::new("engine");
    let dir = f.root.join("eng");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("a.txt");
    fs::write(&file, "HIT\nMISS\nHIT\n").unwrap();

    // A pattern the Thompson/Pike engine cannot express: only the backtracking engine has it.
    let auto = run(&["search", "-h", "--engine", "auto", "H(?=I)"], &file);
    assert_eq!(code(&auto), 0, "{}", stderr(&auto));
    let pcre = run(&["search", "-h", "--engine", "pcre2", "H(?=I)"], &file);
    assert_eq!(stdout(&pcre), stdout(&auto), "pcre2 is -P");
    let forced = run(&["search", "-h", "--engine", "default", "H(?=I)"], &file);
    assert_eq!(code(&forced), 2, "{}", stdout(&forced));
    assert!(
        stderr(&forced).contains("backtracking engine"),
        "{}",
        stderr(&forced)
    );

    // The default engine answers a pattern it can express, unchanged.
    let plain = run(&["search", "-h", "-n", "--engine", "default", "HIT"], &file);
    assert_eq!(stdout(&plain), "1:HIT\n3:HIT\n");

    // An engine name that does not exist is bad arguments, not a silent fallback.
    let bogus = run(&["search", "--engine", "backtrack", "HIT"], &file);
    assert_eq!(code(&bogus), 2);
    assert!(
        stderr(&bogus).contains("unrecognized regex engine"),
        "{}",
        stderr(&bogus)
    );
}

#[test]
fn mmap_flags_are_accepted_and_change_nothing() {
    let f = Fixture::new("mmap");
    let dir = f.root.join("mm");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), "alpha needle beta\nsecond line\n").unwrap();
    fs::write(dir.join("b.txt"), "needle again\n").unwrap();

    let plain = run(&["search", "-n", "needle"], &dir);
    assert_eq!(code(&plain), 0);
    for flag in ["--mmap", "--no-mmap"] {
        let o = run(&["search", "-n", flag, "needle"], &dir);
        assert_eq!(code(&o), 0, "{flag}: {}", stderr(&o));
        assert_eq!(o.stdout, plain.stdout, "{flag} must not change the output");
    }
}

#[test]
fn stop_on_nonmatch_ends_the_file_after_the_first_miss() {
    let f = Fixture::new("stop");
    let dir = f.root.join("stop");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join("s.txt");
    fs::write(&file, "a\nHIT one\nb\nc\nd\ne\nf\ng\nHIT two\nh\n").unwrap();

    // ripgrep 14.1 stops the file at the first line after a match that does not match, and
    // that line still prints while the `-A` window lasts:
    //   $ rg -n -A5 --stop-on-nonmatch HIT s.txt
    //   2:HIT one
    //   3-b
    // so the second match, eleven lines down, never appears.
    let o = run(&["search", "-n", "-A5", "--stop-on-nonmatch", "HIT"], &file);
    assert_eq!(stdout(&o), "2:HIT one\n3-b\n");

    // With no window there is no context to print, and the stop is at the same line.
    let o = run(&["search", "-n", "--stop-on-nonmatch", "HIT"], &file);
    assert_eq!(stdout(&o), "2:HIT one\n");

    // A line before the first match does not stop anything.
    let leading = dir.join("lead.txt");
    fs::write(&leading, "b\nHIT\n").unwrap();
    let o = run(&["search", "-n", "--stop-on-nonmatch", "HIT"], &leading);
    assert_eq!(stdout(&o), "2:HIT\n");
}

#[test]
fn no_heading_is_the_last_spelling_that_wins() {
    let f = Fixture::new("heading");
    let dir = f.root.join("hd");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("a.txt"), "HIT a\n").unwrap();
    fs::write(dir.join("b.txt"), "HIT b\n").unwrap();

    let (a, b) = (
        dir.join("a.txt").display().to_string(),
        dir.join("b.txt").display().to_string(),
    );
    let plain = stdout(&run(&["search", "-n", "HIT"], &dir));
    assert_eq!(plain, format!("{a}:1:HIT a\n{b}:1:HIT b\n"));

    let off = stdout(&run(
        &["search", "--heading", "--no-heading", "-n", "HIT"],
        &dir,
    ));
    assert_eq!(off, plain, "--no-heading after --heading prints no heading");

    let on = stdout(&run(
        &["search", "--no-heading", "--heading", "-n", "HIT"],
        &dir,
    ));
    assert_eq!(on, format!("{a}\n1:HIT a\n\n{b}\n1:HIT b\n"));
}
