//! End-to-end tests of the `fastdel` binary. Fixtures live under the temp dir only.
use std::fs;
#[cfg(windows)]
use std::fs::OpenOptions;
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime};

static N: AtomicU32 = AtomicU32::new(0);

fn fixture(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "dusage_fdcli_{tag}_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

fn fastdel(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fastdel"))
        .args(args)
        .output()
        .unwrap()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn make_old(p: &Path) {
    let t = SystemTime::now() - Duration::from_secs(10 * 86400);
    #[cfg(windows)]
    {
        let mut o = OpenOptions::new();
        // FILE_WRITE_ATTRIBUTES + FILE_FLAG_BACKUP_SEMANTICS opens files and directories alike.
        o.access_mode(0x100).custom_flags(0x0200_0000);
        o.open(p).unwrap().set_modified(t).unwrap();
    }
    #[cfg(target_os = "linux")]
    {
        // Opening a directory read-only is enough to set its times when we own it.
        fs::File::open(p).unwrap().set_modified(t).unwrap();
    }
}

/// A directory link: a junction on Windows, a symlink on Linux.
fn link_dir(link: &Path, target: &Path) {
    #[cfg(windows)]
    {
        let o = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(o.status.success(), "mklink failed");
    }
    #[cfg(target_os = "linux")]
    {
        std::os::unix::fs::symlink(target, link).unwrap();
    }
}

fn cleanup(p: &Path) {
    let _ = fs::remove_dir_all(p);
}

#[test]
fn dry_deletes_nothing_and_lists() {
    let root = fixture("dry");
    fs::write(root.join("a.txt"), b"x").unwrap();
    fs::create_dir(root.join("adir")).unwrap();
    let o = fastdel(&[s(&root), "-1", "--dry"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(out.contains("a.txt") && out.contains("adir"), "{out}");
    assert!(root.join("a.txt").exists() && root.join("adir").exists());
    let o = fastdel(&[s(&root), "--age", "-1", "--dry", "--limit", "1"]);
    assert!(String::from_utf8_lossy(&o.stdout).contains("and 1 more"));
    cleanup(&root);
}

#[test]
fn prefix_filter_keeps_others() {
    let root = fixture("prefix");
    for n in ["tmp_a", "tmp_b", "log_c", "other"] {
        fs::create_dir(root.join(n)).unwrap();
        fs::write(root.join(n).join("f"), b"x").unwrap();
    }
    let o = fastdel(&[s(&root), "-1", "tmp_,log_"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(!root.join("tmp_a").exists() && !root.join("tmp_b").exists());
    assert!(!root.join("log_c").exists());
    assert!(root.join("other/f").exists());
    // Case-sensitive, flag form, repeatable.
    fs::create_dir(root.join("Tmp_z")).unwrap();
    fs::create_dir(root.join("x1")).unwrap();
    let o = fastdel(&[s(&root), "--age", "-1", "--prefix", "tmp_", "--prefix", "x"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(root.join("Tmp_z").exists() && !root.join("x1").exists());
    cleanup(&root);
}

#[test]
fn age_filter_keeps_fresh() {
    let root = fixture("age");
    fs::write(root.join("old.txt"), b"x").unwrap();
    fs::write(root.join("fresh.txt"), b"x").unwrap();
    fs::create_dir(root.join("old_dir")).unwrap();
    fs::create_dir(root.join("fresh_dir")).unwrap();
    make_old(&root.join("old.txt"));
    make_old(&root.join("old_dir"));
    let o = fastdel(&[s(&root), "7d"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(!root.join("old.txt").exists() && !root.join("old_dir").exists());
    assert!(root.join("fresh.txt").exists() && root.join("fresh_dir").exists());
    cleanup(&root);
}

#[test]
fn deep_wide_tree_removed() {
    let root = fixture("deep");
    let victim = root.join("victim");
    for a in 0..50 {
        for b in 0..3 {
            let d = victim.join(format!("a{a}")).join(format!("b{b}"));
            fs::create_dir_all(&d).unwrap();
            for f in 0..10 {
                fs::write(d.join(format!("f{f}")), b"x").unwrap();
            }
        }
    }
    let o = fastdel(&[s(&root), "-1", "--threads", "8"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(!victim.exists());
    // 1 victim + 50 a + 150 b + 1500 files
    assert!(stderr(&o).contains("1701 entries"), "{}", stderr(&o));
    cleanup(&root);
}

#[test]
fn junctions_unlinked_not_followed() {
    let root = fixture("junc");
    let keep = fixture("junc_keep");
    fs::write(keep.join("keep.txt"), b"k").unwrap();
    link_dir(&root.join("link"), &keep); // junction as a candidate
    let inner = root.join("inner");
    fs::create_dir(&inner).unwrap();
    fs::write(inner.join("f"), b"f").unwrap();
    link_dir(&inner.join("jl"), &keep); // junction inside a candidate
    let o = fastdel(&[s(&root), "-1"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(fs::symlink_metadata(root.join("link")).is_err());
    assert!(!inner.exists());
    assert!(keep.join("keep.txt").exists());
    cleanup(&root);
    cleanup(&keep);
}

#[test]
fn readonly_file_removed() {
    let root = fixture("ro");
    let f = root.join("ro.txt");
    fs::write(&f, b"x").unwrap();
    let mut perm = fs::metadata(&f).unwrap().permissions();
    perm.set_readonly(true);
    fs::set_permissions(&f, perm).unwrap();
    let o = fastdel(&[s(&root), "-1"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(!f.exists());
    cleanup(&root);
}

#[test]
fn bad_args_exit_2() {
    let root = fixture("bad");
    fs::write(root.join("f"), b"x").unwrap();
    for args in [
        vec![s(&root), "7x"],
        vec![s(&root)],
        vec![s(&root), "1d", "--bogus"],
        vec![s(&root), "1d", "x", "0"],
        vec![],
    ] {
        let o = fastdel(&args);
        assert_eq!(code(&o), 2, "{args:?}: {}", stderr(&o));
    }
    assert!(root.join("f").exists());
    cleanup(&root);
}

#[test]
fn protected_roots_refused() {
    #[cfg(windows)]
    let mut roots = vec!["C:\\".to_string()];
    #[cfg(target_os = "linux")]
    let mut roots = vec!["/".to_string()];
    #[cfg(windows)]
    roots.extend(std::env::var("USERPROFILE"));
    #[cfg(target_os = "linux")]
    roots.extend(std::env::var("HOME").ok());
    for r in &roots {
        // The prefix matches nothing, so even a guard bug could not delete real data.
        for extra in [&["--dry"][..], &[][..]] {
            let mut args = vec![r.as_str(), "-1", "zzz_no_such_prefix_"];
            args.extend(extra);
            let o = fastdel(&args);
            assert_eq!(code(&o), 2, "{r} {extra:?}: {}", stderr(&o));
            assert!(stderr(&o).contains("refusing"), "{}", stderr(&o));
        }
    }
}

#[test]
fn log_file_contents() {
    let root = fixture("log");
    let logdir = fixture("log_out");
    let log = logdir.join("run.log");
    fs::create_dir(root.join("d1")).unwrap();
    fs::write(root.join("d1/f"), b"x").unwrap();
    fs::write(root.join("f2"), b"x").unwrap();
    let o = fastdel(&[s(&root), "-1", "--dry", "--log", s(&log)]);
    assert_eq!(code(&o), 0);
    let t = fs::read_to_string(&log).unwrap();
    assert!(
        t.contains("DRY ") && t.contains("d1") && t.contains("SUMMARY"),
        "{t}"
    );
    assert!(root.join("d1").exists());
    let o = fastdel(&[s(&root), "-1", "--log", s(&log)]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    let t = fs::read_to_string(&log).unwrap(); // appended, not truncated
    assert_eq!(t.matches("DRY ").count(), 2);
    assert_eq!(t.matches("DELETED ").count(), 2, "{t}");
    assert_eq!(t.matches("SUMMARY").count(), 2);
    cleanup(&root);
    cleanup(&logdir);
}

#[test]
fn no_arguments_exit_2() {
    let o = fastdel(&[]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
}

#[test]
fn empty_flag_prefix_is_an_error_not_match_all() {
    let root = fixture("emptypfx");
    fs::write(root.join("keep.txt"), b"x").unwrap();
    for args in [
        vec![s(&root), "--age", "-1", "--prefix", "", "--dry"],
        vec![s(&root), "--age", "-1", "--prefix", ","],
    ] {
        let o = fastdel(&args);
        assert_eq!(code(&o), 2, "{args:?}: {}", stderr(&o));
        assert!(stderr(&o).contains("prefix"), "{}", stderr(&o));
    }
    assert!(root.join("keep.txt").exists());
    cleanup(&root);
}

#[test]
fn empty_positional_prefix_matches_all_for_back_compat() {
    // README/EXAMPLE form: fastdel <root> <age> "" <threads>
    let root = fixture("emptyposp");
    fs::write(root.join("a.txt"), b"x").unwrap();
    fs::write(root.join("b.txt"), b"x").unwrap();
    let o = fastdel(&[s(&root), "-1", "", "4", "--dry"]);
    assert_eq!(code(&o), 0, "{}", stderr(&o));
    assert!(stderr(&o).contains("2 candidates"), "{}", stderr(&o));
    assert!(root.join("a.txt").exists() && root.join("b.txt").exists());
    cleanup(&root);
}

#[test]
fn reparse_root_refused() {
    let base = fixture("rroot");
    let real = base.join("real");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("f"), b"x").unwrap();
    let link = base.join("lnk");
    link_dir(&link, &real);
    let o = fastdel(&[s(&link), "-1"]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    assert!(stderr(&o).contains("reparse point"), "{}", stderr(&o));
    assert!(real.join("f").exists());
    let _ = fs::remove_dir(&link);
    cleanup(&base);
}

#[cfg(target_os = "linux")]
#[test]
fn symlink_to_protected_dir_refused() {
    let base = fixture("lnkprot");
    let link = base.join("etc");
    link_dir(&link, Path::new("/etc"));
    let o = fastdel(&[s(&link), "-1", "zzz_no_such_prefix_", "--dry"]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    assert!(stderr(&o).contains("refusing"), "{}", stderr(&o));
    let _ = fs::remove_dir_all(&base);
}

#[cfg(windows)]
#[test]
fn junction_to_protected_target_refused() {
    let Some(win) = std::env::var_os("SystemRoot") else {
        return;
    };
    let base = fixture("jprot");
    let link = base.join("lnk");
    link_dir(&link, Path::new(&win));
    let o = fastdel(&[s(&link), "-1", "zzz_no_such_prefix_", "--dry"]);
    assert_eq!(code(&o), 2, "{}", stderr(&o));
    assert!(stderr(&o).contains("refusing"), "{}", stderr(&o));
    let _ = fs::remove_dir(&link);
    cleanup(&base);
}

#[cfg(windows)]
#[test]
fn loopback_admin_share_refused() {
    for r in [r"\\localhost\C$\Windows", r"\\127.0.0.1\C$\Users"] {
        let o = fastdel(&[r, "-1", "zzz_no_such_prefix_", "--dry"]);
        assert_eq!(code(&o), 2, "{r}: {}", stderr(&o));
        assert!(stderr(&o).contains("refusing"), "{}", stderr(&o));
    }
}

#[test]
fn version_flag_prints_package_version() {
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_fastdel"))
        .arg("-V")
        .output()
        .unwrap();
    assert!(o.status.success());
    assert_eq!(
        String::from_utf8_lossy(&o.stdout).trim(),
        format!("fastdel {}", env!("CARGO_PKG_VERSION"))
    );
}
