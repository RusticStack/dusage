//! xz decompression for `-z/--search-zip`, in std alone.
//!
//! Stub: the magic and the entry point exist so the search path can dispatch on the format
//! while the decoder is written.

use std::io;

/// The six bytes every xz stream starts with.
pub const MAGIC: [u8; 6] = [0xfd, b'7', b'z', b'X', b'Z', 0x00];

/// True when `data` starts like an xz stream.
pub fn is_xz(data: &[u8]) -> bool {
    data.len() >= 6 && data[..6] == MAGIC
}

/// Expand every xz stream in `data` into one buffer.
pub fn decompress(_data: &[u8]) -> io::Result<Vec<u8>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "xz: decoder not built yet",
    ))
}
