//! macOS side of `dusage df`: `statfs` for the numbers, `getmntinfo` for the list.
//!
//! `statfs` answers size, free, available and inode counts for the filesystem holding a path,
//! and it also names the source and the type; the mount table only has to supply where that
//! filesystem is mounted. Both come from `sys`, so this module adds no FFI of its own.
//!
//! **Why `statfs` and not `statvfs`, which the Linux arm uses.** Darwin's `struct statvfs`
//! counts blocks in `unsigned int` (`__darwin_fsblkcnt_t` in `sys/_types.h`), so a volume
//! past 2^32 blocks — routine for APFS at 4 KiB — overflows the count entirely, and that
//! struct carries no type name either. `struct statfs` holds every counter as 64 bits and is
//! what BSD `df` reads, so a row from here and a row from `df` are the same arithmetic.
//! The pieces the caller sees are identical to Linux's: `f_bavail` and not `f_bfree` is what
//! an unprivileged caller may still write, `size - free` is used, and the inode triple is
//! `f_files`, `f_files - f_ffree`, `f_ffree`.

use super::sys;
use crate::mac::enumerate;
use std::ffi::CString;
use std::io;
use std::path::{Path, PathBuf};

/// One filesystem: the numbers and the names. The field set and its meanings are the same as
/// `unix::df::FsUsage`, because `src/cmd/df.rs` is one caller for three platforms.
#[derive(Clone, Debug)]
pub struct FsUsage {
    /// The device or source the mount table names: `/dev/disk3s1s1`, `devfs`, a map.
    pub filesystem: String,
    pub fs_type: Option<String>,
    pub mount: PathBuf,
    pub size: u64,
    pub used: u64,
    /// What an unprivileged caller may still write, which is not `size - used`.
    pub available: u64,
    pub inodes: Option<u64>,
    pub inodes_used: Option<u64>,
    pub inodes_free: Option<u64>,
    /// The device a mount lives on, the same `st_dev` the scanner compares with
    /// `same_filesystem`, taken from `stat` because `struct statfs` has none.
    pub device: u64,
}

fn c_path(p: &Path) -> io::Result<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path holds a NUL byte"))
}

/// One `statfs` record for `path`.
fn record_of(path: &Path) -> io::Result<sys::StatFs> {
    let c = c_path(path)?;
    // SAFETY: `c` is NUL-terminated and outlives the call, and `rec` is a live
    // `struct statfs` of exactly the size declared and asserted in `sys`.
    let mut rec: sys::StatFs = unsafe { std::mem::zeroed() };
    let rc = unsafe { sys::statfs(c.as_ptr(), &mut rec) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(rec)
}

fn name_of<const N: usize>(field: &[std::ffi::c_char; N]) -> String {
    // A field is bounded by its own width; anything without a terminator inside it is not a
    // name, so it reads as empty rather than as a truncation.
    sys::c_field(field)
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default()
}

/// One record as the shared shape. `mount` comes from the caller because only the mount table
/// knows where a filesystem is mounted.
fn usage_of_record(rec: &sys::StatFs, mount: PathBuf, device: u64) -> FsUsage {
    let bsize = u64::from(rec.f_bsize);
    let size = rec.f_blocks.saturating_mul(bsize);
    let free = rec.f_bfree.saturating_mul(bsize);
    let available = rec.f_bavail.saturating_mul(bsize);
    let inodes = (rec.f_files > 0).then_some((rec.f_files, rec.f_files - rec.f_ffree, rec.f_ffree));
    let (inodes, inodes_used, inodes_free) = match inodes {
        Some((a, b, c)) => (Some(a), Some(b), Some(c)),
        None => (None, None, None),
    };
    let fs_type = name_of(&rec.f_fstypename);
    FsUsage {
        filesystem: name_of(&rec.f_mntfromname),
        fs_type: (!fs_type.is_empty()).then_some(fs_type),
        mount,
        size,
        used: size - free.min(size),
        available,
        inodes,
        inodes_used,
        inodes_free,
        device,
    }
}

/// Every mounted filesystem, in mount-table order.
///
/// The list is not filtered by whether the source resolves to a device: a network mount's
/// source is a URL, and the synthetic ones (`devfs`, `map auto_home`) are not devices at all,
/// so dropping them would make the output quietly incomplete. A mount point that cannot be
/// stat-ed — a dead network mount — is left out rather than reported as zero bytes.
pub fn filesystems() -> io::Result<Vec<FsUsage>> {
    let mut out = Vec::new();
    for (mount, _) in sys::mounts() {
        let Ok(rec) = record_of(&mount) else {
            continue;
        };
        let device = enumerate::dev_of(&mount).unwrap_or(0);
        out.push(usage_of_record(&rec, mount, device));
    }
    Ok(out)
}

/// The filesystem holding `path`, with the mount point of the table entry whose path is the
/// longest prefix of it — the same rule Linux's arm applies to `/proc/self/mounts`.
pub fn usage_of(path: &Path) -> io::Result<FsUsage> {
    let rec = record_of(path)?;
    let abs = std::path::absolute(path)?;
    let mount = sys::mounts()
        .into_iter()
        .filter(|(m, _)| abs.starts_with(m))
        .max_by_key(|(m, _)| m.as_os_str().len())
        .map(|(m, _)| m)
        .unwrap_or_else(|| abs.clone());
    let device = enumerate::dev_of(&abs).unwrap_or(0);
    Ok(usage_of_record(&rec, mount, device))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_filesystem_is_reported() {
        let f = usage_of(Path::new("/")).unwrap();
        assert!(f.size > 0, "a root filesystem has a size");
        assert!(f.used <= f.size);
        assert!(f.available <= f.size);
        assert_eq!(f.mount, PathBuf::from("/"));
        assert!(
            f.fs_type.as_deref().is_some_and(|t| !t.is_empty()),
            "the root has no type name: {f:?}"
        );
        assert!(!f.filesystem.is_empty(), "the root has no source: {f:?}");
    }

    #[test]
    fn the_mount_table_names_the_root_and_reconciles() {
        let all = filesystems().unwrap();
        assert!(!all.is_empty(), "the kernel reported no mounts");
        assert!(
            all.iter().any(|f| f.mount == Path::new("/")),
            "the root is missing from the list"
        );
        for f in &all {
            assert!(f.used <= f.size, "{}: used above size", f.filesystem);
            assert!(
                f.mount.is_absolute(),
                "{}: mount point is not absolute",
                f.mount.display()
            );
        }
    }

    /// A `df` that answers zeros for a path that does not exist is worse than one that says
    /// so: the numbers look like a real filesystem with nothing on it.
    #[test]
    fn a_missing_path_is_an_error_and_not_zeros() {
        let err = usage_of(Path::new("/no/such/dusage/df/path")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err:?}");
    }

    /// The mount records are read at the offsets `sys` asserts; a struct bound four bytes off
    /// makes the mount point stop being a path, and this says so with the record attached.
    #[test]
    fn the_mount_records_read_as_paths_and_type_names() {
        for (mount, kind) in sys::mounts() {
            assert!(
                mount.is_absolute(),
                "mount point {:?} is not absolute",
                mount.display()
            );
            assert!(
                !kind.is_empty() && kind.chars().all(|c| c.is_ascii_alphanumeric()),
                "type name {:?} for {:?}",
                kind,
                mount.display()
            );
        }
    }
}
