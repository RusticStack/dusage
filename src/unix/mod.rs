//! Linux/POSIX layer. Every `unsafe` block and `extern` declaration in the crate's
//! Linux half lives under this module; callers outside `unix` see only safe APIs.
//!
//! Directory enumeration and deletion use `std::fs` (`lstat`, `unlink`, `rmdir`);
//! only the change feed behind `--watch` needs raw `inotify` calls.

pub mod delete;
pub mod enumerate;
pub mod sys;
pub mod watch;

/// Ceiling for the default worker count. Linux has not been swept on a machine wider than
/// the 2-vCPU box docs/BENCHMARKS.md records, so this keeps the old behaviour rather than
/// borrowing the Windows plateau, which was measured on a filter-driver-protected volume.
pub const MAX_WORKERS: usize = 16;
