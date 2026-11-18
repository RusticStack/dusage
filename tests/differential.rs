//! Differential tests: `dusage search` / `dusage find` against ripgrep and GNU grep.
//!
//! The same corpus as `scripts/differential.sh`, built and run from Rust so `cargo test`
//! covers it too. Every content case compares sorted stdout and the exit code. `Agree`
//! cases must match; `Known` cases are catalogue entries from `scripts/README.md` that are
//! expected to differ, and the test fails once they agree so the entry gets retired.
//!
//! The whole test is skipped when `rg` or `grep` is not installed (CLI version checks), and
//! the symlink and `find(1)` groups are skipped on hosts that cannot create symlinks or do
//! not ship `find`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SEPARATOR: &str = "--";

#[derive(Clone, Copy)]
enum Tools {
    Both,
    Rg,
    Grep,
}

#[derive(Clone, Copy)]
enum Expect {
    Agree,
    Known(&'static str),
}

struct Case {
    name: &'static str,
    expect: Expect,
    tools: Tools,
    tree: &'static str,
    target: &'static str,
    pattern: &'static str,
    dusage: &'static [&'static str],
    rg: &'static [&'static str],
    grep: &'static [&'static str],
    /// Keep `--` group separators in the comparison (only the separator cases do).
    keep_separators: bool,
}

#[allow(clippy::too_many_arguments)]
const fn case(
    name: &'static str,
    expect: Expect,
    tools: Tools,
    tree: &'static str,
    target: &'static str,
    pattern: &'static str,
    dusage: &'static [&'static str],
    rg: &'static [&'static str],
    grep: &'static [&'static str],
) -> Case {
    Case {
        name,
        expect,
        tools,
        tree,
        target,
        pattern,
        dusage,
        rg,
        grep,
        keep_separators: false,
    }
}

#[allow(clippy::too_many_arguments)]
const fn sep_case(
    name: &'static str,
    expect: Expect,
    tree: &'static str,
    target: &'static str,
    pattern: &'static str,
    dusage: &'static [&'static str],
    rg: &'static [&'static str],
    grep: &'static [&'static str],
) -> Case {
    Case {
        keep_separators: true,
        ..case(
            name,
            expect,
            Tools::Both,
            tree,
            target,
            pattern,
            dusage,
            rg,
            grep,
        )
    }
}

use Expect::{Agree, Known};
use Tools::{Both, Grep, Rg};

fn corpus() -> Vec<Case> {
    vec![
        case(
            "literal_basic",
            Agree,
            Both,
            "plain",
            ".",
            "needle",
            &[],
            &[],
            &["-rE"],
        ),
        case(
            "literal_absent",
            Agree,
            Both,
            "plain",
            ".",
            "zzzz-absent",
            &[],
            &[],
            &["-rE"],
        ),
        case(
            "line_numbers",
            Agree,
            Both,
            "plain",
            ".",
            "needle",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        case(
            "case_insensitive",
            Agree,
            Both,
            "plain",
            ".",
            "Needle",
            &["-i"],
            &["-i"],
            &["-riE"],
        ),
        case(
            "case_insensitive_upper",
            Agree,
            Both,
            "plain",
            ".",
            "NEEDLE",
            &["-i", "-n"],
            &["-i", "-n"],
            &["-rniE"],
        ),
        case(
            "word_flag",
            Agree,
            Both,
            "plain",
            ".",
            "needle",
            &["-w"],
            &["-w"],
            &["-rwE"],
        ),
        case(
            "word_flag_no_partial",
            Agree,
            Both,
            "plain",
            ".",
            "needles",
            &["-w"],
            &["-w"],
            &["-rwE"],
        ),
        case(
            "invert_match",
            Agree,
            Both,
            "plain",
            ".",
            "needle",
            &["-v"],
            &["-v"],
            &["-rvE"],
        ),
        case(
            "files_with_matches",
            Agree,
            Both,
            "plain",
            ".",
            "needle",
            &["-l"],
            &["-l"],
            &["-rlE"],
        ),
        case(
            "files_without_match",
            Agree,
            Both,
            "plain",
            ".",
            "needle",
            &["-L"],
            &["--files-without-match"],
            &["-rLE"],
        ),
        case(
            "only_matching",
            Agree,
            Both,
            "plain",
            ".",
            "needle",
            &["-o"],
            &["-o"],
            &["-roE"],
        ),
        case(
            "only_matching_numbers",
            Agree,
            Both,
            "plain",
            ".",
            "needle",
            &["-o", "-n"],
            &["-o", "-n"],
            &["-rnoE"],
        ),
        case(
            "max_count_1",
            Agree,
            Both,
            "plain",
            ".",
            "needle",
            &["-m", "1"],
            &["-m", "1"],
            &["-rE", "-m", "1"],
        ),
        case(
            "fixed_strings",
            Agree,
            Both,
            "plain",
            ".",
            "nee.le",
            &["-F"],
            &["-F"],
            &["-rF"],
        ),
        case(
            "regex_dot",
            Agree,
            Both,
            "plain",
            ".",
            "nee.le",
            &[],
            &[],
            &["-rE"],
        ),
        case(
            "anchored_start",
            Agree,
            Both,
            "plain",
            ".",
            "^alpha",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        case(
            "anchored_end",
            Agree,
            Both,
            "plain",
            ".",
            "line$",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        case(
            "class_range",
            Agree,
            Both,
            "plain",
            ".",
            "[0-9]{2,3}",
            &["-o"],
            &["-o"],
            &["-roE"],
        ),
        case(
            "class_negated",
            Agree,
            Both,
            "plain",
            ".",
            "[^a-z]needle",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        case(
            "word_boundary_escape",
            Agree,
            Both,
            "plain",
            ".",
            r"\bneedle\b",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        case(
            "alternation",
            Agree,
            Rg,
            "plain",
            ".",
            "alpha|needle",
            &["-c"],
            &["-c"],
            &["-rcE"],
        ),
        case(
            "optional_group",
            Agree,
            Both,
            "plain",
            ".",
            "need(le)?",
            &["-o"],
            &["-o"],
            &["-roE"],
        ),
        case(
            "one_or_more",
            Agree,
            Both,
            "plain",
            ".",
            "needle+",
            &["-o"],
            &["-o"],
            &["-roE"],
        ),
        case(
            "space_class",
            Agree,
            Rg,
            "plain",
            ".",
            r"\sneedle",
            &["-c"],
            &["-c"],
            &["-rcE"],
        ),
        case(
            "escaped_parens",
            Agree,
            Both,
            "plain",
            ".",
            r"\(\)",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        case(
            "utf8_literal",
            Agree,
            Both,
            "plain",
            ".",
            "café",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        case(
            "utf8_second_word",
            Agree,
            Both,
            "plain",
            ".",
            "naïve",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        case(
            "tab_escape",
            Agree,
            Rg,
            "plain",
            ".",
            r"needle\there",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        case(
            "no_trailing_newline",
            Agree,
            Both,
            "plain",
            ".",
            "newline needle",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        case(
            "two_file_roots",
            Agree,
            Both,
            "plain",
            "ascii.txt",
            "needle",
            &["-n"],
            &["-n"],
            &["-nE"],
        ),
        case(
            "invalid_regex",
            Agree,
            Both,
            "plain",
            ".",
            "[a-",
            &[],
            &[],
            &["-rE"],
        ),
        case(
            "quit_matched",
            Agree,
            Both,
            "plain",
            ".",
            "needle",
            &["-q"],
            &["-q"],
            &["-rqE"],
        ),
        case(
            "quit_absent",
            Agree,
            Both,
            "plain",
            ".",
            "zzzz-absent",
            &["-q"],
            &["-q"],
            &["-rqE"],
        ),
        case(
            "ctx_after",
            Agree,
            Both,
            "plain",
            "sub",
            "needle",
            &["-A", "1"],
            &["-A1"],
            &["-rE", "-A", "1"],
        ),
        case(
            "ctx_before",
            Agree,
            Both,
            "plain",
            "sub",
            "needle",
            &["-B", "1"],
            &["-B1"],
            &["-rE", "-B", "1"],
        ),
        case(
            "ctx_both",
            Agree,
            Both,
            "plain",
            "sub",
            "needle",
            &["-C", "1"],
            &["-C1"],
            &["-rE", "-C", "1"],
        ),
        case(
            "count_per_file",
            Agree,
            Rg,
            "plain",
            ".",
            "needle",
            &["-c"],
            &["-c"],
            &["-rcE"],
        ),
        case(
            "count_inverted",
            Agree,
            Rg,
            "plain",
            ".",
            "needle",
            &["-c", "-v"],
            &["-c", "-v"],
            &["-rcvE"],
        ),
        case(
            "noncapturing_group",
            Agree,
            Rg,
            "plain",
            ".",
            "(?:alpha|needle)",
            &["-c"],
            &["-c"],
            &["-rcE"],
        ),
        case(
            "inline_case_flag",
            Agree,
            Rg,
            "plain",
            ".",
            "(?i)NEEDLE",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        case(
            "digit_class",
            Agree,
            Rg,
            "plain",
            ".",
            r"\d+",
            &["-o"],
            &["-o"],
            &["-roE"],
        ),
        case(
            "long_line_paths",
            Agree,
            Both,
            "plain",
            ".",
            "needle-tail",
            &["-l"],
            &["-l"],
            &["-rlE"],
        ),
        case(
            "glob_txt",
            Agree,
            Rg,
            "plain",
            ".",
            "needle",
            &["-g", "*.txt"],
            &["-g", "*.txt"],
            &["-rE"],
        ),
        case(
            "type_rust",
            Agree,
            Rg,
            "plain",
            ".",
            "needle",
            &["-t", "rust"],
            &["-t", "rust"],
            &["-rE"],
        ),
        case(
            "max_depth",
            Agree,
            Rg,
            "plain",
            ".",
            "needle",
            &["--max-depth", "1"],
            &["--max-depth", "1"],
            &["-rE"],
        ),
        case(
            "max_count_zero",
            Agree,
            Rg,
            "plain",
            ".",
            "needle",
            &["-m", "0"],
            &["-m", "0"],
            &["-rE", "-m", "0"],
        ),
        sep_case(
            "separator_no_context",
            Agree,
            "plain",
            "ascii.txt",
            "needle",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        sep_case(
            "separator_across_files",
            Agree,
            "plain",
            "sub",
            "needle",
            &["-C", "1"],
            &["-C1"],
            &["-rE", "-C", "1"],
        ),
        case(
            "crlf_start_anchor",
            Known("crlf-strip"),
            Both,
            "crlf",
            ".",
            "^crlf",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        case(
            "crlf_plain_line",
            Known("crlf-strip"),
            Both,
            "crlf",
            ".",
            "needle line",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        case(
            "crlf_end_anchor",
            Known("crlf-anchor"),
            Both,
            "crlf",
            ".",
            "second line$",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        case(
            "ignore_strict_default",
            Agree,
            Rg,
            "strict",
            ".",
            "needle",
            &[],
            &[],
            &["-rE"],
        ),
        case(
            "ignore_strict_no_ignore",
            Agree,
            Rg,
            "strict",
            ".",
            "needle",
            &["--no-ignore"],
            &["--no-ignore"],
            &["-rE"],
        ),
        case(
            "ignore_strict_paths",
            Agree,
            Rg,
            "strict",
            ".",
            "needle",
            &["-l"],
            &["-l"],
            &["-rlE"],
        ),
        case(
            "ignore_tree_default",
            Agree,
            Rg,
            "tree",
            ".",
            "in",
            &["-l"],
            &["-l"],
            &["-rlE"],
        ),
        case(
            "ignore_tree_all",
            Agree,
            Rg,
            "tree",
            ".",
            "in",
            &[],
            &[],
            &["-rE"],
        ),
        case(
            "ignore_tree_no_ignore",
            Agree,
            Rg,
            "tree",
            ".",
            "in",
            &["--no-ignore"],
            &["--no-ignore"],
            &["-rE"],
        ),
        case(
            "ignore_tree_hidden",
            Agree,
            Rg,
            "tree",
            ".",
            "in",
            &["--hidden", "-l"],
            &["--hidden", "-l"],
            &["-rlE"],
        ),
        case(
            "ignore_tree_glob_rs",
            Agree,
            Rg,
            "tree",
            ".",
            "in",
            &["-g", "*.rs"],
            &["-g", "*.rs"],
            &["-rE"],
        ),
        case(
            "ignore_tree_type_rust",
            Agree,
            Rg,
            "tree",
            ".",
            "in",
            &["-t", "rust"],
            &["-t", "rust"],
            &["-rE"],
        ),
        case(
            "ignore_tree_glob_exclude",
            Agree,
            Rg,
            "tree",
            ".",
            "in",
            &["--no-ignore", "-g", "!vendor/**"],
            &["--no-ignore", "-g", "!vendor/**"],
            &["-rE"],
        ),
        case(
            "ignore_nested_rules",
            Agree,
            Rg,
            "tree",
            ".",
            "generated",
            &[],
            &[],
            &["-rE"],
        ),
        case(
            "ignore_parent_rules",
            Agree,
            Rg,
            "tree",
            "src",
            "in",
            &[],
            &[],
            &["-rE"],
        ),
        case(
            "ignore_glob_reinclude",
            Agree,
            Rg,
            "strict",
            ".",
            "in",
            &["-l", "-g", "*.log"],
            &["-l", "-g", "*.log"],
            &["-rlE"],
        ),
        case(
            "ignore_hidden_glob",
            Agree,
            Rg,
            "dot",
            ".",
            "in",
            &["-t", "rust"],
            &["-t", "rust"],
            &["-rE"],
        ),
        case(
            "single_file_name",
            Agree,
            Both,
            "one",
            ".",
            "only-here",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        case(
            "symlink_not_followed",
            Agree,
            Both,
            "links",
            ".",
            "needle",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        case(
            "binary_report",
            Known("binary-report"),
            Rg,
            "enc",
            ".",
            "needle",
            &[],
            &[],
            &["-rE"],
        ),
        case(
            "binary_text",
            Known("utf16-autodetect"),
            Grep,
            "enc",
            ".",
            "needle",
            &["-a"],
            &["-a"],
            &["-raE", "-a"],
        ),
        case(
            "utf16_bom",
            Known("binary-report"),
            Rg,
            "enc",
            ".",
            "needle",
            &[],
            &[],
            &["-rE"],
        ),
        case(
            "ignore_rule_leak",
            Agree,
            Rg,
            "wipe",
            ".",
            "needle",
            &[],
            &[],
            &["-rE"],
        ),
        case(
            "utf8_dot_byte",
            Known("byte-dot"),
            Both,
            "plain",
            ".",
            "^caf.$",
            &["-n"],
            &["-n"],
            &["-rnE"],
        ),
        case(
            "unicode_class_letters",
            Agree,
            Rg,
            "plain",
            ".",
            "\\p{L}{4,}",
            &["-o", "-n"],
            &["-o", "-n", "--pcre2"],
            &["-rE"],
        ),
        case(
            "unicode_case_fold",
            Agree,
            Both,
            "plain",
            ".",
            "CAFÉ",
            &["-i", "-n"],
            &["-i", "-n"],
            &["-rniE"],
        ),
        case(
            "max_columns",
            Known("max-columns"),
            Rg,
            "plain",
            ".",
            "needle",
            &["--max-columns", "20"],
            &["--max-columns", "20"],
            &["-rE"],
        ),
    ]
}

/// `dusage find` cases, compared against `find(1)`; only stdout is compared because
/// `find` exits 0 whether or not anything matched.
struct FindCase {
    name: &'static str,
    tree: &'static str,
    target: &'static str,
    dusage: &'static [&'static str],
    find: &'static [&'static str],
}

fn find_corpus() -> Vec<FindCase> {
    vec![
        FindCase {
            name: "find_glob_txt",
            tree: "plain",
            target: ".",
            dusage: &["*.txt"],
            find: &[".", "-name", "*.txt"],
        },
        FindCase {
            name: "find_glob_recursive",
            tree: "plain",
            target: ".",
            dusage: &["**/*.txt"],
            find: &[".", "-name", "*.txt"],
        },
        FindCase {
            name: "find_path_glob",
            tree: "plain",
            target: ".",
            dusage: &["sub/*.txt"],
            find: &["sub", "-maxdepth", "1", "-name", "*.txt"],
        },
        FindCase {
            name: "find_substring",
            tree: "plain",
            target: ".",
            dusage: &["-F", "deep"],
            find: &[".", "-path", "*deep*"],
        },
        FindCase {
            name: "find_dirs",
            tree: "plain",
            target: ".",
            dusage: &["--dirs", "*"],
            find: &[".", "-mindepth", "1", "-type", "d"],
        },
        FindCase {
            name: "find_no_match",
            tree: "plain",
            target: ".",
            dusage: &["*.nope"],
            find: &[".", "-name", "*.nope"],
        },
    ]
}

// --- fixture ---------------------------------------------------------------

struct Fixture {
    root: PathBuf,
}

fn w(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

impl Fixture {
    fn build(tag: &str) -> Fixture {
        let root =
            std::env::temp_dir().join(format!("dusage_differential_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        w(
            &root,
            "plain/ascii.txt",
            b"alpha needle beta\nsecond line without it\nneedle again needle\n",
        );
        w(
            &root,
            "plain/notes.md",
            b"# Notes\nA needle in markdown.\nfn other() {}\nneedle\there\nno trailing newline needle\n",
        );
        w(
            &root,
            "plain/sub/nested.txt",
            b"nested needle line\nanother nested line\n",
        );
        w(
            &root,
            "plain/sub/deep/leaf.txt",
            b"deep needle here\nterminal line\n",
        );
        w(
            &root,
            "plain/digits.txt",
            b"alpha 123 needle\nalpha 1234 needle\nplain 12 needle\n",
        );
        w(&root, "plain/codes.rs", b"fn needle() { let x = 42; }\n");
        w(&root, "plain/sub/more.rs", b"fn needle() {}\n");
        w(&root, "plain/nonl.txt", b"no trailing newline needle");
        w(&root, "plain/empty.txt", b"");
        w(
            &root,
            "plain/utf8.txt",
            "café needle naïve — ünïcödé\ncafé\n".as_bytes(),
        );
        w(
            &root,
            "plain/sub/utf8b.txt",
            "café naïve at the top\ncafé\n".as_bytes(),
        );
        w(&root, "plain/ws.txt", b"needle\there\ntwo  spaces needle\n");
        let mut long = vec![b'x'; 1024 * 1024];
        long.extend_from_slice(b"needle-tail\n");
        w(&root, "plain/long.txt", &long);

        w(&root, "crlf/crlf.txt", b"crlf needle\r\nsecond line\r\n");
        w(
            &root,
            "crlf/other.txt",
            b"another needle line\r\nsecond line\r\n",
        );
        w(
            &root,
            "crlf/plain.txt",
            b"a plain needle line\ncrlf again\r\n",
        );

        w(&root, "enc/plain.txt", b"needle plain\n");
        w(&root, "enc/blob.bin", b"header\x00needle\x00tail\n");
        w(&root, "enc/latin1.txt", b"caf\xe9 needle latin1\n");
        w(
            &root,
            "enc/utf16le.txt",
            b"\xff\xfen\x00e\x00e\x00d\x00l\x00e\x00 \x00u\x00t\x00f\x001\x006\x00\r\x00\n\x00",
        );

        w(
            &root,
            "tree/.gitignore",
            b"*.log\n!keep.log\nbuild/\nnode_modules/\n",
        );
        w(&root, "tree/.ignore", b"ignored-by-dot-ignore\n");
        w(&root, "tree/app.rs", b"in app\n");
        w(&root, "tree/app.log", b"in a log\n");
        w(&root, "tree/keep.log", b"in keep log\n");
        w(&root, "tree/build/out.txt", b"in build output\n");
        w(&root, "tree/src/lib.rs", b"in lib source\n");
        w(&root, "tree/src/gen.log", b"in generated log\n");
        w(&root, "tree/src/.gitignore", b"generated.txt\n");
        w(&root, "tree/src/generated.txt", b"in generated source\n");
        w(&root, "tree/vendor/index.js", b"in vendor source\n");
        w(&root, "tree/vendor/.gitignore", b"matches-nothing\n");
        w(&root, "tree/node_modules/index.js", b"in node_modules\n");
        w(&root, "tree/.dotfile.txt", b"in hidden file\n");
        w(&root, "tree/hidden/.secret.txt", b"in hidden dir\n");
        w(&root, "tree/hidden/.gitignore", b"matches-nothing\n");

        w(&root, "strict/.gitignore", b"*.log\n!keep.log\n");
        w(&root, "strict/a.log", b"in a log\n");
        w(&root, "strict/keep.log", b"in keep log\n");
        w(&root, "strict/a.txt", b"in a text file\n");

        w(&root, "dot/.hidden.rs", b"in hidden rust\n");
        w(&root, "dot/visible.rs", b"in visible rust\n");

        w(&root, "one/single.txt", b"only-here needle\nsecond line\n");

        w(&root, "links/target.txt", b"needle in target\n");
        w(&root, "links/sub/other.txt", b"needle in other\n");
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink("target.txt", root.join("links/link.txt"));
            let _ = std::os::unix::fs::symlink("sub", root.join("links/linkdir"));
        }

        w(&root, "wipe/.gitignore", b"*.log\n");
        w(&root, "wipe/top.log", b"needle root log\n");
        w(&root, "wipe/p/d1/a.log", b"needle d1 log\n");
        w(&root, "wipe/p/d2/b.log", b"needle d2 log\n");

        // ripgrep honours .gitignore only inside a git repository.
        let _ = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["init", "-q"])
            .output();

        Fixture { root }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

// --- running and comparing -------------------------------------------------

fn tool_present(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn run(prog: &str, dir: &Path, args: &[&str]) -> Output {
    Command::new(prog)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("running {prog} {args:?}: {e}"))
}

/// Sorted stdout with the leading `./` removed; `--` group separators are dropped unless
/// `keep_separators` is set (dusage prints them between any two non-adjacent matches).
fn lines(o: &Output, keep_separators: bool) -> Vec<String> {
    // Splitting on raw `\n` keeps a trailing `\r`, which is the whole point of the CRLF
    // cases: `str::lines` would hide it.
    let mut v: Vec<String> = o
        .stdout
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| {
            let l = String::from_utf8_lossy(l).into_owned();
            match l.strip_prefix("./") {
                Some(rest) => rest.to_string(),
                None => l,
            }
        })
        .filter(|l| keep_separators || l != SEPARATOR)
        .collect();
    v.sort();
    v
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

fn check(
    name: &str,
    expect: Expect,
    got: &[String],
    want: &[String],
    gcode: i32,
    wcode: i32,
    other: &str,
) {
    match expect {
        Agree => {
            assert_eq!(
                (got, gcode),
                (want, wcode),
                "case {name}: dusage differs from {other}\n  dusage: {got:#?}\n  {other}: {want:#?}"
            );
        }
        Known(id) => {
            assert!(
                got != want || gcode != wcode,
                "case {name}: known difference {id} is gone; retire it from scripts/README.md"
            );
        }
    }
}

fn run_case(f: &Fixture, c: &Case, have_rg: bool, have_grep: bool) -> Option<()> {
    let dir = f.root.join(c.tree);
    let mut dargs = vec!["search"];
    dargs.extend_from_slice(c.dusage);
    dargs.push(c.pattern);
    dargs.push(c.target);
    let d = run(env!("CARGO_BIN_EXE_dusage"), &dir, &dargs);
    let dlines = lines(&d, c.keep_separators);

    let mut compared = 0;
    if matches!(c.tools, Both | Tools::Rg) {
        if !have_rg {
            return None;
        }
        let mut rargs = vec!["--no-heading", "--color", "never"];
        rargs.extend_from_slice(c.rg);
        rargs.push(c.pattern);
        rargs.push(c.target);
        let r = run("rg", &dir, &rargs);
        check(
            c.name,
            c.expect,
            &dlines,
            &lines(&r, c.keep_separators),
            code(&d),
            code(&r),
            "rg",
        );
        compared += 1;
    }
    if matches!(c.tools, Both | Tools::Grep) {
        if !have_grep {
            return None;
        }
        let mut gargs: Vec<&str> = Vec::new();
        gargs.extend_from_slice(c.grep);
        gargs.push(c.pattern);
        gargs.push(c.target);
        let g = run("grep", &dir, &gargs);
        check(
            c.name,
            c.expect,
            &dlines,
            &lines(&g, c.keep_separators),
            code(&d),
            code(&g),
            "grep",
        );
        compared += 1;
    }
    assert!(compared > 0, "case {} compares nothing", c.name);
    Some(())
}

#[test]
fn differential_against_rg_and_grep() {
    let have_rg = tool_present("rg");
    let have_grep = tool_present("grep");
    if !have_rg || !have_grep {
        eprintln!(
            "skipping differential tests: rg present = {have_rg}, grep present = {have_grep}"
        );
        return;
    }
    let f = Fixture::build("search");
    let mut skipped = Vec::new();
    for c in corpus() {
        if run_case(&f, &c, have_rg, have_grep).is_none() {
            skipped.push(c.name);
        }
    }
    assert!(
        skipped.is_empty(),
        "cases skipped for a missing tool: {skipped:?}"
    );
}

#[test]
fn differential_find_against_find() {
    if !tool_present("find") {
        eprintln!("skipping find differential tests: no find(1)");
        return;
    }
    let f = Fixture::build("find");
    for c in find_corpus() {
        let dir = f.root.join(c.tree);
        let mut dargs = vec!["find"];
        dargs.extend_from_slice(c.dusage);
        dargs.push(c.target);
        let d = run(env!("CARGO_BIN_EXE_dusage"), &dir, &dargs);
        let mut fargs = c.find.to_vec();
        if fargs.is_empty() {
            fargs.push(".");
        }
        let g = run("find", &dir, &fargs);
        assert_eq!(
            lines(&d, false),
            lines(&g, false),
            "case {}: dusage find differs from find(1)",
            c.name
        );
    }
}

/// `dusage` documents its own misuse paths: exit 2 for a usage error, 0/1 for
/// match/no-match, and nothing on stdout for the error cases.
#[test]
fn documented_usage_paths() {
    let f = Fixture::build("usage");
    let dir = &f.root;
    let cases: &[(&str, &[&str], i32, bool)] = &[
        ("empty pattern", &["search", "", "."], 2, false),
        (
            "attached short value",
            &["search", "-C1", "needle", "."],
            0,
            true,
        ),
        (
            "posix bracket expression",
            &["search", "[[:alpha:]]+", "plain"],
            0,
            true,
        ),
        (
            "unknown posix class",
            &["search", "[[:bogus:]]", "plain"],
            2,
            false,
        ),
    ];
    for (what, args, want_code, want_output) in cases {
        let o = run(env!("CARGO_BIN_EXE_dusage"), dir, args);
        assert_eq!(
            code(&o),
            *want_code,
            "{what}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        assert_eq!(
            !o.stdout.is_empty(),
            *want_output,
            "{what} printed unexpected stdout"
        );
    }
}
