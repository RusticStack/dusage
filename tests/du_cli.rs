//! End-to-end tests of `dusage du` against a fixture tree with known sizes.
//!
//! Every assertion works on the last tab-separated field of a line, with separators
//! normalised to `/`, so the same expectations hold on Windows and Linux.

use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A directory under the system temp dir, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("dusage_du_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Dir(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `root/f1000`, `root/f1`, `root/top1024`, `root/sub/{f1024,f5000,deep/f2048}`, `root/empty`.
///
/// Bytes: sub 8072, deep 2048, empty 0, root 10097.
/// 1 KiB blocks rounded up per file: sub 8, deep 2, empty 0, root 11.
/// Entries for `--inodes`: root 9, sub 5, deep 2, empty 1.
fn tree(tag: &str) -> (Dir, PathBuf) {
    let base = Dir::new(tag);
    let root = base.0.join("root");
    fs::create_dir_all(root.join("sub/deep")).unwrap();
    fs::create_dir_all(root.join("empty")).unwrap();
    fs::write(root.join("f1000"), vec![b'x'; 1000]).unwrap();
    fs::write(root.join("f1"), b"x").unwrap();
    fs::write(root.join("top1024"), vec![b'x'; 1024]).unwrap();
    fs::write(root.join("sub/f1024"), vec![b'x'; 1024]).unwrap();
    fs::write(root.join("sub/f5000"), vec![b'x'; 5000]).unwrap();
    fs::write(root.join("sub/deep/f2048"), vec![b'x'; 2048]).unwrap();
    (base, root)
}

/// A tree with two sizes that exercise the human formatting: 4 KiB and 10 KiB.
fn human_tree(tag: &str) -> (Dir, PathBuf) {
    let base = Dir::new(tag);
    let root = base.0.join("root");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("f4096"), vec![b'x'; 4096]).unwrap();
    fs::write(root.join("f10240"), vec![b'x'; 10240]).unwrap();
    (base, root)
}

fn du(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dusage"))
        .arg("du")
        .args(args)
        .output()
        .unwrap()
}

/// stdout, whatever the exit code: the error tests need the lines of a failed run.
#[cfg(unix)]
fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn ok(o: &Output) -> String {
    assert!(
        o.status.success(),
        "exit {:?}, stderr: {}",
        o.status.code(),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8(o.stdout.clone()).unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// `(size, path)` for every line, separator normalised, so Windows and Linux agree.
fn rows(o: &Output) -> Vec<(String, String)> {
    parse(&ok(o))
}

/// The same, for a run that is expected to exit non-zero.
#[cfg(unix)]
fn rows_any(o: &Output) -> Vec<(String, String)> {
    parse(&stdout(o))
}

fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let mut parts = l.split('\t');
            let size = parts.next().unwrap_or("").to_string();
            let path = parts.next_back().unwrap_or("").replace('\\', "/");
            (size, path)
        })
        .collect()
}

/// The size printed for the entry whose path ends with `rel` (a `/`-separated suffix).
fn size(rows: &[(String, String)], rel: &str) -> String {
    let want = format!("/{rel}");
    rows.iter()
        .find(|(_, p)| p == rel || p.ends_with(&want))
        .unwrap_or_else(|| panic!("no line for {rel} in {rows:?}"))
        .0
        .clone()
}

/// The position of the entry whose path ends with `rel`.
fn at(rows: &[(String, String)], rel: &str) -> usize {
    let want = format!("/{rel}");
    rows.iter()
        .position(|(_, p)| p == rel || p.ends_with(&want))
        .unwrap_or_else(|| panic!("no line for {rel} in {rows:?}"))
}

/// 1 KiB blocks of the apparent size, which is what `--apparent-size -k` asks for: the
/// default base is now the allocated size, so the numbers this fixture pins down are
/// spelled with the base that produces them.
#[test]
fn blocks_are_rounded_up_per_line_and_printed_deepest_first() {
    let (_d, root) = tree("blocks");
    let r = rows(&du(&["--apparent-size", "-k", root.to_str().unwrap()]));
    assert_eq!(r.len(), 4, "{r:?}");
    assert_eq!(size(&r, "deep"), "2");
    assert_eq!(size(&r, "sub"), "8");
    assert_eq!(size(&r, "empty"), "0");
    // Files are not listed without -a, and the start point is the last line.
    assert!(!r.iter().any(|(_, p)| p.ends_with("f1024")), "{r:?}");
    assert_eq!(at(&r, "root"), r.len() - 1, "{r:?}");
    // A line converts its whole subtree total once (10097 bytes is ten 1 KiB blocks), as
    // du's lines do; the old per-file rounding that made this 11 is gone with the base.
    assert_eq!(size(&r, "root"), "10");
    // A subdirectory is always printed before the directory that contains it.
    assert!(at(&r, "deep") < at(&r, "sub"));
    assert!(at(&r, "sub") < at(&r, "root"));
}

#[test]
fn bytes_mode_reports_exact_sizes() {
    let (_d, root) = tree("bytes");
    let r = rows(&du(&["-b", "-a", root.to_str().unwrap()]));
    assert_eq!(size(&r, "f1"), "1");
    assert_eq!(size(&r, "f1000"), "1000");
    assert_eq!(size(&r, "sub"), "8072");
    assert_eq!(size(&r, "deep"), "2048");
    assert_eq!(size(&r, "root"), "10097");
    // --apparent-size is the same base in GNU's default 1 KiB block (`-b` is GNU's
    // `--apparent-size --block-size=1`), so the byte spelling is --block-size=1.
    let a = rows(&du(&["--apparent-size", root.to_str().unwrap()]));
    assert_eq!(size(&a, "root"), "10");
    let a = rows(&du(&[
        "--apparent-size",
        "--block-size=1",
        root.to_str().unwrap(),
    ]));
    assert_eq!(size(&a, "root"), "10097");
}

#[test]
fn all_lists_every_file() {
    let (_d, root) = tree("all");
    let r = rows(&du(&[
        "-a",
        "--apparent-size",
        "-k",
        root.to_str().unwrap(),
    ]));
    assert_eq!(r.len(), 10, "{r:?}");
    for (name, want) in [
        ("f1000", "1"),
        ("f1", "1"),
        ("top1024", "1"),
        ("sub/f1024", "1"),
        ("sub/f5000", "5"),
        ("sub/deep/f2048", "2"),
        ("empty", "0"),
    ] {
        assert_eq!(size(&r, name), want, "{name} in {r:?}");
    }
    assert!(at(&r, "sub/deep/f2048") < at(&r, "deep"));
    assert!(at(&r, "sub/f5000") < at(&r, "sub"));
    assert!(at(&r, "f1000") < at(&r, "root"));
    assert_eq!(at(&r, "root"), r.len() - 1);
}

#[test]
fn summarize_prints_only_the_start_point() {
    let (_d, root) = tree("summ");
    let r = rows(&du(&[
        "-s",
        "--apparent-size",
        "-k",
        root.to_str().unwrap(),
    ]));
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0].0, "10", "one line converts the whole total once");
    assert_eq!(
        r[0].1.replace('\\', "/"),
        root.to_str().unwrap().replace('\\', "/")
    );
    // -d 0 is the same thing.
    let d = rows(&du(&[
        "-d",
        "0",
        "--apparent-size",
        "-k",
        root.to_str().unwrap(),
    ]));
    assert_eq!(d, r);
    // Summary and -a together are refused, as du refuses them.
    let bad = du(&["-s", "-a", root.to_str().unwrap()]);
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn total_adds_a_line_and_sums_every_start_point() {
    let (_d, root) = tree("total");
    let r = rows(&du(&[
        "-c",
        "-s",
        "--apparent-size",
        "-k",
        root.to_str().unwrap(),
    ]));
    assert_eq!(r.len(), 2, "{r:?}");
    assert_eq!(r[1].0, "10");
    assert!(r[1].1.ends_with("total") && !r[1].1.contains('/'), "{r:?}");
    // A second, unrelated tree adds to the same total.
    let other = root.with_file_name("other");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("f3000"), vec![b'x'; 3000]).unwrap();
    let two = rows(&du(&[
        "-c",
        "-s",
        "--apparent-size",
        "-k",
        root.to_str().unwrap(),
        other.to_str().unwrap(),
    ]));
    assert_eq!(size(&two, "other"), "3");
    // 13097 bytes: the total line converts its own sum once, so 13 and not 10 + 3.
    assert_eq!(two.last().unwrap().0, "13", "{two:?}");
    assert_eq!(two.last().unwrap().1, "total");
}

#[test]
fn max_depth_limits_the_lines_not_the_totals() {
    let (_d, root) = tree("depth");
    let r = rows(&du(&[
        "-d",
        "1",
        "--apparent-size",
        "-k",
        root.to_str().unwrap(),
    ]));
    assert_eq!(r.len(), 3, "{r:?}");
    assert_eq!(size(&r, "sub"), "8");
    assert_eq!(size(&r, "empty"), "0");
    assert_eq!(size(&r, "root"), "10");
    // Files obey the depth too: only depth-2 files appear under -d 2.
    let a = rows(&du(&[
        "-a",
        "-d",
        "2",
        "--apparent-size",
        "-k",
        root.to_str().unwrap(),
    ]));
    assert!(size(&a, "sub/f1024") == "1", "{a:?}");
    assert!(!a.iter().any(|(_, p)| p.ends_with("f2048")), "{a:?}");
}

#[test]
fn inodes_counts_entries() {
    let (_d, root) = tree("inodes");
    let r = rows(&du(&["--inodes", "-a", root.to_str().unwrap()]));
    assert_eq!(size(&r, "root"), "10");
    assert_eq!(size(&r, "sub"), "5");
    assert_eq!(size(&r, "deep"), "2");
    assert_eq!(size(&r, "empty"), "1");
    assert_eq!(size(&r, "f5000"), "1");
}

#[test]
fn threshold_keeps_small_or_large_entries() {
    let (_d, root) = tree("thresh");
    // -t compares the base in force, as du's does, and the default base is now allocation,
    // where every file here occupies a whole 4 KiB block; -b pins the apparent sizes.
    let big = rows(&du(&["-a", "-b", "-t", "2048", root.to_str().unwrap()]));
    for kept in ["root", "sub", "deep", "sub/f5000", "sub/deep/f2048"] {
        assert!(at(&big, kept) < big.len(), "{kept} missing from {big:?}");
    }
    for gone in ["f1024", "f1000", "f1", "top1024", "empty"] {
        assert!(
            !big.iter().any(|(_, p)| p.ends_with(&format!("/{gone}"))),
            "{gone} should be filtered out of {big:?}"
        );
    }
    let small = rows(&du(&[
        "-a",
        "-b",
        "--threshold=-1024",
        root.to_str().unwrap(),
    ]));
    for kept in ["f1024", "f1000", "f1", "top1024", "empty"] {
        assert!(
            at(&small, kept) < small.len(),
            "{kept} missing from {small:?}"
        );
    }
    assert!(
        !small.iter().any(|(_, p)| p.ends_with("/root")),
        "{small:?}"
    );
}

#[test]
fn separate_dirs_reports_own_size_only() {
    let (_d, root) = tree("sep");
    let r = rows(&du(&[
        "-a",
        "-S",
        "--apparent-size",
        "-k",
        root.to_str().unwrap(),
    ]));
    assert_eq!(size(&r, "sub"), "6");
    assert_eq!(size(&r, "deep"), "2");
    // The directory's own files round to two blocks, as `du -S` reports for this fixture.
    assert_eq!(size(&r, "root"), "2");
}

#[test]
fn human_sizes_follow_du() {
    let (_d, root) = human_tree("human");
    let r = rows(&du(&[
        "-h",
        "-a",
        "--apparent-size",
        root.to_str().unwrap(),
    ]));
    assert_eq!(size(&r, "f4096"), "4.0K");
    assert_eq!(size(&r, "f10240"), "10K");
    assert_eq!(size(&r, "root"), "14K");
    let si = rows(&du(&[
        "--si",
        "-a",
        "--apparent-size",
        root.to_str().unwrap(),
    ]));
    assert_eq!(size(&si, "f4096"), "4.1k");
    assert_eq!(size(&si, "f10240"), "11k");
    assert_eq!(size(&si, "root"), "15k");
}

#[test]
fn hardlinks_count_once_and_stats_say_so() {
    let (_d, root) = tree("hard");
    let a = root.join("hard-a");
    let b = root.join("hard-b");
    fs::write(&a, vec![b'x'; 4096]).unwrap();
    if fs::hard_link(&a, &b).is_err() {
        eprintln!("hard links are not supported here; skipping");
        return;
    }
    let o = du(&["-a", "-b", "--stats", root.to_str().unwrap()]);
    let r = rows(&o);
    // One of the two names is charged, the other is dropped from the listing.
    let counted = [size_of(&r, "hard-a"), size_of(&r, "hard-b")];
    let sum: u64 = counted
        .iter()
        .flatten()
        .map(|s| s.parse::<u64>().unwrap())
        .sum();
    assert_eq!(counted.iter().filter(|v| v.is_some()).count(), 1, "{r:?}");
    assert_eq!(sum, 4096, "{r:?}");
    // root is the 11 blocks of the base tree plus one 4 KiB file.
    assert_eq!(size(&r, "root"), (10097 + 4096).to_string());
    assert!(
        stderr(&o).contains("hardlink names already counted"),
        "{}",
        stderr(&o)
    );
}

fn size_of(rows: &[(String, String)], rel: &str) -> Option<String> {
    let want = format!("/{rel}");
    rows.iter()
        .find(|(_, p)| p == rel || p.ends_with(&want))
        .map(|(s, _)| s.clone())
}

#[test]
fn dereference_is_refused_with_a_pointer_to_find() {
    let (_d, root) = tree("deref");
    let o = du(&["-L", root.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(2));
    let e = stderr(&o);
    assert!(e.contains("dusage find --follow"), "{e}");
    // The default, spelled out, is fine.
    assert!(du(&["-P", root.to_str().unwrap()]).status.success());
}

#[test]
fn time_prints_a_stamp_between_size_and_path() {
    let (_d, root) = tree("time");
    let s = ok(&du(&["--time", "-s", root.to_str().unwrap()]));
    let line = s.lines().next().unwrap();
    let fields: Vec<&str> = line.split('\t').collect();
    assert_eq!(fields.len(), 3, "{line}");
    assert!(fields[0].parse::<u64>().is_ok(), "{line}");
    let stamp = fields[1];
    assert_eq!(stamp.len(), 16, "{line}");
    assert_eq!(stamp.as_bytes()[4], b'-', "{line}");
    assert_eq!(stamp.as_bytes()[10], b' ', "{line}");
    assert_eq!(stamp.as_bytes()[13], b':', "{line}");
    // `iso` is the date alone, which is what GNU du prints for `--time-style=iso`.
    let iso = ok(&du(&[
        "--time",
        "--time-style=iso",
        "-s",
        root.to_str().unwrap(),
    ]));
    let stamp = iso.lines().next().unwrap().split('\t').nth(1).unwrap();
    assert_eq!(stamp.len(), 10, "{stamp}");
    assert_eq!(stamp.as_bytes()[4], b'-', "{stamp}");
    assert_eq!(stamp.as_bytes()[7], b'-', "{stamp}");
    // A custom format drops the constant parts it was not asked for.
    let c = ok(&du(&[
        "--time",
        "--time-style=+%Y/%m/%d",
        "-s",
        root.to_str().unwrap(),
    ]));
    assert_eq!(
        c.lines().next().unwrap().split('\t').nth(1).unwrap().len(),
        10
    );
}

#[test]
fn exclude_skips_matching_entries() {
    let (_d, root) = tree("excl");
    let r = rows(&du(&[
        "-a",
        "--apparent-size",
        "-k",
        "--exclude",
        "f*",
        root.to_str().unwrap(),
    ]));
    assert!(!r.iter().any(|(_, p)| p.contains("f1024")), "{r:?}");
    assert_eq!(size(&r, "sub"), "0");
    // top1024 does not match `f*`, so it is still counted.
    assert_eq!(size(&r, "root"), "1");
    let sub = rows(&du(&[
        "--apparent-size",
        "-k",
        "--exclude",
        "sub/deep",
        root.to_str().unwrap(),
    ]));
    assert!(at(&sub, "sub") < sub.len());
    assert!(!sub.iter().any(|(_, p)| p.ends_with("deep")), "{sub:?}");
    let from = root.join("patterns");
    fs::write(&from, "# a comment\n\nsub/f5000\n").unwrap();
    let f = rows(&du(&[
        "-a",
        "--apparent-size",
        "-k",
        "--exclude-from",
        from.to_str().unwrap(),
        root.to_str().unwrap(),
    ]));
    assert!(!f.iter().any(|(_, p)| p.ends_with("f5000")), "{f:?}");
    assert_eq!(size(&f, "sub"), "3");
    fs::remove_file(&from).unwrap();
}

#[cfg(unix)]
#[test]
fn unreadable_directory_is_reported_and_counts_zero() {
    use std::os::unix::fs::PermissionsExt;
    let (_d, root) = tree("deny");
    let locked = root.join("locked");
    fs::create_dir_all(&locked).unwrap();
    fs::write(locked.join("f"), vec![b'x'; 64]).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let o = du(&["-b", root.to_str().unwrap()]);
    let readable = o.status.success();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        eprintln!("this user can read an 0000 directory (root?); skipping");
        return;
    }
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("cannot read directory"),
        "{}",
        stderr(&o)
    );
    let r = rows_any(&o);
    assert_eq!(size(&r, "locked"), "0");
    assert_eq!(size(&r, "root"), "10097");
}

#[test]
fn missing_start_point_fails_but_keeps_the_others() {
    let (_d, root) = tree("missing");
    let gone = root.join("nope");
    let r = du(&["-s", root.to_str().unwrap(), gone.to_str().unwrap()]);
    assert_eq!(r.status.code(), Some(1));
    assert!(stderr(&r).contains("cannot access"), "{}", stderr(&r));
    let s = ok(&du(&[
        "-s",
        root.to_str().unwrap(),
        root.join("sub").to_str().unwrap(),
    ]));
    assert_eq!(s.lines().count(), 2, "{s}");
}

#[test]
fn help_lists_the_mode_and_no_short_help_flag() {
    let h = ok(&du(&["--help"]));
    for needle in [
        "--max-depth",
        "--inodes",
        "--threshold",
        "--exclude",
        "--stats",
        "--block-size",
    ] {
        assert!(h.contains(needle), "{needle} missing from help");
    }
    // -h is human-readable, so it sizes rather than printing help.
    let (_d, root) = tree("helpsize");
    let r = ok(&du(&["-h", "-s", root.to_str().unwrap()]));
    assert!(
        r.trim_end()
            .ends_with(&root.to_string_lossy().replace('\\', "/"))
            || r.contains("K"),
        "{r}"
    );
    assert!(!r.contains("USAGE"), "-h must not print the help");
}

#[test]
fn thread_count_never_changes_the_output() {
    let (_d, root) = tree("threads");
    let a = ok(&du(&["-a", "--threads", "1", root.to_str().unwrap()]));
    let b = ok(&du(&["-a", "--threads", "8", root.to_str().unwrap()]));
    assert_eq!(a, b);
}

/// Guard against a start point being walked as a file: `du <file>` is one line.
#[test]
fn a_file_start_point_is_one_line() {
    let (_d, root) = tree("file");
    let f = root.join("f5000").with_file_name("sub").join("f5000");
    let r = rows(&du(&["-b", f.to_str().unwrap()]));
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0].0, "5000");
    assert!(Path::new(&r[0].1).ends_with("sub/f5000"), "{r:?}");
}

/// A sparse file: 4 KiB, a hole past 1 MiB, then 4 KiB. Its `st_size` is over a megabyte and
/// the blocks it occupies are a couple of 4 KiB pages, which is the gap the default now
/// reports the way du does.
fn sparse_file(path: &Path) {
    let mut f = fs::File::create(path).unwrap();
    f.write_all(&[b'x'; 4096]).unwrap();
    f.seek(SeekFrom::Start(1 << 20)).unwrap();
    f.write_all(&[b'y'; 4096]).unwrap();
    f.sync_all().unwrap();
}

/// `root/{sparse,plain,sub/nested}`, the sparse file made at test time.
fn sparse_tree(tag: &str) -> (Dir, PathBuf) {
    let base = Dir::new(tag);
    let root = base.0.join("root");
    fs::create_dir_all(root.join("sub")).unwrap();
    sparse_file(&root.join("sparse"));
    fs::write(root.join("plain"), vec![b'p'; 2048]).unwrap();
    fs::write(root.join("sub/nested"), vec![b'n'; 512]).unwrap();
    (base, root)
}

/// The bytes one path occupies, which is what du sums.
#[cfg(unix)]
fn alloc_of(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path).unwrap().blocks() * 512
}

/// The bytes `dir` and everything under it occupies, itself included.
#[cfg(unix)]
fn tree_alloc(dir: &Path) -> u64 {
    let mut total = alloc_of(dir);
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        total += if p.is_dir() {
            tree_alloc(&p)
        } else {
            alloc_of(&p)
        };
    }
    total
}

/// GNU `du`'s own rows for the same arguments, when the system has one: the oracle the
/// default has to match, byte for byte after sorting. `None` where there is no `du`.
#[cfg(unix)]
fn gnu_du(args: &[&str]) -> Option<Vec<(String, String)>> {
    let o = Command::new("du").args(args).output().ok()?;
    o.status
        .success()
        .then(|| by_path(parse(&String::from_utf8_lossy(&o.stdout))))
}

/// Rows sorted by path, so two producers of the same numbers compare row for row.
#[cfg(unix)]
fn by_path(mut rows: Vec<(String, String)>) -> Vec<(String, String)> {
    rows.sort();
    rows
}

fn value(rows: &[(String, String)], rel: &str) -> u64 {
    size(rows, rel).parse().unwrap()
}

/// The default counts what a file occupies, not the length it claims, and it is du's number:
/// the sparse file is over a megabyte by `st_size` and two blocks on disk.
#[cfg(unix)]
#[test]
fn default_counts_allocated_blocks_and_matches_du() {
    let (_d, root) = sparse_tree("alloc");
    let r = rows(&du(&["-a", root.to_str().unwrap()]));
    let sparse = root.join("sparse");
    assert_eq!(
        value(&r, "sparse"),
        alloc_of(&sparse).div_ceil(1024),
        "{r:?}"
    );
    assert!(
        value(&r, "sparse") * 1024 < fs::metadata(&sparse).unwrap().len(),
        "the hole must not be charged: {r:?}"
    );
    for dir in ["root", "sub"] {
        let want = match dir {
            "root" => tree_alloc(&root),
            _ => tree_alloc(&root.join("sub")),
        };
        assert_eq!(value(&r, dir), want.div_ceil(1024), "{dir} in {r:?}");
    }
    let want = gnu_du(&["-a", root.to_str().unwrap()]);
    if let Some(want) = want {
        assert_eq!(by_path(r), want);
    } else {
        eprintln!("no du on this system; compared against st_blocks only");
    }
}

/// A file of nothing but holes occupies nothing where the filesystem reports blocks, and du
/// charges it nothing. The apparent size still reports what it claims to hold.
#[cfg(unix)]
#[test]
fn a_file_of_only_holes_costs_nothing() {
    let (_d, root) = sparse_tree("holes");
    let f = root.join("holes");
    fs::File::create(&f).unwrap().set_len(1 << 20).unwrap();
    if alloc_of(&f) != 0 {
        eprintln!("this filesystem allocates a block for a hole; skipping");
        return;
    }
    let r = rows(&du(&["-a", root.to_str().unwrap()]));
    assert_eq!(size(&r, "holes"), "0", "{r:?}");
    let b = rows(&du(&["-a", "-b", root.to_str().unwrap()]));
    assert_eq!(size(&b, "holes"), (1 << 20).to_string(), "{b:?}");
    if let Some(want) = gnu_du(&["-a", root.to_str().unwrap()]) {
        assert_eq!(by_path(r), want);
    }
}

/// Two names for one inode are charged once, in the allocated base as in the apparent one.
#[test]
fn hardlinks_are_charged_once_in_the_default_base() {
    let (_d, root) = sparse_tree("hardalloc");
    let a = root.join("pair-a");
    fs::write(&a, vec![b'x'; 8192]).unwrap();
    if fs::hard_link(&a, root.join("pair-b")).is_err() {
        eprintln!("hard links are not supported here; skipping");
        return;
    }
    let both = rows(&du(&["-s", root.to_str().unwrap()]));
    let listed = rows(&du(&["-a", root.to_str().unwrap()]));
    assert_eq!(
        [size_of(&listed, "pair-a"), size_of(&listed, "pair-b")]
            .iter()
            .filter(|v| v.is_some())
            .count(),
        1,
        "one of the two names is dropped: {listed:?}"
    );
    fs::remove_file(root.join("pair-b")).unwrap();
    assert_eq!(both, rows(&du(&["-s", root.to_str().unwrap()])));
}

/// `--block-size` divides the base in force, and `-b` is `--block-size=1` of it.
#[test]
fn block_size_scales_the_apparent_base() {
    let (_d, root) = tree("bsize");
    let r = rows(&du(&[
        "-a",
        "-b",
        "--block-size=4096",
        root.to_str().unwrap(),
    ]));
    assert_eq!(size(&r, "f1"), "1");
    assert_eq!(size(&r, "f1000"), "1");
    assert_eq!(size(&r, "f5000"), "2");
    assert_eq!(size(&r, "sub"), "2");
    assert_eq!(size(&r, "root"), "3");
    let one = rows(&du(&["-a", "-b", "--block-size=1", root.to_str().unwrap()]));
    assert_eq!(one, rows(&du(&["-a", "-b", root.to_str().unwrap()])));
}

/// The same division on the allocated base, which is now the default.
#[cfg(unix)]
#[test]
fn block_size_scales_the_allocated_base() {
    let (_d, root) = sparse_tree("bsizealloc");
    let r = rows(&du(&["-a", "--block-size=4096", root.to_str().unwrap()]));
    assert_eq!(value(&r, "root"), tree_alloc(&root).div_ceil(4096), "{r:?}");
    assert_eq!(
        value(&r, "sparse"),
        alloc_of(&root.join("sparse")).div_ceil(4096),
        "{r:?}"
    );
    if let Some(want) = gnu_du(&["-a", "--block-size=4096", root.to_str().unwrap()]) {
        assert_eq!(by_path(r), want);
    }
}

/// A bare unit in `--block-size` labels the number, as `du --block-size=KiB` prints `23KiB`;
/// one with a number in it prints a plain number.
#[test]
fn block_size_labels_a_bare_unit() {
    let (_d, root) = tree("bsizeunit");
    let r = rows(&du(&[
        "-a",
        "-b",
        "--block-size=KiB",
        root.to_str().unwrap(),
    ]));
    assert_eq!(size(&r, "f1000"), "1KiB", "{r:?}");
    assert_eq!(size(&r, "f5000"), "5KiB", "{r:?}");
    let k = rows(&du(&["-a", "-b", "--block-size=K", root.to_str().unwrap()]));
    assert_eq!(size(&k, "f5000"), "5K", "{k:?}");
    let plain = rows(&du(&[
        "-a",
        "-b",
        "--block-size=1024",
        root.to_str().unwrap(),
    ]));
    assert_eq!(size(&plain, "f5000"), "5", "{plain:?}");
    let si = rows(&du(&[
        "-a",
        "-b",
        "--block-size=KB",
        root.to_str().unwrap(),
    ]));
    assert_eq!(size(&si, "f5000"), "5kB", "{si:?}");
}

#[test]
fn block_size_of_zero_is_refused() {
    let (_d, root) = tree("bsizezero");
    let o = du(&["--block-size=0", root.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(2));
    assert!(stderr(&o).contains("--block-size"), "{}", stderr(&o));
    let bad = du(&["--block-size=nonsense", root.to_str().unwrap()]);
    assert_eq!(bad.status.code(), Some(2));
}

/// `-b` keeps reporting the bytes a sparse file claims, whatever the default now counts.
#[test]
fn bytes_mode_reports_a_sparse_file_whole() {
    let (_d, root) = sparse_tree("sparsebytes");
    let b = rows(&du(&["-a", "-b", root.to_str().unwrap()]));
    assert_eq!(size(&b, "sparse"), ((1 << 20) + 4096).to_string(), "{b:?}");
    let d = rows(&du(&["-a", root.to_str().unwrap()]));
    assert_ne!(size(&d, "sparse"), size(&b, "sparse"), "{d:?}");
}

/// `-h` and `--si` scale whichever base is in force, so on the allocated base the sparse
/// file reads in kilobytes rather than the megabyte it claims.
#[cfg(unix)]
#[test]
fn human_and_si_follow_the_allocated_base() {
    let (_d, root) = sparse_tree("humanalloc");
    let h = rows(&du(&["-a", "-h", root.to_str().unwrap()]));
    let b = rows(&du(&["-a", "-h", "-b", root.to_str().unwrap()]));
    assert_eq!(size(&b, "sparse"), "1.1M", "{b:?}");
    assert_eq!(
        size(&h, "sparse"),
        format!(
            "{:.1}K",
            (alloc_of(&root.join("sparse")) as f64 / 1024.0 * 10.0).ceil() / 10.0
        ),
        "{h:?}"
    );
    let si = rows(&du(&["-a", "--si", root.to_str().unwrap()]));
    assert!(
        size(&si, "sparse").ends_with('k') && size(&si, "sparse") != "1.0k",
        "{si:?}"
    );
    if let Some(want) = gnu_du(&["-a", "-h", root.to_str().unwrap()]) {
        assert_eq!(by_path(h), want);
    }
    if let Some(want) = gnu_du(&["-a", "--si", root.to_str().unwrap()]) {
        assert_eq!(by_path(si), want);
    }
}

/// A subdirectory's own blocks are counted, from the listing that already stat'ed it.
#[cfg(unix)]
#[test]
fn a_child_directory_own_blocks_are_counted() {
    let (_d, root) = sparse_tree("dirblocks");
    let many = root.join("many");
    fs::create_dir_all(&many).unwrap();
    for i in 0..300 {
        fs::write(many.join(format!("f{i:03}")), vec![b'q'; 64]).unwrap();
    }
    let own = alloc_of(&many);
    if own == 0 {
        eprintln!("this filesystem stores a small directory inline; skipping");
        return;
    }
    let files: u64 = fs::read_dir(&many)
        .unwrap()
        .map(|e| alloc_of(&e.unwrap().path()))
        .sum();
    let r = rows(&du(&["-s", many.to_str().unwrap()]));
    assert_eq!(r[0].0, (files + own).div_ceil(1024).to_string(), "{r:?}");
    assert_ne!(
        r[0].0,
        files.div_ceil(1024).to_string(),
        "the directory's own blocks must move the number: {r:?}"
    );
    if let Some(want) = gnu_du(&["-s", many.to_str().unwrap()]) {
        assert_eq!(r, want);
    }
}

/// A start point's own blocks are counted too, from the metadata the run already has.
#[cfg(unix)]
#[test]
fn a_start_point_own_blocks_are_counted() {
    let (_d, root) = sparse_tree("rootblocks");
    for i in 0..300 {
        fs::write(root.join(format!("f{i:03}")), vec![b'q'; 64]).unwrap();
    }
    let own = alloc_of(&root);
    if own == 0 {
        eprintln!("this filesystem stores a small directory inline; skipping");
        return;
    }
    let r = rows(&du(&["-s", root.to_str().unwrap()]));
    assert_eq!(
        r[0].0,
        tree_alloc(&root).div_ceil(1024).to_string(),
        "{r:?}"
    );
    assert_ne!(
        r[0].0,
        (tree_alloc(&root) - own).div_ceil(1024).to_string(),
        "the start point's own blocks must move the number: {r:?}"
    );
    if let Some(want) = gnu_du(&["-s", root.to_str().unwrap()]) {
        assert_eq!(r, want);
    }
}

/// `-S` subtracts the children and keeps the directory itself, its own blocks included.
/// `--block-size=1` prints allocated bytes without rounding, so the arithmetic is exact.
#[test]
fn separate_dirs_excludes_children_in_the_default_base() {
    let (_d, root) = sparse_tree("sepalloc");
    fs::create_dir_all(root.join("other")).unwrap();
    fs::write(root.join("other/f"), vec![b'o'; 4096]).unwrap();
    let all = rows(&du(&["-a", "--block-size=1", root.to_str().unwrap()]));
    let sep = rows(&du(&["-a", "-S", "--block-size=1", root.to_str().unwrap()]));
    for leaf in ["sub", "other"] {
        assert_eq!(size(&sep, leaf), size(&all, leaf), "{leaf} in {sep:?}");
    }
    assert_eq!(
        value(&sep, "root") + value(&all, "sub") + value(&all, "other"),
        value(&all, "root"),
        "{sep:?} vs {all:?}"
    );
    assert!(value(&sep, "root") < value(&all, "root"), "{sep:?}");
}

/// The system `du` is the oracle on a Mac too, and it is BSD du there: `-sk` prints one
/// line per argument in 1 KiB blocks, the unit `dusage du -k` prints. A host without
/// `du(1)` skips with a reason rather than asserting nothing.
#[cfg(target_os = "macos")]
#[test]
fn macos_default_totals_match_the_system_du_on_usr() {
    let Some(want_rows) = gnu_du(&["-sk", "/usr"]) else {
        eprintln!("skipped: no du(1) on this host");
        return;
    };
    let want = value(&want_rows, "/usr");
    let got = value(&rows(&du(&["-k", "-s", "/usr"])), "/usr");
    assert_eq!(got, want, "dusage {got} KiB vs du {want} KiB over /usr");
}

/// A mount point inside a walked tree is the device boundary `--one-filesystem` and the
/// prune both rely on. `/System/Volumes` really does span more than one device on macOS,
/// so `-x` on both tools exercises `other_filesystem` against a real boundary: if either
/// crossed it, the totals would include the Data volume and differ by orders of magnitude.
///
/// What this does not cover: a delete-side mount inside a tree it can delete needs root
/// to create, so `same_filesystem`'s use by the prune walk is covered by its unit tests
/// and not here.
#[cfg(target_os = "macos")]
#[test]
fn macos_the_device_boundary_is_honoured_on_a_real_mount() {
    use std::os::unix::fs::MetadataExt;
    let root = Path::new("/System/Volumes");
    if !root.is_dir() {
        eprintln!("skipped: /System/Volumes does not exist on this host");
        return;
    }
    let here = std::fs::metadata(std::env::temp_dir()).map(|m| m.dev());
    let Ok(dev) = here.map(|d| (d, std::fs::metadata(root).map(|m| m.dev()))) else {
        eprintln!("skipped: cannot read device ids");
        return;
    };
    let (temp_dev, volumes_dev) = (dev.0, dev.1.ok());
    let mut second = volumes_dev.is_some_and(|d| d != temp_dev);
    if !second {
        for e in std::fs::read_dir(root).into_iter().flatten().flatten() {
            if std::fs::metadata(e.path()).is_ok_and(|m| m.dev() != temp_dev) {
                second = true;
                break;
            }
        }
    }
    if !second {
        eprintln!("skipped: no second device to compare against");
        return;
    }
    let Some(want_rows) = gnu_du(&["-sk", "-x", "/System/Volumes"]) else {
        eprintln!("skipped: no du(1) on this host");
        return;
    };
    let want = value(&want_rows, "/System/Volumes");
    let got = value(
        &rows(&du(&["-k", "-s", "-x", "/System/Volumes"])),
        "/System/Volumes",
    );
    assert_eq!(
        got, want,
        "-x over /System/Volumes: dusage {got} KiB vs du {want} KiB"
    );
}
