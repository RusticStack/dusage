//! Linux/POSIX layer. Every `unsafe` block and `extern` declaration in the crate's
//! Linux half lives under this module; callers outside `unix` see only safe APIs.
//!
//! Directory enumeration and deletion use `std::fs` (`lstat`, `unlink`, `rmdir`);
//! only the change feed behind `--watch` needs raw `inotify` calls.

pub mod delete;
pub mod df;
pub mod enumerate;
pub mod sys;
pub mod watch;
