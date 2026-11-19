//! Brotli decompression for `-z/--search-zip`, in std alone.
//!
//! Brotli has no signature: it is identified by the extension of the file it was found in,
//! so unlike the other formats there is no `is_brotli` that can answer from bytes alone.
//!
//! A stream is a 1..7-bit window size and then a series of meta-blocks. Each compressed
//! meta-block brings its own prefix codes — up to 256 literal, 256 insert-and-copy and 256
//! distance trees — and picks one per element from a block type plus a context ID: the last
//! two bytes for a literal, the copy length for a distance. A command is an insert length,
//! that many literals, and a `<length, distance>` pair, where a distance past the window is
//! a reference into the static dictionary of Appendix A. That dictionary is 122,784 bytes of
//! words with 121 transformations each: nothing decodes without its exact bytes, so `dict`
//! carries them.
//!
//! The whole output stays in memory, as the gzip path does: a distance can reach back 16 MiB
//! across meta-block boundaries.

use std::io;

#[path = "archive_brotli_dict.rs"]
mod dict;

/// Expand a Brotli stream into one buffer.
pub fn decompress(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut decoder = Decoder {
        br: Bits::new(data),
        out: Vec::new(),
        dist: [4, 11, 15, 16],
        window: 0,
        word: Vec::new(),
    };
    decoder.run()
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("brotli: {what}"))
}

fn truncated() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "brotli: stream ends inside a meta-block",
    )
}

/// `NDBITS` from section 8: the bit depth of the word index for each copy length.
const DICT_BITS: [u8; 25] = [
    0, 0, 0, 0, 10, 10, 11, 11, 10, 10, 10, 10, 10, 9, 9, 8, 7, 7, 8, 7, 7, 6, 6, 5, 5,
];

/// `DOFFSET` from section 8: where the words of each length start in `dict::DATA`.
const DICT_OFFSET: [u32; 25] = {
    let mut offsets = [0u32; 25];
    let mut len = 1;
    while len < 25 {
        let words = if len - 1 < 4 {
            0
        } else {
            1u32 << DICT_BITS[len - 1]
        };
        offsets[len] = offsets[len - 1] + (len as u32 - 1) * words;
        len += 1;
    }
    offsets
};

/// What a word transform does between the prefix and the suffix.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Transform {
    Identity,
    FermentFirst,
    FermentAll,
    OmitFirst(usize),
    OmitLast(usize),
}

/// The 121 word transformations of Appendix B, in transform ID order.
const TRANSFORMS: [(&[u8], Transform, &[u8]); 121] = [
    (b"", Transform::Identity, b""),
    (b"", Transform::Identity, b" "),
    (b" ", Transform::Identity, b" "),
    (b"", Transform::OmitFirst(1), b""),
    (b"", Transform::FermentFirst, b" "),
    (b"", Transform::Identity, b" the "),
    (b" ", Transform::Identity, b""),
    (b"s ", Transform::Identity, b" "),
    (b"", Transform::Identity, b" of "),
    (b"", Transform::FermentFirst, b""),
    (b"", Transform::Identity, b" and "),
    (b"", Transform::OmitFirst(2), b""),
    (b"", Transform::OmitLast(1), b""),
    (b", ", Transform::Identity, b" "),
    (b"", Transform::Identity, b", "),
    (b" ", Transform::FermentFirst, b" "),
    (b"", Transform::Identity, b" in "),
    (b"", Transform::Identity, b" to "),
    (b"e ", Transform::Identity, b" "),
    (b"", Transform::Identity, b"\""),
    (b"", Transform::Identity, b"."),
    (b"", Transform::Identity, b"\">"),
    (b"", Transform::Identity, b"\n"),
    (b"", Transform::OmitLast(3), b""),
    (b"", Transform::Identity, b"]"),
    (b"", Transform::Identity, b" for "),
    (b"", Transform::OmitFirst(3), b""),
    (b"", Transform::OmitLast(2), b""),
    (b"", Transform::Identity, b" a "),
    (b"", Transform::Identity, b" that "),
    (b" ", Transform::FermentFirst, b""),
    (b"", Transform::Identity, b". "),
    (b".", Transform::Identity, b""),
    (b" ", Transform::Identity, b", "),
    (b"", Transform::OmitFirst(4), b""),
    (b"", Transform::Identity, b" with "),
    (b"", Transform::Identity, b"'"),
    (b"", Transform::Identity, b" from "),
    (b"", Transform::Identity, b" by "),
    (b"", Transform::OmitFirst(5), b""),
    (b"", Transform::OmitFirst(6), b""),
    (b" the ", Transform::Identity, b""),
    (b"", Transform::OmitLast(4), b""),
    (b"", Transform::Identity, b". The "),
    (b"", Transform::FermentAll, b""),
    (b"", Transform::Identity, b" on "),
    (b"", Transform::Identity, b" as "),
    (b"", Transform::Identity, b" is "),
    (b"", Transform::OmitLast(7), b""),
    (b"", Transform::OmitLast(1), b"ing "),
    (b"", Transform::Identity, b"\n\t"),
    (b"", Transform::Identity, b":"),
    (b" ", Transform::Identity, b". "),
    (b"", Transform::Identity, b"ed "),
    (b"", Transform::OmitFirst(9), b""),
    (b"", Transform::OmitFirst(7), b""),
    (b"", Transform::OmitLast(6), b""),
    (b"", Transform::Identity, b"("),
    (b"", Transform::FermentFirst, b", "),
    (b"", Transform::OmitLast(8), b""),
    (b"", Transform::Identity, b" at "),
    (b"", Transform::Identity, b"ly "),
    (b" the ", Transform::Identity, b" of "),
    (b"", Transform::OmitLast(5), b""),
    (b"", Transform::OmitLast(9), b""),
    (b" ", Transform::FermentFirst, b", "),
    (b"", Transform::FermentFirst, b"\""),
    (b".", Transform::Identity, b"("),
    (b"", Transform::FermentAll, b" "),
    (b"", Transform::FermentFirst, b"\">"),
    (b"", Transform::Identity, b"=\""),
    (b" ", Transform::Identity, b"."),
    (b".com/", Transform::Identity, b""),
    (b" the ", Transform::Identity, b" of the "),
    (b"", Transform::FermentFirst, b"'"),
    (b"", Transform::Identity, b". This "),
    (b"", Transform::Identity, b","),
    (b".", Transform::Identity, b" "),
    (b"", Transform::FermentFirst, b"("),
    (b"", Transform::FermentFirst, b"."),
    (b"", Transform::Identity, b" not "),
    (b" ", Transform::Identity, b"=\""),
    (b"", Transform::Identity, b"er "),
    (b" ", Transform::FermentAll, b" "),
    (b"", Transform::Identity, b"al "),
    (b" ", Transform::FermentAll, b""),
    (b"", Transform::Identity, b"='"),
    (b"", Transform::FermentAll, b"\""),
    (b"", Transform::FermentFirst, b". "),
    (b" ", Transform::Identity, b"("),
    (b"", Transform::Identity, b"ful "),
    (b" ", Transform::FermentFirst, b". "),
    (b"", Transform::Identity, b"ive "),
    (b"", Transform::Identity, b"less "),
    (b"", Transform::FermentAll, b"'"),
    (b"", Transform::Identity, b"est "),
    (b" ", Transform::FermentFirst, b"."),
    (b"", Transform::FermentAll, b"\">"),
    (b" ", Transform::Identity, b"='"),
    (b"", Transform::FermentFirst, b","),
    (b"", Transform::Identity, b"ize "),
    (b"", Transform::FermentAll, b"."),
    (b"\xc2\xa0", Transform::Identity, b""),
    (b" ", Transform::Identity, b","),
    (b"", Transform::FermentFirst, b"=\""),
    (b"", Transform::FermentAll, b"=\""),
    (b"", Transform::Identity, b"ous "),
    (b"", Transform::FermentAll, b", "),
    (b"", Transform::FermentFirst, b"='"),
    (b" ", Transform::FermentFirst, b","),
    (b" ", Transform::FermentAll, b"=\""),
    (b" ", Transform::FermentAll, b", "),
    (b"", Transform::FermentAll, b","),
    (b"", Transform::FermentAll, b"("),
    (b"", Transform::FermentAll, b". "),
    (b" ", Transform::FermentAll, b"."),
    (b"", Transform::FermentAll, b"='"),
    (b" ", Transform::FermentAll, b". "),
    (b" ", Transform::FermentFirst, b"=\""),
    (b" ", Transform::FermentAll, b"='"),
    (b" ", Transform::FermentFirst, b"='"),
];

/// Literal context contributions of the previous byte for the UTF8 mode.
const LUT0: [u8; 256] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 0, 0, 4, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    8, 12, 16, 12, 12, 20, 12, 16, 24, 28, 12, 12, 32, 12, 36, 12, //
    44, 44, 44, 44, 44, 44, 44, 44, 44, 44, 32, 32, 24, 40, 28, 12, //
    12, 48, 52, 52, 52, 48, 52, 52, 52, 48, 52, 52, 52, 52, 52, 48, //
    52, 52, 52, 52, 52, 48, 52, 52, 52, 52, 52, 24, 12, 28, 12, 12, //
    12, 56, 60, 60, 60, 56, 60, 60, 60, 56, 60, 60, 60, 60, 60, 56, //
    60, 60, 60, 60, 60, 56, 60, 60, 60, 60, 60, 24, 12, 28, 12, 0, //
    0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, //
    0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, //
    0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, //
    0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, //
    2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, //
    2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, //
    2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, //
    2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, //
];

/// Literal context contributions of the second-to-last byte for the UTF8 mode.
const LUT1: [u8; 256] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, //
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, //
    1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, //
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, //
    1, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, //
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 1, 1, 1, 1, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, //
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, //
];

/// Literal context of a byte for the Signed mode, used for both bytes.
const LUT2: [u8; 256] = [
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, //
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, //
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, //
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, //
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, //
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, //
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, //
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, //
    4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, //
    4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, //
    4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, //
    4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, //
    5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, //
    5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, //
    5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, //
    6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 7, //
];

/// Insert length bases and extra bits for the 24 insert length codes.
const INSERT_BASE: [u32; 24] = [
    0, 1, 2, 3, 4, 5, 6, 8, 10, 14, 18, 26, 34, 50, 66, 98, 130, 194, 322, 578, 1090, 2114, 6210,
    22594,
];

const INSERT_EXTRA: [u32; 24] = [
    0, 0, 0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 7, 8, 9, 10, 12, 14, 24,
];

/// Copy length bases and extra bits for the 24 copy length codes.
const COPY_BASE: [u32; 24] = [
    2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 14, 18, 22, 30, 38, 54, 70, 102, 134, 198, 326, 582, 1094, 2118,
];

const COPY_EXTRA: [u32; 24] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 7, 8, 9, 10, 24,
];

/// The first insert length code of each of the eleven 64-code ranges of the
/// insert-and-copy alphabet, and the first copy length code of the same range.
const INS_RANGE: [u32; 11] = [0, 0, 0, 0, 8, 8, 0, 16, 8, 16, 16];
const COPY_RANGE: [u32; 11] = [0, 8, 0, 8, 0, 8, 16, 0, 16, 8, 16];

/// Block count bases and extra bits for the 26 block count codes.
const BLOCK_COUNT_BASE: [u32; 26] = [
    1, 5, 9, 13, 17, 25, 33, 41, 49, 65, 81, 97, 113, 145, 177, 209, 241, 305, 369, 497, 753, 1265,
    2289, 4337, 8433, 16625,
];

const BLOCK_COUNT_EXTRA: [u32; 26] = [
    2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 6, 6, 7, 8, 9, 10, 11, 12, 13, 24,
];

/// The order the eighteen code length code lengths arrive in.
const CODE_LENGTH_ORDER: [usize; 18] =
    [1, 2, 3, 4, 0, 5, 17, 6, 16, 7, 8, 9, 10, 11, 12, 13, 14, 15];

/// The fixed code for a code length code length, indexed by the next four bits of the
/// stream: how many bits the code occupies and what it means.
const CL_PREFIX_LEN: [u32; 16] = [2, 2, 2, 3, 2, 2, 2, 4, 2, 2, 2, 3, 2, 2, 2, 4];
const CL_PREFIX_VALUE: [u32; 16] = [0, 4, 3, 2, 0, 4, 3, 1, 0, 4, 3, 2, 0, 4, 3, 5];

/// A reader over a packed bit stream, least significant bit first, as every brotli data
/// element but a prefix code is stored.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u64,
    nanb: u32,
    left: u64,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Bits<'a> {
        Bits {
            data,
            pos: 0,
            buf: 0,
            nanb: 0,
            left: data.len() as u64 * 8,
        }
    }

    fn fill(&mut self, n: u32) {
        while self.nanb < n && self.pos < self.data.len() {
            self.buf |= u64::from(self.data[self.pos]) << self.nanb;
            self.nanb += 8;
            self.pos += 1;
        }
    }

    /// The next `n` bits without consuming them. Bits past the end of the stream read as
    /// zero, so a code can be looked at before it is known that all of it is there.
    fn peek(&mut self, n: u32) -> u32 {
        self.fill(n);
        (self.buf & ((1 << n) - 1)) as u32
    }

    fn take(&mut self, n: u32) -> io::Result<u32> {
        if u64::from(n) > self.left {
            return Err(truncated());
        }
        if n == 0 {
            return Ok(0);
        }
        let value = self.peek(n);
        self.buf >>= n;
        self.nanb -= n;
        self.left -= u64::from(n);
        Ok(value)
    }

    fn skip(&mut self, n: u32) -> io::Result<()> {
        self.take(n).map(|_| ())
    }

    /// Drop the bits of the current byte, so the next read starts on a byte boundary.
    fn align(&mut self) {
        let drop = self.nanb % 8;
        self.buf = 0;
        self.pos -= ((self.nanb - drop) / 8) as usize;
        self.nanb = 0;
        self.left -= u64::from(drop);
    }

    /// `n` whole bytes; the caller has aligned first.
    fn bytes(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if (n as u64) * 8 > self.left {
            return Err(truncated());
        }
        let start = self.pos;
        self.pos += n;
        self.left -= n as u64 * 8;
        Ok(&self.data[start..start + n])
    }
}

/// A slot of a prefix code table: the symbol, plus the code length that selects it. A zero
/// length in a root slot means the slot holds the offset of a sub-table for longer codes.
const SYMBOL_MASK: u32 = 0xF_FFFF;
const LENGTH_SHIFT: u32 = 20;

/// A canonical prefix code: a root table indexed by the next bits of the stream, with
/// sub-tables for codes longer than the root.
struct Huffman {
    root: Vec<u32>,
    sub: Vec<u32>,
    /// Zero for a code of one symbol, which occupies no bits at all.
    root_bits: u32,
    sub_bits: u32,
}

impl Huffman {
    /// A code of one symbol: no bits are read and that symbol always comes back.
    fn single(symbol: u32) -> Huffman {
        Huffman {
            root: vec![symbol],
            sub: Vec::new(),
            root_bits: 0,
            sub_bits: 0,
        }
    }

    /// Read one prefix code description, for an alphabet of `alphabet` symbols.
    fn read(br: &mut Bits, alphabet: usize) -> io::Result<Huffman> {
        let kind = br.take(2)?;
        if kind == 1 {
            let nsym = br.take(2)? as usize + 1;
            let bits = 32 - (alphabet as u32 - 1).leading_zeros();
            let mut symbols = [0u16; 4];
            for k in 0..nsym {
                let sym = br.take(bits)?;
                if sym as usize >= alphabet {
                    return Err(invalid("simple prefix code symbol outside the alphabet"));
                }
                if symbols[..k].contains(&(sym as u16)) {
                    return Err(invalid("simple prefix code repeats a symbol"));
                }
                symbols[k] = sym as u16;
            }
            if nsym == 1 {
                return Ok(Huffman::single(symbols[0] as u32));
            }
            let mut lengths = vec![0u8; alphabet];
            let sym = |i: usize| symbols[i] as usize;
            match nsym {
                2 => {
                    lengths[sym(0)] = 1;
                    lengths[sym(1)] = 1;
                }
                3 => {
                    lengths[sym(0)] = 1;
                    lengths[sym(1)] = 2;
                    lengths[sym(2)] = 2;
                }
                _ => {
                    if br.take(1)? == 0 {
                        for i in 0..4 {
                            lengths[sym(i)] = 2;
                        }
                    } else {
                        lengths[sym(0)] = 1;
                        lengths[sym(1)] = 2;
                        lengths[sym(2)] = 3;
                        lengths[sym(3)] = 3;
                    }
                }
            }
            return Huffman::new(&lengths);
        }
        let mut cl_lengths = [0u8; 18];
        let mut space = 32i32;
        let mut codes = 0;
        for &index in CODE_LENGTH_ORDER.iter().skip(kind as usize) {
            let slot = br.peek(4) as usize;
            let length = CL_PREFIX_VALUE[slot];
            br.skip(CL_PREFIX_LEN[slot])?;
            cl_lengths[index] = length as u8;
            if length != 0 {
                space -= 32 >> length;
                codes += 1;
                if space <= 0 {
                    break;
                }
            }
        }
        if codes == 0 || (codes != 1 && space != 0) {
            return Err(invalid("code length prefix code does not fill its tree"));
        }
        let cl = Huffman::new(&cl_lengths)?;
        let mut lengths = vec![0u8; alphabet];
        let mut sym = 0usize;
        let mut space = 32768i32;
        let mut prev = 8u8;
        let mut repeat = 0u32;
        let mut repeated = 0u8;
        while sym < alphabet && space > 0 {
            let code = cl.decode(br)? as u8;
            if code < 16 {
                repeat = 0;
                if code != 0 {
                    prev = code;
                    space -= 32768 >> code;
                }
                lengths[sym] = code;
                sym += 1;
                continue;
            }
            let extra = if code == 16 { 2 } else { 3 };
            let delta = br.take(extra)?;
            let length = if code == 16 { prev } else { 0 };
            if repeated != length {
                repeat = 0;
                repeated = length;
            }
            let old = repeat;
            if repeat > 0 {
                repeat = (repeat - 2) << extra;
            }
            repeat += delta + 3;
            let add = (repeat - old) as usize;
            if sym + add > alphabet {
                return Err(invalid(
                    "code length repeat runs past the end of the alphabet",
                ));
            }
            if length != 0 {
                space -= (add as i32) << (15 - u32::from(length));
            }
            for slot in lengths[sym..sym + add].iter_mut() {
                *slot = length;
            }
            sym += add;
        }
        if space != 0 {
            return Err(invalid("prefix code lengths do not fill their tree"));
        }
        Huffman::new(&lengths)
    }

    fn new(lengths: &[u8]) -> io::Result<Huffman> {
        let mut count = [0u16; 16];
        let mut used = 0;
        let mut max_len = 0u32;
        for &length in lengths {
            if length != 0 {
                count[length as usize] += 1;
                used += 1;
                max_len = max_len.max(u32::from(length));
            }
        }
        if used == 0 {
            return Err(invalid("prefix code has no symbols"));
        }
        if used == 1 {
            let symbol = lengths.iter().position(|&l| l != 0).unwrap_or(0);
            return Ok(Huffman::single(symbol as u32));
        }
        let mut left = 1i32;
        for &of_length in count.iter().skip(1) {
            left <<= 1;
            left -= i32::from(of_length);
            if left < 0 {
                return Err(invalid("prefix code is over-subscribed"));
            }
        }
        if left != 0 {
            return Err(invalid("prefix code is incomplete"));
        }
        let root_bits = max_len.min(8);
        let sub_bits = max_len - root_bits;
        let root_size = 1usize << root_bits;
        let mut root = vec![0u32; root_size];
        let mut allocated = vec![false; root_size];
        let mut sub: Vec<u32> = Vec::new();
        let mut next = [0u32; 16];
        let mut code = 0u32;
        for len in 1..=15 {
            code = (code + u32::from(count[len - 1])) << 1;
            next[len] = code;
        }
        for (symbol, &length) in lengths.iter().enumerate() {
            if length == 0 {
                continue;
            }
            let len = u32::from(length);
            let bits = next[len as usize];
            next[len as usize] += 1;
            let reversed = bits.reverse_bits() >> (32 - len);
            let slot = symbol as u32 | len << LENGTH_SHIFT;
            if len <= root_bits {
                let mut at = reversed as usize;
                while at < root_size {
                    root[at] = slot;
                    at += 1 << len;
                }
            } else {
                let first = (reversed as usize) & (root_size - 1);
                if !allocated[first] {
                    allocated[first] = true;
                    root[first] = sub.len() as u32;
                    sub.resize(sub.len() + (1usize << sub_bits), 0);
                }
                let base = root[first] as usize;
                let mut at = (reversed as usize) >> root_bits;
                while at < (1usize << sub_bits) {
                    sub[base + at] = slot;
                    at += 1 << (len - root_bits);
                }
            }
        }
        Ok(Huffman {
            root,
            sub,
            root_bits,
            sub_bits,
        })
    }

    fn decode(&self, br: &mut Bits) -> io::Result<u32> {
        if self.root_bits == 0 {
            return Ok(self.root[0]);
        }
        let entry = self.root[br.peek(self.root_bits) as usize];
        let length = entry >> LENGTH_SHIFT;
        if length != 0 {
            br.skip(length)?;
            return Ok(entry & SYMBOL_MASK);
        }
        let index = (br.peek(self.root_bits + self.sub_bits) >> self.root_bits) as usize;
        let entry = *self
            .sub
            .get((entry & SYMBOL_MASK) as usize + index)
            .ok_or_else(|| invalid("prefix code has no code for these bits"))?;
        let length = entry >> LENGTH_SHIFT;
        if length == 0 {
            return Err(invalid("prefix code has no code for these bits"));
        }
        br.skip(length)?;
        Ok(entry & SYMBOL_MASK)
    }
}

/// One of the three block categories: which of its prefix codes is current, and how many
/// elements are still coded with it.
struct Blocks {
    types: usize,
    current: usize,
    previous: usize,
    left: u32,
    type_tree: Option<Huffman>,
    count_tree: Option<Huffman>,
}

impl Blocks {
    fn read(br: &mut Bits) -> io::Result<Blocks> {
        let types = varlen(br)? as usize;
        let mut blocks = Blocks {
            types,
            current: 0,
            previous: 1,
            left: 1 << 24,
            type_tree: None,
            count_tree: None,
        };
        if types >= 2 {
            blocks.type_tree = Some(Huffman::read(br, types + 2)?);
            let count_tree = Huffman::read(br, 26)?;
            blocks.left = block_count(br, &count_tree)?;
            blocks.count_tree = Some(count_tree);
        }
        Ok(blocks)
    }

    /// The block type for the next element of the category, switching to a new block first
    /// if the previous one has been used up.
    fn next(&mut self, br: &mut Bits) -> io::Result<usize> {
        if self.left == 0 {
            let (type_tree, count_tree) = match (&self.type_tree, &self.count_tree) {
                (Some(types), Some(counts)) => (types, counts),
                _ => return Err(invalid("block switch in a category with one block type")),
            };
            let symbol = type_tree.decode(br)? as usize;
            self.left = block_count(br, count_tree)?;
            let new_type = match symbol {
                0 => self.previous,
                1 => self.current + 1,
                other => other - 2,
            };
            let new_type = if new_type >= self.types {
                new_type - self.types
            } else {
                new_type
            };
            self.previous = self.current;
            self.current = new_type;
        }
        self.left -= 1;
        Ok(self.current)
    }
}

/// A meta-block's context map: the prefix code index for each `<block type, context ID>`.
struct ContextMap {
    map: Vec<u8>,
    trees: usize,
}

impl ContextMap {
    fn read(br: &mut Bits, size: usize) -> io::Result<ContextMap> {
        let trees = varlen(br)? as usize;
        let mut map = vec![0u8; size];
        if trees == 1 {
            return Ok(ContextMap { map, trees });
        }
        let rlemax = if br.peek(1) == 1 {
            (br.take(5)? >> 1) + 1
        } else {
            br.take(1)?;
            0
        };
        let tree = Huffman::read(br, trees + rlemax as usize)?;
        let mut at = 0;
        while at < size {
            let symbol = tree.decode(br)? as usize;
            if symbol == 0 {
                at += 1;
            } else if symbol <= rlemax as usize {
                let run = (1usize << symbol) + br.take(symbol as u32)? as usize;
                if at + run > size {
                    return Err(invalid("context map run past the end of the map"));
                }
                at += run;
            } else {
                map[at] = (symbol - rlemax as usize) as u8;
                at += 1;
            }
        }
        if br.take(1)? == 1 {
            inverse_move_to_front(&mut map);
        }
        Ok(ContextMap { map, trees })
    }

    fn index(&self, block_type: usize, context: usize, stride: usize) -> io::Result<usize> {
        let index = self.map[block_type * stride + context] as usize;
        if index >= self.trees {
            return Err(invalid("context map names a prefix code that is not there"));
        }
        Ok(index)
    }
}

fn inverse_move_to_front(map: &mut [u8]) {
    let mut order = [0u8; 256];
    for (byte, slot) in order.iter_mut().enumerate() {
        *slot = byte as u8;
    }
    for value in map.iter_mut() {
        let index = *value as usize;
        let moved = order[index];
        order[..=index].rotate_right(1);
        *value = moved;
    }
}

/// The variable-length code that counts block types and context maps: the value the RFC
/// prints, so 1..256.
fn varlen(br: &mut Bits) -> io::Result<u32> {
    if br.take(1)? == 0 {
        return Ok(1);
    }
    let bits = br.take(3)?;
    if bits == 0 {
        return Ok(2);
    }
    Ok((1 << bits) + br.take(bits)? + 1)
}

fn block_count(br: &mut Bits, tree: &Huffman) -> io::Result<u32> {
    let code = tree.decode(br)? as usize;
    let base = *BLOCK_COUNT_BASE
        .get(code)
        .ok_or_else(|| invalid("block count code outside its alphabet"))?;
    Ok(base + br.take(BLOCK_COUNT_EXTRA[code])?)
}

struct Decoder<'a> {
    br: Bits<'a>,
    out: Vec<u8>,
    /// The four last distances, most recent first, as section 4 leaves them at the start of
    /// a stream.
    dist: [u32; 4],
    window: u32,
    /// One transformed dictionary word, reused per reference.
    word: Vec<u8>,
}

impl Decoder<'_> {
    fn run(&mut self) -> io::Result<Vec<u8>> {
        self.window = self.window_bits()?;
        while !self.metablock()? {}
        Ok(std::mem::take(&mut self.out))
    }

    fn window_bits(&mut self) -> io::Result<u32> {
        if self.br.take(1)? == 0 {
            return Ok(16);
        }
        let bits = self.br.take(3)?;
        if bits != 0 {
            return Ok(17 + bits);
        }
        match self.br.take(3)? {
            0 => Ok(17),
            1 => Err(invalid("window size code 0010001 is reserved")),
            other => Ok(8 + other),
        }
    }

    /// One meta-block; true when it was the last one.
    fn metablock(&mut self) -> io::Result<bool> {
        let last = self.br.take(1)? == 1;
        if last && self.br.take(1)? == 1 {
            return Ok(true);
        }
        let nibbles = match self.br.take(2)? {
            0 => 4,
            1 => 5,
            2 => 6,
            _ => 0,
        };
        if nibbles == 0 {
            self.metadata()?;
            return Ok(false);
        }
        let length = self.br.take(nibbles * 4)?;
        if nibbles > 4 && length >> ((nibbles - 1) * 4) == 0 {
            return Err(invalid("meta-block length has an empty leading nibble"));
        }
        let length = length as usize + 1;
        if !last && self.br.take(1)? == 1 {
            self.br.align();
            let chunk = self.br.bytes(length)?;
            self.out.extend_from_slice(chunk);
            return Ok(false);
        }
        self.compressed(length)?;
        Ok(last)
    }

    /// A meta-block of metadata: bytes that belong to neither the window nor the output.
    fn metadata(&mut self) -> io::Result<()> {
        if self.br.take(1)? != 0 {
            return Err(invalid("metadata meta-block reserved bit is set"));
        }
        let width = self.br.take(2)?;
        let mut length = 0u64;
        if width > 0 {
            for byte in 0..width {
                length |= u64::from(self.br.take(8)?) << (8 * byte);
            }
            if width > 1 && length >> (8 * (width - 1)) == 0 {
                return Err(invalid("metadata length has an empty leading byte"));
            }
            length += 1;
        }
        self.br.align();
        self.br.bytes(length as usize)?;
        Ok(())
    }

    fn compressed(&mut self, length: usize) -> io::Result<()> {
        let end = self.out.len() + length;
        self.out.reserve(length);

        let mut lit_blocks = Blocks::read(&mut self.br)?;
        let mut cmd_blocks = Blocks::read(&mut self.br)?;
        let mut dist_blocks = Blocks::read(&mut self.br)?;

        let postfix = self.br.take(2)?;
        let direct = self.br.take(4)? << postfix;

        let mut modes = Vec::with_capacity(lit_blocks.types);
        for _ in 0..lit_blocks.types {
            modes.push(self.br.take(2)? as u8);
        }

        let lit_map = ContextMap::read(&mut self.br, 64 * lit_blocks.types)?;
        let dist_map = ContextMap::read(&mut self.br, 4 * dist_blocks.types)?;

        let lit_trees = trees(&mut self.br, lit_map.trees, 256)?;
        let cmd_trees = trees(&mut self.br, cmd_blocks.types, 704)?;
        let dist_trees = trees(
            &mut self.br,
            dist_map.trees,
            16 + direct as usize + (48usize << postfix),
        )?;

        while self.out.len() < end {
            let made = self.out.len();
            let read = self.br.left;
            let command_tree = &cmd_trees[cmd_blocks.next(&mut self.br)?];
            let command = command_tree.decode(&mut self.br)? as usize;
            if command >= 704 {
                return Err(invalid("insert-and-copy code outside its alphabet"));
            }
            let range = command >> 6;
            let insert_code = (INS_RANGE[range] + ((command >> 3) & 7) as u32) as usize;
            let copy_code = (COPY_RANGE[range] + (command & 7) as u32) as usize;
            let insert = INSERT_BASE[insert_code] as usize
                + self.br.take(INSERT_EXTRA[insert_code])? as usize;
            let copy =
                COPY_BASE[copy_code] as usize + self.br.take(COPY_EXTRA[copy_code])? as usize;

            if insert > end - self.out.len() {
                return Err(invalid("meta-block produces more than its declared length"));
            }
            for _ in 0..insert {
                let block_type = lit_blocks.next(&mut self.br)?;
                let context = self.literal_context(modes[block_type]);
                let tree = &lit_trees[lit_map.index(block_type, context, 64)?];
                let byte = tree.decode(&mut self.br)?;
                if byte > 255 {
                    return Err(invalid("literal code outside its alphabet"));
                }
                self.out.push(byte as u8);
            }
            if self.out.len() == end {
                break;
            }

            let mut code = 0;
            let mut distance = self.dist[0];
            if command >= 128 {
                let block_type = dist_blocks.next(&mut self.br)?;
                let context = copy_context(copy);
                let tree = &dist_trees[dist_map.index(block_type, context, 4)?];
                code = tree.decode(&mut self.br)?;
                distance = self.distance(code, postfix, direct)?;
            }
            let max = self.max_distance();
            if distance as usize > max {
                self.dictionary(distance as usize - max - 1, copy, end)?;
            } else {
                if code != 0 {
                    self.dist = [distance, self.dist[0], self.dist[1], self.dist[2]];
                }
                if copy > end - self.out.len() {
                    return Err(invalid("meta-block produces more than its declared length"));
                }
                let from = self.out.len() - distance as usize;
                if (distance as usize) >= copy {
                    self.out.extend_from_within(from..from + copy);
                } else {
                    for at in 0..copy {
                        let byte = self.out[from + at];
                        self.out.push(byte);
                    }
                }
            }
            if self.out.len() == made && self.br.left == read {
                return Err(invalid("command neither produced a byte nor read a bit"));
            }
        }
        Ok(())
    }

    /// The context of the next literal, from the two bytes before it in the output.
    fn literal_context(&self, mode: u8) -> usize {
        let len = self.out.len();
        let p1 = if len > 0 { self.out[len - 1] } else { 0 };
        let p2 = if len > 1 { self.out[len - 2] } else { 0 };
        match mode {
            0 => usize::from(p1 & 0x3f),
            1 => usize::from(p1 >> 2),
            2 => usize::from(LUT0[usize::from(p1)] | LUT1[usize::from(p2)]),
            _ => usize::from((LUT2[usize::from(p1)] << 3) | LUT2[usize::from(p2)]),
        }
    }

    /// The largest distance that may refer to the output, which is what separates a copy
    /// from a static dictionary reference.
    fn max_distance(&self) -> usize {
        self.out.len().min((1usize << self.window) - 16)
    }

    /// Turn a distance symbol into a backward distance.
    fn distance(&mut self, symbol: u32, postfix: u32, direct: u32) -> io::Result<u32> {
        const SHORT_FROM: [usize; 16] = [0, 1, 2, 3, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1];
        const SHORT_DELTA: [i64; 16] = [0, 0, 0, 0, -1, 1, -2, 2, -3, 3, -1, 1, -2, 2, -3, 3];
        if symbol < 16 {
            let symbol = symbol as usize;
            let base = i64::from(self.dist[SHORT_FROM[symbol]]);
            let value = base + SHORT_DELTA[symbol];
            if value <= 0 {
                return Err(invalid("distance short code resolves to zero"));
            }
            return Ok(value as u32);
        }
        if symbol < 16 + direct {
            return Ok(symbol - 15);
        }
        let code = symbol - direct - 16;
        let bits = 1 + (code >> (postfix + 1));
        let hcode = code >> postfix;
        let lcode = code & ((1 << postfix) - 1);
        let offset = ((2 + (hcode & 1)) << bits) - 4;
        let extra = self.br.take(bits)?;
        Ok(((offset + extra) << postfix) + lcode + direct + 1)
    }

    /// Copy a word from the static dictionary, transformed as its ID says.
    fn dictionary(&mut self, word_id: usize, length: usize, end: usize) -> io::Result<()> {
        if !(4..=24).contains(&length) {
            return Err(invalid(
                "static dictionary copy length is outside the 4..=24 it defines",
            ));
        }
        let bits = DICT_BITS[length];
        let index = word_id & ((1 << bits) - 1);
        let (prefix, transform, suffix) = *TRANSFORMS
            .get(word_id >> bits)
            .ok_or_else(|| invalid("static dictionary transform outside 0..=120"))?;
        let start = DICT_OFFSET[length] as usize + index * length;
        let word = &dict::DATA[start..start + length];

        self.word.clear();
        self.word.extend_from_slice(prefix);
        match transform {
            Transform::Identity => self.word.extend_from_slice(word),
            Transform::OmitFirst(count) => {
                let skip = count.min(word.len());
                self.word.extend_from_slice(&word[skip..]);
            }
            Transform::OmitLast(count) => {
                let keep = word.len().saturating_sub(count);
                self.word.extend_from_slice(&word[..keep]);
            }
            Transform::FermentFirst => {
                let at = self.word.len();
                self.word.extend_from_slice(word);
                ferment(&mut self.word[at..], 0);
            }
            Transform::FermentAll => {
                let at = self.word.len();
                self.word.extend_from_slice(word);
                let mut pos = 0;
                while pos < word.len() {
                    pos += ferment(&mut self.word[at..], pos);
                }
            }
        }
        self.word.extend_from_slice(suffix);
        if self.word.len() > end - self.out.len() {
            return Err(invalid("meta-block produces more than its declared length"));
        }
        self.out.extend_from_slice(&self.word);
        Ok(())
    }
}

/// The distance context of a copy length: 2, 3, 4, and more than 4 are contexts 0..3.
fn copy_context(copy: usize) -> usize {
    (copy.saturating_sub(2)).min(3)
}

/// Capitalise the character at `at`, and report how many bytes it occupies. A byte at or
/// above 192 opens a multi-byte character, whose case bit is not its first byte.
fn ferment(word: &mut [u8], at: usize) -> usize {
    if word[at] < 192 {
        if word[at].is_ascii_lowercase() {
            word[at] ^= 32;
        }
        return 1;
    }
    if word[at] < 224 {
        if at + 1 < word.len() {
            word[at + 1] ^= 32;
        }
        return 2;
    }
    if at + 2 < word.len() {
        word[at + 2] ^= 5;
    }
    3
}

/// Read `count` prefix codes of an alphabet of `alphabet` symbols.
fn trees(br: &mut Bits, count: usize, alphabet: usize) -> io::Result<Vec<Huffman>> {
    let mut trees = Vec::with_capacity(count);
    for _ in 0..count {
        trees.push(Huffman::read(br, alphabet)?);
    }
    Ok(trees)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::crc32;

    fn fixture(name: &str) -> Vec<u8> {
        let path = format!(
            "{}/tests/fixtures/brotli/{}",
            env!("CARGO_MANIFEST_DIR"),
            name
        );
        std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    /// The message of the error `result` carries, or a panic naming what was accepted.
    fn failure<T>(result: io::Result<T>, what: &str) -> String {
        match result {
            Ok(_) => panic!("{what} was accepted"),
            Err(e) => e.to_string(),
        }
    }

    fn expands(raw: &str, plain: &str) {
        let expanded = decompress(&fixture(raw)).expect(raw);
        assert_eq!(expanded.len(), fixture(plain).len(), "{raw} length");
        assert!(expanded == fixture(plain), "{raw} content");
    }

    #[test]
    fn the_dictionary_is_the_rfc_bytes() {
        assert_eq!(dict::DATA.len(), 122_784);
        assert_eq!(crc32(dict::DATA), 0x5136_cb04);
        assert_eq!(
            DICT_OFFSET[24] as usize + 24 * (1 << DICT_BITS[24]),
            dict::DATA.len()
        );
    }

    #[test]
    fn the_transforms_are_the_rfc_list() {
        let mut blob = Vec::new();
        for (prefix, transform, suffix) in TRANSFORMS {
            let code = match transform {
                Transform::Identity => 0,
                Transform::FermentFirst => 1,
                Transform::FermentAll => 2,
                Transform::OmitFirst(k) => 2 + k as u8,
                Transform::OmitLast(k) => 11 + k as u8,
            };
            blob.extend_from_slice(prefix);
            blob.push(0);
            blob.push(code);
            blob.extend_from_slice(suffix);
            blob.push(0);
        }
        assert_eq!(blob.len(), 648);
        assert_eq!(crc32(&blob), 0x3d96_5f81);
    }

    #[test]
    fn the_context_tables_are_the_rfc_bytes() {
        assert_eq!(crc32(&LUT0), 0x8e91_efb7);
        assert_eq!(crc32(&LUT1), 0xd01a_32f4);
        assert_eq!(crc32(&LUT2), 0x0dd7_a0d6);
    }

    #[test]
    fn an_empty_stream_expands_to_nothing() {
        assert!(decompress(&fixture("empty.br")).unwrap().is_empty());
    }

    #[test]
    fn one_byte_expands() {
        expands("one.br", "one.txt");
    }

    #[test]
    fn a_mixed_text_expands_at_every_quality() {
        for name in ["mixed-q0.br", "mixed-q5.br", "mixed-q11.br"] {
            expands(name, "mixed.txt");
        }
    }

    #[test]
    fn an_incompressible_input_expands() {
        expands("noise.br", "noise.bin");
    }

    #[test]
    fn a_stream_that_needs_the_static_dictionary_expands() {
        expands("prose-lgwin10.br", "prose.txt");
        expands("prose-q5-lgwin10.br", "prose.txt");
        expands("prose-q11.br", "prose.txt");
    }

    #[test]
    fn every_prefix_of_a_stream_is_an_error() {
        for name in ["one.br", "noise.br", "mixed-q11.br", "prose-lgwin10.br"] {
            let data = fixture(name);
            let mut cuts: Vec<usize> = (0..data.len()).step_by(97).collect();
            cuts.extend(data.len().saturating_sub(4)..data.len());
            for cut in cuts {
                let expanded = decompress(&data[..cut]);
                assert!(expanded.is_err(), "{name} decoded from {cut} bytes");
            }
        }
    }

    #[test]
    fn a_broken_stream_is_an_error() {
        for garbage in [
            vec![0u8; 64],
            vec![0x5Au8; 40],
            b"\x1b\x00\x00\x00\x00\x00\x00\x00garbage beyond a header".to_vec(),
        ] {
            assert!(decompress(&garbage).is_err());
        }
    }

    #[test]
    fn bytes_past_the_last_meta_block_are_ignored() {
        let mut data = fixture("one.br");
        assert_eq!(decompress(&data).unwrap(), b"x");
        data.extend_from_slice(&[0u8, 0x7F, 0xFF]);
        assert_eq!(decompress(&data).unwrap(), b"x");
        assert!(decompress(&[0xFF]).unwrap().is_empty());
    }

    #[test]
    fn an_over_subscribed_code_is_an_error() {
        let err = failure(Huffman::new(&[1, 1, 1]), "three one-bit codes");
        assert!(err.to_string().contains("over-subscribed"), "{err}");
        let err = failure(Huffman::new(&[2, 2]), "two two-bit codes");
        assert!(err.to_string().contains("incomplete"), "{err}");
        let err = failure(Huffman::new(&[0, 0]), "no codes at all");
        assert!(err.to_string().contains("no symbols"), "{err}");
    }

    #[test]
    fn every_literal_context_mode_names_the_context_the_rfc_says() {
        let decoder = Decoder {
            br: Bits::new(&[]),
            out: vec![0x8B, 0x41],
            dist: [4, 11, 15, 16],
            window: 16,
            word: Vec::new(),
        };
        // The most recent byte is p1 and the one before it is p2.
        assert_eq!(decoder.literal_context(0), 0x41 & 0x3f);
        assert_eq!(decoder.literal_context(1), 0x41 >> 2);
        assert_eq!(
            decoder.literal_context(2),
            usize::from(LUT0[0x41] | LUT1[0x8B])
        );
        assert_eq!(
            decoder.literal_context(3),
            usize::from((LUT2[0x41] << 3) | LUT2[0x8B])
        );
        let empty = Decoder {
            br: Bits::new(&[]),
            out: Vec::new(),
            dist: [4, 11, 15, 16],
            window: 16,
            word: Vec::new(),
        };
        assert_eq!(empty.literal_context(2), 0);
    }

    #[test]
    fn a_distance_short_code_that_reaches_zero_is_an_error() {
        let mut decoder = Decoder {
            br: Bits::new(&[]),
            out: Vec::new(),
            dist: [1, 1, 1, 1],
            window: 16,
            word: Vec::new(),
        };
        let err = failure(
            decoder.distance(4, 0, 0),
            "a short code one below the last distance",
        );
        assert!(err.to_string().contains("resolves to zero"), "{err}");
    }

    #[test]
    fn a_prefix_code_that_names_no_word_is_an_error() {
        let mut decoder = Decoder {
            br: Bits::new(&[]),
            out: vec![0; 64],
            dist: [4, 11, 15, 16],
            window: 16,
            word: Vec::new(),
        };
        let err = failure(decoder.dictionary(121 << 10, 4, 1 << 20), "transform 121");
        assert!(err.to_string().contains("transform"), "{err}");
        let err = failure(decoder.dictionary(0, 25, 1 << 20), "a 25-byte word");
        assert!(err.to_string().contains("copy length"), "{err}");
    }

    /// A stream writer for the hand-built streams the RFC's own examples imply.
    struct BitWriter {
        out: Vec<u8>,
        acc: u64,
        nanb: u32,
    }

    impl BitWriter {
        fn new() -> BitWriter {
            BitWriter {
                out: Vec::new(),
                acc: 0,
                nanb: 0,
            }
        }

        fn bits(&mut self, value: u32, count: u32) {
            self.acc |= u64::from(value & ((1 << count) - 1)) << self.nanb;
            self.nanb += count;
            while self.nanb >= 8 {
                self.out.push(self.acc as u8);
                self.acc >>= 8;
                self.nanb -= 8;
            }
        }

        fn align(&mut self) {
            if self.nanb > 0 {
                self.out.push(self.acc as u8);
                self.acc = 0;
                self.nanb = 0;
            }
        }

        fn bytes(&mut self, data: &[u8]) {
            self.out.extend_from_slice(data);
        }

        /// ISLAST and ISLASTEMPTY, which end every stream.
        fn end(&mut self) {
            self.bits(1, 1);
            self.bits(1, 1);
        }

        fn finish(mut self) -> Vec<u8> {
            self.align();
            self.out
        }
    }

    /// The trivial compressor of section 11.1, which stores every byte.
    fn trivial(words: &[u8]) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.bits(0, 1);
        if words.is_empty() {
            w.end();
            return w.finish();
        }
        w.bits(0, 1);
        w.bits(0, 2);
        w.bits(words.len() as u32 - 1, 16);
        w.bits(1, 1);
        w.align();
        w.bytes(words);
        w.end();
        w.finish()
    }

    #[test]
    fn a_stored_meta_block_expands() {
        for text in [
            &b"hello needle world\n"[..],
            &b"a"[..],
            &[0u8, 255, 1, 254, 128][..],
        ] {
            assert_eq!(decompress(&trivial(text)).unwrap(), text);
        }
    }

    #[test]
    fn a_metadata_meta_block_is_skipped() {
        let mut w = BitWriter::new();
        w.bits(0, 1);
        // An empty metadata meta-block: MNIBBLES 0, reserved bit 0, no length bytes.
        w.bits(0, 1);
        w.bits(3, 2);
        w.bits(0, 1);
        w.bits(0, 2);
        w.align();
        // A metadata meta-block carrying four bytes that must not reach the output.
        w.bits(0, 1);
        w.bits(3, 2);
        w.bits(0, 1);
        w.bits(1, 2);
        w.bits(3, 8);
        w.align();
        w.bytes(b"skip");
        // Then one stored meta-block of real data.
        w.bits(0, 1);
        w.bits(0, 2);
        w.bits(5, 16);
        w.bits(1, 1);
        w.align();
        w.bytes(b"needle");
        w.end();
        assert_eq!(decompress(&w.finish()).unwrap(), b"needle");
    }

    #[test]
    fn a_reserved_window_code_is_an_error() {
        let mut w = BitWriter::new();
        w.bits(1, 1);
        w.bits(0, 3);
        w.bits(1, 3);
        w.end();
        let err = failure(decompress(&w.finish()), "reserved window");
        assert!(err.to_string().contains("reserved"), "{err}");
    }

    #[test]
    fn a_meta_block_longer_than_the_stream_is_an_error() {
        let mut w = BitWriter::new();
        w.bits(0, 1);
        w.bits(0, 1);
        w.bits(0, 2);
        w.bits(15, 16);
        w.bits(1, 1);
        w.align();
        w.bytes(b"short");
        let err = failure(decompress(&w.finish()), "a 16-byte meta-block");
        assert!(err.to_string().contains("ends inside"), "{err}");
    }

    #[test]
    fn a_set_metadata_reserved_bit_is_an_error() {
        let mut w = BitWriter::new();
        w.bits(0, 1);
        w.bits(0, 1);
        w.bits(3, 2);
        w.bits(1, 1);
        let err = failure(decompress(&w.finish()), "metadata with a reserved bit");
        assert!(err.to_string().contains("reserved bit"), "{err}");
    }

    #[test]
    fn a_meta_block_length_with_an_empty_nibble_is_an_error() {
        let mut w = BitWriter::new();
        w.bits(0, 1);
        w.bits(0, 1);
        w.bits(2, 2);
        w.bits(0xFF_FFFF & 0x00_FFFF, 24);
        let err = failure(decompress(&w.finish()), "an empty leading nibble");
        assert!(err.to_string().contains("empty leading nibble"), "{err}");
    }
}
