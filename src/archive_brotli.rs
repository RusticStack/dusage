//! Brotli decompression for `-z/--search-zip`, in std alone.
//!
//! Brotli has no signature: it is identified by the extension of the file it was found in,
//! so unlike the other formats there is no `is_brotli` that can answer from bytes alone.
//!
//! Stub: the entry point exists so the search path can dispatch on the format while the
//! decoder is written.

use std::io;

/// Expand a Brotli stream into one buffer.
pub fn decompress(_data: &[u8]) -> io::Result<Vec<u8>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "brotli: decoder not built yet",
    ))
}
