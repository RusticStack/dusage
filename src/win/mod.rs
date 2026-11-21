//! Windows-native layer. Every `unsafe` block and `extern` declaration in the
//! crate lives under this module; callers outside `win` see only safe APIs.

pub mod delete;
pub mod df;
pub mod enumerate;
pub mod mft;
pub mod sys;
pub mod usn;
pub mod watch;

/// Ceiling for the default worker count. The thread sweep in docs/BENCHMARKS.md plateaus
/// at eight on both a flat 1M-file tree and `C:\Windows`, and 24 or 32 threads are 6-13%
/// slower on the directory-heavy real tree, so the default stops there whatever the core
/// count: past the plateau the extra workers only add contention.
pub const MAX_WORKERS: usize = 8;
