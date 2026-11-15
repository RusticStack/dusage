//! dusage: dependency-free disk tooling for Windows (NTFS-first).
//!
//! Module ownership and contracts are described in `docs/ARCHITECTURE.md`.
//! All `unsafe` and FFI lives under [`win`].

#[cfg(not(windows))]
compile_error!("dusage targets Windows only (it uses Win32/NTFS APIs under `win`).");

pub mod cli;
pub mod guard;
pub mod model;
pub mod prune;
pub mod queue;
pub mod report;
pub mod scan;
pub mod win;

pub use queue::par_for_each;
