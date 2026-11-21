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

fn run_with_env(args: &[&str], root: &Path, env: &[(&str, &str)]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_dusage"));
    c.args(args).arg(root);
    for (k, v) in env {
        c.env(k, v);
    }
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
fn a_pattern_that_burns_the_step_budget_exits_2() {
    let f = Fixture::new("budget");
    let hay = format!("{}b\n", "a".repeat(40));
    fs::write(f.root.join("slow.txt"), hay).unwrap();
    // `rg -P` exits 2 here as well: an unknown answer is not "no match".
    let o = run(&["search", "-P", "-g", "slow.txt", "(a+)+$"], &f.root);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    assert!(stdout(&o).is_empty(), "{}", stdout(&o));
    assert!(stderr(&o).contains("step budget"), "{}", stderr(&o));
    // The linear engine answers the same pattern, so the exit code stays grep's 1.
    let ok = run(&["search", "-g", "slow.txt", "(a+)+$"], &f.root);
    assert_eq!(code(&ok), 1, "{}", stderr(&ok));
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

#[cfg(unix)]
#[test]
fn a_fifo_under_a_walked_root_is_skipped_not_opened() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let f = Fixture::new("fifo");
    let fifo = f.root.join("a-pipe");
    // `mkfifo` is POSIX and in coreutils; without it there is nothing to test.
    let made = Command::new("mkfifo")
        .arg(&fifo)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !made {
        return;
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_dusage"))
        .args(["search", "needle"])
        .arg(&f.root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Opening a FIFO for reading blocks until a writer appears, so a regression here would
    // hang the whole search: wait a bounded time and kill it rather than wedging the suite.
    let start = Instant::now();
    let mut status = None;
    while start.elapsed() < Duration::from_secs(10) {
        if let Some(s) = child.try_wait().unwrap() {
            status = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        panic!("the search hung on a FIFO in the tree");
    };
    assert_eq!(
        status.code(),
        Some(0),
        "the file beside the pipe still matches"
    );
    let mut out = String::new();
    use std::io::Read;
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert!(out.contains("needle"), "{out}");
}

#[test]
fn quiet_answers_the_question_and_an_error_does_not_outrank_a_match() {
    let f = Fixture::new("quiet_exit");
    let missing = f.root.join("absent.txt");
    let missing = missing.to_str().unwrap();
    let ok = f.root.join("src/main.rs");
    let ok = ok.to_str().unwrap();
    // A found line is the answer `-q` asked for, so a path that could not be read does not
    // turn it into a failure — grep's rule, and the same for a path that is not there.
    let found = run(&["search", "-q", "needle", ok, missing], &f.root);
    assert_eq!(code(&found), 0, "{}", stderr(&found));
    // Without a match the error is the answer.
    let nothing = run(&["search", "-q", "definitely-absent", ok, missing], &f.root);
    assert_eq!(code(&nothing), 2, "{}", stderr(&nothing));
    // Every other mode still refuses a bad starting point, match or no match.
    let loud = run(&["search", "needle", ok, missing], &f.root);
    assert_eq!(code(&loud), 2, "{}", stderr(&loud));
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

/// A listing with no sort key streams as the walk finds it, and it comes out in exactly the
/// order `--sort path` prints: the order every listing has used. Nothing about what prints
/// is new — the same lines, one result at a time instead of a list held to the end.
#[test]
fn find_streams_the_sorted_listing() {
    let f = Fixture::new("stream");
    let dir = f.root.join("many");
    // Enough results that they used to be all held before the first line printed, and
    // named so that a tree order and a path order disagree: `alpha-b.txt` sorts before
    // everything inside `alpha-b/`, and `alpha/`'s own entries come after both.
    for name in ["alpha", "alpha-b", "beta", "delta", "zeta"] {
        fs::create_dir_all(dir.join(name)).unwrap();
        for n in 0..50 {
            fs::write(dir.join(name).join(format!("f{n:02}.txt")), "").unwrap();
        }
    }
    fs::write(dir.join("alpha-b.txt"), "").unwrap();
    fs::write(dir.join("beta.txt"), "").unwrap();

    let plain = stdout(&run(&["find", "*"], &dir));
    let sorted = stdout(&run(&["find", "*", "--sort", "path"], &dir));
    assert_eq!(plain, sorted, "a plain listing is the path-sorted one");

    let lines: Vec<&str> = plain.lines().collect();
    assert_eq!(lines.len(), 5 * 50 + 5 + 2, "{}", plain);
    let mut by_name = lines.clone();
    by_name.sort();
    assert_eq!(by_name, lines, "the stream is in path order");
    for line in &lines {
        assert!(
            dir.join(line).exists(),
            "a line is a path under the root: {line}"
        );
    }

    // `--json` collects the walk and sorts it before printing, so the two lists agreeing is
    // the set and the order agreeing at once.
    let json = stdout(&run(&["find", "*", "--json"], &dir));
    let rels: Vec<&str> = json
        .split("\"rel\":\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .collect();
    assert_eq!(rels, lines, "the streamed listing is the collected one");
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

    // `-F` makes every pattern in the file a literal. The file lives outside the tree, so a
    // literal pattern is not found in the pattern file itself.
    let wild = f.root.parent().unwrap().join("wild.txt");
    fs::write(&wild, "nee.le\n").unwrap();
    let o = run(&["search", "-f", wild.to_str().unwrap()], &f.root);
    assert_eq!(code(&o), 0, "a '.' is a wildcard here");
    let o = run(&["search", "-F", "-f", wild.to_str().unwrap()], &f.root);
    assert_eq!(code(&o), 1, "-F is a literal, so 'nee.le' never matches");

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

#[test]
fn an_explicit_ignore_file_applies_and_can_fold_case() {
    let f = Fixture::new("ignore-file");
    let extra = f.root.join("extra.ignore");
    fs::write(&extra, "README.md\n").unwrap();
    let base = run(&["search", "-i", "needle"], &f.root);
    assert!(stdout(&base).contains("README.md"), "{}", stdout(&base));
    let extra_s = extra.to_str().unwrap();
    let o = run(
        &["search", "-i", "needle", "--ignore-file", extra_s],
        &f.root,
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(!stdout(&o).contains("README.md"), "{}", stdout(&o));

    fs::write(&extra, "readme.md\n").unwrap();
    let exact = run(
        &["search", "-i", "needle", "--ignore-file", extra_s],
        &f.root,
    );
    assert!(stdout(&exact).contains("README.md"), "{}", stdout(&exact));
    let folded = run(
        &[
            "search",
            "-i",
            "needle",
            "--ignore-file",
            extra_s,
            "--ignore-file-case-insensitive",
        ],
        &f.root,
    );
    assert!(
        !stdout(&folded).contains("README.md"),
        "{}",
        stdout(&folded)
    );
}

#[test]
fn a_missing_ignore_file_is_misuse() {
    let f = Fixture::new("ignore-file-missing");
    let missing = f.root.join("no-such.ignore");
    let o = run(
        &[
            "search",
            "needle",
            "--ignore-file",
            missing.to_str().unwrap(),
        ],
        &f.root,
    );
    assert_eq!(code(&o), 2);
    assert!(stderr(&o).contains("no-such.ignore"), "{}", stderr(&o));
}

#[test]
fn the_global_excludes_file_is_read_and_can_be_switched_off() {
    let f = Fixture::new("global-excludes");
    let home = f.root.parent().unwrap().join("home");
    fs::create_dir_all(home.join(".config/git")).unwrap();
    fs::write(home.join(".config/git/ignore"), "tests/\n").unwrap();
    let home_s = home.to_str().unwrap();
    let config_s = home.join(".config").to_str().unwrap().to_string();
    // `XDG_CONFIG_HOME` wins over the home directory when it is set, and a CI runner sets
    // it, so the test points both at the same tree.
    let env = [
        ("HOME", home_s),
        ("USERPROFILE", home_s),
        ("XDG_CONFIG_HOME", config_s.as_str()),
    ];
    let o = run_with_env(&["search", "-n", "needle"], &f.root, &env);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(!stdout(&o).contains("it.rs"), "{}", stdout(&o));
    let o = run_with_env(
        &["search", "-n", "needle", "--no-ignore-global"],
        &f.root,
        &env,
    );
    assert!(
        stdout(&o).contains("it.rs:1:needle in a test"),
        "{}",
        stdout(&o)
    );
}

/// One plaintext and a compressed sibling of it, for each format `-z` understands. The
/// pairs are checked in by the decoder tests, so the search path is tested against streams
/// a real encoder produced rather than against anything this test could build.
fn fixture_pairs() -> Vec<(&'static str, PathBuf, PathBuf)> {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    vec![
        (
            "bzip2",
            base.join("bz2/mixed_200k.bz2"),
            base.join("bz2/mixed_200k.txt"),
        ),
        (
            "lz4",
            base.join("lz4/mixed-100k.lz4"),
            base.join("lz4/mixed-100k.txt"),
        ),
        ("xz", base.join("xz/alpha.xz"), base.join("xz/alpha.txt")),
        (
            "zstd",
            base.join("zstd/text3.zst"),
            base.join("zstd/text.bin"),
        ),
        (
            "brotli",
            base.join("brotli/mixed-q11.br"),
            base.join("brotli/mixed.txt"),
        ),
    ]
}

#[test]
fn search_zip_expands_every_format_it_claims() {
    for (name, packed, plain) in fixture_pairs() {
        assert!(packed.exists(), "{name}: {} is missing", packed.display());
        let text = String::from_utf8(fs::read(&plain).unwrap()).unwrap();
        // The needle is a whole line of the plaintext, so it can only be found inside the
        // expansion, never in the compressed bytes.
        let needle = text
            .lines()
            .map(str::trim)
            .find(|l| l.len() >= 12 && l.is_ascii())
            .unwrap_or_else(|| panic!("{name}: no usable line in {}", plain.display()));
        let o = run(&["search", "-z", "-F", "--no-filename", needle], &packed);
        assert_eq!(
            code(&o),
            0,
            "{name}: -z missed {needle:?} in {}: {}",
            packed.display(),
            stderr(&o)
        );
        assert!(
            stdout(&o).contains(needle),
            "{name}: {} {}",
            stdout(&o),
            stderr(&o)
        );
        // Without -z the same file is searched as it lies, and the line is not in it.
        let plain_run = run(&["search", "-F", "--no-filename", needle], &packed);
        assert_eq!(code(&plain_run), 1, "{name}: searched compressed bytes");
    }
}

#[test]
fn search_zip_reports_a_corrupt_stream() {
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let corrupt = base.join("bz2/corrupt.bz2");
    let o = run(&["search", "-z", "-F", "needle"], &corrupt);
    // A stream that starts like bzip2 and is corrupt is an unreadable path: the run fails
    // with the aggregate diagnostic rather than answering from half an expansion.
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    assert!(stderr(&o).contains("could not be read"), "{}", stderr(&o));
}
// ---------------------------------------------------------------------------
// `dusage find <paths...> <expression...>` — find's own expression language
// ---------------------------------------------------------------------------

/// Run `dusage find <root> <expression...>`: a find expression comes after the starting
/// points, as it does in find itself.
fn find_at(root: &Path, args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_dusage"));
    c.arg("find").arg(root).args(args);
    c.output().unwrap()
}

/// The path `dusage find` prints for `rel` below an absolute `root`, so assertions do not
/// have to know the platform's separator.
fn shown(root: &Path, rel: &str) -> String {
    let mut path = root.to_path_buf();
    for part in rel.split('/') {
        path.push(part);
    }
    path.display().to_string()
}

/// A tree with one of everything an expression can ask about.
fn expr_tree(tag: &str) -> (Fixture, PathBuf) {
    let f = Fixture::new(tag);
    let dir = f.root.join("ex");
    fs::create_dir_all(dir.join("sub/deeper")).unwrap();
    fs::create_dir_all(dir.join("cache/inner")).unwrap();
    fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
    fs::write(dir.join("b.md"), "# doc\n").unwrap();
    fs::write(dir.join("sub/c.rs"), "fn c() {}\n").unwrap();
    fs::write(dir.join("sub/deeper/d.txt"), "x").unwrap();
    fs::write(dir.join("cache/inner/buried.rs"), "hidden deep\n").unwrap();
    (f, dir)
}

#[cfg(unix)]
#[test]
fn find_expression_samefile_matches_the_other_name_of_an_inode() {
    let (_f, dir) = expr_tree("expr_samefile");
    // A hardlink pair: same device and inode, so both names answer `-samefile`.
    fs::hard_link(dir.join("a.rs"), dir.join("linked.rs")).unwrap();
    let o = find_at(&dir, &["-samefile", dir.join("a.rs").to_str().unwrap()]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.contains(&shown(&dir, "a.rs")), "{out}");
    assert!(out.contains(&shown(&dir, "linked.rs")), "{out}");
    assert_eq!(out.lines().count(), 2, "{out}");
}

#[test]
fn find_expression_name_or_not_and_parentheses() {
    let (_f, dir) = expr_tree("expr_or");
    // `-o` loosens, so both names come out.
    let both = find_at(&dir, &["-name", "*.rs", "-o", "-name", "*.md"]);
    assert_eq!(code(&both), 0, "{}", stderr(&both));
    let out = stdout(&both);
    assert!(out.contains(&shown(&dir, "a.rs")), "{out}");
    assert!(out.contains(&shown(&dir, "b.md")), "{out}");
    assert!(!out.contains("d.txt"), "{out}");

    // `!` negates one test.
    let not = stdout(&find_at(&dir, &["!", "-name", "*.rs", "-type", "f"]));
    assert!(not.contains("b.md"), "{not}");
    assert!(not.contains("d.txt"), "{not}");
    assert!(!not.contains("a.rs"), "{not}");

    // Parentheses group, so the `-o` stays inside the group and `-type f` applies to both.
    let grouped = stdout(&find_at(
        &dir,
        &[
            "(", "-name", "*.md", "-o", "-name", "*.txt", ")", "-type", "f",
        ],
    ));
    assert!(grouped.contains("b.md"), "{grouped}");
    assert!(grouped.contains("d.txt"), "{grouped}");
    assert!(!grouped.contains("a.rs"), "{grouped}");
}

#[test]
fn find_expression_iname_folds_case() {
    let (_f, dir) = expr_tree("expr_iname");
    let plain = find_at(&dir, &["-name", "B.MD"]);
    assert_eq!(code(&plain), 1, "case matters without -iname");
    let folded = find_at(&dir, &["-iname", "B.MD"]);
    assert_eq!(code(&folded), 0, "{}", stderr(&folded));
    assert_eq!(stdout(&folded).trim(), shown(&dir, "b.md"));
}

#[test]
fn find_expression_type_selects_directories() {
    let (_f, dir) = expr_tree("expr_type");
    let out = stdout(&find_at(&dir, &["-type", "d"]));
    assert!(out.contains(&shown(&dir, "sub")), "{out}");
    assert!(out.contains(&shown(&dir, "cache")), "{out}");
    assert!(!out.contains("a.rs"), "{out}");
    // The starting point is an entry like any other, and it is a directory.
    assert!(out.lines().any(|l| l == dir.display().to_string()), "{out}");
    let links = find_at(&dir, &["-type", "l", "-name", "*"]);
    assert_eq!(code(&links), 1, "no links in the fixture");
}

#[test]
fn find_expression_maxdepth_and_mindepth_apply() {
    let (_f, dir) = expr_tree("expr_depth");
    let shallow = stdout(&find_at(&dir, &["-maxdepth", "1", "-name", "*"]));
    assert!(shallow.contains(&shown(&dir, "a.rs")), "{shallow}");
    assert!(!shallow.contains(&shown(&dir, "sub/c.rs")), "{shallow}");
    assert!(
        !shallow.contains(&shown(&dir, "sub/deeper/d.txt")),
        "{shallow}"
    );
    assert!(
        !shallow.lines().any(|l| l == shown(&dir, "ex")),
        "maxdepth 0 is the only level that hides the starting point: {shallow}"
    );

    let deep = stdout(&find_at(&dir, &["-mindepth", "2", "-type", "f"]));
    assert!(!deep.contains(&shown(&dir, "a.rs")), "{deep}");
    assert!(deep.contains(&shown(&dir, "sub/c.rs")), "{deep}");
    assert!(deep.contains(&shown(&dir, "sub/deeper/d.txt")), "{deep}");

    // The expression's option overrides the flag of the same name, which is applied first.
    let flag_and_expr = stdout(&find_at(
        &dir,
        &["--max-depth", "1", "-maxdepth", "2", "-type", "f"],
    ));
    assert!(
        flag_and_expr.contains(&shown(&dir, "sub/c.rs")),
        "{flag_and_expr}"
    );
    assert!(
        !flag_and_expr.contains(&shown(&dir, "sub/deeper/d.txt")),
        "{flag_and_expr}"
    );
}

#[test]
fn find_expression_size_counts_in_units() {
    let (_f, dir) = expr_tree("expr_size");
    fs::write(dir.join("big.bin"), vec![b'x'; 5000]).unwrap();
    // `-size` rounds up to the unit, so 5000 bytes is 5 blocks of 1k, or exactly 5000c.
    let k = stdout(&find_at(&dir, &["-size", "5k", "-type", "f"]));
    assert!(k.contains("big.bin"), "{k}");
    assert!(!k.contains("a.rs"), "{k}");
    let bytes = stdout(&find_at(&dir, &["-size", "+4000c", "-type", "f"]));
    assert!(bytes.contains("big.bin"), "{bytes}");
    assert!(!bytes.contains("a.rs"), "{bytes}");
    // `-empty` is the size-0 case, and it counts an empty directory too.
    let empty = find_at(&dir, &["-empty"]);
    assert_eq!(
        code(&empty),
        1,
        "nothing in this fixture is empty: {}",
        stdout(&empty)
    );
}

#[test]
fn find_expression_mtime_uses_whole_days() {
    let (_f, dir) = expr_tree("expr_mtime");
    let aged = dir.join("aged.txt");
    fs::write(&aged, "old").unwrap();
    let fresh = dir.join("fresh.txt");
    fs::write(&fresh, "new").unwrap();
    let three_days =
        std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 86_400 + 3600);
    fs::File::options()
        .write(true)
        .open(&aged)
        .unwrap()
        .set_modified(three_days)
        .unwrap();

    let older = stdout(&find_at(&dir, &["-mtime", "+1", "-type", "f"]));
    assert!(older.contains("aged.txt"), "{older}");
    assert!(!older.contains("fresh.txt"), "{older}");
    let newer = stdout(&find_at(&dir, &["-mtime", "-1", "-type", "f"]));
    assert!(newer.contains("fresh.txt"), "{newer}");
    assert!(!newer.contains("aged.txt"), "{newer}");
}

#[test]
fn find_expression_prune_stops_the_descent() {
    let (_f, dir) = expr_tree("expr_prune");
    // A pruned directory is still reported by the test that prunes it, but its contents are
    // never walked, so `buried.rs` cannot appear.
    let out = stdout(&find_at(
        &dir,
        &["-name", "cache", "-prune", "-o", "-name", "*.rs", "-print"],
    ));
    assert!(!out.contains("buried.rs"), "{out}");
    assert!(out.contains(&shown(&dir, "a.rs")), "{out}");
    assert!(out.contains(&shown(&dir, "sub/c.rs")), "{out}");

    // Under `-depth` prune has nothing to stop, exactly as in find.
    let depth = stdout(&find_at(
        &dir,
        &[
            "-name",
            "cache",
            "-prune",
            "-o",
            "-name",
            "buried.rs",
            "-print",
            "-depth",
        ],
    ));
    assert!(depth.contains("buried.rs"), "{depth}");
}

#[test]
fn find_expression_printf_renders_fields() {
    let (_f, dir) = expr_tree("expr_printf");
    let out = stdout(&find_at(&dir, &["-name", "a.rs", "-printf", "%p %s %y\n"]));
    assert_eq!(out.trim(), format!("{} 13 f", shown(&dir, "a.rs")), "{out}");
    // `%f` is the name alone and `%P` the path below the starting point.
    let rel = stdout(&find_at(&dir, &["-name", "c.rs", "-printf", "%f|%P|%d\n"]));
    assert_eq!(
        rel.trim(),
        format!("c.rs|sub{}c.rs|2", std::path::MAIN_SEPARATOR),
        "{rel}"
    );
}

#[test]
fn find_expression_print0_terminates_with_nul() {
    let (_f, dir) = expr_tree("expr_print0");
    let o = find_at(&dir, &["-name", "*.md", "-print0"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(o.stdout.last(), Some(&0), "the last byte is the terminator");
    let text = String::from_utf8_lossy(&o.stdout);
    assert_eq!(text.split('\0').filter(|p| !p.is_empty()).count(), 1);
    assert!(text.contains(&shown(&dir, "b.md")), "{text}");
}

#[test]
fn find_expression_ls_prints_find_columns() {
    let (_f, dir) = expr_tree("expr_ls");
    let out = stdout(&find_at(&dir, &["-name", "a.rs", "-ls"]));
    let line = out.lines().next().unwrap_or_default();
    let fields: Vec<&str> = line.split_whitespace().collect();
    assert!(fields.len() >= 8, "find's column layout: {line}");
    assert!(
        line.ends_with(&shown(&dir, "a.rs")),
        "the path comes last: {line}"
    );
    let mode = fields[2];
    assert_eq!(mode.len(), 10, "a mode column: {line}");
    assert!(
        mode.chars().all(|c| "rwxst-?".contains(c)),
        "a mode string: {line}"
    );
}

#[test]
fn find_expression_exec_is_refused_without_the_flag() {
    let (_f, dir) = expr_tree("expr_exec_no");
    let o = find_at(&dir, &["-name", "a.rs", "-exec", "echo", "ran", "{}", ";"]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    assert!(stderr(&o).contains("--allow-exec"), "{}", stderr(&o));
    assert!(stdout(&o).is_empty(), "nothing ran: {}", stdout(&o));

    // `-delete` is refused whatever the flag says.
    let del = find_at(&dir, &["-name", "a.rs", "-delete", "--allow-exec"]);
    assert_eq!(code(&del), 2, "{}", stderr(&del));
    assert!(stderr(&del).contains("-delete"), "{}", stderr(&del));
}

#[cfg(unix)]
#[test]
fn find_expression_exec_runs_with_the_flag() {
    let (_f, dir) = expr_tree("expr_exec_yes");
    let one = find_at(
        &dir,
        &[
            "-name",
            "a.rs",
            "--allow-exec",
            "-exec",
            "echo",
            "RAN",
            "{}",
            ";",
        ],
    );
    assert_eq!(code(&one), 0, "{}", stderr(&one));
    assert!(
        stdout(&one).contains(&format!("RAN {}", shown(&dir, "a.rs"))),
        "{}",
        stdout(&one)
    );
    // A trailing `{} +` batches every match into one invocation, run at the end.
    let batched = find_at(
        &dir,
        &[
            "-name",
            "*.rs",
            "--allow-exec",
            "-exec",
            "echo",
            "BATCH",
            "{}",
            "+",
        ],
    );
    assert_eq!(code(&batched), 0, "{}", stderr(&batched));
    let out = stdout(&batched);
    assert_eq!(out.lines().count(), 1, "one invocation: {out}");
    assert!(out.contains("BATCH"), "{out}");
    assert!(out.contains("a.rs") && out.contains("buried.rs"), "{out}");

    // The command's exit status is the predicate's value, so a failing command is not a match.
    let failing = find_at(
        &dir,
        &["-name", "a.rs", "--allow-exec", "-exec", "false", "{}", ";"],
    );
    assert_eq!(code(&failing), 1, "{}", stderr(&failing));
}

#[test]
fn find_expression_quit_stops_the_run() {
    let (_f, dir) = expr_tree("expr_quit");
    let all = find_at(&dir, &["-name", "*.rs", "-print"]);
    assert!(stdout(&all).lines().count() > 1, "{}", stdout(&all));
    let one = find_at(&dir, &["-name", "*.rs", "-print", "-quit"]);
    assert_eq!(code(&one), 0, "{}", stderr(&one));
    assert_eq!(stdout(&one).lines().count(), 1, "{}", stdout(&one));

    // `-quit` on its own prints nothing, and being true for one entry still exits 0.
    let quiet = find_at(&dir, &["-name", "*.rs", "-quit"]);
    assert_eq!(code(&quiet), 0, "{}", stderr(&quiet));
    assert!(stdout(&quiet).is_empty(), "{}", stdout(&quiet));
}

#[test]
fn find_expression_exit_codes_and_errors() {
    let (_f, dir) = expr_tree("expr_exit");
    let none = find_at(&dir, &["-name", "*.nope"]);
    assert_eq!(code(&none), 1, "nothing matched");
    let bad = find_at(&dir, &["-name", "*.rs", "-frobnicate"]);
    assert_eq!(code(&bad), 2, "{}", stderr(&bad));
    assert!(stderr(&bad).contains("frobnicate"), "{}", stderr(&bad));
    // `-json` has no shape for arbitrary actions.
    let json = find_at(&dir, &["-name", "*.rs", "--json"]);
    assert_eq!(code(&json), 2, "{}", stderr(&json));
}

#[test]
fn find_glob_form_still_works_unchanged() {
    // The positional glob still means what it meant, with no expression token in sight.
    let (_f, dir) = expr_tree("expr_legacy");
    // `run` appends the root, which is where the glob form wants it: `<glob> [paths...]`.
    let glob = run(&["find", "*.rs"], &dir);
    assert_eq!(code(&glob), 0, "{}", stderr(&glob));
    let out = stdout(&glob);
    assert!(out.contains("a.rs"), "{out}");
    assert!(out.contains("buried.rs"), "{out}");
    assert!(!out.contains("b.md"), "{out}");
    let o = run(&["find", "*.md"], &dir);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert_eq!(stdout(&o).trim(), "b.md");
}

#[test]
fn find_expression_path_and_regex_tests() {
    let (_f, dir) = expr_tree("expr_path");
    // Neither pattern spells a separator, so the test reads the same on either platform.
    let path = stdout(&find_at(&dir, &["-path", "*sub*c.rs"]));
    assert_eq!(path.trim(), shown(&dir, "sub/c.rs"), "{path}");
    let re = stdout(&find_at(&dir, &["-regex", ".*[ab]\\.(rs|md)"]));
    assert!(re.contains("a.rs"), "{re}");
    assert!(re.contains("b.md"), "{re}");
    assert!(!re.contains("c.rs"), "{re}");
}

#[test]
fn find_expression_options_are_accepted() {
    let (_f, dir) = expr_tree("expr_opts");
    // `-xdev` keeps the walk on the starting device; the fixture is on it, so nothing drops.
    let xdev = find_at(&dir, &["-xdev", "-maxdepth", "1", "-name", "a.rs"]);
    assert_eq!(code(&xdev), 0, "{}", stderr(&xdev));
    assert!(stdout(&xdev).contains("a.rs"), "{}", stdout(&xdev));
    // `--files` and `--dirs` still narrow the walk, and `--hidden` still widens it.
    fs::write(dir.join(".dot.rs"), "dot").unwrap();
    fs::create_dir_all(dir.join("dir.rs")).unwrap();
    let files = find_at(&dir, &["--files", "-name", "*.rs"]);
    let out = stdout(&files);
    assert!(!out.contains(".dot.rs"), "{out}");
    assert!(
        !out.contains("dir.rs"),
        "--files excludes directories: {out}"
    );
    let dirs = find_at(&dir, &["--dirs", "-name", "*.rs"]);
    assert!(stdout(&dirs).contains("dir.rs"), "{}", stdout(&dirs));
    let shown_hidden = find_at(&dir, &["--hidden", "--files", "-name", "*.rs"]);
    assert!(
        stdout(&shown_hidden).contains(".dot.rs"),
        "{}",
        stdout(&shown_hidden)
    );
    // `--empty` becomes the `-empty` test, which costs the same `read_dir` per directory.
    let empties = find_at(&dir, &["--empty", "-name", "*"]);
    assert!(stdout(&empties).contains("dir.rs"), "{}", stdout(&empties));

    // A flag whose meaning is the glob itself is refused rather than ignored.
    let refused = find_at(&dir, &["--size", "+1M", "-name", "*.rs"]);
    assert_eq!(code(&refused), 2, "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("-size"),
        "the refusal names the expression spelling: {}",
        stderr(&refused)
    );
}

#[test]
fn find_expression_empty_counts_an_empty_directory() {
    let (f, _dir) = expr_tree("expr_empty");
    let dir = f.root.join("emp");
    fs::create_dir_all(dir.join("hollow")).unwrap();
    fs::create_dir_all(dir.join("full")).unwrap();
    fs::write(dir.join("zero.txt"), "").unwrap();
    fs::write(dir.join("full/f.txt"), "x").unwrap();
    let out = stdout(&find_at(&dir, &["-empty"]));
    assert!(out.contains("hollow"), "{out}");
    assert!(out.contains("zero.txt"), "{out}");
    assert!(!out.contains("full"), "{out}");
    // With `-type f` only the empty file is left, as `-empty -type f` in find.
    let files = stdout(&find_at(&dir, &["-empty", "-type", "f"]));
    assert_eq!(files.trim(), shown(&dir, "zero.txt"));
}

#[test]
fn find_expression_follow_accepts_its_setting() {
    let (_f, dir) = expr_tree("expr_follow");
    // `-follow` is a global option of the expression, so it may sit among the tests.
    let o = find_at(&dir, &["-type", "d", "-follow", "-maxdepth", "1"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stdout(&o).contains("sub"), "{}", stdout(&o));
}

#[test]
fn find_expression_depth_reports_contents_first() {
    let (_f, dir) = expr_tree("expr_depth_order");
    let out = stdout(&find_at(&dir, &["-maxdepth", "1", "-print", "-depth"]));
    let lines: Vec<String> = out.lines().map(str::to_string).collect();
    let root = dir.display().to_string();
    assert_eq!(
        lines.last().map(String::as_str),
        Some(root.as_str()),
        "the starting point comes last: {out}"
    );
    assert!(lines.contains(&shown(&dir, "a.rs")), "{out}");
}

#[cfg(unix)]
#[test]
fn find_expression_reports_symlinks_without_following_them() {
    let (_f, dir) = expr_tree("expr_link");
    let link = dir.join("link.rs");
    if std::os::unix::fs::symlink("a.rs", &link).is_err() {
        eprintln!("skipped: this filesystem does not allow symlinks");
        return;
    }
    let links = stdout(&find_at(&dir, &["-type", "l"]));
    assert!(links.contains("link.rs"), "{links}");
    let target = stdout(&find_at(&dir, &["-name", "link.rs", "-printf", "%l %y\n"]));
    assert_eq!(target.trim(), "a.rs l", "{target}");
    // `-xtype` asks about the target without following the link, which is find's meaning.
    let xtype = stdout(&find_at(&dir, &["-name", "link.rs", "-xtype", "f"]));
    assert!(xtype.contains("link.rs"), "{xtype}");
}

#[cfg(unix)]
#[test]
fn find_expression_execdir_runs_in_the_entry_directory() {
    let (_f, dir) = expr_tree("expr_execdir");
    // `-execdir` runs in the entry's own directory and names it `./name`, so `pwd` prints
    // the directory twice (once per directory) and `{}` is a bare name.
    let o = find_at(
        &dir,
        &[
            "-name",
            "*.rs",
            "--allow-exec",
            "-execdir",
            "echo",
            "E",
            "{}",
            ";",
        ],
    );
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = stdout(&o);
    assert!(out.lines().all(|l| l.starts_with("E ./")), "{out}");
    assert!(!out.contains("sub"), "the path is a bare name: {out}");
    assert_eq!(out.lines().count(), 3, "{out}");
}

/// A cluster of short flags has to mean exactly what the same flags spelled apart mean, in the
/// forms ripgrep accepts: `-in`, `-inA2`, `-nm1`, and a value flag ending the cluster.
#[test]
fn short_flags_bundle_the_way_ripgrep_bundles_them() {
    let f = Fixture::new("bundle");
    fs::write(
        f.root.join("case.txt"),
        "needle\nsecond needle\nNEEDLE upper\nthird\nfourth\n",
    )
    .unwrap();
    for (bundled, separate) in [
        (["-in"].as_slice(), ["-i", "-n"].as_slice()),
        (["-inA2"].as_slice(), ["-i", "-n", "-A", "2"].as_slice()),
        (["-nm1"].as_slice(), ["-n", "-m", "1"].as_slice()),
        (["-inm1"].as_slice(), ["-i", "-n", "-m", "1"].as_slice()),
        (["-inB1"].as_slice(), ["-i", "-n", "-B", "1"].as_slice()),
        (["-Fi"].as_slice(), ["-F", "-i"].as_slice()),
    ] {
        let mut one = vec!["search"];
        one.extend(bundled);
        one.push("needle");
        let mut two = vec!["search"];
        two.extend(separate);
        two.push("needle");
        let got = run(&one, &f.root);
        let want = run(&two, &f.root);
        assert_eq!(
            (code(&got), stdout(&got)),
            (code(&want), stdout(&want)),
            "{bundled:?} does not mean {separate:?}: {}",
            stderr(&got)
        );
        assert_eq!(code(&got), 0, "{bundled:?} found nothing");
    }
    // The flag at the end of a cluster was applied, not silently dropped.
    let out = stdout(&run(&["search", "-in", "needle"], &f.root));
    assert!(out.contains("case.txt:1:needle"), "{out}");
    assert!(out.contains("case.txt:3:NEEDLE upper"), "{out}");
    // A value flag that ends the cluster takes the next argument as its value.
    let tail = run(&["search", "-inA", "2", "needle"], &f.root);
    let spaced = run(&["search", "-i", "-n", "-A", "2", "needle"], &f.root);
    assert_eq!(stdout(&tail), stdout(&spaced));
    // A character that is not one of our flags is an error naming the whole cluster.
    let o = run(&["search", "-inQ", "needle"], &f.root);
    assert_eq!(code(&o), 2);
    assert!(stderr(&o).contains("-inQ"), "{}", stderr(&o));
}

/// The option names in the `find` help's OPTIONS block: the flag column of each line, with
/// aliases split apart and `--color[=WHEN]` reduced to its name.
fn find_help_flags(help: &str) -> Vec<String> {
    let options = help
        .split_once("OPTIONS:")
        .map(|(_, rest)| rest)
        .unwrap_or("");
    let mut flags = Vec::new();
    for line in options.lines() {
        let Some((column, _)) = line.trim_start().split_once("  ") else {
            continue;
        };
        for word in column.split_whitespace() {
            if let Some(name) = word.strip_prefix('-') {
                flags.push(format!("-{}", name.trim_end_matches(',')));
            }
        }
    }
    flags
}

/// Every flag `find --help` names is one the parser accepts. The list comes out of the help
/// text, so a flag added to one without the other fails here.
#[test]
fn every_flag_the_find_help_names_is_accepted() {
    let o = Command::new(env!("CARGO_BIN_EXE_dusage"))
        .args(["find", "--help"])
        .output()
        .unwrap();
    let help = stdout(&o);
    let flags = find_help_flags(&help);
    assert!(flags.len() > 20, "the help parser found {flags:?}");
    for flag in &flags {
        // A value flag eats the dummy and a boolean one leaves it as a starting point, so
        // either way the parser gets a well-formed command line, and `--help` answers before
        // any of the values are used.
        let o = Command::new(env!("CARGO_BIN_EXE_dusage"))
            .args(["find", flag, "dummy", "--help"])
            .output()
            .unwrap();
        assert_eq!(
            code(&o),
            0,
            "{flag} is in the find help but the parser refuses it: {}",
            stderr(&o)
        );
        assert!(!stdout(&o).is_empty(), "{flag} printed nothing");
    }
    // find matches paths, not contents, so the content-search flags are gone from both.
    for gone in [
        "--pre",
        "--pre-glob",
        "--binary-files",
        "-I",
        "-d",
        "--directories",
    ] {
        assert!(
            !flags.iter().any(|f| f == gone),
            "{gone} is still in the find help"
        );
        let o = Command::new(env!("CARGO_BIN_EXE_dusage"))
            .args(["find", ".", gone, "x"])
            .output()
            .unwrap();
        assert_eq!(code(&o), 2, "{gone} should be refused by find, not ignored");
    }
    // The glob-narrowing spellings the help names do narrow a path search.
    let (_f, dir) = expr_tree("find_glob_flags");
    let o = run(&["find", "--include", "*.rs", "*"], &dir);
    let out = stdout(&o);
    assert!(out.contains("a.rs"), "{out}");
    assert!(
        !out.contains("b.md"),
        "--include '*.rs' kept a markdown file: {out}"
    );
    let o = run(&["find", "--exclude", "*.md", "*"], &dir);
    assert!(!stdout(&o).contains("b.md"), "{}", stdout(&o));
    let o = run(&["find", "--exclude-dir", "sub", "*"], &dir);
    let out = stdout(&o);
    assert!(
        !out.contains("c.rs"),
        "--exclude-dir sub kept sub/c.rs: {out}"
    );
    let o = run(&["find", "--iglob", "*.TXT", "*"], &dir);
    assert!(stdout(&o).contains("d.txt"), "{}", stdout(&o));
    // They carry into an expression walk too, which the help promises.
    let o = find_at(&dir, &["-name", "*", "--exclude-dir", "cache"]);
    let out = stdout(&o);
    assert!(!out.contains("buried.rs"), "{out}");
    assert!(out.contains("a.rs"), "{out}");
}

/// `-F` means "this text is literal" however the pattern arrives: positionally, through `-e`,
/// through `-f`, or several of them at once.
#[test]
fn fixed_strings_is_literal_for_every_pattern_source() {
    let f = Fixture::new("fixedpatterns");
    let hay = f.root.join("dots.txt");
    fs::write(&hay, "a.b\naxb\nA.B\na+b\n(x)\n").unwrap();
    let pats = f.root.parent().unwrap().join("pats.txt");
    fs::write(&pats, "a.b\n").unwrap();

    let found = |args: &[&str]| -> String { stdout(&run(args, &f.root)) };
    // The reported defect: `-e` escaped the pattern and the literal matcher searched for the
    // escape itself.
    for args in [
        ["search", "-F", "-e", "a.b"].as_slice(),
        ["search", "-F", "a.b"].as_slice(),
        ["search", "-F", "-f", pats.to_str().unwrap()].as_slice(),
        ["search", "-F", "-e", "a.b", "-f", pats.to_str().unwrap()].as_slice(),
        ["search", "-F", "-e", "a.b", "-e", "a+b"].as_slice(),
        ["search", "-F", "--line-regexp", "a.b"].as_slice(),
    ] {
        let out = found(args);
        assert!(
            out.contains("a.b"),
            "{args:?} lost the literal match: {out}"
        );
        assert!(
            !out.contains("axb"),
            "{args:?} let '.' match any byte: {out}"
        );
    }
    // Metacharacters stay themselves: `a+b` is not a repetition and `(x)` is not a group.
    let out = found(&["search", "-F", "-e", "a+b"]);
    assert_eq!(out.lines().count(), 1, "{out}");
    assert!(out.trim_end().ends_with(":a+b"), "{out}");
    let out = found(&["search", "-F", "-e", "(x)"]);
    assert_eq!(out.lines().count(), 1, "{out}");
    assert!(out.trim_end().ends_with(":(x)"), "{out}");
    // Without `-F` the same pattern is still a regex, so the flag is what decides.
    let out = found(&["search", "-e", "a.b"]);
    assert_eq!(out.lines().count(), 3, "'.' matches any byte here: {out}");
    // Case folding composes with it, and `-F` still respects `-i`.
    let out = found(&["search", "-F", "-i", "-e", "a.b"]);
    assert_eq!(out.lines().count(), 2, "{out}");
}

#[test]
fn find_ls_prints_finds_columns_and_a_link_target() {
    let (_f, dir) = expr_tree("expr_ls");
    let target = dir.join("b.md");
    let link = dir.join("link.md");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&target, &link).unwrap();

    let out = stdout(&find_at(&dir, &["-name", "a.rs", "-ls"]));
    let line = out.lines().next().unwrap_or_default();
    let fields: Vec<&str> = line.split_whitespace().collect();
    // inode, blocks, mode, links, user, group, size, month, day, time, path.
    assert!(fields.len() >= 11, "find's column layout: {line}");
    assert_eq!(fields[2].len(), 10, "a mode column: {line}");
    assert!(
        fields[7..10].iter().all(|f| !f.is_empty()),
        "a date split over three columns: {line}"
    );
    assert!(line.ends_with(&shown(&dir, "a.rs")), "{line}");

    let linked = stdout(&find_at(&dir, &["-name", "link.md", "-ls"]));
    // The target's spelling belongs to the platform (Windows can hand back a verbatim
    // prefix), so the assertion is on the field and the name it ends with.
    let (_, target) = linked
        .rsplit_once(" -> ")
        .unwrap_or_else(|| panic!("a symlink names what it points at: {linked}"));
    assert!(
        target.trim_end().ends_with("b.md"),
        "the target is the file it points at: {linked}"
    );
    // Windows exposes no POSIX permission bits, so the mode column there is `l---------`;
    // what both platforms have to say is that it is ten characters and starts with `l`.
    let mode = linked.split_whitespace().nth(2).unwrap_or_default();
    assert_eq!(mode.len(), 10, "a mode column: {linked}");
    assert!(
        mode.starts_with('l'),
        "a symlink's mode starts with l: {linked}"
    );
    // A symlink's permission bits are the filesystem's convention, not the format's: Linux
    // stores 0777 and macOS stores 0755, so asserting `lrwxrwxrwx` was asserting ext4. What
    // the format guarantees is ten characters beginning with `l` and nine permission
    // characters that mirror what `lstat` reports for this link.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let md = fs::symlink_metadata(&link).unwrap();
        let expected = format!("l{}", rwx(md.permissions().mode() & 0o777));
        assert_eq!(mode, expected, "mirrors lstat: {linked}");
    }
}

/// The nine `rwx` characters of a permission mask, so a test can compare a printed column
/// with what the filesystem reports instead of with one platform's constant.
#[cfg(unix)]
fn rwx(perm: u32) -> String {
    let c = |bit: u32, set: char| if perm & bit != 0 { set } else { '-' };
    [
        c(0o400, 'r'),
        c(0o200, 'w'),
        c(0o100, 'x'),
        c(0o040, 'r'),
        c(0o020, 'w'),
        c(0o010, 'x'),
        c(0o004, 'r'),
        c(0o002, 'w'),
        c(0o001, 'x'),
    ]
    .iter()
    .collect()
}

#[test]
fn find_lname_matches_the_link_target() {
    let (_f, dir) = expr_tree("expr_lname");
    let target = dir.join("b.md");
    let link = dir.join("link.md");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&target, &link).unwrap();

    let hit = stdout(&find_at(&dir, &["-lname", "*.md"]));
    assert!(hit.contains(&shown(&dir, "link.md")), "{hit}");
    // `-lname` is about links, so a regular file with the same name never matches.
    assert!(!hit.contains(&shown(&dir, "b.md")), "{hit}");
    let folded = stdout(&find_at(&dir, &["-ilname", "*.MD"]));
    assert!(folded.contains(&shown(&dir, "link.md")), "{folded}");
    let plain = stdout(&find_at(&dir, &["-lname", "*.MD"]));
    assert!(plain.is_empty(), "case matters without the i: {plain}");
}

/// A first smoke pass of both halves on the new platform: `search` finds content in the
/// fixture and still honours the builtin `node_modules` ignore, and `find` walks the same
/// tree for names. Everything else in this file is platform-neutral and runs unchanged on
/// macOS; this one exists so a break in the macOS walk is caught by the suite on the Mac
/// itself rather than by a cross-check that cannot run it.
#[cfg(target_os = "macos")]
#[test]
fn macos_search_and_find_smoke() {
    // `find` takes its paths *before* the expression, which is why this uses `find_at`
    // rather than appending the root the way `search` tolerates. Getting that wrong answers
    // "paths must precede expression" and nothing else, so every assertion below reports
    // the whole invocation.
    let saw = |o: &Output| {
        format!(
            "status {:?}\nstderr: {}\nstdout: {}",
            o.status.code(),
            String::from_utf8_lossy(&o.stderr),
            String::from_utf8_lossy(&o.stdout)
        )
    };
    let f = Fixture::new("macos_smoke");

    let o = run(&["search", "-l", "needle"], &f.root);
    assert!(o.status.success(), "search of the fixture: {}", saw(&o));
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(out.contains("main.rs"), "search found main.rs: {}", saw(&o));
    assert!(out.contains("it.rs"), "search found it.rs: {}", saw(&o));
    assert!(
        !out.contains("index.js"),
        "node_modules is a builtin ignore: {}",
        saw(&o)
    );

    let o = find_at(&f.root, &["-name", "*.rs", "-type", "f"]);
    assert!(o.status.success(), "find of the fixture: {}", saw(&o));
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(
        out.contains("main.rs"),
        "find -type f kept main.rs: {}",
        saw(&o)
    );
    assert!(
        out.contains("util.rs"),
        "find -type f kept util.rs: {}",
        saw(&o)
    );
    // `-type f` rejects a directory, so the walk agreeing with `d_type` here is the whole
    // point of the check: a wrong kind would filter every file out and print nothing.
    assert!(
        !out.lines()
            .any(|l| l.ends_with("/src") || l.ends_with("/tests")),
        "find -type f dropped the directories: {}",
        saw(&o)
    );
    assert!(
        !out.contains("index.js"),
        "find stayed out of node_modules: {}",
        saw(&o)
    );
}

/// `-fstype` asks the filesystem type table, which on macOS is one kernel call, and the
/// assertion has to be able to fail in both directions: a name that is really mounted
/// selects, one that is not rejects. The name is read from the same table the binary uses,
/// keyed by the device `stat` reports for the fixture — the two halves agreeing is the
/// test, since a table keyed by anything else would select nothing and reject everything.
#[cfg(target_os = "macos")]
#[test]
fn macos_fstype_selects_by_the_type_the_kernel_reports() {
    use std::os::unix::fs::MetadataExt;
    let saw = |o: &Output| {
        format!(
            "status {:?}\nstderr: {}\nstdout: {}",
            o.status.code(),
            String::from_utf8_lossy(&o.stderr),
            String::from_utf8_lossy(&o.stdout)
        )
    };
    let f = Fixture::new("macos_fstype");
    let dev = fs::metadata(&f.root).unwrap().dev();
    let types = dusage::native::enumerate::filesystem_types();
    let name = types
        .get(&dev)
        .unwrap_or_else(|| panic!("no filesystem type for device {dev}: {types:?}"))
        .clone();

    let o = find_at(&f.root, &["-fstype", name.as_str(), "-type", "f"]);
    assert!(o.status.success(), "find -fstype {name}: {}", saw(&o));
    let out = stdout(&o);
    assert!(
        out.contains("main.rs"),
        "-fstype {name} kept main.rs: {}",
        saw(&o)
    );

    // Nothing matched, so the documented contract (docs/ARCHITECTURE.md, "Search exit
    // codes": 0 = something matched, 1 = nothing did, 2 = bad) says 1 — not success.
    // Emptiness is the substantive half: it catches a predicate that matches everything.
    let o = find_at(&f.root, &["-fstype", "no_such_filesystem", "-type", "f"]);
    assert_eq!(code(&o), 1, "find -fstype no_such_filesystem: {}", saw(&o));
    assert!(
        stdout(&o).is_empty(),
        "an unmounted type selected something: {}",
        saw(&o)
    );
    assert!(
        stderr(&o).is_empty(),
        "an unmounted type was reported as an error: {}",
        saw(&o)
    );
}

/// The `(files, matches, directory)` of every per-directory row `--stats` printed.
fn stats_rows(stderr: &str) -> Vec<(u64, u64, String)> {
    let mut rows = Vec::new();
    for line in stderr.lines() {
        let (Some(f), Some(m)) = (line.find(" files "), line.find(" matches  ")) else {
            continue;
        };
        let last = |s: &str| s.split_whitespace().last().unwrap().parse::<u64>().unwrap();
        rows.push((
            last(&line[..f]),
            last(&line[..m]),
            line[m + " matches  ".len()..].to_string(),
        ));
    }
    rows
}

#[test]
fn stats_breaks_the_scan_down_by_directory() {
    let fx = Fixture::new("stats_dirs");
    let plain = run(&["search", "-j1", "needle"], &fx.root);
    let o = run(&["search", "-j1", "--stats", "needle"], &fx.root);
    assert_eq!(o.stdout, plain.stdout, "--stats changed stdout");
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(!String::from_utf8_lossy(&plain.stderr).contains("by directory"));

    let summary = err.lines().find(|l| l.contains("files scanned")).unwrap();
    let num = |after: &str| -> u64 {
        let head = &summary[..summary.find(after).unwrap()];
        let word = head.split(|c: char| !c.is_ascii_digit()).next_back();
        word.unwrap().parse().unwrap()
    };
    let rows = stats_rows(&err);
    assert!(err.contains("by directory, slowest first"), "{err}");
    assert_eq!(
        rows.iter().map(|r| r.0).sum::<u64>(),
        num(" files scanned"),
        "{err}"
    );
    assert_eq!(
        rows.iter().map(|r| r.1).sum::<u64>(),
        num(" matches,"),
        "{err}"
    );
    // A file counts under its own directory, not under an ancestor.
    let of = |dir: &Path| {
        let name = dir.to_string_lossy();
        rows.iter().find(|r| r.2 == name).map(|r| (r.0, r.1))
    };
    assert_eq!(of(&fx.root.join("tests")), Some((1, 2)), "{err}");
    assert_eq!(of(&fx.root.join("src")), Some((1, 2)), "{err}");
    assert_eq!(of(&fx.root.join("src").join("deep")), Some((1, 0)), "{err}");
    assert_eq!(of(&fx.root.join("node_modules").join("pkg")), None, "{err}");
}

#[test]
fn stats_folds_directories_past_the_tenth_into_one_row() {
    let fx = Fixture::new("stats_fold");
    for i in 0..12 {
        let d = fx.root.join(format!("many/d{i:02}"));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("f.txt"), "needle\n").unwrap();
    }
    let o = run(&["search", "--stats", "needle"], &fx.root.join("many"));
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    let rows = stats_rows(&err);
    assert!(err.contains("(10 of 12;"), "{err}");
    assert_eq!(rows.len(), 11, "{err}");
    assert_eq!(
        rows[10],
        (2, 2, "(2 more directories)".to_string()),
        "{err}"
    );
}
