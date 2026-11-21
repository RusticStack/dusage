//! Linux side of `dusage df`: `statvfs` for the numbers, `/proc/self/mounts` for the list.
//!
//! `statvfs` answers size, free and inode counts for the filesystem holding a path; it does
//! not name the device, its type or where it is mounted. `/proc/self/mounts` is the table
//! that does, and it is the same table GNU df reads (its `fstab`-ordered output comes from
//! there too), so a `dusage df` row and a `df` row name the same filesystem, type and mount
//! point. One `statvfs` per mount point turns the table into rows.

use std::ffi::{c_char, c_int, c_ulong, CString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// `struct statvfs` from `bits/statvfs.h`: every count is a 64-bit word on the 64-bit
/// targets this crate builds for, and the tail is `f_type` plus spare words (glibc before
/// 2.39 called the whole tail spare — the size is the same either way).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct StatVfs {
    f_bsize: c_ulong,
    f_frsize: c_ulong,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_favail: u64,
    f_fsid: c_ulong,
    f_flag: c_ulong,
    f_namemax: c_ulong,
    f_type: u32,
    f_spare: [c_int; 5],
}

// `struct statvfs` as `bits/statvfs.h` lays it out for x86_64/aarch64 Linux.
const _: () = {
    assert!(std::mem::size_of::<StatVfs>() == 112);
    assert!(std::mem::offset_of!(StatVfs, f_bsize) == 0);
    assert!(std::mem::offset_of!(StatVfs, f_frsize) == 8);
    assert!(std::mem::offset_of!(StatVfs, f_blocks) == 16);
    assert!(std::mem::offset_of!(StatVfs, f_bfree) == 24);
    assert!(std::mem::offset_of!(StatVfs, f_bavail) == 32);
    assert!(std::mem::offset_of!(StatVfs, f_files) == 40);
    assert!(std::mem::offset_of!(StatVfs, f_ffree) == 48);
    assert!(std::mem::offset_of!(StatVfs, f_favail) == 56);
    assert!(std::mem::offset_of!(StatVfs, f_fsid) == 64);
    assert!(std::mem::offset_of!(StatVfs, f_flag) == 72);
    assert!(std::mem::offset_of!(StatVfs, f_namemax) == 80);
    assert!(std::mem::offset_of!(StatVfs, f_type) == 88);
};

extern "C" {
    fn statvfs(path: *const c_char, buf: *mut StatVfs) -> c_int;
}

/// One filesystem: the numbers and the names.
#[derive(Clone, Debug)]
pub struct FsUsage {
    /// The device or source the mount table names: `/dev/root`, `tmpfs`, `capy-drive`.
    pub filesystem: String,
    pub fs_type: Option<String>,
    pub mount: PathBuf,
    pub size: u64,
    pub used: u64,
    /// What an unprivileged caller may still write, which is not `size - used` on a
    /// filesystem that keeps reserved blocks.
    pub available: u64,
    pub inodes: Option<u64>,
    pub inodes_used: Option<u64>,
    pub inodes_free: Option<u64>,
    /// The device a mount lives on, the same `st_dev` the scanner compares with
    /// `same_filesystem`. Two mount points of one filesystem share it; two tmpfs mounts do
    /// not, which is why `df` lists them separately.
    pub device: u64,
}

/// The three byte counts one `statvfs` reports, plus its inode triple when the filesystem
/// keeps inodes at all.
type Counts = (u64, u64, u64, Option<(u64, u64, u64)>);

fn stat_of(path: &Path) -> io::Result<Counts> {
    let bytes = path.as_os_str().as_encoded_bytes();
    let c = CString::new(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path holds a NUL byte"))?;
    let mut buf = StatVfs::default();
    // SAFETY: `c` is a NUL-terminated path and `buf` is a valid, correctly sized, writable
    // `struct statvfs` for the call to fill.
    let rc = unsafe { statvfs(c.as_ptr(), &mut buf) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let frsize = if buf.f_frsize == 0 {
        buf.f_bsize
    } else {
        buf.f_frsize
    };
    let size = buf.f_blocks.saturating_mul(frsize);
    let free = buf.f_bfree.saturating_mul(frsize);
    let available = buf.f_bavail.saturating_mul(frsize);
    let inodes = (buf.f_files > 0).then_some((buf.f_files, buf.f_files - buf.f_ffree, buf.f_ffree));
    Ok((size, size - free.min(size), available, inodes))
}

/// One line of `/proc/self/mounts`: source, mount point, type.
struct MountEntry {
    source: String,
    mount: PathBuf,
    fs_type: String,
}

/// `/proc/self/mounts` escapes a space, a tab, a newline and a backslash as `\040`, `\011`,
/// `\012` and `\134`; a path with a space is otherwise unparseable, so they must be undone.
fn unescape(field: &str) -> String {
    let b = field.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 4 <= b.len() {
            let oct = &b[i + 1..i + 4];
            if oct.iter().all(|c| (b'0'..=b'7').contains(c)) {
                let v = (oct[0] - b'0') * 64 + (oct[1] - b'0') * 8 + (oct[2] - b'0');
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn mounts() -> io::Result<Vec<MountEntry>> {
    let text = fs::read_to_string("/proc/self/mounts")?;
    let mut out = Vec::new();
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let (Some(source), Some(mount), Some(fs_type)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        out.push(MountEntry {
            source: unescape(source),
            mount: PathBuf::from(unescape(mount)),
            fs_type: unescape(fs_type),
        });
    }
    Ok(out)
}

/// Every mounted filesystem, in mount-table order. A mount point that cannot be stat'ed
/// (a stale network mount) is left out rather than reported as a zero-byte filesystem.
pub fn filesystems() -> io::Result<Vec<FsUsage>> {
    Ok(usage_of_each(mounts()?))
}

fn usage_of_each(entries: Vec<MountEntry>) -> Vec<FsUsage> {
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        let Ok((size, used, available, inodes)) = stat_of(&e.mount) else {
            continue;
        };
        let (inodes, inodes_used, inodes_free) = match inodes {
            Some((a, b, c)) => (Some(a), Some(b), Some(c)),
            None => (None, None, None),
        };
        out.push(FsUsage {
            device: crate::native::enumerate::dev_of(&e.mount).unwrap_or(0),
            filesystem: e.source,
            fs_type: Some(e.fs_type),
            mount: e.mount,
            size,
            used,
            available,
            inodes,
            inodes_used,
            inodes_free,
        });
    }
    out
}

/// The filesystem holding `path`: its numbers from `statvfs`, its names from the mount
/// table entry with the longest mount point that contains it.
pub fn usage_of(path: &Path) -> io::Result<FsUsage> {
    let (size, used, available, inodes) = stat_of(path)?;
    let abs = std::path::absolute(path)?;
    let table = mounts()?;
    let best = table
        .iter()
        .filter(|e| abs.starts_with(&e.mount))
        .max_by_key(|e| e.mount.as_os_str().len());
    let (inodes, inodes_used, inodes_free) = match inodes {
        Some((a, b, c)) => (Some(a), Some(b), Some(c)),
        None => (None, None, None),
    };
    let (filesystem, fs_type, mount) = match best {
        Some(e) => (e.source.clone(), Some(e.fs_type.clone()), e.mount.clone()),
        None => ("?".to_string(), None, abs.clone()),
    };
    Ok(FsUsage {
        device: crate::native::enumerate::dev_of(&abs).unwrap_or(0),
        filesystem,
        fs_type,
        mount,
        size,
        used,
        available,
        inodes,
        inodes_used,
        inodes_free,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn octal_escapes_are_undone() {
        assert_eq!(unescape("/media/My\\040Disk"), "/media/My Disk");
        assert_eq!(unescape("plain"), "plain");
        assert_eq!(unescape("a\\134b"), "a\\b");
        assert_eq!(unescape("tab\\011here"), "tab\there");
        assert_eq!(unescape("trailing\\04"), "trailing\\04");
    }

    #[test]
    fn the_root_filesystem_is_reported() {
        let f = usage_of(Path::new("/")).unwrap();
        assert!(f.size > 0, "a root filesystem has a size");
        assert!(f.used <= f.size);
        assert!(f.available <= f.size);
        assert_eq!(f.mount, PathBuf::from("/"));
        assert!(f.fs_type.is_some());
    }

    #[test]
    fn the_mount_table_names_more_than_one_filesystem() {
        let all = filesystems().unwrap();
        assert!(!all.is_empty());
        assert!(all.iter().any(|f| f.mount == Path::new("/")));
        for f in &all {
            assert!(f.used <= f.size, "{}: used above size", f.filesystem);
        }
    }
}
