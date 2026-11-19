//! LZ4 frame decompression for `-z/--search-zip`, in std alone.
//!
//! LZ4 is what `lz4` writes and what the kernel packs with, and its frame format is small
//! enough to read from the specification: a descriptor whose flags say how the frame is
//! laid out, then blocks, then an optional checksum over the whole expansion. A block is
//! either stored as it lies or a run of LZ4 sequences — a token, the literals it names, a
//! two-byte offset back into what has been produced, and a match length.
//!
//! Both checksums are xxHash-32 seeded 0, which is implemented here as well: the descriptor
//! hash catches a damaged header, and the content hash is the only proof that a search is
//! looking at the file's real contents rather than at a corrupted expansion.
//!
//! Only the frame format is handled. `lz4 -l` writes the legacy format, which has no magic
//! and no checksums, so telling it apart from arbitrary bytes is not worth the
//! false positives.

use std::io;

/// The four bytes every LZ4 frame starts with.
pub const MAGIC: [u8; 4] = [0x04, 0x22, 0x4d, 0x18];

const FRAME_MAGIC: u32 = u32::from_le_bytes(MAGIC);

/// Skippable frames, whose payload a reader steps over.
const SKIPPABLE: std::ops::RangeInclusive<u32> = 0x184D_2A50..=0x184D_2A5F;

/// The largest expansion a block size code allows: 4 = 64 KiB, 5 = 256 KiB, 6 = 1 MiB,
/// 7 = 4 MiB. Codes 0-3 are undefined.
const BLOCK_MAX: [usize; 8] = [0, 0, 0, 0, 64 << 10, 256 << 10, 1 << 20, 4 << 20];

/// True when `data` starts like an LZ4 stream: a frame, or a skippable frame ahead of one.
pub fn is_lz4(data: &[u8]) -> bool {
    data.len() >= 4
        && (data[..4] == MAGIC
            || SKIPPABLE.contains(&u32::from_le_bytes([data[0], data[1], data[2], data[3]])))
}

/// Expand every LZ4 frame in `data` into one buffer. Trailing bytes that are not a frame are
/// left out, and a frame that is truncated, lies about its blocks, or fails a checksum is an
/// error rather than a silently short answer.
pub fn decompress(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Ok(magic) = word(data, &mut at) {
        if magic == FRAME_MAGIC {
            frame(data, &mut at, &mut out)?;
        } else if SKIPPABLE.contains(&magic) {
            let n = word(data, &mut at)? as usize;
            take(data, &mut at, n)?;
        } else {
            break;
        }
    }
    Ok(out)
}

/// One frame: descriptor and its checksum, blocks until the end mark, then the content
/// checksum. `at` is left on the first byte after the frame, so frames concatenate.
fn frame(data: &[u8], at: &mut usize, out: &mut Vec<u8>) -> io::Result<()> {
    let start = out.len();
    let descriptor = *at;
    let flg = byte(data, at)?;
    let bd = byte(data, at)?;
    let independent = flg & 0x20 != 0;
    let block_sum = flg & 0x10 != 0;
    let content_sum = flg & 0x04 != 0;
    let sized = flg & 0x08 != 0;
    if flg >> 6 != 1 {
        return Err(bad(format!("lz4: frame version {} is not 1", flg >> 6)));
    }
    if flg & 0x02 != 0 || bd & 0x8F != 0 {
        return Err(bad("lz4: reserved descriptor bits are set"));
    }
    let code = (bd >> 4) & 0x07;
    if code < 4 {
        return Err(bad(format!("lz4: block size code {code} is undefined")));
    }
    let limit = BLOCK_MAX[code as usize];
    let size = if sized { Some(wide(data, at)?) } else { None };
    if flg & 0x01 != 0 {
        // The dictionary id names an external dictionary this tool never has. The match
        // offsets that need it reach before the frame's own data, and are refused below.
        take(data, at, 4)?;
    }
    let checksum = byte(data, at)?;
    if (xxh32(&data[descriptor..*at - 1], 0) >> 8) as u8 != checksum {
        return Err(bad("lz4: header checksum does not match the descriptor"));
    }
    loop {
        let raw = word(data, at)?;
        if raw == 0 {
            break;
        }
        let stored = raw & 0x8000_0000 != 0;
        let n = (raw & 0x7FFF_FFFF) as usize;
        if n > limit {
            return Err(bad(format!(
                "lz4: block of {n} bytes exceeds the frame's {limit}"
            )));
        }
        let block = take(data, at, n)?;
        let block_start = out.len();
        if stored {
            out.extend_from_slice(block);
        } else {
            // A linked block may match bytes from the blocks before it; an independent one
            // starts with no history.
            let floor = if independent { block_start } else { start };
            decode_block(block, out, floor, limit)?;
        }
        if block_sum {
            let want = word(data, at)?;
            if xxh32(block, 0) != want {
                return Err(bad("lz4: block checksum does not match the block"));
            }
        }
    }
    if content_sum {
        let want = word(data, at)?;
        if xxh32(&out[start..], 0) != want {
            return Err(bad(
                "lz4: content checksum does not match the expanded data",
            ));
        }
    }
    if let Some(size) = size {
        let n = (out.len() - start) as u64;
        if n != size {
            return Err(bad(format!(
                "lz4: content size {size} does not match the {n} bytes expanded"
            )));
        }
    }
    Ok(())
}

/// Expand one LZ4 sequence block. Matches may read back to `floor`, which is the start of
/// the block for an independent frame and the start of the frame for a linked one, and the
/// expansion stops at `limit` bytes, the block size the descriptor declared.
fn decode_block(src: &[u8], out: &mut Vec<u8>, floor: usize, limit: usize) -> io::Result<()> {
    let start = out.len();
    let mut at = 0;
    loop {
        let token = *src.get(at).ok_or_else(short)?;
        at += 1;
        let mut literals = (token >> 4) as usize;
        if literals == 15 {
            literals += extension(src, &mut at)?;
        }
        let end = at.checked_add(literals).ok_or_else(short)?;
        out.extend_from_slice(src.get(at..end).ok_or_else(short)?);
        at = end;
        // The last sequence of a block carries literals only; nothing follows them.
        if at < src.len() {
            let offset = u16::from_le_bytes([
                *src.get(at).ok_or_else(short)?,
                *src.get(at + 1).ok_or_else(short)?,
            ]) as usize;
            at += 2;
            let produced = out.len() - floor;
            if offset == 0 || offset > produced {
                return Err(bad(format!(
                    "lz4: match offset {offset} from the {produced} bytes before it"
                )));
            }
            let mut len = (token & 0x0F) as usize;
            if len == 15 {
                len += extension(src, &mut at)?;
            }
            len += 4;
            let from = out.len() - offset;
            // A match may name bytes it is still producing, which is how LZ4 encodes runs.
            if offset >= len {
                out.extend_from_within(from..from + len);
            } else {
                for k in 0..len {
                    out.push(out[from + k]);
                }
            }
        }
        if out.len() - start > limit {
            return Err(bad(format!(
                "lz4: block expands past the frame's {limit} bytes"
            )));
        }
        if at == src.len() {
            return Ok(());
        }
    }
}

/// The 255-valued tail of a literal or match length, which the token's nibble cannot hold.
fn extension(src: &[u8], at: &mut usize) -> io::Result<usize> {
    let mut n = 0usize;
    loop {
        let b = *src.get(*at).ok_or_else(short)?;
        *at += 1;
        n += b as usize;
        if b != 255 {
            return Ok(n);
        }
    }
}

fn short() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "lz4: file ends mid-frame")
}

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn take<'a>(data: &'a [u8], at: &mut usize, n: usize) -> io::Result<&'a [u8]> {
    let end = at.checked_add(n).ok_or_else(short)?;
    let bytes = data.get(*at..end).ok_or_else(short)?;
    *at = end;
    Ok(bytes)
}

fn byte(data: &[u8], at: &mut usize) -> io::Result<u8> {
    Ok(take(data, at, 1)?[0])
}

fn word(data: &[u8], at: &mut usize) -> io::Result<u32> {
    let b = take(data, at, 4)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn wide(data: &[u8], at: &mut usize) -> io::Result<u64> {
    let mut b = [0u8; 8];
    b.copy_from_slice(take(data, at, 8)?);
    Ok(u64::from_le_bytes(b))
}

const P1: u32 = 0x9E37_79B1;
const P2: u32 = 0x85EB_CA77;
const P3: u32 = 0xC2B2_AE3D;
const P4: u32 = 0x27D4_EB2F;
const P5: u32 = 0x1656_67B1;

/// xxHash-32, the hash every LZ4 frame checksum is made with.
fn xxh32(data: &[u8], seed: u32) -> u32 {
    let len = data.len();
    let mut at = 0;
    let mut h = if len >= 16 {
        let mut lanes = [
            seed.wrapping_add(P1).wrapping_add(P2),
            seed.wrapping_add(P2),
            seed,
            seed.wrapping_sub(P1),
        ];
        while at + 16 <= len {
            for (i, lane) in lanes.iter_mut().enumerate() {
                let w = u32::from_le_bytes([
                    data[at + i * 4],
                    data[at + i * 4 + 1],
                    data[at + i * 4 + 2],
                    data[at + i * 4 + 3],
                ]);
                *lane = lane
                    .wrapping_add(w.wrapping_mul(P2))
                    .rotate_left(13)
                    .wrapping_mul(P1);
            }
            at += 16;
        }
        lanes[0]
            .rotate_left(1)
            .wrapping_add(lanes[1].rotate_left(7))
            .wrapping_add(lanes[2].rotate_left(12))
            .wrapping_add(lanes[3].rotate_left(18))
    } else {
        seed.wrapping_add(P5)
    };
    h = h.wrapping_add(len as u32);
    while at + 4 <= len {
        let w = u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
        h = h
            .wrapping_add(w.wrapping_mul(P3))
            .rotate_left(17)
            .wrapping_mul(P4);
        at += 4;
    }
    while at < len {
        h = h
            .wrapping_add(u32::from(data[at]).wrapping_mul(P5))
            .rotate_left(11)
            .wrapping_mul(P1);
        at += 1;
    }
    h ^= h >> 15;
    h = h.wrapping_mul(P2);
    h ^= h >> 13;
    h = h.wrapping_mul(P3);
    h ^= h >> 16;
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &[u8] = include_bytes!("../tests/fixtures/lz4/empty.lz4");
    const ONE_BYTE: &[u8] = include_bytes!("../tests/fixtures/lz4/one-byte.lz4");
    const MIXED: &[u8] = include_bytes!("../tests/fixtures/lz4/mixed-100k.lz4");
    const MIXED_TEXT: &[u8] = include_bytes!("../tests/fixtures/lz4/mixed-100k.txt");
    const MULTIBLOCK: &[u8] = include_bytes!("../tests/fixtures/lz4/multiblock.lz4");
    const LINKED: &[u8] = include_bytes!("../tests/fixtures/lz4/linked.lz4");
    const UNCOMPRESSED: &[u8] = include_bytes!("../tests/fixtures/lz4/uncompressed.lz4");
    const RANDOM: &[u8] = include_bytes!("../tests/fixtures/lz4/random-100k.txt");
    const CHECKSUM_SIZE: &[u8] = include_bytes!("../tests/fixtures/lz4/checksum-size.lz4");
    const CONCATENATED: &[u8] = include_bytes!("../tests/fixtures/lz4/concatenated.lz4");

    /// A frame around blocks this test encoded itself, with a correct descriptor checksum.
    /// `extra` is the optional content-size and dictionary-id bytes, `tail` the optional
    /// content checksum after the end mark.
    fn frame_bytes(flg: u8, bd: u8, extra: &[u8], blocks: &[&[u8]], tail: &[u8]) -> Vec<u8> {
        let mut f = MAGIC.to_vec();
        f.push(flg);
        f.push(bd);
        f.extend_from_slice(extra);
        let hc = (xxh32(&f[4..], 0) >> 8) as u8;
        f.push(hc);
        for block in blocks {
            f.extend_from_slice(&(block.len() as u32).to_le_bytes());
            f.extend_from_slice(block);
        }
        f.extend_from_slice(&0u32.to_le_bytes());
        f.extend_from_slice(tail);
        f
    }

    #[test]
    fn xxh32_matches_the_reference_vectors() {
        // The empty, 1-byte and 3-byte vectors are the specification's; the rest are
        // `xxhsum -H0` on the same bytes (0.8.2), which also covers the 16-byte lane path.
        assert_eq!(xxh32(b"", 0), 0x02CC_5D05);
        assert_eq!(xxh32(b"a", 0), 0x550D_7456);
        assert_eq!(xxh32(b"abc", 0), 0x32D1_53FF);
        assert_eq!(xxh32(b"abcd", 0), 0xA364_3705);
        assert_eq!(xxh32(b"test", 0), 0x3E20_23CF);
        assert_eq!(xxh32(b"a\0b", 0), 0x9146_4384);
        assert_eq!(
            xxh32(b"the quick brown fox jumps over the lazy dog 0123456789", 0),
            0xD1F7_0EDB
        );
    }

    #[test]
    fn frames_decode_to_the_original_bytes() {
        assert!(is_lz4(EMPTY));
        assert_eq!(decompress(EMPTY).unwrap(), b"");
        assert_eq!(decompress(ONE_BYTE).unwrap(), b"a");
        assert_eq!(decompress(MIXED).unwrap(), MIXED_TEXT);
        assert_eq!(decompress(MULTIBLOCK).unwrap(), MIXED_TEXT);
        assert_eq!(decompress(LINKED).unwrap(), MIXED_TEXT);
        assert_eq!(decompress(UNCOMPRESSED).unwrap(), RANDOM);
        assert_eq!(decompress(CHECKSUM_SIZE).unwrap(), MIXED_TEXT);
    }

    #[test]
    fn frames_concatenate_in_order() {
        let mut want = MIXED_TEXT.to_vec();
        want.push(b'a');
        assert_eq!(decompress(CONCATENATED).unwrap(), want);
    }

    #[test]
    fn a_skippable_frame_is_stepped_over() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&0x184D_2A53u32.to_le_bytes());
        stream.extend_from_slice(&5u32.to_le_bytes());
        stream.extend_from_slice(b"https");
        stream.extend_from_slice(ONE_BYTE);
        assert!(is_lz4(&stream));
        assert_eq!(decompress(&stream).unwrap(), b"a");
        stream.extend_from_slice(b"trailing junk");
        assert_eq!(decompress(&stream).unwrap(), b"a");
    }

    #[test]
    fn a_linked_frame_matches_into_the_block_before_it() {
        // "abcd", then a block that copies those four bytes from the previous block.
        let first: &[u8] = &[0x40, b'a', b'b', b'c', b'd'];
        let second: &[u8] = &[0x00, 0x04, 0x00, 0x10, b'z'];
        let linked = frame_bytes(0x40, 0x40, &[], &[first, second], &[]);
        assert_eq!(decompress(&linked).unwrap(), b"abcdabcdz");
        let independent = frame_bytes(0x60, 0x40, &[], &[first, second], &[]);
        assert!(decompress(&independent).is_err());
    }

    #[test]
    fn an_overlapping_match_repeats_what_it_produces() {
        // One literal 'a', then a match of length 8 over the single byte before it.
        let block: &[u8] = &[0x14, b'a', 0x01, 0x00, 0x10, b'z'];
        let frame = frame_bytes(0x60, 0x40, &[], &[block], &[]);
        assert_eq!(decompress(&frame).unwrap(), b"aaaaaaaaaz");
    }

    #[test]
    fn a_match_offset_past_the_output_is_an_error() {
        // 'a', then a match five bytes back, with one byte produced.
        let block: &[u8] = &[0x10, b'a', 0x05, 0x00, 0x00, 0x10, b'z'];
        assert!(decompress(&frame_bytes(0x60, 0x40, &[], &[block], &[])).is_err());
        let zero: &[u8] = &[0x10, b'a', 0x00, 0x00, 0x00, 0x10, b'z'];
        assert!(decompress(&frame_bytes(0x60, 0x40, &[], &[zero], &[])).is_err());
    }

    #[test]
    fn a_truncated_frame_is_an_error() {
        for n in 4..ONE_BYTE.len() {
            assert!(decompress(&ONE_BYTE[..n]).is_err(), "{n} bytes");
        }
        for n in [7, 8, 11, 4000, 9000] {
            assert!(decompress(&MIXED[..n]).is_err(), "{n} bytes");
        }
        for n in [1000, 9000, MULTIBLOCK.len() - 1] {
            assert!(decompress(&MULTIBLOCK[..n]).is_err(), "{n} bytes");
        }
    }

    #[test]
    fn a_block_claiming_more_bytes_than_remain_is_an_error() {
        let mut cut = MIXED.to_vec();
        cut[7..11].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        assert!(decompress(&cut).is_err());
    }

    #[test]
    fn a_block_past_the_frames_block_size_is_an_error() {
        let big = vec![0u8; (64 << 10) + 1];
        let stored = 0x8000_0000u32 | big.len() as u32;
        let mut frame = frame_bytes(0x60, 0x40, &[], &[], &[]);
        frame.truncate(8);
        frame.extend_from_slice(&stored.to_le_bytes());
        frame.extend_from_slice(&big);
        frame.extend_from_slice(&0u32.to_le_bytes());
        assert!(decompress(&frame).is_err());
    }

    #[test]
    fn checksums_are_verified() {
        let mut header = MIXED.to_vec();
        header[5] = 0x60;
        assert!(decompress(&header).is_err());

        let mut content = MIXED.to_vec();
        let last = content.len() - 1;
        content[last] ^= 0xFF;
        assert!(decompress(&content).is_err());

        let mut block = CHECKSUM_SIZE.to_vec();
        block[40] ^= 0xFF;
        assert!(decompress(&block).is_err());

        let mut stored = UNCOMPRESSED.to_vec();
        stored[100] ^= 0xFF;
        assert!(decompress(&stored).is_err());
    }

    #[test]
    fn a_content_size_that_does_not_match_is_an_error() {
        let block: &[u8] = &[0x10, b'a'];
        let sized = frame_bytes(0x68, 0x40, &4u64.to_le_bytes(), &[block], &[]);
        assert!(decompress(&sized).is_err());
        let sized = frame_bytes(0x68, 0x40, &1u64.to_le_bytes(), &[block], &[]);
        assert_eq!(decompress(&sized).unwrap(), b"a");
    }

    #[test]
    fn an_undefined_descriptor_is_an_error() {
        // Version 0, a reserved bit, and a block size code below 64 KiB.
        assert!(decompress(&frame_bytes(0x00, 0x40, &[], &[], &[])).is_err());
        assert!(decompress(&frame_bytes(0x62, 0x40, &[], &[], &[])).is_err());
        assert!(decompress(&frame_bytes(0x60, 0x30, &[], &[], &[])).is_err());
    }

    #[test]
    fn bytes_that_are_not_frames_expand_to_nothing() {
        assert_eq!(decompress(b"").unwrap(), b"");
        assert_eq!(decompress(b"not an lz4 file").unwrap(), b"");
        assert!(!is_lz4(b"not an lz4 file"));
    }
}
