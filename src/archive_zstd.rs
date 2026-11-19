//! zstd frame decompression for `-z/--search-zip`, in std alone.
//!
//! Stub: the magic and the entry point exist so the search path can dispatch on the format
//! while the decoder is written.

use std::io;

/// The four bytes a zstd frame starts with.
pub const MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// True when `data` starts like a zstd frame.
pub fn is_zstd(data: &[u8]) -> bool {
    data.len() >= 4 && data[..4] == MAGIC
}

/// Expand every zstd frame in `data` into one buffer.
pub fn decompress(_data: &[u8]) -> io::Result<Vec<u8>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "zstd: decoder not built yet",
    ))
}
