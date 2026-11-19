//! dusage: dependency-free disk tooling for Windows and Linux.

#[cfg(not(any(windows, target_os = "linux")))]
compile_error!("dusage supports Windows (NTFS) and Linux only.");

pub mod archive;
pub mod bytes;
pub mod cli;
pub mod cmd;
pub mod encodings;
pub mod glob;
pub mod guard;
pub mod ignore;
pub mod model;
pub mod pattern;
pub mod pcre;
pub mod preset;
pub mod prune;
pub mod queue;
pub mod regex;
pub mod report;
pub mod scan;
pub mod search;
pub mod simd;
pub mod unicode;

#[cfg(target_os = "linux")]
pub mod unix;
#[cfg(windows)]
pub mod win;

#[cfg(target_os = "linux")]
pub use unix as native;
#[cfg(windows)]
pub use win as native;

pub use queue::par_for_each;
