//! xz decompression for `-z/--search-zip`, in std alone.
//!
//! An xz file is a chain of streams. Each opens with a 12-byte stream header whose CRC-32
//! covers the two flag bytes, holds blocks, then an index, and closes with a 12-byte footer that
//! repeats the flags and names the index size backwards. A block is a header (its optional
//! sizes and the filter chain, CRC-32 checked), the compressed data, zero padding to a multiple
//! of four, and a check over the block's expanded bytes: CRC-32, CRC-64, SHA-256 or nothing.
//! Streams may be concatenated, separated by stream padding of a multiple of four zero bytes.
//!
//! The only filter here is LZMA2 (`0x21`). It is a sequence of chunks, each either a run of
//! uncompressed bytes or a range-coded LZMA chunk that declares how many bytes it produces and
//! can reset the dictionary, the model state, or the lc/lp/pb properties. LZMA is an 11-bit
//! probability range coder over the literal, length, position-slot and alignment models, plus
//! four repeat distances that turn a match into a handful of bits.
//!
//! The dictionary a header asks for is capped at 128 MiB. The decoder holds the expanded bytes
//! itself, so the cap exists to reject a stream whose declared window could not be honest here
//! rather than to size an allocation.

use std::io;

/// The six bytes every xz stream starts with.
pub const MAGIC: [u8; 6] = [0xfd, b'7', b'z', b'X', b'Z', 0x00];

/// The largest dictionary an xz block header may ask for, in bytes.
pub const MAX_DICT: u32 = 128 << 20;

/// The LZMA2 filter id, the only filter this decoder knows.
const FILTER_LZMA2: u64 = 0x21;

/// Where a probability starts: a half, in an eleven-bit model.
const PROB_INIT: u16 = 1 << 10;

/// True when `data` starts like an xz stream.
pub fn is_xz(data: &[u8]) -> bool {
    data.len() >= 6 && data[..6] == MAGIC
}

/// Expand every xz stream in `data` into one buffer.
///
/// Blocks are checked as they are expanded, so a mismatch between a declared size, the index,
/// or a block's CRC-32/CRC-64/SHA-256 is an error rather than a silently short answer.
pub fn decompress(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        if at > 0 {
            let mut pad = 0;
            while data.get(at) == Some(&0) {
                at += 1;
                pad += 1;
            }
            if pad % 4 != 0 {
                return Err(bad("stream padding is not a multiple of four bytes"));
            }
            if at == data.len() {
                break;
            }
        }
        if !is_xz(&data[at..]) {
            return Err(bad(if at == 0 {
                "bad magic"
            } else {
                "trailing data after the last stream"
            }));
        }
        at = stream(data, at, &mut out)?;
    }
    Ok(out)
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("xz: {msg}"))
}

fn short() -> io::Error {
    bad("file ends mid-stream")
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn be16(data: &[u8], at: usize) -> io::Result<u32> {
    let b = data.get(at..at + 2).ok_or_else(short)?;
    Ok(u32::from(b[0]) << 8 | u32::from(b[1]))
}

/// One stream, from its magic to the end of its footer. Returns the offset just past it.
fn stream(data: &[u8], at: usize, out: &mut Vec<u8>) -> io::Result<usize> {
    let header = data.get(at..at + 12).ok_or_else(short)?;
    let flags = [header[6], header[7]];
    if flags[0] != 0 || flags[1] & 0xF0 != 0 {
        return Err(bad("reserved stream flag is set"));
    }
    let check = Check::from_flags(flags[1] & 0x0F)?;
    if le32(&header[8..12]) != crc32(&flags) {
        return Err(bad("stream header CRC does not match"));
    }
    let mut at = at + 12;
    let mut blocks: Vec<(u64, u64)> = Vec::new();
    loop {
        match data.get(at) {
            None => return Err(bad("file ends before the index")),
            Some(0x00) => break,
            Some(_) => {}
        }
        let (next, unpadded, expanded) = block(data, at, check, out)?;
        blocks.push((unpadded, expanded));
        at = next;
    }

    let index_start = at;
    at += 1;
    let (count, next) = varint(data, at)?;
    at = next;
    if count as usize != blocks.len() {
        return Err(bad("index record count does not match the blocks"));
    }
    for &(unpadded, expanded) in &blocks {
        let (u, next) = varint(data, at)?;
        let (e, next) = varint(data, next)?;
        at = next;
        if (u, e) != (unpadded, expanded) {
            return Err(bad("index sizes do not match a block"));
        }
    }
    while !(at - index_start).is_multiple_of(4) {
        if data.get(at) != Some(&0) {
            return Err(bad("index padding is not zero"));
        }
        at += 1;
    }
    let crc = data.get(at..at + 4).ok_or_else(short)?;
    if le32(crc) != crc32(&data[index_start..at]) {
        return Err(bad("index CRC does not match"));
    }
    let index_size = at + 4 - index_start;
    at += 4;

    let footer = data.get(at..at + 12).ok_or_else(short)?;
    if footer[10..12] != *b"YZ" {
        return Err(bad("bad stream footer magic"));
    }
    if le32(&footer[0..4]) != crc32(&footer[4..10]) {
        return Err(bad("stream footer CRC does not match"));
    }
    if le32(&footer[4..8]) as usize != index_size / 4 - 1 {
        return Err(bad("stream footer size does not match the index"));
    }
    if footer[8..10] != flags {
        return Err(bad("stream footer flags do not match the header"));
    }
    Ok(at + 12)
}

/// One block: header, compressed data, padding, check. Returns the offset past the check and
/// the block's unpadded and expanded sizes for the index.
fn block(data: &[u8], at: usize, check: Check, out: &mut Vec<u8>) -> io::Result<(usize, u64, u64)> {
    let size_byte = *data.get(at).ok_or_else(short)? as usize;
    if size_byte == 0 {
        return Err(bad("block header size byte is zero"));
    }
    let header_size = (size_byte + 1) * 4;
    let header = data.get(at..at + header_size).ok_or_else(short)?;
    let fields = &header[..header_size - 4];
    if le32(&header[header_size - 4..]) != crc32(fields) {
        return Err(bad("block header CRC does not match"));
    }
    let flags = fields[1];
    if flags & 0x3C != 0 {
        return Err(bad("reserved block flag is set"));
    }
    let mut r = Reader {
        data: fields,
        at: 2,
    };
    let declared_compressed = if flags & 0x40 != 0 {
        Some(r.varint()?)
    } else {
        None
    };
    let declared_expanded = if flags & 0x80 != 0 {
        Some(r.varint()?)
    } else {
        None
    };
    let mut filters = Vec::new();
    for _ in 0..(flags & 0x03) + 1 {
        let id = r.varint()?;
        let len = r.varint()? as usize;
        let props = r.take(len)?.to_vec();
        filters.push((id, props));
    }
    if r.rest().iter().any(|&b| b != 0) {
        return Err(bad("block header padding is not zero"));
    }
    let (id, props) = match filters.as_slice() {
        [(id, props)] => (*id, props.as_slice()),
        _ => return Err(bad("a chain of several filters is not supported")),
    };
    if id != FILTER_LZMA2 {
        return Err(bad(&format!("unsupported filter 0x{id:02X}")));
    }
    let dict = dict_size(props)?;

    let start_of_block = at;
    let start = out.len();
    let data_start = at + header_size;
    let end = lzma2(data, data_start, dict, out)?;
    let compressed_size = end - data_start;
    if let Some(c) = declared_compressed {
        if c != compressed_size as u64 {
            return Err(bad("block header size does not match the compressed data"));
        }
    }
    let mut at = end;
    while !(at - start_of_block).is_multiple_of(4) {
        if data.get(at) != Some(&0) {
            return Err(bad("block padding is not zero"));
        }
        at += 1;
    }
    let expanded = &out[start..];
    let stored = data.get(at..at + check.len()).ok_or_else(short)?;
    check.verify(expanded, stored)?;
    if let Some(e) = declared_expanded {
        if e != expanded.len() as u64 {
            return Err(bad("block header size does not match the expanded data"));
        }
    }
    let unpadded = (header_size + compressed_size + check.len()) as u64;
    Ok((at + check.len(), unpadded, expanded.len() as u64))
}

/// The dictionary size an LZMA2 properties byte names.
fn dict_size(props: &[u8]) -> io::Result<u32> {
    let p = match props {
        [p] => *p,
        _ => return Err(bad("LZMA2 filter properties must be one byte")),
    };
    if p > 40 {
        return Err(bad(&format!(
            "LZMA2 dictionary properties {p} are out of range"
        )));
    }
    let dict = if p == 40 {
        u32::MAX
    } else {
        (2 | (u32::from(p) & 1)) << (u32::from(p) / 2 + 11)
    };
    if dict > MAX_DICT {
        return Err(bad(&format!(
            "dictionary of {dict} bytes exceeds the {MAX_DICT} byte limit"
        )));
    }
    Ok(dict)
}

/// Which check a stream's blocks carry.
#[derive(Clone, Copy)]
enum Check {
    None,
    Crc32,
    Crc64,
    Sha256,
}

impl Check {
    fn from_flags(bits: u8) -> io::Result<Check> {
        Ok(match bits {
            0 => Check::None,
            1 => Check::Crc32,
            4 => Check::Crc64,
            10 => Check::Sha256,
            _ => return Err(bad(&format!("check id {bits} is not defined"))),
        })
    }

    fn len(self) -> usize {
        match self {
            Check::None => 0,
            Check::Crc32 => 4,
            Check::Crc64 => 8,
            Check::Sha256 => 32,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Check::None => "none",
            Check::Crc32 => "crc32",
            Check::Crc64 => "crc64",
            Check::Sha256 => "sha256",
        }
    }

    fn verify(self, data: &[u8], stored: &[u8]) -> io::Result<()> {
        let ok = match self {
            Check::None => true,
            Check::Crc32 => le32(stored) == crc32(data),
            Check::Crc64 => {
                let mut b = [0u8; 8];
                b.copy_from_slice(stored);
                u64::from_le_bytes(b) == crc64(data)
            }
            Check::Sha256 => stored == sha256(data),
        };
        if ok {
            Ok(())
        } else {
            Err(bad(&format!(
                "{} check does not match the block data",
                self.name()
            )))
        }
    }
}

/// A cursor over header fields.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        let s = self.data.get(self.at..self.at + n).ok_or_else(short)?;
        self.at += n;
        Ok(s)
    }

    fn rest(&self) -> &'a [u8] {
        &self.data[self.at..]
    }

    fn varint(&mut self) -> io::Result<u64> {
        let (v, next) = varint(self.data, self.at)?;
        self.at = next;
        Ok(v)
    }
}

/// A multibyte integer: seven bits per byte, low group first, at most nine bytes.
fn varint(data: &[u8], at: usize) -> io::Result<(u64, usize)> {
    let mut value = 0u64;
    for i in 0..9 {
        let b = *data.get(at + i).ok_or_else(short)?;
        value |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            if i != 0 && b == 0 {
                return Err(bad("multibyte integer is not minimal"));
            }
            return Ok((value, at + i + 1));
        }
    }
    Err(bad("multibyte integer is longer than nine bytes"))
}

/// CRC-32 (IEEE 802.3), the check xz streams use by default.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// CRC-64/XZ: the reflected 0xC96C5795D7870F42 polynomial, all ones at both ends.
fn crc64(data: &[u8]) -> u64 {
    let mut crc = u64::MAX;
    for &b in data {
        crc ^= u64::from(b);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xC96C_5795_D787_0F42 & mask);
        }
    }
    !crc
}

/// SHA-256, for streams built with `--check=sha256`.
fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let (blocks, rest) = data.as_chunks::<64>();
    for c in blocks {
        sha256_block(&mut h, c);
    }
    let padded = if rest.len() < 56 { 64 } else { 128 };
    let mut tail = [0u8; 128];
    tail[..rest.len()].copy_from_slice(rest);
    tail[rest.len()] = 0x80;
    let bits = (data.len() as u64).wrapping_mul(8);
    tail[padded - 8..padded].copy_from_slice(&bits.to_be_bytes());
    let (blocks, _) = tail[..padded].as_chunks::<64>();
    for c in blocks {
        sha256_block(&mut h, c);
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

fn sha256_block(h: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([
            block[i * 4],
            block[i * 4 + 1],
            block[i * 4 + 2],
            block[i * 4 + 3],
        ]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = hh
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(SHA256_K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
        *slot = slot.wrapping_add(v);
    }
}

const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// The LZMA2 chunk sequence of one block: each chunk is either a run of plain bytes or a
/// twenty-one-bit-declared run of range-coded bytes, and a zero control byte ends the block.
/// Returns the offset past that end marker.
fn lzma2(data: &[u8], at: usize, dict: u32, out: &mut Vec<u8>) -> io::Result<usize> {
    let mut at = at;
    let mut lzma: Option<Lzma> = None;
    let mut dict_start = out.len();
    let mut need_dict_reset = true;
    let mut need_state_reset = true;
    let mut need_props = true;
    loop {
        let control = *data.get(at).ok_or_else(short)?;
        at += 1;
        if control == 0 {
            return Ok(at);
        }
        if control <= 0x02 {
            let size = be16(data, at)? as usize + 1;
            at += 2;
            let chunk = data.get(at..at + size).ok_or_else(short)?;
            if control == 0x01 {
                dict_start = out.len();
                need_dict_reset = false;
            } else if need_dict_reset {
                return Err(bad("LZMA2 stream does not start with a dictionary reset"));
            }
            out.extend_from_slice(chunk);
            at += size;
            need_state_reset = true;
            continue;
        }
        if control < 0x80 {
            return Err(bad(&format!(
                "LZMA2 chunk control {control:#04x} is reserved"
            )));
        }
        let expanded = (((control & 0x1F) as usize) << 16) + be16(data, at)? as usize + 1;
        at += 2;
        let compressed = be16(data, at)? as usize + 1;
        at += 2;
        let reset = (control >> 5) & 0x03;
        if reset >= 3 {
            dict_start = out.len();
            need_dict_reset = false;
        } else if need_dict_reset {
            return Err(bad("LZMA2 stream does not start with a dictionary reset"));
        }
        let decoder = lzma.get_or_insert_with(|| Lzma::new(data));
        if reset >= 2 {
            let props = *data.get(at).ok_or_else(short)?;
            at += 1;
            decoder.set_props(props)?;
            need_props = false;
        } else if need_props {
            return Err(bad("LZMA2 chunk before the first properties byte"));
        }
        if reset >= 1 {
            decoder.reset_state();
        } else if need_state_reset {
            return Err(bad(
                "LZMA2 chunk after an uncompressed chunk without a state reset",
            ));
        }
        need_state_reset = false;
        let end = at + compressed;
        if end > data.len() {
            return Err(short());
        }
        decoder.chunk(at, end, expanded, out, dict_start, dict)?;
        at = end;
    }
}

const NUM_STATES: usize = 12;
const POS_STATES_MAX: usize = 16;
const LEN_TO_POS_STATES: usize = 4;
const MATCH_MIN_LEN: usize = 2;
const END_POS_MODEL_INDEX: u32 = 14;

/// Where each position slot's reverse bit-tree models start.
const SPEC_BASE: [usize; 14] = [0, 0, 0, 0, 0, 2, 4, 8, 12, 20, 28, 44, 60, 92];

/// One LZMA model set plus the range coder that drives it. Probabilities and the state machine
/// survive chunk boundaries; only the range coder is re-initialised per chunk.
struct Lzma<'a> {
    rc: Range<'a>,
    state: usize,
    rep: [u32; 4],
    is_match: [u16; NUM_STATES * POS_STATES_MAX],
    is_rep: [u16; NUM_STATES],
    is_rep_g0: [u16; NUM_STATES],
    is_rep_g1: [u16; NUM_STATES],
    is_rep_g2: [u16; NUM_STATES],
    is_rep0_long: [u16; NUM_STATES * POS_STATES_MAX],
    literal: Vec<u16>,
    pos_slot: [u16; LEN_TO_POS_STATES * 64],
    spec_pos: [u16; 128],
    align: [u16; 16],
    len: LenProbs,
    rep_len: LenProbs,
    lc: u32,
    lp: u32,
    pb: u32,
}

impl<'a> Lzma<'a> {
    fn new(data: &'a [u8]) -> Lzma<'a> {
        Lzma {
            rc: Range {
                data,
                pos: 0,
                limit: 0,
                range: 0,
                code: 0,
            },
            state: 0,
            rep: [0; 4],
            is_match: [PROB_INIT; NUM_STATES * POS_STATES_MAX],
            is_rep: [PROB_INIT; NUM_STATES],
            is_rep_g0: [PROB_INIT; NUM_STATES],
            is_rep_g1: [PROB_INIT; NUM_STATES],
            is_rep_g2: [PROB_INIT; NUM_STATES],
            is_rep0_long: [PROB_INIT; NUM_STATES * POS_STATES_MAX],
            literal: vec![PROB_INIT; 0x300],
            pos_slot: [PROB_INIT; LEN_TO_POS_STATES * 64],
            spec_pos: [PROB_INIT; 128],
            align: [PROB_INIT; 16],
            len: LenProbs::new(),
            rep_len: LenProbs::new(),
            lc: 3,
            lp: 0,
            pb: 2,
        }
    }

    /// A properties byte: lc in its low three bits, then lp, then pb.
    fn set_props(&mut self, props: u8) -> io::Result<()> {
        if props >= 9 * 5 * 5 {
            return Err(bad("LZMA properties byte is out of range"));
        }
        let props = u32::from(props);
        self.lc = props % 9;
        self.lp = props / 9 % 5;
        self.pb = props / 45;
        self.literal = vec![PROB_INIT; 0x300 << (self.lc + self.lp)];
        self.reset_state();
        Ok(())
    }

    fn reset_state(&mut self) {
        self.state = 0;
        self.rep = [0; 4];
        self.is_match.fill(PROB_INIT);
        self.is_rep.fill(PROB_INIT);
        self.is_rep_g0.fill(PROB_INIT);
        self.is_rep_g1.fill(PROB_INIT);
        self.is_rep_g2.fill(PROB_INIT);
        self.is_rep0_long.fill(PROB_INIT);
        self.literal.fill(PROB_INIT);
        self.pos_slot.fill(PROB_INIT);
        self.spec_pos.fill(PROB_INIT);
        self.align.fill(PROB_INIT);
        self.len.reset();
        self.rep_len.reset();
    }

    /// Decode one chunk: `bytes` of expanded output appended to `out`, read from the
    /// compressed range `start..end`, for which `dict_start` is where the window began.
    fn chunk(
        &mut self,
        start: usize,
        end: usize,
        bytes: usize,
        out: &mut Vec<u8>,
        dict_start: usize,
        dict: u32,
    ) -> io::Result<()> {
        self.rc.pos = start;
        self.rc.limit = end;
        self.rc.range = u32::MAX;
        self.rc.code = 0;
        if self.rc.byte()? != 0 {
            return Err(bad("range coder does not start with a zero byte"));
        }
        for _ in 0..4 {
            let b = self.rc.byte()?;
            self.rc.code = (self.rc.code << 8) | u32::from(b);
        }
        let target = out.len() + bytes;
        while out.len() < target {
            let pos = out.len() - dict_start;
            let pos_state = (pos as u32 & ((1 << self.pb) - 1)) as usize;
            let state = self.state;
            if self
                .rc
                .bit(&mut self.is_match[state * POS_STATES_MAX + pos_state])?
                == 0
            {
                self.literal_byte(out, pos, dict_start)?;
            } else {
                self.match_byte(state, pos_state, out, dict_start, dict)?;
            }
        }
        if self.rc.code != 0 {
            return Err(bad("range coder is not finished at the end of a chunk"));
        }
        Ok(())
    }

    /// `pos` is the position since the last dictionary reset: it drives both the position
    /// state and the literal context, whose previous byte is zero at a reset.
    fn literal_byte(&mut self, out: &mut Vec<u8>, pos: usize, dict_start: usize) -> io::Result<()> {
        let prev = u32::from(if pos == 0 { 0 } else { out[out.len() - 1] });
        let ctx = (((pos as u32) & ((1 << self.lp) - 1)) << self.lc) + (prev >> (8 - self.lc));
        let base = 0x300 * ctx as usize;
        let probs = &mut self.literal[base..base + 0x300];
        let mut sym = 1usize;
        if self.state >= 7 {
            let dist = self.rep[0] as usize + 1;
            if dist > out.len() - dict_start {
                return Err(bad("literal context points before the dictionary"));
            }
            let mut match_byte = u32::from(out[out.len() - dist]);
            let mut matched = true;
            while sym < 0x100 {
                let bit = if matched {
                    match_byte <<= 1;
                    let mb = (match_byte & 0x100) as usize;
                    let bit = self.rc.bit(&mut probs[0x100 + mb + sym])?;
                    matched = (mb != 0) == (bit == 1);
                    bit
                } else {
                    self.rc.bit(&mut probs[sym])?
                };
                sym = (sym << 1) | bit as usize;
            }
        } else {
            while sym < 0x100 {
                let bit = self.rc.bit(&mut probs[sym])?;
                sym = (sym << 1) | bit as usize;
            }
        }
        out.push(sym as u8);
        self.state = match self.state {
            0..=3 => 0,
            4..=9 => self.state - 3,
            _ => self.state - 6,
        };
        Ok(())
    }

    fn match_byte(
        &mut self,
        state: usize,
        pos_state: usize,
        out: &mut Vec<u8>,
        dict_start: usize,
        dict: u32,
    ) -> io::Result<()> {
        if self.rc.bit(&mut self.is_rep[state])? == 0 {
            self.rep[3] = self.rep[2];
            self.rep[2] = self.rep[1];
            self.rep[1] = self.rep[0];
            let len = self.len.decode(&mut self.rc, pos_state)? as usize + MATCH_MIN_LEN;
            self.state = if state < 7 { 7 } else { 10 };
            let dist = self.distance(len)?;
            if dist == u32::MAX {
                return Err(bad("end marker inside an LZMA2 chunk"));
            }
            self.rep[0] = dist;
            return self.copy(dist + 1, len, out, dict_start, dict);
        }
        if self.rc.bit(&mut self.is_rep_g0[state])? == 0 {
            if self
                .rc
                .bit(&mut self.is_rep0_long[state * POS_STATES_MAX + pos_state])?
                == 0
            {
                self.state = if state < 7 { 9 } else { 11 };
                let dist = self.rep[0] + 1;
                return self.copy(dist, 1, out, dict_start, dict);
            }
        } else {
            let dist = if self.rc.bit(&mut self.is_rep_g1[state])? == 0 {
                self.rep[1]
            } else {
                let d = if self.rc.bit(&mut self.is_rep_g2[state])? == 0 {
                    self.rep[2]
                } else {
                    let d = self.rep[3];
                    self.rep[3] = self.rep[2];
                    d
                };
                self.rep[2] = self.rep[1];
                d
            };
            self.rep[1] = self.rep[0];
            self.rep[0] = dist;
        }
        self.state = if state < 7 { 8 } else { 11 };
        let len = self.rep_len.decode(&mut self.rc, pos_state)? as usize + MATCH_MIN_LEN;
        let dist = self.rep[0] + 1;
        self.copy(dist, len, out, dict_start, dict)
    }

    /// The distance of a match, in the four cases the position slots define: the low four
    /// directly, a reverse bit-tree for the next ten, and direct bits plus the align tree above.
    fn distance(&mut self, len: usize) -> io::Result<u32> {
        let slot_state = (len - MATCH_MIN_LEN).min(LEN_TO_POS_STATES - 1);
        let slot = self
            .rc
            .tree(&mut self.pos_slot[slot_state * 64..slot_state * 64 + 64], 6)?;
        if slot < 4 {
            return Ok(slot);
        }
        if slot == 63 {
            return Ok(u32::MAX);
        }
        let direct = (slot >> 1) - 1;
        let mut dist = (2 | (slot & 1)) << direct;
        if slot < END_POS_MODEL_INDEX {
            let base = SPEC_BASE[slot as usize];
            dist += self.rc.rev_tree(&mut self.spec_pos[base..], direct)?;
        } else {
            dist += self.rc.direct(direct - 4)? << 4;
            dist += self.rc.rev_tree(&mut self.align, 4)?;
        }
        Ok(dist)
    }

    fn copy(
        &self,
        dist: u32,
        len: usize,
        out: &mut Vec<u8>,
        dict_start: usize,
        dict: u32,
    ) -> io::Result<()> {
        let d = dist as usize;
        let avail = out.len() - dict_start;
        if d > avail || dist > dict {
            return Err(bad("back-reference before the start of the dictionary"));
        }
        let from = out.len() - d;
        let mut left = len;
        while left > 0 {
            let take = left.min(d);
            out.extend_from_within(from..from + take);
            left -= take;
        }
        Ok(())
    }
}

/// The length models: a two-way choice and three trees, per position state.
struct LenProbs {
    choice: u16,
    choice2: u16,
    low: [[u16; 8]; POS_STATES_MAX],
    mid: [[u16; 8]; POS_STATES_MAX],
    high: [u16; 256],
}

impl LenProbs {
    fn new() -> LenProbs {
        LenProbs {
            choice: PROB_INIT,
            choice2: PROB_INIT,
            low: [[PROB_INIT; 8]; POS_STATES_MAX],
            mid: [[PROB_INIT; 8]; POS_STATES_MAX],
            high: [PROB_INIT; 256],
        }
    }

    fn reset(&mut self) {
        self.choice = PROB_INIT;
        self.choice2 = PROB_INIT;
        self.low = [[PROB_INIT; 8]; POS_STATES_MAX];
        self.mid = [[PROB_INIT; 8]; POS_STATES_MAX];
        self.high.fill(PROB_INIT);
    }

    /// A length with two subtracted: 0..7, 8..15, then 16..271.
    fn decode(&mut self, rc: &mut Range<'_>, pos_state: usize) -> io::Result<u32> {
        if rc.bit(&mut self.choice)? == 0 {
            rc.tree(&mut self.low[pos_state], 3)
        } else if rc.bit(&mut self.choice2)? == 0 {
            Ok(8 + rc.tree(&mut self.mid[pos_state], 3)?)
        } else {
            Ok(16 + rc.tree(&mut self.high, 8)?)
        }
    }
}

/// The eleven-bit range decoder of the LZMA model.
struct Range<'a> {
    data: &'a [u8],
    pos: usize,
    /// The chunk's last byte is at `limit`.
    limit: usize,
    range: u32,
    code: u32,
}

impl<'a> Range<'a> {
    fn byte(&mut self) -> io::Result<u8> {
        if self.pos >= self.limit {
            return Err(bad("compressed chunk ends inside its range-coded data"));
        }
        let b = self.data[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn normalize(&mut self) -> io::Result<()> {
        while self.range < 0x0100_0000 {
            if self.range == 0 {
                return Err(bad("range coder collapsed"));
            }
            self.range <<= 8;
            let b = self.byte()?;
            self.code = (self.code << 8) | u32::from(b);
        }
        Ok(())
    }

    fn bit(&mut self, p: &mut u16) -> io::Result<u32> {
        let bound = (self.range >> 11) * u32::from(*p);
        let bit = if self.code < bound {
            self.range = bound;
            *p += (2048 - *p) >> 5;
            0
        } else {
            self.range -= bound;
            self.code -= bound;
            *p -= *p >> 5;
            1
        };
        self.normalize()?;
        Ok(bit)
    }

    fn direct(&mut self, bits: u32) -> io::Result<u32> {
        let mut v = 0u32;
        for _ in 0..bits {
            self.range >>= 1;
            let bit = if self.code >= self.range {
                self.code -= self.range;
                1
            } else {
                0
            };
            self.normalize()?;
            v = (v << 1) | bit;
        }
        Ok(v)
    }

    fn tree(&mut self, probs: &mut [u16], bits: u32) -> io::Result<u32> {
        let mut m = 1usize;
        for _ in 0..bits {
            let b = self.bit(&mut probs[m])?;
            m = (m << 1) | b as usize;
        }
        Ok((m - (1 << bits)) as u32)
    }

    fn rev_tree(&mut self, probs: &mut [u16], bits: u32) -> io::Result<u32> {
        let mut m = 1usize;
        let mut v = 0u32;
        for i in 0..bits {
            let b = self.bit(&mut probs[m])?;
            m = (m << 1) | b as usize;
            v |= b << i;
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const MIXED: &[u8] = include_bytes!("../tests/fixtures/xz/mixed.txt");
    const ONE: &[u8] = include_bytes!("../tests/fixtures/xz/one_byte.txt");
    const ALPHA: &[u8] = include_bytes!("../tests/fixtures/xz/alpha.txt");
    const BETA: &[u8] = include_bytes!("../tests/fixtures/xz/beta.txt");
    const INCOMPRESSIBLE: &[u8] = include_bytes!("../tests/fixtures/xz/incompressible.bin");
    const EMPTY: &[u8] = include_bytes!("../tests/fixtures/xz/empty.txt");

    const FIXTURES: &[(&[u8], &[u8])] = &[
        (
            include_bytes!("../tests/fixtures/xz/empty.xz"),
            include_bytes!("../tests/fixtures/xz/empty.txt"),
        ),
        (
            include_bytes!("../tests/fixtures/xz/one_byte.xz"),
            include_bytes!("../tests/fixtures/xz/one_byte.txt"),
        ),
        (
            include_bytes!("../tests/fixtures/xz/mixed.xz"),
            include_bytes!("../tests/fixtures/xz/mixed.txt"),
        ),
        (
            include_bytes!("../tests/fixtures/xz/mixed_crc32.xz"),
            include_bytes!("../tests/fixtures/xz/mixed.txt"),
        ),
        (
            include_bytes!("../tests/fixtures/xz/mixed_crc64.xz"),
            include_bytes!("../tests/fixtures/xz/mixed.txt"),
        ),
        (
            include_bytes!("../tests/fixtures/xz/mixed_sha256.xz"),
            include_bytes!("../tests/fixtures/xz/mixed.txt"),
        ),
        (
            include_bytes!("../tests/fixtures/xz/mixed_0.xz"),
            include_bytes!("../tests/fixtures/xz/mixed.txt"),
        ),
        (
            include_bytes!("../tests/fixtures/xz/mixed_9e.xz"),
            include_bytes!("../tests/fixtures/xz/mixed.txt"),
        ),
        (
            include_bytes!("../tests/fixtures/xz/mixed_dict64k.xz"),
            include_bytes!("../tests/fixtures/xz/mixed.txt"),
        ),
        (
            include_bytes!("../tests/fixtures/xz/mixed_blocks.xz"),
            include_bytes!("../tests/fixtures/xz/mixed.txt"),
        ),
        (
            include_bytes!("../tests/fixtures/xz/mixed_mt.xz"),
            include_bytes!("../tests/fixtures/xz/mixed.txt"),
        ),
        (
            include_bytes!("../tests/fixtures/xz/incompressible.xz"),
            include_bytes!("../tests/fixtures/xz/incompressible.bin"),
        ),
        (
            include_bytes!("../tests/fixtures/xz/alpha.xz"),
            include_bytes!("../tests/fixtures/xz/alpha.txt"),
        ),
        (
            include_bytes!("../tests/fixtures/xz/beta.xz"),
            include_bytes!("../tests/fixtures/xz/beta.txt"),
        ),
    ];

    fn cat(a: &[u8], b: &[u8]) -> Vec<u8> {
        let mut v = a.to_vec();
        v.extend_from_slice(b);
        v
    }

    #[test]
    fn fixtures_decode_to_their_plaintext() {
        for (packed, plain) in FIXTURES {
            assert!(is_xz(packed), "fixture does not start with the magic");
            assert_eq!(decompress(packed).unwrap(), *plain);
        }
        assert_eq!(EMPTY, b"");
        assert_eq!(ONE, b"x");
        assert_eq!(MIXED.len(), 300 * 1024);
        assert_eq!(INCOMPRESSIBLE.len(), 64 * 1024);
    }

    #[test]
    fn concatenated_streams_join_in_order() {
        let want = cat(ALPHA, BETA);
        let plain = decompress(include_bytes!("../tests/fixtures/xz/concat.xz")).unwrap();
        assert_eq!(plain, want);
        let padded = decompress(include_bytes!("../tests/fixtures/xz/concat_padded.xz")).unwrap();
        assert_eq!(padded, want);
    }

    fn data_start(packed: &[u8]) -> usize {
        12 + (packed[12] as usize + 1) * 4
    }

    #[test]
    fn a_flipped_byte_inside_a_block_is_an_error() {
        let packed = include_bytes!("../tests/fixtures/xz/mixed.xz");
        let mut corrupt = packed.to_vec();
        corrupt[data_start(packed) + 40] ^= 0x01;
        let err = decompress(&corrupt).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().starts_with("xz: "), "{err}");
    }

    /// The block check sits exactly where the index begins, so the footer's backward size
    /// names its offset.
    #[test]
    fn a_corrupted_check_byte_is_named() {
        let mut corrupt = include_bytes!("../tests/fixtures/xz/mixed.xz").to_vec();
        let n = corrupt.len();
        let backward = le32(&corrupt[n - 8..n - 4]) as usize;
        let index = n - 12 - (backward + 1) * 4;
        corrupt[index - 1] ^= 0xFF;
        let err = decompress(&corrupt).unwrap_err();
        assert!(err.to_string().contains("crc64 check"), "{err}");
    }

    #[test]
    fn truncation_is_an_error() {
        let packed = include_bytes!("../tests/fixtures/xz/mixed.xz");
        for cut in [6, 12, packed.len() / 2, packed.len() - 13] {
            let err = decompress(&packed[..cut]).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn padding_and_trailing_bytes_are_judged() {
        let packed = include_bytes!("../tests/fixtures/xz/mixed.xz");
        let mut short_padding = packed.to_vec();
        short_padding.extend_from_slice(&[0, 0]);
        let err = decompress(&short_padding).unwrap_err();
        assert!(err.to_string().contains("multiple of four"), "{err}");

        let mut junk = packed.to_vec();
        junk.extend_from_slice(b"junk");
        let err = decompress(&junk).unwrap_err();
        assert!(err.to_string().contains("trailing data"), "{err}");

        let mut check = packed.to_vec();
        check[7] = 0x02;
        let err = decompress(&check).unwrap_err();
        assert!(err.to_string().contains("check id 2"), "{err}");
    }

    #[test]
    fn bad_magic_is_named() {
        let err = decompress(b"not an xz stream").unwrap_err();
        assert!(err.to_string().contains("bad magic"), "{err}");
        assert_eq!(decompress(b"").unwrap(), Vec::<u8>::new());
    }

    /// A stream header plus a block header carrying `filter` and `props`, checksummed so the
    /// container alone is not what fails.
    fn synthetic(filter: u64, props: &[u8]) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        let flags = [0u8, 0x01];
        out.extend_from_slice(&flags);
        out.extend_from_slice(&crc32(&flags).to_le_bytes());
        let mut fields = vec![0u8, 0u8];
        fields.extend_from_slice(&varint_bytes(filter));
        fields.extend_from_slice(&varint_bytes(props.len() as u64));
        fields.extend_from_slice(props);
        let header_size = (fields.len() + 4).div_ceil(4) * 4;
        fields.resize(header_size - 4, 0);
        fields[0] = (header_size / 4 - 1) as u8;
        let crc = crc32(&fields);
        let mut block = fields;
        block.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&block);
        out.extend_from_slice(&[0x00, 0x01, 0x80]);
        out.extend_from_slice(&[0x00, 0x00, 0x00]);
        out.extend_from_slice(&0u32.to_le_bytes());
        out
    }

    fn varint_bytes(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }

    #[test]
    fn a_dictionary_above_the_cap_is_refused() {
        let err = decompress(&synthetic(FILTER_LZMA2, &[40])).unwrap_err();
        assert!(err.to_string().contains("exceeds the"), "{err}");
        let err = decompress(&synthetic(FILTER_LZMA2, &[39])).unwrap_err();
        assert!(err.to_string().contains("exceeds the"), "{err}");
    }

    #[test]
    fn another_filter_is_named() {
        let err = decompress(&synthetic(0x03, &[])).unwrap_err();
        assert!(err.to_string().contains("unsupported filter 0x03"), "{err}");
    }

    #[test]
    fn checks_and_hashes_match_their_vectors() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc64(b"123456789"), 0x995D_C9BB_DF19_39FA);
        let hex = |b: [u8; 32]| {
            b.iter()
                .map(|x| format!("{x:02x}"))
                .collect::<Vec<_>>()
                .join("")
        };
        assert_eq!(
            hex(sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let long = vec![b'a'; 1000];
        assert_eq!(
            hex(sha256(&long)),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    #[ignore = "timing measurement, run with --release --ignored"]
    fn throughput() {
        let rounds = 200;
        for (name, packed, plain) in [
            (
                "mixed.xz",
                include_bytes!("../tests/fixtures/xz/mixed.xz") as &[u8],
                MIXED,
            ),
            (
                "mixed_9e.xz",
                include_bytes!("../tests/fixtures/xz/mixed_9e.xz"),
                MIXED,
            ),
            (
                "incompressible.xz",
                include_bytes!("../tests/fixtures/xz/incompressible.xz"),
                INCOMPRESSIBLE,
            ),
        ] {
            let start = Instant::now();
            let mut bytes = 0usize;
            for _ in 0..rounds {
                bytes += decompress(packed).unwrap().len();
            }
            let secs = start.elapsed().as_secs_f64();
            println!(
                "xz: {name}: {} bytes ({:?}) per decode, {:.1} MB/s over {rounds} decodes",
                plain.len(),
                packed.len(),
                bytes as f64 / secs / 1e6,
            );
        }
    }
}
