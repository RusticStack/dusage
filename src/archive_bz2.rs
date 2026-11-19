//! bzip2 decompression for `-z/--search-zip`, in std alone.
//!
//! A bzip2 member is a run-length stage, a Burrows-Wheeler transform, a move-to-front stage
//! and then a Huffman-coded multiple-table stream; the container's trailer carries a CRC-32
//! over the expanded block, which is what proves the expansion was right.
//!
//! The inverse transform is a counting sort, never a matrix: the block comes back through a
//! `Vec<u32>` permutation of its own length, so the transform costs four bytes per block byte
//! (3.6 MB at the 900 KiB maximum) and nothing else is proportional to the matrix a block
//! would otherwise need. A block expands to at most 52 times its size, since the first
//! run-length stage packs 259 bytes into 5, so a whole block's working set stays bounded by
//! its header's level whatever the file claims. Like the gzip path there is no cap on the
//! total expanded size; the caller reads the file whole and decides what to search.
//!
//! A file may hold several members back to back (`pbzip2` writes them), and each is expanded
//! in order. A corrupt or truncated member is an error, never a silently short answer; bytes
//! after the last member that do not open another one are left out, as `gunzip` does.

use std::io;

/// The three bytes every bzip2 stream starts with.
pub const MAGIC: &[u8] = b"BZh";

/// The 48-bit marker that opens a block.
const BLOCK_MAGIC: u64 = 0x3141_5926_5359;

/// The 48-bit marker that ends a stream.
const END_MAGIC: u64 = 0x1772_4538_5090;

/// Symbols a Huffman table codes before the selector moves to the next table.
const GROUP_SIZE: usize = 50;

/// Selectors a block can use: enough for the largest block at `GROUP_SIZE` symbols each.
const MAX_SELECTORS: usize = 2 + 900_000 / GROUP_SIZE;

/// The longest code a bzip2 Huffman table may use.
const MAX_LEN: usize = 20;

/// How far a block can grow when the first run-length stage is undone: five bytes hold four
/// plus a count of at most 255, so 259 bytes, and 259 of every 5 is under 52.
const RLE_GROWTH: usize = 52;

/// CRC-32 as bzip2 defines it: polynomial 0x04C11DB7 fed most significant bit first, which
/// is not the reflected CRC-32 a gzip trailer carries.
const CRC_TABLE: [u32; 256] = crc_table();

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u32) << 24;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc = (crc << 8) ^ CRC_TABLE[((crc >> 24) as u8 ^ byte) as usize];
    }
    !crc
}

/// True when `data` starts like a bzip2 stream.
pub fn is_bzip2(data: &[u8]) -> bool {
    data.len() >= 3 && &data[..3] == MAGIC
}

/// Expand every bzip2 member in `data` into one buffer. A member whose CRC does not match its
/// expansion, or that ends early, is an error rather than a silently short answer.
pub fn decompress(data: &[u8]) -> io::Result<Vec<u8>> {
    if !is_bzip2(data) {
        return Err(bad("bzip2: not a bzip2 stream"));
    }
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() && data[at..].starts_with(MAGIC) {
        // The digit fixes the block size, 100 KB for '1' up to 900 KB for '9'.
        let level = *data.get(at + 3).ok_or_else(short)?;
        let block_max = match level {
            b'1'..=b'9' => (level - b'0') as usize * 100_000,
            _ => {
                return Err(bad(format!(
                    "bzip2: block size digit '{}' is not 1..9",
                    level as char
                )))
            }
        };
        let mut bits = Bits::new(data, at + 4);
        let mut combined = 0u32;
        loop {
            let magic = u64::from(bits.number(24)?) << 24 | u64::from(bits.number(24)?);
            if magic == END_MAGIC {
                let stored = bits.number(32)?;
                if stored != combined {
                    return Err(bad("bzip2: combined CRC does not match the expanded data"));
                }
                break;
            }
            if magic != BLOCK_MAGIC {
                return Err(bad("bzip2: bad or missing block magic"));
            }
            let stored_crc = bits.number(32)?;
            if bits.bit()? == 1 {
                return Err(bad("bzip2: randomised (pre-0.9) block is not supported"));
            }
            let origin = bits.number(24)? as usize;
            let block = decode_block(&mut bits, block_max, origin)?;
            let crc = crc32(&block);
            if crc != stored_crc {
                return Err(bad("bzip2: block CRC does not match the expanded block"));
            }
            // Each block's CRC is folded in with a one-bit rotation, so a block moved or
            // dropped between two members shows up in the stream trailer as well.
            combined = combined.rotate_left(1) ^ crc;
            out.extend_from_slice(&block);
        }
        at = bits.aligned();
    }
    Ok(out)
}

fn bad(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn short() -> io::Error {
    bad("bzip2: file ends mid-stream")
}

/// Bits are read most significant first, which is the order bzip2 writes them in.
struct Bits<'a> {
    data: &'a [u8],
    /// Index of the next byte to load; the current byte is already behind it.
    pos: usize,
    byte: u32,
    left: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8], at: usize) -> Bits<'a> {
        Bits {
            data,
            pos: at,
            byte: 0,
            left: 0,
        }
    }

    fn bit(&mut self) -> io::Result<u32> {
        if self.left == 0 {
            self.byte = u32::from(*self.data.get(self.pos).ok_or_else(short)?);
            self.pos += 1;
            self.left = 8;
        }
        self.left -= 1;
        Ok((self.byte >> self.left) & 1)
    }

    fn number(&mut self, n: u32) -> io::Result<u32> {
        let mut value = 0;
        for _ in 0..n {
            value = value << 1 | self.bit()?;
        }
        Ok(value)
    }

    /// Drop the rest of the current byte: a stream ends on a byte boundary, so this is where
    /// the next member would start.
    fn aligned(&mut self) -> usize {
        self.left = 0;
        self.pos
    }

    fn symbol(&mut self, code: &Code) -> io::Result<u32> {
        let mut value = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..=MAX_LEN {
            value |= self.bit()? as i32;
            let count = code.count[len] as i32;
            if value - first < count {
                return Ok(u32::from(code.symbol[(index + value - first) as usize]));
            }
            index += count;
            first = (first + count) << 1;
            value <<= 1;
        }
        Err(bad("bzip2: invalid Huffman code"))
    }
}

/// A canonical Huffman table: how many codes carry each length, and the symbols in code order.
struct Code {
    count: [u16; MAX_LEN + 1],
    symbol: Vec<u16>,
}

impl Code {
    fn new(lengths: &[u8]) -> Code {
        let mut count = [0u16; MAX_LEN + 1];
        for &len in lengths {
            count[len as usize] += 1;
        }
        // Codes are assigned shortest first, then by symbol value, which is how bzip2 orders
        // its `perm` array. The unused lengths are not codes, so they are not counted.
        count[0] = 0;
        let mut offset = [0u16; MAX_LEN + 1];
        for len in 1..MAX_LEN {
            offset[len + 1] = offset[len] + count[len];
        }
        let mut symbol = vec![0u16; lengths.iter().filter(|&&len| len != 0).count()];
        for (value, &len) in lengths.iter().enumerate() {
            if len != 0 {
                let slot = offset[len as usize] as usize;
                symbol[slot] = value as u16;
                offset[len as usize] += 1;
            }
        }
        Code { count, symbol }
    }
}

/// Hands out symbols, moving to the next selector's Huffman table every `GROUP_SIZE` codes.
struct Chooser<'b, 'a> {
    bits: &'b mut Bits<'a>,
    tables: &'b [Code],
    selectors: &'b [usize],
    used: usize,
    left: usize,
    table: usize,
}

impl Chooser<'_, '_> {
    fn next(&mut self) -> io::Result<u32> {
        if self.left == 0 {
            let table = *self
                .selectors
                .get(self.used)
                .ok_or_else(|| bad("bzip2: block uses more selectors than it declares"))?;
            self.used += 1;
            self.table = table;
            self.left = GROUP_SIZE;
        }
        self.left -= 1;
        self.bits.symbol(&self.tables[self.table])
    }
}

/// One block: Huffman, move-to-front with the second run-length stage, the inverse
/// Burrows-Wheeler transform, then the first run-length stage, whose output is the block.
fn decode_block(bits: &mut Bits, block_max: usize, origin: usize) -> io::Result<Vec<u8>> {
    let mut symbols = Vec::new();
    let map = bits.number(16)?;
    for i in 0..16 {
        if map & (0x8000 >> i) != 0 {
            let word = bits.number(16)?;
            for j in 0..16 {
                if word & (0x8000 >> j) != 0 {
                    symbols.push((i * 16 + j) as u8);
                }
            }
        }
    }
    let n_in_use = symbols.len();
    if n_in_use == 0 {
        return Err(bad("bzip2: block declares no symbols"));
    }
    let groups = bits.number(3)? as usize;
    if !(2..=6).contains(&groups) {
        return Err(bad(format!(
            "bzip2: {groups} Huffman tables is outside 2..6"
        )));
    }
    let declared = bits.number(15)? as usize;
    if declared == 0 {
        return Err(bad("bzip2: block declares no selectors"));
    }
    // Some encoders round the selector count up past what a block can use. The reference
    // decoder consumes those entries and ignores them rather than rejecting the file.
    let selectors = declared.min(MAX_SELECTORS);
    let mut mtf = Vec::with_capacity(selectors);
    for _ in 0..declared {
        let mut index = 0usize;
        while bits.bit()? == 1 {
            index += 1;
            if index >= groups {
                return Err(bad("bzip2: selector is outside the tables it chooses from"));
            }
        }
        if mtf.len() < selectors {
            mtf.push(index);
        }
    }
    let mut order = [0usize, 1, 2, 3, 4, 5];
    let mut chosen = Vec::with_capacity(selectors);
    for &index in &mtf {
        let table = order[index];
        order.copy_within(0..index, 1);
        order[0] = table;
        chosen.push(table);
    }

    let mut tables = Vec::with_capacity(groups);
    for _ in 0..groups {
        let mut lengths = vec![0u8; n_in_use + 2];
        // Each length is 5 bits, then adjusted by `+1`/`-1` steps until a 0 bit ends it.
        let mut current = bits.number(5)? as i32;
        for slot in lengths.iter_mut() {
            loop {
                if !(1..=MAX_LEN as i32).contains(&current) {
                    return Err(bad(format!(
                        "bzip2: code length {current} is outside 1..{MAX_LEN}"
                    )));
                }
                if bits.bit()? == 0 {
                    break;
                }
                if bits.bit()? == 0 {
                    current += 1;
                } else {
                    current -= 1;
                }
            }
            *slot = current as u8;
        }
        tables.push(Code::new(&lengths));
    }

    // Symbol 0 and 1 are the run codes, 2..=n_in_use index the move-to-front list (an
    // occurrence of the front value is only ever coded as a run), and n_in_use+1 ends the
    // block.
    let eob = (n_in_use + 1) as u32;
    let mut front: Vec<u8> = (0..n_in_use as u8).collect();
    let mut reader = Chooser {
        bits,
        tables: &tables,
        selectors: &chosen,
        used: 0,
        left: 0,
        table: 0,
    };
    let mut block: Vec<u8> = Vec::new();
    let mut symbol = reader.next()?;
    loop {
        if symbol == eob {
            break;
        }
        if symbol < 2 {
            // A run is a base-2 number: RUNA adds its place value once, RUNB twice.
            let mut run = 0usize;
            let mut place = 1usize;
            while symbol < 2 {
                run += if symbol == 0 { place } else { 2 * place };
                place = place
                    .checked_mul(2)
                    .ok_or_else(|| bad("bzip2: run is longer than any block"))?;
                if run > block_max {
                    return Err(bad("bzip2: run expands past the block size"));
                }
                symbol = reader.next()?;
            }
            repeat(&mut block, symbols[front[0] as usize], run, block_max)?;
            continue;
        }
        let index = (symbol - 1) as usize;
        let value = front[index];
        front.copy_within(0..index, 1);
        front[0] = value;
        repeat(&mut block, symbols[value as usize], 1, block_max)?;
        symbol = reader.next()?;
    }

    if block.is_empty() {
        return Ok(block);
    }
    if origin >= block.len() {
        return Err(bad("bzip2: block origin is outside the block"));
    }
    let sorted = inverse_bwt(&block, origin);
    unrle(&sorted, block_max * RLE_GROWTH)
}

fn repeat(out: &mut Vec<u8>, byte: u8, n: usize, max: usize) -> io::Result<()> {
    if out.len() + n > max {
        return Err(bad("bzip2: run expands past what the block can hold"));
    }
    out.resize(out.len() + n, byte);
    Ok(())
}

/// Undo the Burrows-Wheeler transform. The sorted rotations are the block sorted by first
/// byte, so a counting sort maps each row to the row that follows it; walking that permutation
/// from the origin row spells the original block. The permutation is the only extra structure,
/// four bytes per block byte.
fn inverse_bwt(last: &[u8], origin: usize) -> Vec<u8> {
    let n = last.len();
    let mut start = [0u32; 256];
    for &byte in last {
        start[byte as usize] += 1;
    }
    let mut total = 0u32;
    for slot in start.iter_mut() {
        let count = *slot;
        *slot = total;
        total += count;
    }
    let mut next = vec![0u32; n];
    for (at, &byte) in last.iter().enumerate() {
        let slot = &mut start[byte as usize];
        next[*slot as usize] = at as u32;
        *slot += 1;
    }
    let mut out = Vec::with_capacity(n);
    let mut at = origin;
    for _ in 0..n {
        at = next[at] as usize;
        out.push(last[at]);
    }
    out
}

/// Undo the first run-length stage: four equal bytes introduce a count byte saying how many
/// more of them follow, so a run of up to 255 bytes costs five.
fn unrle(src: &[u8], max: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(src.len().min(max));
    let mut at = 0;
    while at < src.len() {
        let byte = src[at];
        let mut run = 1usize;
        while run < 4 && at + run < src.len() && src[at + run] == byte {
            run += 1;
        }
        if run < 4 {
            repeat(&mut out, byte, run, max)?;
            at += run;
        } else {
            let count = *src
                .get(at + 4)
                .ok_or_else(|| bad("bzip2: block ends inside a run"))?;
            repeat(&mut out, byte, 4 + usize::from(count), max)?;
            at += 5;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `bzip2 -9` of one byte.
    const ONE: &[u8] = include_bytes!("../tests/fixtures/bz2/one.bz2");
    /// `bzip2 -9` of an empty file.
    const EMPTY: &[u8] = include_bytes!("../tests/fixtures/bz2/empty.bz2");
    /// `bzip2 -9` of 200 KiB of mixed text.
    const MIXED: &[u8] = include_bytes!("../tests/fixtures/bz2/mixed_200k.bz2");
    const MIXED_TEXT: &[u8] = include_bytes!("../tests/fixtures/bz2/mixed_200k.txt");
    /// `bzip2 -1` of the same text, so one member holds three 100 KB blocks.
    const MIXED_LEVEL1: &[u8] = include_bytes!("../tests/fixtures/bz2/mixed_200k_level1.bz2");
    /// `bzip2 -9` of one 900k block of repetitive text.
    const BIG: &[u8] = include_bytes!("../tests/fixtures/bz2/big_block.bz2");
    const BIG_TEXT: &[u8] = include_bytes!("../tests/fixtures/bz2/big_block.txt");
    /// Two independent members, as `pbzip2` writes them.
    const TWO: &[u8] = include_bytes!("../tests/fixtures/bz2/two_streams.bz2");
    const TWO_TEXT: &[u8] = include_bytes!("../tests/fixtures/bz2/two_streams.txt");
    /// `mixed_200k.bz2` with one bit flipped inside the first block.
    const CORRUPT: &[u8] = include_bytes!("../tests/fixtures/bz2/corrupt.bz2");

    #[test]
    fn one_byte_expands() {
        assert!(is_bzip2(ONE));
        assert_eq!(decompress(ONE).unwrap(), b"x");
    }

    #[test]
    fn an_empty_file_expands_to_nothing() {
        assert!(is_bzip2(EMPTY));
        assert_eq!(decompress(EMPTY).unwrap(), b"");
    }

    #[test]
    fn mixed_text_expands() {
        assert_eq!(decompress(MIXED).unwrap(), MIXED_TEXT);
    }

    #[test]
    fn several_blocks_in_one_member_expand() {
        assert_eq!(decompress(MIXED_LEVEL1).unwrap(), MIXED_TEXT);
    }

    #[test]
    fn a_full_nine_hundred_kilobyte_block_expands() {
        assert_eq!(decompress(BIG).unwrap(), BIG_TEXT);
    }

    #[test]
    fn members_expand_in_order() {
        assert_eq!(decompress(TWO).unwrap(), TWO_TEXT);
    }

    #[test]
    fn a_flipped_byte_is_an_error() {
        let err = decompress(CORRUPT).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().starts_with("bzip2: "), "{err}");
    }

    #[test]
    fn a_corrupt_later_member_is_an_error() {
        let second = TWO[1..]
            .windows(MAGIC.len())
            .position(|w| w == MAGIC)
            .expect("two members")
            + 1;
        let mut broken = TWO.to_vec();
        broken[second + 8] ^= 0x10;
        let err = decompress(&broken).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(decompress(&TWO[..second + 6]).is_err());
        // The stream trailer's combined CRC covers every block before it. The last byte of a
        // stream ends in padding zeros, so flip its top bit rather than its low one.
        let mut trailer = TWO.to_vec();
        let last = trailer.len() - 1;
        trailer[last] ^= 0x80;
        let err = decompress(&trailer).unwrap_err();
        assert!(err.to_string().contains("combined CRC"), "{err}");
    }

    #[test]
    fn corrupted_input_never_panics() {
        // Flipping bytes and cutting members short must leave an error or a valid expansion,
        // never a panic out of a table or an index built from the damaged stream.
        for at in 0..TWO.len() {
            let mut broken = TWO.to_vec();
            broken[at] ^= 0xFF;
            let _ = decompress(&broken);
        }
        for at in (0..MIXED.len()).step_by(251) {
            let mut broken = MIXED.to_vec();
            broken[at] = broken[at].wrapping_add(1);
            let _ = decompress(&broken);
        }
        for cut in 0..12 {
            let _ = decompress(&ONE[..cut]);
            let _ = decompress(&EMPTY[..cut.min(EMPTY.len())]);
        }
    }

    #[test]
    fn a_truncated_member_is_an_error() {
        let exact = decompress(MIXED).unwrap();
        for cut in [4, MIXED.len() / 2, MIXED.len() - 5, MIXED.len() - 1] {
            assert!(
                decompress(&MIXED[..cut]).is_err(),
                "a member cut at {cut} of {} decoded",
                MIXED.len()
            );
        }
        assert_eq!(exact.len(), MIXED_TEXT.len());
        // A cut inside the trailer is short, never quietly accepted.
        assert!(decompress(&MIXED[..MIXED.len() - 4]).is_err());
    }

    #[test]
    fn a_bad_header_is_an_error() {
        assert!(!is_bzip2(b"BZx9"));
        assert!(decompress(b"").is_err());
        assert!(decompress(b"BZx9rest").is_err());
        // The level digit fixes the block size, so 0 and ':' are both out of range.
        assert!(decompress(b"BZh0").is_err());
        assert!(decompress(b"BZh:").is_err());
        assert!(decompress(b"BZh9").is_err());
    }

    #[test]
    fn a_bad_block_magic_is_an_error() {
        // A level digit and then six bytes that are neither a block nor an end marker.
        let mut raw = b"BZh9".to_vec();
        raw.extend_from_slice(&[0x31, 0x41, 0x59, 0x26, 0x53, 0x5A]);
        raw.extend_from_slice(&[0u8; 8]);
        let err = decompress(&raw).unwrap_err();
        assert!(err.to_string().contains("block magic"), "{err}");
    }

    #[test]
    fn trailing_bytes_after_the_last_member_are_left_out() {
        let mut with_junk = ONE.to_vec();
        with_junk.extend_from_slice(b"\0\0not a member");
        assert_eq!(decompress(&with_junk).unwrap(), b"x");
    }

    #[test]
    fn decode_throughput_is_reported() {
        let bytes = MIXED_TEXT.len();
        let at = std::time::Instant::now();
        let mut runs = 0;
        let mut expanded = 0;
        while at.elapsed() < std::time::Duration::from_millis(400) {
            expanded += decompress(MIXED).unwrap().len();
            runs += 1;
        }
        let elapsed = at.elapsed().as_secs_f64();
        assert_eq!(expanded, bytes * runs);
        let mib = expanded as f64 / (1024.0 * 1024.0);
        eprintln!(
            "bzip2: {runs} runs, {:.3} MiB expanded in {elapsed:.3}s = {:.1} MiB/s",
            mib,
            mib / elapsed
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_real_bzip2_agrees_with_the_decoder() {
        let have = std::process::Command::new("sh")
            .args(["-c", "command -v bzip2"])
            .output()
            .is_ok_and(|o| o.status.success());
        if !have {
            eprintln!("skipped: bzip2 is not installed");
            return;
        }
        let mut seed = 0x1234_5678_u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for round in 0..128 {
            let mut raw = Vec::new();
            let len = 1 + (next() % 7000) as usize;
            while raw.len() < len {
                match next() % 4 {
                    0 => {
                        let byte = next() as u8;
                        let run = 1 + (next() % 400) as usize;
                        raw.resize(raw.len() + run, byte);
                    }
                    1 => raw.extend((0..=255u8).cycle().take(1 + (next() % 60) as usize)),
                    2 => raw.extend_from_slice(b"needle alpha gamma delta 0123456789\n"),
                    _ => raw.push(next() as u8),
                }
            }
            raw.truncate(len);
            let level = if round % 2 == 0 { "2" } else { "9" };
            let packed = std::process::Command::new("sh")
                .args(["-c", &format!("bzip2 -{level} -c")])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    child.stdin.as_mut().expect("piped stdin").write_all(&raw)?;
                    child.wait_with_output()
                })
                .expect("run bzip2");
            assert!(packed.status.success(), "bzip2 refused round {round}");
            assert_eq!(
                decompress(&packed.stdout).unwrap(),
                raw,
                "round {round} ({} bytes at level {level})",
                raw.len()
            );
        }
    }
}
