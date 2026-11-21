//! Windows side of `dusage df`: `GetDiskFreeSpaceExW` for the numbers, the drive table and
//! the volume list for the rows.
//!
//! Windows has no mount table file. What it has is drive letters (`GetLogicalDrives`) and
//! volumes, each of which may be mounted on several directories (`GetVolumePathNamesForVolumeNameW`).
//! Walking both gives what `/proc/self/mounts` gives on Linux: one row per mounted
//! filesystem, named by its mount point, with `GetDriveTypeW` saying whether it is a fixed
//! disk, a removable one or a share. Inode counts do not exist here, so those columns are
//! absent rather than zero.

use super::sys::HANDLE;
use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

const DRIVE_NO_ROOT_DIR: u32 = 1;

#[link(name = "kernel32")]
extern "system" {
    fn GetLogicalDrives() -> u32;
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn GetDiskFreeSpaceExW(
        dir: *const u16,
        free_to_caller: *mut u64,
        total: *mut u64,
        free_total: *mut u64,
    ) -> i32;
    fn GetVolumeInformationW(
        root: *const u16,
        name: *mut u16,
        name_size: u32,
        serial: *mut u32,
        max_component: *mut u32,
        flags: *mut u32,
        fs_name: *mut u16,
        fs_name_size: u32,
    ) -> i32;
    fn GetVolumePathNameW(path: *const u16, out: *mut u16, len: u32) -> i32;
    fn FindFirstVolumeW(name: *mut u16, len: u32) -> HANDLE;
    fn FindNextVolumeW(handle: HANDLE, name: *mut u16, len: u32) -> i32;
    fn FindVolumeClose(handle: HANDLE) -> i32;
    fn GetVolumePathNamesForVolumeNameW(
        name: *const u16,
        buf: *mut u16,
        len: u32,
        used: *mut u32,
    ) -> i32;
}

/// One filesystem: the numbers and the names.
#[derive(Clone, Debug)]
pub struct FsUsage {
    /// The device the row is named by: `C:`.
    pub filesystem: String,
    pub fs_type: Option<String>,
    pub mount: PathBuf,
    pub size: u64,
    pub used: u64,
    /// What the calling process may still write, which is `TotalNumberOfFreeBytes` restricted
    /// by the volume's quota.
    pub available: u64,
    /// Windows reports no inode counts, so these are always `None` here.
    pub inodes: Option<u64>,
    pub inodes_used: Option<u64>,
    pub inodes_free: Option<u64>,
    /// Windows has no device number to compare: rows are deduplicated by their volume name.
    pub device: u64,
}

fn wide(p: &std::ffi::OsStr) -> Vec<u16> {
    p.encode_wide().chain(std::iter::once(0)).collect()
}

fn text(w: &[u16]) -> String {
    let end = w.iter().position(|c| *c == 0).unwrap_or(w.len());
    OsString::from_wide(&w[..end])
        .to_string_lossy()
        .into_owned()
}

/// Size, used and available for a path or volume root.
fn space(root: &[u16]) -> io::Result<(u64, u64, u64)> {
    let (mut free_to_caller, mut total, mut free_total) = (0u64, 0u64, 0u64);
    // SAFETY: `root` is NUL-terminated UTF-16; the three out-pointers are valid, writable
    // `u64`s for the duration of the call.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            root.as_ptr(),
            &mut free_to_caller,
            &mut total,
            &mut free_total,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((total, total - free_total.min(total), free_to_caller))
}

/// The filesystem type name (`NTFS`, `FAT32`, ...) of a volume root, when Windows will say.
fn fs_type(root: &[u16]) -> Option<String> {
    let mut name = [0u16; 64];
    // SAFETY: `root` is NUL-terminated UTF-16, `name` is a writable buffer of `name.len()`
    // `u16`s, and every other pointer argument is null, which the API accepts.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    let t = text(&name);
    (!t.is_empty()).then_some(t)
}

/// The volume root ("`C:\`") that holds `path`.
fn volume_root_of(path: &Path) -> io::Result<Vec<u16>> {
    let abs = std::path::absolute(path)?;
    let w = wide(abs.as_os_str());
    let mut out = vec![0u16; 1024];
    // SAFETY: `w` is NUL-terminated UTF-16 and `out` is a writable buffer of `out.len()`
    // `u16`s, its size passed as the length the API may write.
    let ok = unsafe { GetVolumePathNameW(w.as_ptr(), out.as_mut_ptr(), out.len() as u32) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(out)
}

fn row(device: String, root: &[u16]) -> io::Result<FsUsage> {
    let (size, used, available) = space(root)?;
    Ok(FsUsage {
        filesystem: crate::model::mount_label(&device),
        fs_type: fs_type(root),
        mount: PathBuf::from(text(root)),
        size,
        used,
        available,
        inodes: None,
        inodes_used: None,
        inodes_free: None,
        device: 0,
    })
}

/// The volume paths of one volume: a drive letter, a folder mount, both, or neither.
fn volume_paths(volume: &[u16]) -> Vec<Vec<u16>> {
    let mut used = 0u32;
    // SAFETY: `volume` is NUL-terminated UTF-16; a null buffer with length 0 asks the API
    // for the size it needs, written into `used`.
    unsafe {
        GetVolumePathNamesForVolumeNameW(volume.as_ptr(), std::ptr::null_mut(), 0, &mut used);
    }
    if used == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u16; used as usize];
    // SAFETY: `buf` is `used` `u16`s long and writable, which is exactly what the previous
    // call reported it needs; `used` receives the count actually written.
    let ok = unsafe {
        GetVolumePathNamesForVolumeNameW(volume.as_ptr(), buf.as_mut_ptr(), used, &mut used)
    };
    if ok == 0 {
        return Vec::new();
    }
    buf.split(|c| *c == 0)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut v: Vec<u16> = s.to_vec();
            v.push(0);
            v
        })
        .collect()
}

/// Every mounted filesystem: one row per volume, named by its first mount path, plus any
/// drive letter whose volume Windows would not enumerate. A drive that is present but not
/// ready (an empty card reader, a disconnected share) is skipped rather than reported as a
/// zero-byte filesystem, which is what `df` does with an unmounted row.
pub fn filesystems() -> io::Result<Vec<FsUsage>> {
    let masks = {
        // SAFETY: no arguments, no pointers; the call only reads the drive bitmask.
        let m = unsafe { GetLogicalDrives() };
        if m == 0 {
            return Err(io::Error::last_os_error());
        }
        m
    };
    let mut out: Vec<FsUsage> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut buf = [0u16; 1024];
    // SAFETY: `buf` is a writable buffer of `buf.len()` `u16`s for the volume name, which the
    // API NUL-terminates; a GUID volume path is well inside that size.
    let handle = unsafe { FindFirstVolumeW(buf.as_mut_ptr(), buf.len() as u32) };
    if !handle.is_null() && handle as isize != -1 {
        loop {
            let volume = {
                let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
                let mut v = buf[..end].to_vec();
                v.push(0);
                v
            };
            for path in volume_paths(&volume) {
                let Ok(r) = row(text(&path), &path) else {
                    continue;
                };
                // `row` normalised the label, and the letter fallback below is written in
                // the same spelling, so one volume cannot arrive twice.
                if seen.contains(&r.filesystem) {
                    continue;
                }
                seen.push(r.filesystem.clone());
                out.push(r);
            }
            // SAFETY: `handle` came from FindFirstVolumeW and is still open here; `buf` is
            // the same writable buffer.
            let more = unsafe { FindNextVolumeW(handle, buf.as_mut_ptr(), buf.len() as u32) };
            if more == 0 {
                break;
            }
        }
        // SAFETY: `handle` is the search handle from FindFirstVolumeW and is closed once.
        unsafe {
            FindVolumeClose(handle);
        }
    }
    // A drive letter Windows did not hand back as a volume (a subst'ed drive, or a share
    // reached through a mapped letter) still answers GetDiskFreeSpaceExW.
    for bit in 0..26u32 {
        if masks & (1 << bit) == 0 {
            continue;
        }
        let letter = (b'A' + bit as u8) as char;
        let name = format!("{letter}:");
        // Compare the normalised spelling: the volume list above names this volume `E:\`,
        // and a plain string comparison against `E:` would add it a second time.
        if seen.iter().any(|s| s == &name) {
            continue;
        }
        let root = wide(std::ffi::OsStr::new(&format!("{name}\\")));
        // SAFETY: `root` is NUL-terminated UTF-16; the call only reads it.
        let kind = unsafe { GetDriveTypeW(root.as_ptr()) };
        if kind == DRIVE_NO_ROOT_DIR {
            continue;
        }
        if let Ok(r) = row(name.clone(), &root) {
            seen.push(name);
            out.push(r);
        }
    }
    Ok(out)
}

/// The filesystem holding `path`.
pub fn usage_of(path: &Path) -> io::Result<FsUsage> {
    let root = volume_root_of(path)?;
    let mount = text(&root);
    let device = if mount.len() >= 2 && mount.as_bytes()[1] == b':' {
        mount[..2].to_string()
    } else {
        mount.clone()
    };
    row(device, &root)
}
