//! Handle-based deletion that never follows reparse points. Owned by wave-1 lane `delete`.

use super::enumerate::DirReader;
use super::sys::{
    self, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_DISPOSITION_INFO_CLASS, FILE_DISPOSITION_INFO_EX_CLASS, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_WRITE_ATTRIBUTES, HANDLE,
};
use crate::model::ErrorCounts;
use std::ffi::c_void;
use std::io;
use std::os::windows::fs::MetadataExt;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};

/// FILE_INFO_BY_HANDLE_CLASS::FileBasicInfo.
const FILE_BASIC_INFO_CLASS: i32 = 0;

// FILE_DISPOSITION_INFORMATION_EX flags (ntddk.h).
const FILE_DISPOSITION_FLAG_DELETE: u32 = 0x1;
const FILE_DISPOSITION_FLAG_POSIX_SEMANTICS: u32 = 0x2;
const FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE: u32 = 0x10;

/// FILE_DISPOSITION_INFO_EX (winbase.h).
#[repr(C)]
struct FileDispositionInfoEx {
    flags: u32,
}

/// FILE_DISPOSITION_INFO (minwinbase.h); `BOOLEAN` is one byte.
#[repr(C)]
struct FileDispositionInfo {
    delete_file: u8,
}

/// FILE_BASIC_INFO (minwinbase.h): four LARGE_INTEGER times then DWORD attributes.
#[repr(C)]
struct FileBasicInfo {
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    file_attributes: u32,
}

fn raw(h: &OwnedHandle) -> HANDLE {
    h.as_raw_handle()
}

fn set_info<T>(h: &OwnedHandle, class: i32, v: &T) -> io::Result<()> {
    // SAFETY: `h` is a valid open handle; `v` points to a live #[repr(C)] struct of exactly
    // size_of::<T>() bytes matching the layout `class` expects.
    sys::check(unsafe {
        sys::SetFileInformationByHandle(
            raw(h),
            class,
            v as *const T as *const c_void,
            std::mem::size_of::<T>() as u32,
        )
    })
}

fn clear_readonly(h: &OwnedHandle) -> io::Result<()> {
    let mut bi = FileBasicInfo {
        creation_time: 0,
        last_access_time: 0,
        last_write_time: 0,
        change_time: 0,
        file_attributes: 0,
    };
    // SAFETY: `h` is valid; `bi` is a writable FILE_BASIC_INFO of the size passed.
    sys::check(unsafe {
        sys::GetFileInformationByHandleEx(
            raw(h),
            FILE_BASIC_INFO_CLASS,
            &mut bi as *mut FileBasicInfo as *mut c_void,
            std::mem::size_of::<FileBasicInfo>() as u32,
        )
    })?;
    if bi.file_attributes & FILE_ATTRIBUTE_READONLY == 0 {
        return Ok(());
    }
    let mut attrs = bi.file_attributes & !FILE_ATTRIBUTE_READONLY;
    if attrs == 0 {
        attrs = FILE_ATTRIBUTE_NORMAL;
    }
    // Zero time fields mean "leave unchanged".
    let set = FileBasicInfo {
        creation_time: 0,
        last_access_time: 0,
        last_write_time: 0,
        change_time: 0,
        file_attributes: attrs,
    };
    set_info(h, FILE_BASIC_INFO_CLASS, &set)
}

/// Delete one entry *without following* it: a junction/symlink is removed as a link.
/// A non-empty directory yields `ERROR_DIR_NOT_EMPTY`.
pub fn delete_entry(path: &Path) -> io::Result<()> {
    let h = sys::open(
        path,
        sys::DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
        FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
    )?;
    let ex = FileDispositionInfoEx {
        flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    match set_info(&h, FILE_DISPOSITION_INFO_EX_CLASS, &ex) {
        Ok(()) => Ok(()),
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(sys::ERROR_INVALID_PARAMETER) | Some(sys::ERROR_NOT_SUPPORTED)
            ) =>
        {
            clear_readonly(&h)?;
            set_info(
                &h,
                FILE_DISPOSITION_INFO_CLASS,
                &FileDispositionInfo { delete_file: 1 },
            )
        }
        Err(e) => Err(e),
    }
}

fn del(p: &Path, errs: &mut ErrorCounts, deleted: &mut u64) {
    match delete_entry(p) {
        Ok(()) => *deleted += 1,
        Err(e) => {
            errs.record(&e);
        }
    }
}

fn is_reparse(m: &std::fs::Metadata) -> bool {
    m.file_type().is_symlink() || m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// Recursive delete of `path` that never descends into reparse points (they are unlinked),
/// deepest-first and iterative. Per-entry errors are counted and the walk continues.
/// Returns (deleted_entries, errors).
pub fn remove_tree(path: &Path) -> (u64, ErrorCounts) {
    let mut deleted = 0u64;
    let mut errs = ErrorCounts::default();

    let root_meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            errs.record(&e);
            return (0, errs);
        }
    };
    if !root_meta.is_dir() || is_reparse(&root_meta) {
        del(path, &mut errs, &mut deleted);
        return (deleted, errs);
    }

    // (dir, children_already_handled)
    let mut stack: Vec<(PathBuf, bool)> = vec![(path.to_path_buf(), false)];
    let mut reader = DirReader::new();
    let mut ents = Vec::new();
    while let Some((dir, done)) = stack.pop() {
        if done {
            del(&dir, &mut errs, &mut deleted);
            continue;
        }
        ents.clear();
        if let Err(e) = reader.read(&dir, &mut ents, &mut errs) {
            errs.record(&e);
            continue;
        }
        stack.push((dir.clone(), true));
        for ent in &ents {
            let p = dir.join(&ent.name);
            if ent.is_dir() && !ent.is_reparse() {
                stack.push((p, false));
            } else {
                del(&p, &mut errs, &mut deleted);
            }
        }
    }
    (deleted, errs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::windows::fs::OpenOptionsExt;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "dusage_del_{}_{}_{}",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = remove_tree(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn make_readonly(f: &Path) {
        let mut perm = fs::metadata(f).unwrap().permissions();
        perm.set_readonly(true);
        fs::set_permissions(f, perm).unwrap();
    }

    fn mklink_j(link: &Path, target: &Path) {
        let st = Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(st.status.success(), "mklink failed");
    }

    #[test]
    fn struct_layouts() {
        assert_eq!(std::mem::size_of::<FileDispositionInfoEx>(), 4);
        assert_eq!(std::mem::size_of::<FileDispositionInfo>(), 1);
        assert_eq!(std::mem::offset_of!(FileBasicInfo, file_attributes), 32);
        assert_eq!(std::mem::align_of::<FileBasicInfo>(), 8);
        // Trailing padding to 8-byte alignment gives the documented 40-byte FILE_BASIC_INFO.
        assert_eq!(std::mem::size_of::<FileBasicInfo>(), 40);
    }

    #[test]
    fn readonly_file() {
        let d = tmp("ro");
        let f = d.join("ro.txt");
        fs::write(&f, b"x").unwrap();
        make_readonly(&f);
        delete_entry(&f).unwrap();
        assert!(!f.exists());
        delete_entry(&d).unwrap();
        assert!(!d.exists());
    }

    #[test]
    fn readonly_fallback_path() {
        let d = tmp("rofb");
        let f = d.join("ro.txt");
        fs::write(&f, b"x").unwrap();
        make_readonly(&f);
        let h = sys::open(
            &f,
            sys::DELETE | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES,
            0,
        )
        .unwrap();
        clear_readonly(&h).unwrap();
        set_info(
            &h,
            FILE_DISPOSITION_INFO_CLASS,
            &FileDispositionInfo { delete_file: 1 },
        )
        .unwrap();
        drop(h);
        assert!(!f.exists());
        remove_tree(&d);
    }

    #[test]
    fn nonempty_dir_reports_not_empty() {
        let d = tmp("ne");
        fs::write(d.join("a"), b"x").unwrap();
        let e = delete_entry(&d).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(sys::ERROR_DIR_NOT_EMPTY));
        remove_tree(&d);
    }

    #[test]
    fn nested_tree() {
        let d = tmp("nest");
        fs::create_dir_all(d.join("a/b/c")).unwrap();
        fs::create_dir_all(d.join("z")).unwrap();
        fs::write(d.join("a/1"), b"1").unwrap();
        fs::write(d.join("a/b/2"), b"2").unwrap();
        fs::write(d.join("a/b/c/3"), b"3").unwrap();
        let f = d.join("z/ro");
        fs::write(&f, b"r").unwrap();
        make_readonly(&f);
        let (n, errs) = remove_tree(&d);
        assert_eq!(errs.total(), 0, "{errs:?}");
        // dirs: root,a,b,c,z = 5; files: 4
        assert_eq!(n, 9);
        assert!(!d.exists());
    }

    #[test]
    fn missing_root_is_not_found() {
        let d = tmp("miss");
        let (n, errs) = remove_tree(&d.join("nope"));
        assert_eq!((n, errs.not_found), (0, 1));
        remove_tree(&d);
    }

    #[test]
    fn junction_not_followed() {
        let base = tmp("junc");
        let target = base.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep.txt"), b"keep").unwrap();
        let victim = base.join("victim");
        fs::create_dir_all(&victim).unwrap();
        fs::write(victim.join("f"), b"f").unwrap();
        mklink_j(&victim.join("link"), &target);
        let (n, errs) = remove_tree(&victim);
        assert_eq!(errs.total(), 0, "{errs:?}");
        assert_eq!(n, 3); // f, link, victim
        assert!(!victim.exists());
        assert!(target.join("keep.txt").exists());
        remove_tree(&base);
    }

    #[test]
    fn junction_as_root_unlinks_only_link() {
        let base = tmp("juncroot");
        let target = base.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep.txt"), b"keep").unwrap();
        let link = base.join("link");
        mklink_j(&link, &target);
        let (n, errs) = remove_tree(&link);
        assert_eq!((n, errs.total()), (1, 0));
        assert!(fs::symlink_metadata(&link).is_err());
        assert!(target.join("keep.txt").exists());
        remove_tree(&base);
    }

    #[test]
    fn dir_symlink_not_followed() {
        let base = tmp("sym");
        let target = base.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("keep.txt"), b"keep").unwrap();
        let victim = base.join("victim");
        fs::create_dir_all(&victim).unwrap();
        if std::os::windows::fs::symlink_dir(&target, victim.join("sl")).is_err() {
            eprintln!("skipped: cannot create symlink (needs Developer Mode)");
            remove_tree(&base);
            return;
        }
        let (n, errs) = remove_tree(&victim);
        assert_eq!((n, errs.total()), (2, 0), "{errs:?}");
        assert!(target.join("keep.txt").exists());
        remove_tree(&base);
    }

    #[test]
    fn long_path_tree() {
        let base = tmp("long");
        let mut deep = base.clone();
        let seg = "d".repeat(50);
        for _ in 0..8 {
            deep.push(&seg);
        }
        let verb = PathBuf::from(format!(r"\\?\{}", deep.display()));
        assert!(verb.as_os_str().len() > 260);
        fs::create_dir_all(&verb).unwrap();
        fs::write(verb.join("f.txt"), b"x").unwrap();
        let (n, errs) = remove_tree(&base);
        assert_eq!(errs.total(), 0, "{errs:?}");
        assert_eq!(n, 8 + 1 + 1); // 8 deep dirs + file + base
        assert!(!base.exists());
    }

    #[test]
    fn open_file_sharing_violation() {
        let d = tmp("busy");
        let f = d.join("busy.txt");
        fs::write(&f, b"x").unwrap();
        fs::write(d.join("free.txt"), b"y").unwrap();
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&f)
            .unwrap();
        let (n, errs) = remove_tree(&d);
        assert_eq!(errs.sharing_violation, 1, "{errs:?}");
        assert_eq!(n, 1); // only free.txt
        drop(held);
        let (n2, e2) = remove_tree(&d);
        assert_eq!((n2, e2.total()), (2, 0));
    }
}
