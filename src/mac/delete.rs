//! Deletion for macOS. A symlink is unlinked as a link (never followed), a directory is
//! removed with `rmdir` so a non-empty result is reported instead of silently recursing,
//! and every other entry is unlinked.
//!
//! Both calls are the path-based libSystem ones, not `unlinkat`: `delete_entry` is handed
//! a full path and no directory descriptor, so a relative call would mean opening the
//! parent on every entry purely to drop the descriptor again. `unlink` removes the link
//! itself and `rmdir` removes the directory itself, which is the property the invariant
//! needs; neither follows a symlink.

use crate::mac::enumerate::DirReader;
use crate::model::ErrorCounts;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

fn c_path(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// Remove one entry without following it.
pub fn delete_entry(path: &Path) -> io::Result<()> {
    let c = c_path(path)?;
    // `symlink_metadata` is `lstat`, so a symlink to a directory is removed as a link.
    let is_dir = fs::symlink_metadata(path)?.is_dir();
    // SAFETY: `c` is NUL-terminated and outlives the call; both functions take a path and
    // return -1 with errno set on failure.
    let rc = unsafe {
        if is_dir {
            crate::mac::sys::rmdir(c.as_ptr())
        } else {
            crate::mac::sys::unlink(c.as_ptr())
        }
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Recursive delete that never descends into symlinks. Per-entry errors are counted and
/// the walk continues. Returns (deleted entries, errors).
pub fn remove_tree(path: &Path) -> (u64, ErrorCounts) {
    enum Item {
        Open(PathBuf),
        Close(PathBuf),
    }
    let mut deleted = 0u64;
    let mut errs = ErrorCounts::default();
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            // A link or a file is removed as itself, never as its target.
            match delete_entry(path) {
                Ok(()) => return (1, errs),
                Err(e) => {
                    errs.record(&e);
                    return (0, errs);
                }
            }
        }
        Err(e) => {
            errs.record(&e);
            return (0, errs);
        }
    }
    let mut reader = DirReader::new();
    let mut stack = vec![Item::Open(path.to_path_buf())];
    while let Some(item) = stack.pop() {
        match item {
            Item::Close(dir) => match fs::remove_dir(&dir) {
                Ok(()) => deleted += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    errs.record(&e);
                }
            },
            Item::Open(dir) => {
                let mut ents = Vec::new();
                match reader.read(&dir, &mut ents, &mut errs) {
                    Ok(()) => {}
                    Err(e) => {
                        if e.kind() != io::ErrorKind::NotFound {
                            errs.record(&e);
                        }
                        continue;
                    }
                }
                stack.push(Item::Close(dir.clone()));
                for e in &ents {
                    let p = dir.join(&e.name);
                    if e.is_dir && !e.is_reparse() {
                        stack.push(Item::Open(p));
                    } else {
                        match delete_entry(&p) {
                            Ok(()) => deleted += 1,
                            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                            Err(e) => {
                                errs.record(&e);
                            }
                        }
                    }
                }
            }
        }
    }
    (deleted, errs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp(tag: &str) -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "dusage_mac_del_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn removes_tree_and_counts() {
        let base = tmp("tree");
        let root = base.join("t");
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::write(root.join("a/b/f"), b"x").unwrap();
        fs::write(root.join("g"), b"x").unwrap();
        let (n, errs) = remove_tree(&root);
        assert_eq!(errs.total(), 0, "{errs:?}");
        assert_eq!(n, 5, "f, g, a/b, a and the root itself");
        assert!(!root.exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn symlink_unlinked_target_survives() {
        let base = tmp("link");
        let target = base.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), b"x").unwrap();
        let tree = base.join("tree");
        fs::create_dir(&tree).unwrap();
        std::os::unix::fs::symlink(&target, tree.join("sl")).unwrap();
        std::os::unix::fs::symlink(target.join("keep"), tree.join("sf")).unwrap();
        let (n, errs) = remove_tree(&tree);
        assert_eq!(errs.total(), 0, "{errs:?}");
        assert_eq!(n, 3, "sf, sl, tree");
        assert!(!tree.exists());
        assert!(target.join("keep").exists(), "target must survive");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn delete_entry_removes_the_link_not_its_target() {
        let base = tmp("one");
        let target = base.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep"), b"x").unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        delete_entry(&link).unwrap();
        assert!(!link.exists());
        assert!(target.join("keep").exists(), "target must survive");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn non_empty_directory_reports_not_empty() {
        let base = tmp("notempty");
        let d = base.join("d");
        fs::create_dir(&d).unwrap();
        fs::write(d.join("f"), b"x").unwrap();
        let e = delete_entry(&d).unwrap_err();
        assert_eq!(
            crate::model::ErrorKind::of(&e),
            crate::model::ErrorKind::DirNotEmpty
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn missing_entry_is_not_found() {
        let base = tmp("missing");
        let e = delete_entry(&base.join("nope")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        let _ = fs::remove_dir_all(&base);
    }
}
