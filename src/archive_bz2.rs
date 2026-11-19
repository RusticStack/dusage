//! bzip2 decompression for `-z/--search-zip`, in std alone.
//!
//! A bzip2 member is a run-length stage, a Burrows-Wheeler transform, a move-to-front stage
//! and then a Huffman-coded multiple-table stream; the container's trailer carries a CRC-32
//! over the expanded block, which is what proves the expansion was right.
//!
//! Stub: the magic and the entry point exist so the search path can dispatch on the format
//! while the decoder is written.

use std::io;

/// The three bytes every bzip2 stream starts with.
pub const MAGIC: &[u8] = b"BZh";

/// True when `data` starts like a bzip2 stream.
pub fn is_bzip2(data: &[u8]) -> bool {
    data.len() >= 3 && &data[..3] == MAGIC
}

/// Expand every bzip2 member in `data` into one buffer.
pub fn decompress(_data: &[u8]) -> io::Result<Vec<u8>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "bzip2: decoder not built yet",
    ))
}
