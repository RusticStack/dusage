//! Windows-native layer. Every `unsafe` block and `extern` declaration in the
//! crate lives under this module; callers outside `win` see only safe APIs.

pub mod delete;
pub mod df;
pub mod enumerate;
pub mod mft;
pub mod sys;
pub mod usn;
pub mod watch;
