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
        fs::write(root.join("src/deep/util.rs"), "pub fn helper() {}\n// no match here\n").unwrap();
        fs::write(root.join("tests/it.rs"), "needle in a test\nneedle twice\n").unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "needle in a dependency\n").unwrap();
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
    let o = run(&["search"], &f.root);
    assert_eq!(code(&o), 2, "a missing pattern is misuse");
}

#[test]
fn flags_case_word_invert_count_list() {
    let f = Fixture::new("flags");
    let o = run(&["search", "-i", "NEEDLE", "--no-ignore"], &f.root);
    assert_eq!(code(&o), 0);
    assert!(stdout(&o).contains("README.md:3:Needle with capital N"));
    let o = run(&["search", "-w", "needles"], &f.root);
    assert_eq!(code(&o), 1, "-w must not match a longer word");
    let o = run(&["search", "-c", "needle", "--no-ignore"], &f.root);
    assert_eq!(code(&o), 0);
    let out = stdout(&o);
    assert!(out.lines().any(|l| l.ends_with(":2")), "counts per file: {out}");
    let o = run(&["search", "-l", "needle"], &f.root);
    assert_eq!(code(&o), 0);
    let out = stdout(&o);
    assert_eq!(out.lines().count(), 2, "two files match: {out}");
    assert!(out.lines().all(|l| !l.contains(':')));
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
    let o = run(&["search", "-C", "1", "twice", "--no-ignore"], &f.root);
    let out = stdout(&o);
    assert!(out.contains("-needle in a test"), "context: {out}");
    let o = run(&["search", "--json", "needle", "--no-ignore"], &f.root);
    let out = stdout(&o);
    assert!(out.trim_start().starts_with("{\"files\":["), "{out}");
    assert!(out.contains("\"line_number\":2"), "{out}");
    assert!(out.contains("\"submatches\":[{\"start\":4,\"end\":10"), "{out}");
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
    let o = run(&["search", "--no-ignore", "-g", "!node_modules/**", "-l", "needle"], &f.root);
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
    assert!(out.lines().any(|l| l.ends_with("src/")), "{out}");
    let o = run(&["find", "-F", "deep", "*"], &f.root);
    assert!(stdout(&o).contains("src/deep/util.rs"), "{stdout(&o)}");
    let o = run(&["find", "--long", "*.md"], &f.root);
    let out = stdout(&o);
    assert!(out.contains("README.md"), "{out}");
    assert!(out.contains(':' ) && out.lines().count() == 1, "one long row: {out}");
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
    for flag in ["--fixed-strings", "--invert-match", "--only-matching", "--context", "--glob"] {
        assert!(h.contains(flag), "{flag} missing from search help");
    }
    let o = Command::new(env!("CARGO_BIN_EXE_dusage"))
        .args(["find", "--help"])
        .output()
        .unwrap();
    assert!(stdout(&o).contains("--long"));
}
