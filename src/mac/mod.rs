//! macOS-native layer. The third arm of `crate::native`, alongside `unix` and `win`.
//!
//! Slice 1 of the port: the whole surface the engines above need, backed by `std::fs`
//! so the crate compiles and cross-checks for `x86_64-apple-darwin` before any Darwin
//! FFI is introduced. Directory enumeration, deletion and the change feed are
//! substituted for the real Darwin calls in later slices; the shapes the rest of the
//! crate sees do not change when they are.

pub mod delete;
pub mod df;
pub mod enumerate;
pub mod fsevents;
pub mod sys;
pub mod watch;

/// Ceiling for the default worker count. Nobody has swept the thread count on an Apple
/// Silicon or Intel Mac, so this borrows the Linux ceiling rather than the Windows plateau
/// that docs/BENCHMARKS.md measured on a filter-driver-protected volume: a number measured
/// on one platform is not evidence for another, and 16 is the value the core-count clamp
/// needs to reach at all on a machine with more than four cores.
pub const MAX_WORKERS: usize = 16;
