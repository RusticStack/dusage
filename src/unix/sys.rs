//! Shared raw libc declarations and small safe helpers for the Unix layer.
//!
//! Every value here is taken from the Linux `asm-generic` errno table and
//! `inotify.h`/`poll.h`; the crate is gated to Windows and Linux in `lib.rs`.
#![allow(non_camel_case_types)]

use std::ffi::{c_int, c_short, c_void};
use std::io;

// errno values.
pub const EPERM: i32 = 1;
pub const ENOENT: i32 = 2;
pub const EAGAIN: i32 = 11;
pub const EACCES: i32 = 13;
pub const EBUSY: i32 = 16;
pub const ENOTDIR: i32 = 20;
pub const EINVAL: i32 = 22;
pub const ETXTBSY: i32 = 26;
pub const ENOSPC: i32 = 28;
pub const ENOSYS: i32 = 38;
pub const ENOTEMPTY: i32 = 39;
pub const EOPNOTSUPP: i32 = 95;

/// The filesystem cannot serve a per-directory detail (or the whole feature).
pub fn is_unsupported(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(EINVAL | ENOSYS | EOPNOTSUPP | ENOTDIR)
    )
}

pub fn is_denied(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(EACCES | EPERM)) || e.kind() == io::ErrorKind::PermissionDenied
}

// inotify (inotify.h).
pub const IN_NONBLOCK: c_int = 0o4000;
pub const IN_CLOEXEC: c_int = 0o2_000_000;

pub const IN_MODIFY: u32 = 0x0000_0002;
pub const IN_ATTRIB: u32 = 0x0000_0004;
pub const IN_CLOSE_WRITE: u32 = 0x0000_0008;
pub const IN_MOVED_FROM: u32 = 0x0000_0040;
pub const IN_MOVED_TO: u32 = 0x0000_0080;
pub const IN_CREATE: u32 = 0x0000_0100;
pub const IN_DELETE: u32 = 0x0000_0200;
pub const IN_DELETE_SELF: u32 = 0x0000_0400;
pub const IN_MOVE_SELF: u32 = 0x0000_0800;
pub const IN_Q_OVERFLOW: u32 = 0x0000_4000;
pub const IN_IGNORED: u32 = 0x0000_8000;
pub const IN_ONLYDIR: u32 = 0x0100_0000;
pub const IN_ISDIR: u32 = 0x4000_0000;

// poll (poll.h).
pub const POLLIN: c_short = 0x001;

#[repr(C)]
pub struct PollFd {
    pub fd: c_int,
    pub events: c_short,
    pub revents: c_short,
}

// open (fcntl.h).
pub const O_RDONLY: c_int = 0;
/// A directory open never blocks in practice, but a FIFO-shaped path would.
pub const O_NONBLOCK: c_int = 0o4000;
pub const O_DIRECTORY: c_int = 0o200_000;
pub const O_CLOEXEC: c_int = 0o2_000_000;
pub const SEEK_SET: c_int = 0;

// statx (fcntl.h).
pub const AT_FDCWD: c_int = -100;
pub const AT_SYMLINK_NOFOLLOW: c_int = 0x100;
/// The mask behind `lstat`: everything but the btime/mount fields.
pub const STATX_BASIC_STATS: u32 = 0x7ff;

// st_mode file types (sys/stat.h).
pub const S_IFMT: u16 = 0o170_000;
pub const S_IFIFO: u16 = 0o010_000;
pub const S_IFCHR: u16 = 0o020_000;
pub const S_IFDIR: u16 = 0o040_000;
pub const S_IFBLK: u16 = 0o060_000;
pub const S_IFREG: u16 = 0o100_000;
pub const S_IFLNK: u16 = 0o120_000;
pub const S_IFSOCK: u16 = 0o140_000;

/// `struct statx_timestamp` (linux/stat.h).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct StatxTimestamp {
    pub tv_sec: i64,
    pub tv_nsec: u32,
    pub reserved: u32,
}

/// `struct statx` (linux/stat.h), 256 bytes: the fields `statx` fills for the basic
/// statistics, plus the trailing spare space the kernel may write into.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Statx {
    pub mask: u32,
    pub blksize: u32,
    pub attributes: u64,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub mode: u16,
    pub spare0: u16,
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub attributes_mask: u64,
    pub atime: StatxTimestamp,
    pub btime: StatxTimestamp,
    pub ctime: StatxTimestamp,
    pub mtime: StatxTimestamp,
    pub rdev_major: u32,
    pub rdev_minor: u32,
    pub dev_major: u32,
    pub dev_minor: u32,
    pub mnt_id: u64,
    pub dio_mem_align: u32,
    pub dio_offset_align: u32,
    pub spare3: [u64; 12],
}

/// The 64-bit `dev_t` glibc hands out from `major`/`minor`, so a statx-derived device
/// compares equal to `std`'s `Metadata::dev` and to `MetadataExt::dev`.
pub fn make_dev(major: u32, minor: u32) -> u64 {
    let (maj, min) = (major as u64, minor as u64);
    ((maj & 0x0000_0fff) << 8)
        | (min & 0x0000_00ff)
        | ((min & 0xffff_ff00) << 12)
        | ((maj & 0xffff_f000) << 32)
}

extern "C" {
    pub fn inotify_init1(flags: c_int) -> c_int;
    pub fn inotify_add_watch(fd: c_int, path: *const std::ffi::c_char, mask: u32) -> c_int;
    /// `nfds_t` is `unsigned long` on Linux.
    pub fn poll(fds: *mut PollFd, nfds: u64, timeout: c_int) -> c_int;
    pub fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    pub fn openat(dirfd: c_int, path: *const std::ffi::c_char, flags: c_int, mode: c_int) -> c_int;
    pub fn getdents64(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    pub fn lseek(fd: c_int, offset: i64, whence: c_int) -> i64;
    pub fn statx(
        dirfd: c_int,
        path: *const std::ffi::c_char,
        flags: c_int,
        mask: u32,
        buf: *mut Statx,
    ) -> c_int;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_classification() {
        assert!(is_unsupported(&io::Error::from_raw_os_error(EINVAL)));
        assert!(is_unsupported(&io::Error::from_raw_os_error(EOPNOTSUPP)));
        assert!(!is_unsupported(&io::Error::from_raw_os_error(EACCES)));
        assert!(is_denied(&io::Error::from_raw_os_error(EACCES)));
        assert!(is_denied(&io::Error::from_raw_os_error(EPERM)));
        assert!(!is_denied(&io::Error::from_raw_os_error(ENOTEMPTY)));
    }

    #[test]
    fn pollfd_layout_matches_libc() {
        assert_eq!(std::mem::size_of::<PollFd>(), 8);
        assert_eq!(std::mem::offset_of!(PollFd, fd), 0);
        assert_eq!(std::mem::offset_of!(PollFd, events), 4);
        assert_eq!(std::mem::offset_of!(PollFd, revents), 6);
    }

    #[test]
    fn statx_layout_matches_linux_stat_h() {
        assert_eq!(std::mem::size_of::<StatxTimestamp>(), 16);
        assert_eq!(std::mem::size_of::<Statx>(), 256);
        assert_eq!(std::mem::offset_of!(Statx, mask), 0);
        assert_eq!(std::mem::offset_of!(Statx, blksize), 4);
        assert_eq!(std::mem::offset_of!(Statx, attributes), 8);
        assert_eq!(std::mem::offset_of!(Statx, nlink), 16);
        assert_eq!(std::mem::offset_of!(Statx, uid), 20);
        assert_eq!(std::mem::offset_of!(Statx, gid), 24);
        assert_eq!(std::mem::offset_of!(Statx, mode), 28);
        assert_eq!(std::mem::offset_of!(Statx, ino), 32);
        assert_eq!(std::mem::offset_of!(Statx, size), 40);
        assert_eq!(std::mem::offset_of!(Statx, blocks), 48);
        assert_eq!(std::mem::offset_of!(Statx, attributes_mask), 56);
        assert_eq!(std::mem::offset_of!(Statx, atime), 64);
        assert_eq!(std::mem::offset_of!(Statx, btime), 80);
        assert_eq!(std::mem::offset_of!(Statx, ctime), 96);
        assert_eq!(std::mem::offset_of!(Statx, mtime), 112);
        assert_eq!(std::mem::offset_of!(Statx, rdev_major), 128);
        assert_eq!(std::mem::offset_of!(Statx, dev_major), 136);
        assert_eq!(std::mem::offset_of!(Statx, dev_minor), 140);
        assert_eq!(std::mem::offset_of!(Statx, mnt_id), 144);
        assert_eq!(std::mem::offset_of!(Statx, spare3), 160);
    }

    #[test]
    fn dev_encoding_matches_std() {
        let dir = std::env::temp_dir();
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::metadata(&dir).unwrap();
        let want = md.dev();
        let major = ((want >> 8) & 0xfff) as u32 | ((want >> 32) as u32 & !0xfffu32);
        let minor = (want & 0xff) as u32 | (((want >> 12) & 0xffff_ff00) as u32);
        assert_eq!(make_dev(major, minor), want);
        assert_ne!(make_dev(8, 1), 0);
    }
}
