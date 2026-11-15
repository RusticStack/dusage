//! Shared raw Win32 declarations and small safe helpers.
//!
//! Lanes may *use* anything here but must not edit this file; put extra
//! declarations, structs and constants in the lane's own module instead.
#![allow(non_camel_case_types, clippy::upper_case_acronyms)]

use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::Path;

pub type HANDLE = *mut c_void;
pub type BOOL = i32;
pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;

// Access rights.
pub const GENERIC_READ: u32 = 0x8000_0000;
pub const DELETE: u32 = 0x0001_0000;
pub const SYNCHRONIZE: u32 = 0x0010_0000;
pub const FILE_LIST_DIRECTORY: u32 = 0x0001;
pub const FILE_READ_ATTRIBUTES: u32 = 0x0080;
pub const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;

// Share modes / creation disposition / flags.
pub const FILE_SHARE_READ: u32 = 0x1;
pub const FILE_SHARE_WRITE: u32 = 0x2;
pub const FILE_SHARE_DELETE: u32 = 0x4;
pub const FILE_SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
pub const OPEN_EXISTING: u32 = 3;
pub const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
pub const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

// File attributes.
pub const FILE_ATTRIBUTE_READONLY: u32 = 0x0001;
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0010;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x0080;
pub const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

// Reparse tags.
pub const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

// Win32 error codes (winerror.h).
pub const ERROR_FILE_NOT_FOUND: i32 = 2;
pub const ERROR_PATH_NOT_FOUND: i32 = 3;
pub const ERROR_ACCESS_DENIED: i32 = 5;
pub const ERROR_NO_MORE_FILES: i32 = 18;
pub const ERROR_SHARING_VIOLATION: i32 = 32;
pub const ERROR_HANDLE_EOF: i32 = 38;
pub const ERROR_INVALID_PARAMETER: i32 = 87;
pub const ERROR_INVALID_LEVEL: i32 = 124;
pub const ERROR_DIR_NOT_EMPTY: i32 = 145;
pub const ERROR_MORE_DATA: i32 = 234;
pub const ERROR_NOT_SUPPORTED: i32 = 50;

// FILE_INFO_BY_HANDLE_CLASS (minwinbase.h), in enum order.
pub const FILE_DISPOSITION_INFO_CLASS: i32 = 4;
pub const FILE_ATTRIBUTE_TAG_INFO_CLASS: i32 = 9;
pub const FILE_ID_BOTH_DIRECTORY_INFO_CLASS: i32 = 10;
pub const FILE_ID_BOTH_DIRECTORY_RESTART_INFO_CLASS: i32 = 11;
pub const FILE_ID_INFO_CLASS: i32 = 18;
pub const FILE_ID_EXTD_DIRECTORY_INFO_CLASS: i32 = 19;
pub const FILE_ID_EXTD_DIRECTORY_RESTART_INFO_CLASS: i32 = 20;
pub const FILE_DISPOSITION_INFO_EX_CLASS: i32 = 21;

#[link(name = "kernel32")]
extern "system" {
    pub fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *mut c_void,
        disposition: u32,
        flags: u32,
        template: HANDLE,
    ) -> HANDLE;
    pub fn GetFileInformationByHandleEx(h: HANDLE, class: i32, buf: *mut c_void, size: u32)
        -> BOOL;
    pub fn SetFileInformationByHandle(h: HANDLE, class: i32, buf: *const c_void, size: u32)
        -> BOOL;
    pub fn DeviceIoControl(
        h: HANDLE,
        code: u32,
        in_buf: *const c_void,
        in_size: u32,
        out_buf: *mut c_void,
        out_size: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> BOOL;
    pub fn GetVolumeInformationByHandleW(
        h: HANDLE,
        name: *mut u16,
        name_size: u32,
        serial: *mut u32,
        max_component: *mut u32,
        fs_flags: *mut u32,
        fs_name: *mut u16,
        fs_name_size: u32,
    ) -> BOOL;
}

/// `Err(last_os_error)` when a Win32 BOOL is zero.
pub fn check(ok: BOOL) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// NUL-terminated UTF-16 path in verbatim form (`\\?\C:\...`, `\\?\UNC\srv\share\...`)
/// so paths beyond MAX_PATH work. Relative paths are made absolute first.
pub fn wide_verbatim(p: &Path) -> io::Result<Vec<u16>> {
    let abs = std::path::absolute(p)?;
    let s = abs.as_os_str();
    let w: Vec<u16> = s.encode_wide().collect();
    let bs = b'\\' as u16;
    let mut out: Vec<u16> = Vec::with_capacity(w.len() + 9);
    let starts = |pre: &str| w.starts_with(&pre.encode_utf16().collect::<Vec<_>>());
    if starts(r"\\?\") || starts(r"\\.\") {
        out.extend_from_slice(&w);
    } else if w.len() >= 2 && w[0] == bs && w[1] == bs {
        out.extend(r"\\?\UNC".encode_utf16());
        out.extend_from_slice(&w[1..]);
    } else {
        out.extend(r"\\?\".encode_utf16());
        out.extend_from_slice(&w);
    }
    out.push(0);
    Ok(out)
}

/// Plain NUL-terminated UTF-16 (no verbatim prefix), e.g. for `\\.\C:` device paths.
pub fn wide_plain(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// `CreateFileW(OPEN_EXISTING)` on a verbatim path, returning an owned handle.
pub fn open(p: &Path, access: u32, flags: u32) -> io::Result<OwnedHandle> {
    let w = wide_verbatim(p)?;
    open_wide(&w, access, flags)
}

/// `CreateFileW(OPEN_EXISTING)` on an already NUL-terminated wide path.
pub fn open_wide(w: &[u16], access: u32, flags: u32) -> io::Result<OwnedHandle> {
    debug_assert_eq!(w.last(), Some(&0));
    // SAFETY: `w` is NUL-terminated; all pointer args are null or valid.
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            access,
            FILE_SHARE_ALL,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            flags,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `h` is a fresh, valid handle we exclusively own.
    Ok(unsafe { OwnedHandle::from_raw_handle(h) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(p: &str) -> String {
        let w = wide_verbatim(Path::new(p)).unwrap();
        String::from_utf16(&w[..w.len() - 1]).unwrap()
    }

    #[test]
    fn verbatim_forms() {
        assert_eq!(s(r"C:\a\b"), r"\\?\C:\a\b");
        assert_eq!(s(r"\\srv\share\x"), r"\\?\UNC\srv\share\x");
        assert_eq!(s(r"\\?\C:\a"), r"\\?\C:\a");
        assert_eq!(s(r"C:\a\..\b"), r"\\?\C:\b");
    }
}
