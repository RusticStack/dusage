//! gzip decompression for `-z/--search-zip`, in std alone.
//!
//! Every other mode searches the file as it lies on disk. `-z` asks for the compressed
//! bytes to be expanded first, and with no dependencies to do it with, the DEFLATE decoder
//! is here: a canonical-Huffman decoder over a bit reader, plus the CRC-32 that a gzip
//! trailer uses to prove the expansion was right.
//!
//! Only gzip is handled. It is the format logs are rotated into, and the one an inflater
//! this size gets right; bzip2, xz, LZ4 and zstd would each need a decoder of their own.

use std::io;

/// The two bytes every gzip member starts with.
pub const MAGIC: [u8; 2] = [0x1f, 0x8b];

/// True when `data` starts like a gzip stream.
pub fn is_gzip(data: &[u8]) -> bool {
    data.len() >= 2 && data[..2] == MAGIC
}

/// Expand every gzip member in `data` into one buffer. Trailing bytes that are not a gzip
/// member are left out, and a member whose CRC or length does not match is an error rather
/// than a silently short answer.
pub fn gunzip(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at + 2 <= data.len() && data[at..at + 2] == MAGIC {
        at += 2;
        // ID: 8 = deflate, which is the only method gzip defines.
        let method = data.get(at).copied().ok_or_else(short)?;
        at += 1;
        if method != 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("gzip: compression method {method} is not deflate"),
            ));
        }
        let flags = data.get(at).copied().ok_or_else(short)?;
        at += 1;
        at += 6; // mtime, xfl, os
        if flags & 0x04 != 0 {
            // FEXTRA carries its own length, little endian.
            let lo = *data.get(at).ok_or_else(short)? as usize;
            let hi = *data.get(at + 1).ok_or_else(short)? as usize;
            at += 2 + (hi << 8 | lo);
        }
        for mask in [0x08u8, 0x10] {
            if flags & mask != 0 {
                // FNAME or FCOMMENT: zero-terminated.
                while *data.get(at).ok_or_else(short)? != 0 {
                    at += 1;
                }
                at += 1;
            }
        }
        if flags & 0x02 != 0 {
            at += 2; // FHCRC
        }
        if at > data.len() {
            return Err(short());
        }
        let mut inflater = Inflater {
            out: Vec::new(),
            data: &data[at..],
            pos: 0,
            bitbuf: 0,
            bitcnt: 0,
        };
        inflater.run()?;
        let body = inflater.out;
        // The trailer sits on the byte after the last one DEFLATE consumed.
        let trailer = at + inflater.pos;
        let crc = u32::from_le_bytes(
            data.get(trailer..trailer + 4)
                .and_then(|s| s.try_into().ok())
                .ok_or_else(short)?,
        );
        let size = u32::from_le_bytes(
            data.get(trailer + 4..trailer + 8)
                .and_then(|s| s.try_into().ok())
                .ok_or_else(short)?,
        );
        if crc32(&body) != crc || body.len() as u32 != size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gzip: checksum does not match the expanded data",
            ));
        }
        let start = out.len();
        out.extend_from_slice(&body);
        // A member is a whole file: keep a newline between them so a match cannot join the
        // last line of one to the first line of the next.
        if start > 0 && !out[..start].ends_with(b"\n") {
            out.insert(start, b'\n');
        }
        at = trailer + 8;
    }
    Ok(out)
}

fn short() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "gzip: file ends mid-member")
}

/// One DEFLATE stream, expanded into `out`.
struct Inflater<'a> {
    out: Vec<u8>,
    data: &'a [u8],
    /// Bytes of `data` consumed, which is where the trailer begins once the stream ends.
    pos: usize,
    bitbuf: u64,
    bitcnt: u32,
}

impl<'a> Inflater<'a> {
    fn bit(&mut self) -> io::Result<u32> {
        if self.bitcnt == 0 {
            let b = *self.data.get(self.pos).ok_or_else(short)?;
            self.pos += 1;
            self.bitbuf = u64::from(b);
            self.bitcnt = 8;
        }
        let bit = (self.bitbuf & 1) as u32;
        self.bitbuf >>= 1;
        self.bitcnt -= 1;
        Ok(bit)
    }

    fn bits(&mut self, n: u32) -> io::Result<u32> {
        let mut v = 0;
        for i in 0..n {
            v |= self.bit()? << i;
        }
        Ok(v)
    }

    /// DEFLATE reads its extra bits big-endian-looking but stores them little endian.
    fn align(&mut self) {
        self.bitbuf = 0;
        self.bitcnt = 0;
    }

    fn run(&mut self) -> io::Result<()> {
        loop {
            let last = self.bit()? == 1;
            match self.bits(2)? {
                0 => self.stored()?,
                1 => {
                    let (lit, dist) = fixed_tables();
                    self.codes(&lit, &dist)?;
                }
                2 => {
                    let (lit, dist) = self.dynamic_tables()?;
                    self.codes(&lit, &dist)?;
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "gzip: reserved block type",
                    ))
                }
            }
            if last {
                return Ok(());
            }
        }
    }

    fn stored(&mut self) -> io::Result<()> {
        self.align();
        let len = {
            let lo = *self.data.get(self.pos).ok_or_else(short)? as usize;
            let hi = *self.data.get(self.pos + 1).ok_or_else(short)? as usize;
            hi << 8 | lo
        };
        self.pos += 4; // LEN and its complement
        let end = self.pos + len;
        let chunk = self.data.get(self.pos..end).ok_or_else(short)?;
        self.out.extend_from_slice(chunk);
        self.pos = end;
        Ok(())
    }

    fn codes(&mut self, lit: &Huffman, dist: &Huffman) -> io::Result<()> {
        loop {
            let sym = self.decode(lit)?;
            match sym {
                0..=255 => self.out.push(sym as u8),
                256 => return Ok(()),
                _ => {
                    let i = (sym - 257) as usize;
                    if i >= LENGTH_BASE.len() {
                        return Err(bad_symbol(sym));
                    }
                    let len = LENGTH_BASE[i] as usize + self.bits(LENGTH_EXTRA[i])? as usize;
                    let dsym = self.decode(dist)? as usize;
                    if dsym >= DIST_BASE.len() {
                        return Err(bad_symbol(dsym as u32));
                    }
                    let d = DIST_BASE[dsym] as usize + self.bits(DIST_EXTRA[dsym])? as usize;
                    if d == 0 || d > self.out.len() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "gzip: back-reference before the start of the data",
                        ));
                    }
                    // A copy may overlap its own output, which is how DEFLATE encodes runs.
                    let from = self.out.len() - d;
                    for k in 0..len {
                        let b = self.out[from + k];
                        self.out.push(b);
                    }
                }
            }
        }
    }

    fn decode(&mut self, h: &Huffman) -> io::Result<u32> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..=15usize {
            code |= self.bit()? as i32;
            let count = h.count[len] as i32;
            if code - first < count {
                return Ok(h.symbol[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "gzip: invalid Huffman code",
        ))
    }

    fn dynamic_tables(&mut self) -> io::Result<(Huffman, Huffman)> {
        let nlen = self.bits(5)? as usize + 257;
        let ndist = self.bits(5)? as usize + 1;
        let ncode = self.bits(4)? as usize + 4;
        if nlen > 286 || ndist > 30 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gzip: bad table sizes",
            ));
        }
        let mut lengths = [0u8; 320];
        for i in 0..ncode {
            lengths[ORDER[i]] = self.bits(3)? as u8;
        }
        let code_table = Huffman::new(&lengths[..19])?;
        let mut all = [0u8; 320];
        let mut i = 0;
        while i < nlen + ndist {
            let sym = self.decode(&code_table)?;
            match sym {
                0..=15 => {
                    all[i] = sym as u8;
                    i += 1;
                }
                16..=18 => {
                    let (extra, base) = match sym {
                        16 => (2, 3usize),
                        17 => (3, 3),
                        _ => (7, 11),
                    };
                    let n = base + self.bits(extra)? as usize;
                    let prev = if sym == 16 {
                        *all.get(i.wrapping_sub(1)).ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidData, "gzip: repeat with no code")
                        })?
                    } else {
                        0
                    };
                    if i + n > nlen + ndist {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "gzip: code lengths overflow",
                        ));
                    }
                    for _ in 0..n {
                        all[i] = prev;
                        i += 1;
                    }
                }
                _ => return Err(bad_symbol(sym)),
            }
        }
        let lit = Huffman::new(&all[..nlen])?;
        let dist = Huffman::new(&all[nlen..nlen + ndist])?;
        Ok((lit, dist))
    }
}

fn bad_symbol(sym: u32) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("gzip: symbol {sym} outside the tables"),
    )
}

/// A canonical Huffman table: how many codes of each length, and the symbols in code order.
struct Huffman {
    count: [u16; 16],
    symbol: Vec<u32>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> io::Result<Huffman> {
        let mut count = [0u16; 16];
        for &l in lengths {
            count[l as usize] += 1;
        }
        let mut left = 1i32;
        for c in count.iter().skip(1) {
            left <<= 1;
            left -= *c as i32;
            if left < 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "gzip: over-subscribed Huffman table",
                ));
            }
        }
        let mut offs = [0u16; 16];
        for len in 1..15 {
            offs[len + 1] = offs[len] + count[len];
        }
        let mut symbol = vec![0u32; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[offs[l as usize] as usize] = sym as u32;
                offs[l as usize] += 1;
            }
        }
        // `count[0]` includes the unused lengths, which the decode never reaches.
        count[0] = 0;
        Ok(Huffman { count, symbol })
    }
}

/// The order the code-length code lengths arrive in.
const ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];

const LENGTH_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];

const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// The fixed tables of block type 1.
fn fixed_tables() -> (Huffman, Huffman) {
    let mut lit = [0u8; 288];
    for (i, l) in lit.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let dist = [5u8; 30];
    (
        Huffman::new(&lit).expect("fixed literal table"),
        Huffman::new(&dist).expect("fixed distance table"),
    )
}

/// CRC-32 (IEEE 802.3), which is what a gzip trailer carries.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn stored_block_round_trips() {
        // One stored block: final bit, type 00, then LEN and its complement.
        let text = b"hello\n";
        let mut raw = vec![0x01u8];
        raw.extend_from_slice(&(text.len() as u16).to_le_bytes());
        raw.extend_from_slice(&(!(text.len() as u16)).to_le_bytes());
        raw.extend_from_slice(text);
        let mut member = MAGIC.to_vec();
        member.extend_from_slice(&[8, 0, 0, 0, 0, 0, 0, 0xFF]);
        member.extend_from_slice(&raw);
        member.extend_from_slice(&crc32(text).to_le_bytes());
        member.extend_from_slice(&(text.len() as u32).to_le_bytes());
        assert_eq!(gunzip(&member).unwrap(), text);
    }

    #[test]
    fn a_bad_checksum_is_an_error() {
        let text = b"hello\n";
        let mut raw = vec![0x01u8];
        raw.extend_from_slice(&(text.len() as u16).to_le_bytes());
        raw.extend_from_slice(&(!(text.len() as u16)).to_le_bytes());
        raw.extend_from_slice(text);
        let mut member = MAGIC.to_vec();
        member.extend_from_slice(&[8, 0, 0, 0, 0, 0, 0, 0xFF]);
        member.extend_from_slice(&raw);
        member.extend_from_slice(&0u32.to_le_bytes());
        member.extend_from_slice(&(text.len() as u32).to_le_bytes());
        assert!(gunzip(&member).is_err());
    }

    /// `gzip -9` of "alpha needle beta\ngamma needle delta\n", header and trailer included.
    const REAL: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x08, 0xf1, 0xef, 0xfe, 0x6a, 0x02, 0x03, 0x67, 0x2e, 0x74, 0x78, 0x74,
        0x00, 0x4b, 0xcc, 0x29, 0xc8, 0x48, 0x54, 0xc8, 0x4b, 0x4d, 0x4d, 0xc9, 0x49, 0x55, 0x48,
        0x4a, 0x2d, 0x49, 0xe4, 0x4a, 0x4f, 0xcc, 0xcd, 0x85, 0x8b, 0xa4, 0xa4, 0xe6, 0x00, 0x85,
        0x00, 0x1e, 0xea, 0x96, 0x3e, 0x25, 0x00, 0x00, 0x00,
    ];
    const TEXT: &str = "alpha needle beta\ngamma needle delta\n";

    #[test]
    fn a_real_dynamic_huffman_member_expands() {
        assert!(is_gzip(REAL));
        assert_eq!(gunzip(REAL).unwrap(), TEXT.as_bytes());
    }

    #[test]
    fn members_concatenate_in_order() {
        let mut two = REAL.to_vec();
        two.extend_from_slice(REAL);
        assert_eq!(
            String::from_utf8(gunzip(&two).unwrap()).unwrap(),
            TEXT.repeat(2)
        );
    }

    #[test]
    fn a_truncated_member_is_an_error() {
        assert!(gunzip(&[0x1f, 0x8b, 8, 0]).is_err());
    }
}
