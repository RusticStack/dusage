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

extern "C" {
    pub fn inotify_init1(flags: c_int) -> c_int;
    pub fn inotify_add_watch(fd: c_int, path: *const std::ffi::c_char, mask: u32) -> c_int;
    /// `nfds_t` is `unsigned long` on Linux.
    pub fn poll(fds: *mut PollFd, nfds: u64, timeout: c_int) -> c_int;
    pub fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
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
}
