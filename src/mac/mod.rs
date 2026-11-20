//! macOS-native layer. The third arm of `crate::native`, alongside `unix` and `win`.
//!
//! Slice 1 of the port: the whole surface the engines above need, backed by `std::fs`
//! so the crate compiles and cross-checks for `x86_64-apple-darwin` before any Darwin
//! FFI is introduced. Directory enumeration, deletion and the change feed are
//! substituted for the real Darwin calls in later slices; the shapes the rest of the
//! crate sees do not change when they are.

pub mod delete;
pub mod enumerate;
pub mod sys;
pub mod watch;
