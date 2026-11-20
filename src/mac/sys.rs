//! Shared raw libSystem declarations and small safe helpers for the macOS layer.
//!
//! Every value here is taken from Darwin's `fcntl.h`, `sys/stat.h` and `dirent.h`; the
//! crate is gated to Windows, Linux and macOS in `lib.rs`. The struct layouts are pinned
//! by the `const` assertions at the bottom of this file, which run during a cross-check
//! from Linux as well as on a Mac.

#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_int};

// open (fcntl.h). BSD numbers these differently from Linux: `O_NONBLOCK` is 4, not
// 0o4000, which is why the constant is reached through `crate::native::sys` and never
// written out at a call site.
pub const O_RDONLY: c_int = 0;
pub const O_NONBLOCK: c_int = 0x0004;
pub const O_DIRECTORY: c_int = 0x10_0000;
pub const O_CLOEXEC: c_int = 0x100_0000;

// fstatat (fcntl.h): the one flag this layer passes.
pub const AT_SYMLINK_NOFOLLOW: c_int = 0x0020;

// st_mode file types (sys/stat.h).
pub const S_IFMT: u32 = 0o170_000;
pub const S_IFIFO: u32 = 0o010_000;
pub const S_IFCHR: u32 = 0o020_000;
pub const S_IFDIR: u32 = 0o040_000;
pub const S_IFBLK: u32 = 0o060_000;
pub const S_IFREG: u32 = 0o100_000;
pub const S_IFLNK: u32 = 0o120_000;
pub const S_IFSOCK: u32 = 0o140_000;

// d_type (dirent.h). The values match Linux's, which is why the mapping in
// `enumerate::kind_of_dtype` reads the same on both.
pub const DT_UNKNOWN: u8 = 0;
pub const DT_FIFO: u8 = 1;
pub const DT_CHR: u8 = 2;
pub const DT_DIR: u8 = 4;
pub const DT_BLK: u8 = 6;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;
pub const DT_SOCK: u8 = 12;

/// An opaque `DIR *` from `opendir`/`fdopendir`. Never dereferenced here.
#[repr(C)]
pub struct DIR {
    _private: [u8; 0],
}

/// `struct stat` (sys/stat.h) for the 64-bit Darwin layout.
///
/// Field order and widths are Apple's; the offsets are asserted below. Linux's `statx`
/// equivalents map one-for-one: `st_dev` is `dev`, `st_ino` is `ino`, `st_blocks` is
/// `blocks`, and the three timestamps carry a separate nanosecond field each.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct StatBuf {
    pub st_dev: i32,
    pub st_mode: u16,
    pub st_nlink: u16,
    pub st_ino: u64,
    pub st_uid: u32,
    pub st_gid: u32,
    pub st_rdev: i32,
    pub st_atime: i64,
    pub st_atime_nsec: i64,
    pub st_mtime: i64,
    pub st_mtime_nsec: i64,
    pub st_ctime: i64,
    pub st_ctime_nsec: i64,
    pub st_birthtime: i64,
    pub st_birthtime_nsec: i64,
    pub st_size: i64,
    pub st_blocks: i64,
    pub st_blksize: i32,
    pub st_flags: u32,
    pub st_gen: u32,
    pub st_lspare: i32,
    pub st_qspare: [i64; 2],
}

/// The largest name a directory record can carry (`__DARWIN_MAXNAMLEN` rounded up).
pub const DIRENT_NAME_MAX: usize = 1024;

/// `struct dirent` (dirent.h) with `_DARWIN_FEATURE_64_BIT_INODE`, which is the default
/// on every release this crate could run on and the layout `readdir$INODE64` returns.
#[repr(C)]
pub struct Dirent {
    pub d_ino: u64,
    pub d_seekoff: u64,
    pub d_reclen: u16,
    pub d_namlen: u16,
    pub d_type: u8,
    /// NUL-terminated. The parser finds the end by scanning for that terminator, not by
    /// trusting `d_namlen`; this array only bounds the scan.
    pub d_name: [u8; DIRENT_NAME_MAX],
}

// struct stat, struct dirent and the layout the parser relies on. These are the
// documented 64-bit Darwin layouts; the values are asserted so a declaration that drifts
// from them fails a cross-check rather than at runtime on a Mac.
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<StatBuf>() == 144, "struct stat is 144 bytes");
    assert!(offset_of!(StatBuf, st_dev) == 0);
    assert!(offset_of!(StatBuf, st_mode) == 4);
    assert!(offset_of!(StatBuf, st_nlink) == 6);
    assert!(offset_of!(StatBuf, st_ino) == 8);
    assert!(offset_of!(StatBuf, st_uid) == 16);
    assert!(offset_of!(StatBuf, st_gid) == 20);
    assert!(offset_of!(StatBuf, st_rdev) == 24);
    assert!(offset_of!(StatBuf, st_atime) == 32);
    assert!(offset_of!(StatBuf, st_atime_nsec) == 40);
    assert!(offset_of!(StatBuf, st_mtime) == 48);
    assert!(offset_of!(StatBuf, st_mtime_nsec) == 56);
    assert!(offset_of!(StatBuf, st_ctime) == 64);
    assert!(offset_of!(StatBuf, st_ctime_nsec) == 72);
    assert!(offset_of!(StatBuf, st_birthtime) == 80);
    assert!(offset_of!(StatBuf, st_birthtime_nsec) == 88);
    assert!(offset_of!(StatBuf, st_size) == 96);
    assert!(offset_of!(StatBuf, st_blocks) == 104);
    assert!(offset_of!(StatBuf, st_blksize) == 112);
    assert!(offset_of!(StatBuf, st_flags) == 116);
    assert!(offset_of!(StatBuf, st_gen) == 120);
    assert!(offset_of!(StatBuf, st_lspare) == 124);
    assert!(offset_of!(StatBuf, st_qspare) == 128);
    assert!(offset_of!(Dirent, d_ino) == 0);
    assert!(offset_of!(Dirent, d_seekoff) == 8);
    assert!(offset_of!(Dirent, d_reclen) == 16);
    assert!(offset_of!(Dirent, d_namlen) == 18);
    assert!(offset_of!(Dirent, d_type) == 20);
    assert!(offset_of!(Dirent, d_name) == 21);
    assert!(offset_of!(Dirent, d_name) + DIRENT_NAME_MAX <= size_of::<Dirent>());
};

extern "C" {
    /// Variadic in C (`int open(const char *, int, ...)`), and declared that way here:
    /// rustc rejects a fixed-arity declaration of this symbol on Darwin, where it is a
    /// runtime symbol the standard library itself calls.
    pub fn open(path: *const c_char, flags: c_int, ...) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn dirfd(dirp: *mut DIR) -> c_int;
    pub fn closedir(dirp: *mut DIR) -> c_int;
    /// Sets `errno`; read it after a call returns the failure value.
    pub fn __error() -> *mut c_int;
    pub fn unlink(path: *const c_char) -> c_int;
    pub fn rmdir(path: *const c_char) -> c_int;
    /// The 64-bit-inode aliases the system headers resolve these names to on every
    /// architecture but arm64, whose exports are already 64-bit. `dirent.h` and
    /// `sys/stat.h` macro the plain spellings onto these, so on x86_64 the unaliased
    /// symbol is not the one the C caller would bind.
    ///
    /// `fdopendir` carries the alias for a reason worth recording: a stream allocated by
    /// the legacy 32-bit-inode `fdopendir` and then read with `readdir$INODE64` returns
    /// records in the wrong shape, which read as a zero-length name and failed every
    /// directory listing on the first real macOS run.
    #[cfg_attr(not(target_arch = "aarch64"), link_name = "readdir$INODE64")]
    pub fn readdir(dirp: *mut DIR) -> *mut Dirent;
    #[cfg_attr(not(target_arch = "aarch64"), link_name = "fdopendir$INODE64")]
    pub fn fdopendir(fd: c_int) -> *mut DIR;
    #[cfg_attr(not(target_arch = "aarch64"), link_name = "fstatat$INODE64")]
    pub fn fstatat(fd: c_int, path: *const c_char, buf: *mut StatBuf, flag: c_int) -> c_int;
    #[cfg_attr(not(target_arch = "aarch64"), link_name = "lstat$INODE64")]
    pub fn lstat(path: *const c_char, buf: *mut StatBuf) -> c_int;
    #[cfg_attr(not(target_arch = "aarch64"), link_name = "stat$INODE64")]
    pub fn stat(path: *const c_char, buf: *mut StatBuf) -> c_int;
}

/// The filesystem cannot serve a per-directory detail (or the whole feature).
///
/// Classified through `ErrorKind` rather than raw errno numbers, so `std`'s own mapping
/// for the target decides what `EINVAL`, `ENOTSUP` and `ENOSYS` mean here.
pub fn is_unsupported(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::InvalidInput
            | std::io::ErrorKind::Unsupported
            | std::io::ErrorKind::NotADirectory
    )
}

pub fn is_denied(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::PermissionDenied
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn error_classification() {
        assert!(is_denied(&io::Error::from(io::ErrorKind::PermissionDenied)));
        assert!(!is_denied(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(is_unsupported(&io::Error::from(io::ErrorKind::Unsupported)));
        assert!(!is_unsupported(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn stat_and_dirent_layouts_match_darwin() {
        assert_eq!(std::mem::size_of::<StatBuf>(), 144);
        assert_eq!(std::mem::offset_of!(StatBuf, st_size), 96);
        assert_eq!(std::mem::offset_of!(StatBuf, st_blocks), 104);
        assert_eq!(std::mem::offset_of!(Dirent, d_type), 20);
        assert_eq!(std::mem::offset_of!(Dirent, d_name), 21);
    }

    #[test]
    fn the_directory_record_kind_codes_are_the_ones_readdir_reports() {
        assert_eq!((DT_UNKNOWN, DT_FIFO, DT_CHR, DT_DIR), (0, 1, 2, 4));
        assert_eq!((DT_BLK, DT_REG, DT_LNK, DT_SOCK), (6, 8, 10, 12));
    }
}
