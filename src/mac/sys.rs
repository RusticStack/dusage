//! Shared raw declarations and small safe helpers for the macOS layer.
//!
//! Every `unsafe` block and `extern` declaration in the crate's macOS half lives under
//! `src/mac/`; callers above see only safe APIs.

use std::ffi::c_int;
use std::io;

// open (fcntl.h). BSD numbers `O_NONBLOCK` at 4, where Linux has 0o4000, which is why
// this constant is reached through `crate::native::sys` and never written out at a call
// site.
pub const O_NONBLOCK: c_int = 0x0004;

/// The filesystem cannot serve a per-directory detail (or the whole feature).
///
/// Classified through `ErrorKind` rather than raw errno numbers, so `std`'s own mapping
/// for the target decides what `EINVAL`, `ENOTSUP` and `ENOSYS` mean here.
pub fn is_unsupported(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported | io::ErrorKind::NotADirectory
    )
}

pub fn is_denied(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_classification() {
        assert!(is_denied(&io::Error::from(io::ErrorKind::PermissionDenied)));
        assert!(!is_denied(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(is_unsupported(&io::Error::from(io::ErrorKind::Unsupported)));
        assert!(!is_unsupported(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }
}
