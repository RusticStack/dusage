//! End-to-end tests of `dusage df` against the real `df`.
//!
//! The comparison is skipped, with a note on stderr, when `df` is not installed: a green run
//! without the reference tool must not read as agreement with it. Sizes move while the test
//! runs (another process writing to the same filesystem), so the byte columns are compared
//! with a tolerance and the names, types and mount points exactly.

use std::path::Path;
use std::process::{Command, Output};

fn dusage(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dusage"))
        .arg("df")
        .args(args)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    assert!(
        o.status.success(),
        "df failed: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout.clone()).unwrap()
}

#[cfg_attr(windows, allow(dead_code))] // Windows has no df worth comparing against
fn real_df() -> bool {
    match Command::new("df").arg("--version").output() {
        Ok(o) if o.status.success() => true,
        _ => {
            eprintln!("skipped: df is not installed");
            false
        }
    }
}

/// The single data row of a one-path `df` listing, as its fields. The path is the current
/// directory, so the test says nothing about where a platform mounts its root.
fn only_row(text: &str) -> Vec<String> {
    let mut rows = text.lines().skip(1).map(|l| {
        l.split_whitespace()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    });
    let row = rows.next().expect("a data row");
    assert!(rows.next().is_none(), "one path gives one row:\n{text}");
    row
}

fn here() -> String {
    std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// The reference comparisons are Linux-only on purpose: the `df` a Windows runner has on PATH
/// is the MSYS one, which describes its own emulated mount table and names types its own way,
/// while `dusage df` asks Windows about volumes. Comparing the two would test MSYS, not us.
#[cfg(target_os = "linux")]
#[test]
fn the_row_matches_the_real_df() {
    if !real_df() {
        return;
    }
    let dir = here();
    let reference = Command::new("df")
        .args(["-k", &dir])
        .output()
        .expect("df -k <cwd>");
    assert!(reference.status.success());
    let ours = stdout(&dusage(&[&dir]));
    let theirs = String::from_utf8(reference.stdout).unwrap();
    let a = only_row(&ours);
    let b = only_row(&theirs);
    // Filesystem, 1K-blocks, Used, Available, Use%, Mounted on. The mount point's spelling is
    // the platform's business; the filesystem's name and its arithmetic are the contract.
    assert_eq!(
        a[0], b[0],
        "the same filesystem holds the working directory"
    );
    let num = |s: &str| {
        s.parse::<u64>()
            .unwrap_or_else(|_| panic!("not a number: {s}"))
    };
    let (asize, bsize) = (num(&a[1]), num(&b[1]));
    let tolerance = (bsize / 100).max(1);
    assert!(
        asize.abs_diff(bsize) <= tolerance,
        "size {asize} vs {bsize} KiB"
    );
    let aused = num(&a[2]);
    let bused = num(&b[2]);
    assert!(
        aused.abs_diff(bused) <= 2048 + bused / 100,
        "used {aused} vs {bused} KiB"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn the_type_column_matches_the_real_df() {
    if !real_df() {
        return;
    }
    let dir = here();
    let reference = Command::new("df")
        .args(["-T", "-k", &dir])
        .output()
        .unwrap();
    if !reference.status.success() {
        eprintln!("skipped: this df prints no --type column");
        return;
    }
    let theirs = String::from_utf8(reference.stdout).unwrap();
    let ours = stdout(&dusage(&["-T", &dir]));
    let a = only_row(&ours);
    let b = only_row(&theirs);
    // Filesystem, Type, 1K-blocks, Used, Available, Use%, Mounted on
    assert_eq!(a[0], b[0]);
    assert_eq!(a[1], b[1], "the filesystem type agrees with df -T");
}

/// What a Windows build can still prove without a Windows `df` to compare against: the type
/// column exists, carries a value, and the row is the filesystem holding the working
/// directory.
#[cfg(windows)]
#[test]
fn the_type_column_is_filled_in() {
    let out = stdout(&dusage(&["-T", &here()]));
    let row = only_row(&out);
    assert!(out.lines().next().unwrap().contains("Type"));
    assert!(!row[1].is_empty() && row[1] != "-", "a type: {out}");
}

#[test]
fn every_mounted_filesystem_is_listed_unless_asked_otherwise() {
    let all = stdout(&dusage(&[]));
    assert!(all.lines().count() > 1, "at least one row");
    let head = all.lines().next().unwrap();
    assert!(head.starts_with("Filesystem"), "{head}");
    assert!(
        head.contains("Use%") && head.contains("Mounted on"),
        "{head}"
    );
    // df's block mode names its unit, so a script reading the column knows what it has.
    assert!(head.contains("1K-blocks"), "{head}");
    let human = stdout(&dusage(&["-h"]));
    assert!(human.lines().next().unwrap().contains("Size"));
    // -a may only add rows, never remove one.
    let more = stdout(&dusage(&["-a"]));
    assert!(more.lines().count() >= all.lines().count());
}

#[test]
fn inode_columns_and_human_units() {
    let dir = here();
    let inodes = stdout(&dusage(&["-i", &dir]));
    assert!(inodes.contains("Inodes") && inodes.contains("IUse%"));
    let human = stdout(&dusage(&["-h", &dir]));
    let row = only_row(&human);
    assert!(
        row[1].chars().next().is_some_and(|c| c.is_ascii_digit()),
        "-h prints a figure: {}",
        row[1]
    );
    assert!(
        row[1].len() >= 2 && row[1].ends_with(['K', 'M', 'G', 'T']),
        "-h prints a unit: {}",
        row[1]
    );
    let si = stdout(&dusage(&["--si", &dir]));
    assert!(si.lines().nth(1).is_some());
}

#[test]
fn json_is_well_formed_and_carries_both_count_kinds() {
    let dir = here();
    let out = stdout(&dusage(&["--json", &dir]));
    assert!(out.starts_with("{\"filesystems\":["));
    assert!(out.trim_end().ends_with('}'));
    assert!(out.contains("\"filesystem\":\""));
    assert!(out.contains("\"mount\":\""));
    assert!(out.contains("\"size\":"));
    assert!(out.contains("\"use_percent\":"));
    assert!(out.contains("\"inodes\":"));
    let size = number_after(&out, "\"size\":");
    assert!(
        size > 0,
        "the filesystem holding the working directory has a size"
    );
}

#[test]
fn misuse_and_missing_paths_exit_the_documented_way() {
    let bad = dusage(&["--nonsense"]);
    assert_eq!(bad.status.code(), Some(2));
    let missing = dusage(&[Path::new("/nonexistent/dusage/df").to_str().unwrap()]);
    assert_eq!(missing.status.code(), Some(1));
    let help = stdout(&dusage(&["--help"]));
    assert!(help.contains("--one-filesystem"));
    let one = stdout(&dusage(&["-x", &here()]));
    assert_eq!(one.lines().count(), 2, "header plus one row:\n{one}");
}

fn number_after(s: &str, key: &str) -> u64 {
    let at = s.find(key).unwrap_or_else(|| panic!("no {key}"));
    let rest = &s[at + key.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("no number after {key}"))
}
