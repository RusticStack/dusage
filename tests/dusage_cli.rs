//! End-to-end tests of the `dusage` binary on a small fixture tree.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Create `link` as a directory link to `target`: a junction on Windows, a symlink on
/// Linux. Both are what the scanner calls a reparse point and refuses to follow.
fn link_dir(link: &Path, target: &Path) -> bool {
    #[cfg(windows)]
    {
        Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(target_os = "linux")]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
}

struct Fixture {
    root: PathBuf,
    outside: PathBuf,
}

impl Fixture {
    /// Two top dirs (6000 + 5300 bytes), a root file (70), a hardlink pair (4000 each
    /// name) in the second dir, and a junction to a 100000-byte dir outside the root.
    fn new(tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("dusage_cli_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("one/deep")).unwrap();
        fs::create_dir_all(root.join("two")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("one/a.TXT"), vec![0u8; 1000]).unwrap();
        fs::write(root.join("one/deep/b.bin"), vec![0u8; 5000]).unwrap();
        fs::write(root.join("two/c.txt"), vec![0u8; 1300]).unwrap();
        fs::write(root.join("two/h1.dat"), vec![0u8; 4000]).unwrap();
        fs::hard_link(root.join("two/h1.dat"), root.join("two/h2.dat")).unwrap();
        fs::write(root.join("top.log"), vec![0u8; 70]).unwrap();
        fs::write(outside.join("huge.bin"), vec![0u8; 100_000]).unwrap();
        assert!(
            link_dir(&root.join("junc"), &outside),
            "link creation failed"
        );
        Fixture { root, outside }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Remove the junction as a link first so its target is never traversed.
        let _ = fs::remove_dir(self.root.join("junc"));
        let _ = fs::remove_dir_all(self.root.parent().unwrap());
        let _ = &self.outside;
    }
}

fn dusage(args: &[&str], root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dusage"))
        .arg(root)
        .args(args)
        .output()
        .unwrap()
}

fn stdout(o: &Output) -> String {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout.clone()).unwrap()
}

/// The unsigned integer following `"key":` at the first occurrence at or after `from`.
fn num_after(s: &str, from: usize, key: &str) -> u64 {
    let pat = format!("\"{key}\":");
    let i = from
        + s[from..]
            .find(&pat)
            .unwrap_or_else(|| panic!("{key} in {s}"))
        + pat.len();
    s[i..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap()
}

fn total_bytes(json: &str) -> u64 {
    num_after(json, json.find("\"total\"").unwrap(), "bytes")
}

#[test]
fn json_totals_and_sections() {
    let f = Fixture::new("totals");
    let out = stdout(&dusage(
        &[
            "--json",
            "--backend",
            "batched",
            "--by-ext",
            "--min-big",
            "4000",
        ],
        &f.root,
    ));
    // 1000 + 5000 + 1300 + 4000 + 4000 + 70; the junction target is not counted.
    assert_eq!(total_bytes(&out), 15_370, "{out}");
    assert_eq!(num_after(&out, 0, "skipped_reparse"), 1);
    assert!(out.contains("\"source\":\"batched\""));
    assert!(
        !out.contains("junc\""),
        "junction listed as a top entry: {out}"
    );
    assert!(!out.contains("huge.bin"));
    let one = out.find("\"name\":\"one\"").unwrap();
    assert_eq!(num_after(&out, one, "bytes"), 6000);
    let two = out.find("\"name\":\"two\"").unwrap();
    assert_eq!(num_after(&out, two, "bytes"), 9300);
    let log = out.find("\"name\":\"top.log\"").unwrap();
    assert!(out[log..].starts_with("\"name\":\"top.log\",\"is_dir\":false"));
    assert_eq!(num_after(&out, log, "bytes"), 70);
    // Big files (>= 4000): b.bin, h1.dat, h2.dat.
    let big = out.find("\"big_files\"").unwrap();
    assert_eq!(out[big..].matches("\"path\"").count(), 3);
    // Extension section: "txt" merges a.TXT and c.txt.
    let ext = out.find("\"ext\":\"txt\"").unwrap();
    assert_eq!(num_after(&out, ext, "bytes"), 2300);
    assert!(out.contains("\"ext\":\"bin\""));
}

#[test]
fn by_ext_text_section() {
    let f = Fixture::new("exttext");
    let off = stdout(&dusage(&["--min-big", "1G"], &f.root));
    assert!(!off.contains("by extension"));
    let on = stdout(&dusage(
        &["--min-big", "1G", "--by-ext", "--on-disk"],
        &f.root,
    ));
    assert!(on.contains("by extension"), "{on}");
    assert!(on.contains(".txt"));
    assert!(on.contains("on disk"));
}

#[test]
fn backends_agree() {
    let f = Fixture::new("backends");
    let b = total_bytes(&stdout(&dusage(
        &["--json", "--backend", "batched"],
        &f.root,
    )));
    let r = stdout(&dusage(&["--json", "--backend", "readdir"], &f.root));
    assert!(r.contains("\"source\":\"readdir\""));
    assert_eq!(total_bytes(&r), b);
    let a = total_bytes(&stdout(&dusage(&["--json"], &f.root)));
    assert_eq!(a, b);
}

#[test]
fn dedup_hardlinks_subtracts_link_size() {
    let f = Fixture::new("dedup");
    let plain = stdout(&dusage(&["--json", "--backend", "batched"], &f.root));
    let dedup = stdout(&dusage(&["--json", "--dedup-hardlinks"], &f.root));
    assert_eq!(total_bytes(&plain) - total_bytes(&dedup), 4000);
    assert_eq!(num_after(&dedup, 0, "hardlink_dupes"), 1);
    assert_eq!(num_after(&dedup, 0, "hardlink_dupe_bytes"), 4000);
    assert_eq!(num_after(&plain, 0, "hardlink_dupes"), 0);
}

#[test]
fn legacy_positionals_and_overrides() {
    let f = Fixture::new("legacy");
    let o = Command::new(env!("CARGO_BIN_EXE_dusage"))
        .arg(&f.root)
        .args(["4K", "2", "1", "--json"])
        .output()
        .unwrap();
    let out = stdout(&o);
    // min_big 4K selects b.bin, h1, h2, but top_n = 1 keeps only the largest.
    assert_eq!(out.matches("\"path\"").count(), 1);
    let o = dusage(&["4K", "--min-big", "5000", "--json"], &f.root);
    assert_eq!(stdout(&o).matches("\"path\"").count(), 1);
}

#[test]
fn misuse_exits_2() {
    let f = Fixture::new("misuse");
    for args in [
        &["--bogus"][..],
        &["--backend", "nope"],
        &["--threads", "0"],
        &["--min-big", "12x"],
        &["--top", "x"],
        &["--watch", "0"],
        &["--json=1"],
    ] {
        let o = dusage(args, &f.root);
        assert_eq!(o.status.code(), Some(2), "{args:?}");
        assert!(!o.stderr.is_empty());
    }
    let o = Command::new(env!("CARGO_BIN_EXE_dusage")).output().unwrap();
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn missing_root_exits_2() {
    let f = Fixture::new("missing");
    let o = dusage(&["--json"], &f.root.join("does-not-exist"));
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("not a directory"));
    let o = dusage(&[], &f.root.join("top.log"));
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn help_lists_flags() {
    let o = Command::new(env!("CARGO_BIN_EXE_dusage"))
        .arg("--help")
        .output()
        .unwrap();
    let h = stdout(&o);
    for flag in [
        "--one-filesystem",
        "--min-big",
        "--threads",
        "--top",
        "--json",
        "--by-ext",
        "--on-disk",
        "--dedup-hardlinks",
        "--streams",
        "--backend",
        "--mft",
        "--watch",
    ] {
        assert!(h.contains(flag), "{flag}");
    }
}

#[test]
fn junction_root_is_refused() {
    let f = Fixture::new("jroot");
    let junc = f.root.join("junc");
    for b in ["auto", "readdir", "batched"] {
        let o = dusage(&["--backend", b], &junc);
        assert!(!o.status.success(), "{b}: junction root must not succeed");
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains("reparse point"), "{b}: {err}");
    }
    // Its target is accepted.
    let o = dusage(&["--json"], &f.outside);
    assert!(stdout(&o).contains("huge.bin"));
}

#[test]
fn version_flag_prints_package_version() {
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_dusage"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(o.status.success());
    assert_eq!(
        String::from_utf8_lossy(&o.stdout).trim(),
        format!("dusage {}", env!("CARGO_PKG_VERSION"))
    );
}

#[cfg(target_os = "linux")]
#[test]
fn one_filesystem_is_accepted() {
    let f = Fixture::new("xdev");
    let plain = total_bytes(&stdout(&dusage(&["--json"], &f.root)));
    let xdev = total_bytes(&stdout(&dusage(&["--json", "--one-filesystem"], &f.root)));
    // Nothing in the fixture is a mount point, so the totals agree.
    assert_eq!(plain, xdev);
    let short = total_bytes(&stdout(&dusage(&["--json", "-x"], &f.root)));
    assert_eq!(plain, short);
}

#[cfg(target_os = "linux")]
#[test]
fn mft_backend_is_refused_with_a_reason() {
    let f = Fixture::new("mft");
    for args in [&["--backend", "mft"][..], &["--mft"][..]] {
        let o = dusage(args, &f.root);
        assert_eq!(o.status.code(), Some(1), "{args:?}");
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains("NTFS"), "{args:?}: {err}");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn streams_flag_reports_ntfs_only() {
    let f = Fixture::new("streams");
    let o = dusage(&["--json", "--streams"], &f.root);
    let out = stdout(&o);
    assert_eq!(num_after(&out, 0, "stream_bytes"), 0);
    assert!(
        String::from_utf8_lossy(&o.stderr).contains("NTFS"),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn totals_match_find_and_stat() {
    // `find -printf` is the portable oracle for a Linux scan: every name for the plain
    // total, unique inodes for --dedup-hardlinks.
    let f = Fixture::new("oracle");
    let find = |fmt: &str| -> String {
        let out = Command::new("find")
            .arg(&f.root)
            .args(["-type", "f", "-printf", fmt])
            .output()
            .expect("find");
        assert!(out.status.success(), "find failed");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let plain: u64 = find("%s\n")
        .lines()
        .map(|l| l.trim().parse::<u64>().unwrap())
        .sum();
    let mut seen = std::collections::HashSet::new();
    let mut dedup = 0u64;
    for line in find("%i:%d %s\n").lines() {
        let mut it = line.split_whitespace();
        let (id, size) = (it.next().unwrap(), it.next().unwrap());
        if seen.insert(id.to_string()) {
            dedup += size.parse::<u64>().unwrap();
        }
    }
    assert_eq!(plain, 15_370, "oracle sanity");
    let json = stdout(&dusage(&["--json", "--min-big", "1G"], &f.root));
    assert_eq!(total_bytes(&json), plain, "plain totals vs find");
    let json = stdout(&dusage(
        &["--json", "--dedup-hardlinks", "--min-big", "1G"],
        &f.root,
    ));
    assert_eq!(
        total_bytes(&json),
        dedup,
        "deduped totals vs find -printf %i:%D"
    );
    assert_eq!(
        num_after(&json, json.find("\"total\"").unwrap(), "files"),
        5,
        "counted files (h2 is a duplicate of h1)"
    );
}
