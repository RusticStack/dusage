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
