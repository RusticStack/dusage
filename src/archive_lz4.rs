//! LZ4 frame decompression for `-z/--search-zip`, in std alone.
//!
//! Stub: the magic and the entry point exist so the search path can dispatch on the format
//! while the decoder is written.

use std::io;

/// The four bytes an LZ4 frame starts with.
pub const MAGIC: [u8; 4] = [0x04, 0x22, 0x4d, 0x18];

/// True when `data` starts like an LZ4 frame.
pub fn is_lz4(data: &[u8]) -> bool {
    data.len() >= 4 && data[..4] == MAGIC
}

/// Expand every LZ4 frame in `data` into one buffer.
pub fn decompress(_data: &[u8]) -> io::Result<Vec<u8>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "lz4: decoder not built yet",
    ))
}
