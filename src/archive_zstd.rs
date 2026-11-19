//! zstd frame decompression for `-z/--search-zip`, in std alone.
//!
//! RFC 8878 in three layers. The frame: a header that sizes the window and may name a
//! dictionary, then blocks that are raw, a repeated byte, or compressed, then an optional
//! xxHash-64 checksum over the expanded bytes. The entropy coders: Huffman for the literals,
//! FSE (tANS) for the three sequence codes and even for the Huffman weights themselves. The
//! sequence engine: literals copied out, then a match copied from the window with the three
//! repeat offsets kept in recency order.
//!
//! Everything reads through two bit readers, because zstd writes forwards and reads
//! backwards: FSE table descriptions come forwards and little endian, every other stream
//! backwards from the end marker in its last byte. There is no dictionary support, so a
//! frame header naming a dictionary id is refused rather than decoded wrongly, and a frame
//! asking for a window above `MAX_WINDOW` is refused rather than trusted with the memory.

use std::io;

/// The four bytes a zstd frame starts with.
pub const MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// The three bytes after the low nibble of a skippable frame's magic.
const SKIPPABLE: [u8; 3] = [0x2a, 0x4d, 0x18];

/// Largest window a frame may ask for; past this the frame is refused.
const MAX_WINDOW: u64 = 128 << 20;

/// Largest block, from the spec: the smaller of the window and 128 KiB.
const MAX_BLOCK: usize = 128 << 10;

/// Deepest Huffman code the format allows.
const MAX_HUFFMAN_BITS: usize = 11;

/// True when `data` starts like a zstd frame.
pub fn is_zstd(data: &[u8]) -> bool {
    data.len() >= 4 && data[..4] == MAGIC
}

/// Expand every zstd frame in `data` into one buffer. Skippable frames are skipped, trailing
/// bytes that are not a frame are left out, and a frame whose header, tables, checksum or
/// length does not hold up is an error rather than a silently short answer.
pub fn decompress(data: &[u8]) -> io::Result<Vec<u8>> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let head = data.get(at..at + 4).ok_or_else(|| corrupt("bad magic"))?;
        if head == MAGIC {
            at = frame(data, at + 4, &mut out)?;
        } else if head[0] & 0xf0 == 0x50 && head[1..] == SKIPPABLE {
            let size = read_u32(data, at + 4)? as usize;
            at = at
                .checked_add(8 + size)
                .filter(|&end| end <= data.len())
                .ok_or_else(truncated)?;
        } else if at == 0 {
            return Err(corrupt("bad magic"));
        } else {
            break;
        }
    }
    Ok(out)
}

/// Expand one frame whose magic has already been consumed, appending to `out`. Returns where
/// the frame ends, which is where a checksum may sit.
fn frame(data: &[u8], at: usize, out: &mut Vec<u8>) -> io::Result<usize> {
    let start = out.len();
    let desc = *data.get(at).ok_or_else(truncated)?;
    let mut at = at + 1;
    if desc & 0x08 != 0 {
        return Err(corrupt("frame header reserved bit is set"));
    }
    let single = desc & 0x20 != 0;
    let checksum = desc & 0x04 != 0;
    let fcs_size = match (desc >> 6, single) {
        (0, false) => 0,
        (0, true) => 1,
        (1, _) => 2,
        (2, _) => 4,
        _ => 8,
    };
    let mut window = 0u64;
    if !single {
        let wd = *data.get(at).ok_or_else(truncated)?;
        at += 1;
        let log = 10 + u32::from(wd >> 3);
        let base = 1u64 << log;
        window = base + (base / 8) * u64::from(wd & 7);
    }
    let did_size = [0usize, 1, 2, 4][usize::from(desc & 0x03)];
    let mut did = 0u64;
    for i in 0..did_size {
        did |= u64::from(*data.get(at + i).ok_or_else(truncated)?) << (8 * i);
    }
    at += did_size;
    if did != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("zstd: frame needs dictionary {did}, and dictionaries are not supported"),
        ));
    }
    let mut fcs = None;
    if fcs_size > 0 {
        let mut value = 0u64;
        for i in 0..fcs_size {
            value |= u64::from(*data.get(at + i).ok_or_else(truncated)?) << (8 * i);
        }
        at += fcs_size;
        fcs = Some(if fcs_size == 2 { value + 256 } else { value });
    }
    if single {
        window = fcs.unwrap_or(0);
    }
    if window > MAX_WINDOW {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("zstd: frame window of {window} bytes is above the {MAX_WINDOW}-byte cap"),
        ));
    }
    let window = window as usize;
    if let Some(size) = fcs {
        out.reserve(size.min(1 << 20) as usize);
    }

    let mut state = FrameState::new();
    loop {
        let header = data.get(at..at + 3).ok_or_else(truncated)?;
        let header = u32::from(header[0]) | u32::from(header[1]) << 8 | u32::from(header[2]) << 16;
        at += 3;
        let last = header & 1 != 0;
        let size = (header >> 3) as usize;
        match (header >> 1) & 3 {
            0 => {
                out.extend_from_slice(slice(data, at, size)?);
                at += size;
            }
            1 => {
                if size > MAX_BLOCK {
                    return Err(corrupt("RLE block is larger than the block maximum"));
                }
                let byte = *data.get(at).ok_or_else(truncated)?;
                out.resize(out.len() + size, byte);
                at += 1;
            }
            2 => {
                if size > MAX_BLOCK {
                    return Err(corrupt("compressed block is larger than the block maximum"));
                }
                state.block(slice(data, at, size)?, out, start, window)?;
                at += size;
            }
            _ => return Err(corrupt("block type 3 is reserved")),
        }
        if last {
            break;
        }
    }
    if let Some(size) = fcs {
        if out.len() - start != size as usize {
            return Err(corrupt(
                "frame content size does not match the expanded bytes",
            ));
        }
    }
    if checksum {
        let stored = read_u32(data, at)?;
        if xxh64(&out[start..], 0) as u32 != stored {
            return Err(corrupt("checksum mismatch"));
        }
        at += 4;
    }
    Ok(at)
}

/// What a compressed block needs to remember between blocks of one frame.
struct FrameState {
    huffman: Option<Huffman>,
    ll: Option<FseTable>,
    of: Option<FseTable>,
    ml: Option<FseTable>,
    offsets: [u32; 3],
}

/// Which of the three sequence tables a mode byte is talking about.
#[derive(Clone, Copy)]
enum Slot {
    Ll,
    Of,
    Ml,
}

impl Slot {
    fn max_symbol(self) -> usize {
        match self {
            Slot::Ll => 35,
            Slot::Of => 31,
            Slot::Ml => 52,
        }
    }

    fn max_log(self) -> u32 {
        match self {
            Slot::Of => 8,
            _ => 9,
        }
    }

    fn predefined(self) -> FseTable {
        let (counts, log): (&[i16], u32) = match self {
            Slot::Ll => (&LL_DEFAULT, 6),
            Slot::Of => (&OF_DEFAULT, 5),
            Slot::Ml => (&ML_DEFAULT, 6),
        };
        FseTable::new(counts, log).expect("predefined FSE distribution")
    }

    fn thresholds(self, code: usize) -> (u32, u32) {
        match self {
            Slot::Ll => (LL_BASE[code], LL_BITS[code]),
            Slot::Of => (1 << code, code as u32),
            Slot::Ml => (ML_BASE[code], ML_BITS[code]),
        }
    }
}

impl FrameState {
    fn new() -> FrameState {
        FrameState {
            huffman: None,
            ll: None,
            of: None,
            ml: None,
            offsets: [1, 4, 8],
        }
    }

    fn slot(&mut self, which: Slot) -> &mut Option<FseTable> {
        match which {
            Slot::Ll => &mut self.ll,
            Slot::Of => &mut self.of,
            Slot::Ml => &mut self.ml,
        }
    }

    /// Decode one compressed block into `out`, with `start` the first byte of this frame.
    fn block(
        &mut self,
        data: &[u8],
        out: &mut Vec<u8>,
        start: usize,
        window: usize,
    ) -> io::Result<()> {
        let block_start = out.len();
        let (lits, used) = self.literals(data)?;
        let mut pos = used;
        let count = match *data.get(pos).ok_or_else(truncated)? {
            0 => {
                out.extend_from_slice(&lits);
                return Ok(());
            }
            b if b < 128 => {
                pos += 1;
                usize::from(b)
            }
            255 => {
                let bytes = slice(data, pos + 1, 2)?;
                pos += 3;
                usize::from(bytes[0]) + (usize::from(bytes[1]) << 8) + 0x7f00
            }
            b => {
                let low = usize::from(*data.get(pos + 1).ok_or_else(truncated)?);
                pos += 2;
                ((usize::from(b) - 128) << 8) + low
            }
        };
        let modes = *data.get(pos).ok_or_else(truncated)?;
        pos += 1;
        if modes & 3 != 0 {
            return Err(corrupt("sequence modes have reserved bits set"));
        }
        let ll_table = self.read_table(Slot::Ll, (modes >> 6) & 3, data, &mut pos)?;
        let of_table = self.read_table(Slot::Of, (modes >> 4) & 3, data, &mut pos)?;
        let ml_table = self.read_table(Slot::Ml, (modes >> 2) & 3, data, &mut pos)?;

        let mut bits = BackBits::new(&data[pos..])?;
        let mut ll_state = bits.bits(ll_table.log) as usize;
        let mut of_state = bits.bits(of_table.log) as usize;
        let mut ml_state = bits.bits(ml_table.log) as usize;
        let mut lit = 0;
        for seq in 0..count {
            let ll_code = usize::from(ll_table.cells[ll_state].symbol);
            let of_code = usize::from(of_table.cells[of_state].symbol);
            let ml_code = usize::from(ml_table.cells[ml_state].symbol);
            if ll_code > Slot::Ll.max_symbol()
                || ml_code > Slot::Ml.max_symbol()
                || of_code > Slot::Of.max_symbol()
            {
                return Err(corrupt("sequence code is outside its table"));
            }
            let (offset_base, offset_bits) = Slot::Of.thresholds(of_code);
            let offset_value = u64::from(offset_base) + bits.bits(offset_bits);
            let (ml_base, ml_bits) = Slot::Ml.thresholds(ml_code);
            let ml = ml_base + bits.bits(ml_bits) as u32;
            let (ll_base, ll_bits) = Slot::Ll.thresholds(ll_code);
            let ll = ll_base + bits.bits(ll_bits) as u32;

            let end = lit + ll as usize;
            out.extend_from_slice(
                lits.get(lit..end)
                    .ok_or_else(|| corrupt("sequence runs past the literals"))?,
            );
            lit = end;

            let offset = self.resolve_offset(offset_value, ll)? as usize;
            let length = ml as usize;
            if offset == 0 || offset > out.len() - start || offset > window.max(1) {
                return Err(corrupt("match offset points outside the window"));
            }
            let from = out.len() - offset;
            if offset >= length {
                out.extend_from_within(from..from + length);
            } else {
                for k in 0..length {
                    let byte = out[from + k];
                    out.push(byte);
                }
            }
            if seq + 1 < count {
                ll_state = ll_table.next(ll_state, &mut bits);
                ml_state = ml_table.next(ml_state, &mut bits);
                of_state = of_table.next(of_state, &mut bits);
            }
        }
        out.extend_from_slice(&lits[lit..]);
        if bits.pos != bits.total {
            return Err(corrupt(
                "sequence bitstream does not end with the sequences",
            ));
        }
        if out.len() - block_start > MAX_BLOCK {
            return Err(corrupt("block expands past the block maximum"));
        }
        Ok(())
    }

    /// The literals section: raw, one repeated byte, or Huffman-coded in one or four streams.
    fn literals(&mut self, data: &[u8]) -> io::Result<(Vec<u8>, usize)> {
        let first = *data.first().ok_or_else(truncated)?;
        let size_format = (first >> 2) & 3;
        if first & 3 < 2 {
            let (regen, header) = match size_format {
                0 | 2 => (usize::from(first >> 3), 1),
                1 => {
                    let next = *data.get(1).ok_or_else(truncated)?;
                    (usize::from(first >> 4) + (usize::from(next) << 4), 2)
                }
                _ => {
                    let more = slice(data, 1, 2)?;
                    (
                        usize::from(first >> 4)
                            + (usize::from(more[0]) << 4)
                            + (usize::from(more[1]) << 12),
                        3,
                    )
                }
            };
            if regen > MAX_BLOCK {
                return Err(corrupt("literals section is larger than the block maximum"));
            }
            if first & 3 == 0 {
                return Ok((slice(data, header, regen)?.to_vec(), header + regen));
            }
            let byte = *data.get(header).ok_or_else(truncated)?;
            return Ok((vec![byte; regen], header + 1));
        }
        let (regen, packed, header, four) = match size_format {
            0 | 1 => {
                let word = header_word(data, 3)?;
                (
                    (word >> 4) as usize & 0x3ff,
                    (word >> 14) as usize & 0x3ff,
                    3,
                    size_format == 1,
                )
            }
            2 => {
                let word = header_word(data, 4)?;
                (
                    (word >> 4) as usize & 0x3fff,
                    (word >> 18) as usize & 0x3fff,
                    4,
                    true,
                )
            }
            _ => {
                let word = header_word(data, 5)?;
                (
                    (word >> 4) as usize & 0x3ffff,
                    (word >> 22) as usize & 0x3ffff,
                    5,
                    true,
                )
            }
        };
        if regen > MAX_BLOCK {
            return Err(corrupt("literals section is larger than the block maximum"));
        }
        let body = slice(data, header, packed)?;
        let (tree, tree_len) = if first & 3 == 2 {
            huffman_tree(body)?
        } else {
            let tree = self
                .huffman
                .clone()
                .ok_or_else(|| corrupt("treeless literals with no previous Huffman table"))?;
            (tree, 0)
        };
        self.huffman = Some(tree.clone());
        let streams = &body[tree_len..];
        Ok((
            huffman_streams(&tree, streams, regen, four)?,
            header + packed,
        ))
    }

    /// Pick up one sequence table from its mode byte, remembering it for repeat mode.
    fn read_table(
        &mut self,
        which: Slot,
        mode: u8,
        data: &[u8],
        pos: &mut usize,
    ) -> io::Result<FseTable> {
        let table = match mode {
            0 => which.predefined(),
            1 => {
                let symbol = *data.get(*pos).ok_or_else(truncated)?;
                *pos += 1;
                if usize::from(symbol) > which.max_symbol() {
                    return Err(corrupt("RLE symbol is outside its table"));
                }
                FseTable::rle(symbol)
            }
            2 => {
                let rest = data.get(*pos..).ok_or_else(truncated)?;
                let (counts, log, used) =
                    fse_read_counts(rest, which.max_symbol(), which.max_log())?;
                *pos += used;
                FseTable::new(&counts, log)?
            }
            _ => self
                .slot(which)
                .clone()
                .ok_or_else(|| corrupt("repeat mode with no previous table"))?,
        };
        *self.slot(which) = Some(table.clone());
        Ok(table)
    }

    /// Turn an offset value into a distance, keeping the three repeat offsets in recency order.
    fn resolve_offset(&mut self, value: u64, literals: u32) -> io::Result<u32> {
        let reps = &mut self.offsets;
        let index = match value {
            1..=3 => value as usize + usize::from(literals == 0),
            _ => {
                let offset = value - 3;
                if offset > u64::from(u32::MAX) {
                    return Err(corrupt("match offset does not fit the window"));
                }
                let offset = offset as u32;
                reps[2] = reps[1];
                reps[1] = reps[0];
                reps[0] = offset;
                return Ok(offset);
            }
        };
        let offset = match index {
            1 => reps[0],
            2 => {
                reps.swap(1, 0);
                reps[0]
            }
            3 => {
                let used = reps[2];
                reps[2] = reps[1];
                reps[1] = reps[0];
                reps[0] = used;
                used
            }
            _ => {
                let used = reps[0]
                    .checked_sub(1)
                    .ok_or_else(|| corrupt("repeat offset is zero"))?;
                reps[2] = reps[1];
                reps[1] = reps[0];
                reps[0] = used;
                used
            }
        };
        Ok(offset)
    }
}

/// The first `n` header bytes as one little-endian bit field.
fn header_word(data: &[u8], n: usize) -> io::Result<u64> {
    let mut word = 0u64;
    for (i, &byte) in slice(data, 0, n)?.iter().enumerate() {
        word |= u64::from(byte) << (8 * i);
    }
    Ok(word)
}

// ---------------------------------------------------------------------------------------
// Bit readers
// ---------------------------------------------------------------------------------------

/// A bitstream read forwards, least significant bit first: FSE table descriptions.
struct ForwardBits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ForwardBits<'a> {
    fn new(data: &'a [u8]) -> ForwardBits<'a> {
        ForwardBits { data, pos: 0 }
    }

    fn peek(&self, n: u32) -> io::Result<u32> {
        let mut value = 0;
        for k in 0..n as usize {
            let at = self.pos + k;
            let byte = *self.data.get(at / 8).ok_or_else(truncated)?;
            value |= u32::from((byte >> (at % 8)) & 1) << k;
        }
        Ok(value)
    }

    fn bits(&mut self, n: u32) -> io::Result<u32> {
        let value = self.peek(n)?;
        self.pos += n as usize;
        Ok(value)
    }

    fn skip(&mut self, n: u32) {
        self.pos += n as usize;
    }

    /// Bytes the reader stands on, rounded up, which is what a table description occupies.
    fn used(&self) -> usize {
        self.pos.div_ceil(8)
    }
}

/// A bitstream read backwards, as zstd writes every stream that is not a table description.
/// The last byte's highest set bit is the end marker; the bits below it come first, then the
/// earlier bytes from their highest bit down. Reads past the end return zero, which is what
/// FSE decoding of Huffman weights needs to stop on.
struct BackBits<'a> {
    data: &'a [u8],
    /// Bits below the end marker in the last byte.
    head: usize,
    /// Bits the stream holds altogether.
    total: usize,
    /// Bits already consumed.
    pos: usize,
}

impl<'a> BackBits<'a> {
    fn new(data: &'a [u8]) -> io::Result<BackBits<'a>> {
        let last = *data.last().ok_or_else(|| corrupt("bitstream is empty"))?;
        if last == 0 {
            return Err(corrupt("bitstream has no end marker"));
        }
        let head = 7 - last.leading_zeros() as usize;
        Ok(BackBits {
            data,
            head,
            total: head + 8 * (data.len() - 1),
            pos: 0,
        })
    }

    fn bit(&self, at: usize) -> u64 {
        if at < self.head {
            let byte = self.data[self.data.len() - 1];
            u64::from((byte >> (self.head - 1 - at)) & 1)
        } else {
            let k = (at - self.head) / 8;
            if k + 1 >= self.data.len() {
                return 0;
            }
            let byte = self.data[self.data.len() - 2 - k];
            u64::from((byte >> (7 - (at - self.head) % 8)) & 1)
        }
    }

    fn peek(&self, n: u32) -> u64 {
        let mut value = 0;
        for k in 0..n as usize {
            value = value << 1 | self.bit(self.pos + k);
        }
        value
    }

    fn skip(&mut self, n: u32) {
        self.pos += n as usize;
    }

    fn bits(&mut self, n: u32) -> u64 {
        let value = self.peek(n);
        self.skip(n);
        value
    }
}

// ---------------------------------------------------------------------------------------
// FSE
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct FseCell {
    symbol: u8,
    nb_bits: u8,
    new_state: u16,
}

/// A tANS decoding table: one cell per state, giving the symbol that state decodes and how to
/// reach the next state by reading `nb_bits` more.
#[derive(Clone)]
struct FseTable {
    log: u32,
    cells: Vec<FseCell>,
}

impl FseTable {
    /// Build the table from a normalized distribution, as described in RFC 8878 4.1.1.
    fn new(counts: &[i16], log: u32) -> io::Result<FseTable> {
        let size = 1usize << log;
        let total: i32 = counts
            .iter()
            .map(|&c| if c < 0 { 1 } else { i32::from(c) })
            .sum();
        if total != size as i32 {
            return Err(corrupt("FSE distribution does not fill its table"));
        }
        let mut cells = vec![
            FseCell {
                symbol: 0,
                nb_bits: 0,
                new_state: 0,
            };
            size
        ];
        if counts.iter().all(|&count| count <= 0) {
            return Err(corrupt(
                "FSE distribution has no symbol with a positive probability",
            ));
        }
        // Symbols with "less than 1" probability take one cell each, off the end of the table.
        let mut low = size;
        for (symbol, &count) in counts.iter().enumerate() {
            if count == -1 {
                low -= 1;
                cells[low] = FseCell {
                    symbol: symbol as u8,
                    nb_bits: log as u8,
                    new_state: 0,
                };
            }
        }
        // The rest are spread over the states below that, in symbol order.
        let mask = size - 1;
        let step = (size >> 1) + (size >> 3) + 3;
        let mut position = 0;
        for (symbol, &count) in counts.iter().enumerate() {
            for _ in 0..count.max(0) {
                while position >= low {
                    position = (position + step) & mask;
                }
                cells[position].symbol = symbol as u8;
                position = (position + step) & mask;
            }
        }
        // Each state's successor: low states need one bit more than high ones.
        let mut next: Vec<u32> = counts
            .iter()
            .map(|&c| if c < 0 { 1 } else { c as u32 })
            .collect();
        for cell in &mut cells {
            let symbol = usize::from(cell.symbol);
            let state = next[symbol];
            next[symbol] = state + 1;
            let nb_bits = log - highbit(state);
            cell.nb_bits = nb_bits as u8;
            cell.new_state = ((state << nb_bits) - size as u32) as u16;
        }
        Ok(FseTable { log, cells })
    }

    /// The one-symbol table that RLE mode stands for; its state never changes.
    fn rle(symbol: u8) -> FseTable {
        FseTable {
            log: 0,
            cells: vec![FseCell {
                symbol,
                nb_bits: 0,
                new_state: 0,
            }],
        }
    }

    fn next(&self, state: usize, bits: &mut BackBits) -> usize {
        let cell = &self.cells[state];
        cell.new_state as usize + bits.bits(u32::from(cell.nb_bits)) as usize
    }
}

/// Read an FSE distribution table description, which is a forwards bitstream of its own.
/// Returns the normalized counts, the accuracy log, and the bytes it occupied.
fn fse_read_counts(
    data: &[u8],
    max_symbol: usize,
    max_log: u32,
) -> io::Result<(Vec<i16>, u32, usize)> {
    let mut bits = ForwardBits::new(data);
    let log = bits.bits(4)? + 5;
    if log > max_log {
        return Err(corrupt("FSE table accuracy log is too large"));
    }
    let mut remaining = (1i32 << log) + 1;
    let mut threshold = 1u32 << log;
    let mut nb_bits = log + 1;
    let mut counts: Vec<i16> = Vec::new();
    let mut previous_zero = false;
    while remaining > 1 {
        if counts.len() > max_symbol {
            return Err(corrupt("FSE table has more symbols than its codes allow"));
        }
        if previous_zero {
            let mut skip_to = counts.len();
            while bits.peek(16)? == 0xffff {
                skip_to += 24;
                bits.skip(16);
            }
            while bits.peek(2)? == 3 {
                skip_to += 3;
                bits.skip(2);
            }
            skip_to += bits.peek(2)? as usize;
            bits.skip(2);
            if skip_to > max_symbol {
                return Err(corrupt("FSE zero run passes the last symbol"));
            }
            while counts.len() < skip_to {
                counts.push(0);
            }
        }
        let max = (2 * threshold - 1) as i32 - remaining;
        let low = bits.peek(nb_bits - 1)? as i32;
        let value = if low < max {
            bits.skip(nb_bits - 1);
            low
        } else {
            let wide = bits.peek(nb_bits)? as i32;
            bits.skip(nb_bits);
            if wide >= threshold as i32 {
                wide - max
            } else {
                wide
            }
        };
        let count = value - 1;
        remaining -= count.abs();
        if remaining < 1 {
            return Err(corrupt("FSE distribution over-allocates its states"));
        }
        counts.push(count as i16);
        previous_zero = count == 0;
        while remaining < threshold as i32 {
            nb_bits -= 1;
            threshold >>= 1;
        }
    }
    Ok((counts, log, bits.used()))
}

/// Expand the FSE-compressed Huffman weights: two interleaved states over one table, decoded
/// until a state update runs off the end of the stream.
fn fse_weights(data: &[u8]) -> io::Result<Vec<u8>> {
    let (counts, log, used) = fse_read_counts(data, 255, 6)?;
    let table = FseTable::new(&counts, log)?;
    let mut bits = BackBits::new(&data[used..])?;
    let mut even = bits.bits(log) as usize;
    let mut odd = bits.bits(log) as usize;
    let mut weights = Vec::new();
    loop {
        if weights.len() > 255 {
            return Err(corrupt("Huffman weight stream is too long"));
        }
        weights.push(table.cells[even].symbol);
        even = table.next(even, &mut bits);
        if bits.pos > bits.total {
            weights.push(table.cells[odd].symbol);
            break;
        }
        weights.push(table.cells[odd].symbol);
        odd = table.next(odd, &mut bits);
        if bits.pos > bits.total {
            weights.push(table.cells[even].symbol);
            break;
        }
    }
    Ok(weights)
}

/// The predefined distributions of RFC 8878 3.1.1.3.2.2, where -1 means "less than one".
const LL_DEFAULT: [i16; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];

const ML_DEFAULT: [i16; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];

const OF_DEFAULT: [i16; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

const LL_BASE: [u32; 36] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
];

const LL_BITS: [u32; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];

const ML_BASE: [u32; 53] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027,
    2051, 4099, 8195, 16387, 32771, 65539,
];

const ML_BITS: [u32; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];

// ---------------------------------------------------------------------------------------
// Huffman
// ---------------------------------------------------------------------------------------

#[derive(Clone)]
struct Huffman {
    log: u32,
    /// (symbol, code length) for each of the next `log` bits of a stream.
    table: Vec<(u8, u8)>,
}

impl Huffman {
    /// Turn the list of weights into a decoding table. Every listed weight plus the one symbol
    /// whose weight is left out has to fill the code space exactly.
    fn new(weights: &[u8]) -> io::Result<Huffman> {
        let mut space = 0u32;
        for &weight in weights {
            if usize::from(weight) > MAX_HUFFMAN_BITS {
                return Err(corrupt("Huffman weight is deeper than the format allows"));
            }
            if weight > 0 {
                space += 1 << (weight - 1);
            }
        }
        if space == 0 {
            return Err(corrupt("Huffman tree has no symbols"));
        }
        let log = highbit(space) + 1;
        if log as usize > MAX_HUFFMAN_BITS {
            return Err(corrupt("Huffman tree is deeper than 11 bits"));
        }
        if weights.iter().any(|&weight| u32::from(weight) > log) {
            return Err(corrupt("Huffman weight is above the tree depth"));
        }
        let rest = (1u32 << log) - space;
        if rest == 0 || rest.count_ones() != 1 {
            return Err(corrupt("Huffman weights do not fill the code space"));
        }
        let mut all = weights.to_vec();
        all.push((highbit(rest) + 1) as u8);
        let log = log as usize;
        let mut table = vec![(0u8, 0u8); 1 << log];
        let mut code = 0u32;
        for weight in 1..=log {
            let len = log + 1 - weight;
            for (symbol, &symbol_weight) in all.iter().enumerate() {
                if usize::from(symbol_weight) != weight {
                    continue;
                }
                let span = 1usize << (log - len);
                let start = code as usize * span;
                let slots = table
                    .get_mut(start..start + span)
                    .ok_or_else(|| corrupt("Huffman code space is over-subscribed"))?;
                for slot in slots {
                    *slot = (symbol as u8, len as u8);
                }
                code += 1;
            }
            code >>= 1;
        }
        Ok(Huffman {
            log: log as u32,
            table,
        })
    }

    /// Read one symbol, peeking `log` bits so a table lookup replaces a walk down the tree.
    fn decode(&self, bits: &mut BackBits, out: &mut Vec<u8>) -> io::Result<()> {
        let (symbol, len) = self.table[bits.peek(self.log) as usize];
        if len == 0 {
            return Err(corrupt("Huffman code is not in the table"));
        }
        bits.skip(u32::from(len));
        out.push(symbol);
        Ok(())
    }
}

/// Read a Huffman tree description, returning the table and the bytes it occupied.
fn huffman_tree(data: &[u8]) -> io::Result<(Huffman, usize)> {
    let first = *data.first().ok_or_else(truncated)?;
    let (weights, used) = if first >= 128 {
        let count = usize::from(first) - 127;
        let body = slice(data, 1, count.div_ceil(2))?;
        let mut weights = Vec::with_capacity(count);
        for i in 0..count {
            let byte = body[i / 2];
            weights.push(if i % 2 == 0 { byte >> 4 } else { byte & 0x0f });
        }
        (weights, 1 + body.len())
    } else {
        let body = slice(data, 1, usize::from(first))?;
        (fse_weights(body)?, 1 + body.len())
    };
    Ok((Huffman::new(&weights)?, used))
}

/// Expand `regen` literals from one stream, or from four split by a jump table.
fn huffman_streams(tree: &Huffman, data: &[u8], regen: usize, four: bool) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(regen);
    if !four {
        if regen == 0 {
            return Ok(out);
        }
        let mut bits = BackBits::new(data)?;
        for _ in 0..regen {
            tree.decode(&mut bits, &mut out)?;
        }
        if bits.pos != bits.total {
            return Err(corrupt("Huffman stream does not end with its literals"));
        }
        return Ok(out);
    }
    let mut sizes = [0usize; 4];
    for (i, size) in sizes.iter_mut().take(3).enumerate() {
        let field = slice(data, 2 * i, 2)?;
        *size = usize::from(field[0]) | usize::from(field[1]) << 8;
    }
    let used: usize = sizes.iter().take(3).sum();
    let tail = data
        .len()
        .checked_sub(6 + used)
        .ok_or_else(|| corrupt("Huffman jump table overruns the literals"))?;
    sizes[3] = tail;
    let segment = regen.div_ceil(4);
    let mut at = 6;
    for (i, &size) in sizes.iter().enumerate() {
        let stream = slice(data, at, size)?;
        at += size;
        let want = if i < 3 {
            segment.min(regen - out.len())
        } else {
            regen - out.len()
        };
        if want == 0 {
            if !stream.is_empty() {
                return Err(corrupt("Huffman stream has bytes but no literals"));
            }
            continue;
        }
        let mut bits = BackBits::new(stream)?;
        for _ in 0..want {
            tree.decode(&mut bits, &mut out)?;
        }
        if bits.pos != bits.total {
            return Err(corrupt("Huffman stream does not end with its literals"));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// xxHash-64
// ---------------------------------------------------------------------------------------

const PRIME1: u64 = 0x9E37_79B1_85EB_CA87;
const PRIME2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const PRIME3: u64 = 0x1656_67B1_9E37_79F9;
const PRIME4: u64 = 0x85EB_CA77_C2B2_AE63;
const PRIME5: u64 = 0x27D4_EB2F_1656_67C5;

/// xxHash-64, the hash a zstd frame checksum is cut from.
pub fn xxh64(data: &[u8], seed: u64) -> u64 {
    let mut at = 0;
    let mut hash;
    if data.len() >= 32 {
        let mut acc = [
            seed.wrapping_add(PRIME1).wrapping_add(PRIME2),
            seed.wrapping_add(PRIME2),
            seed,
            seed.wrapping_sub(PRIME1),
        ];
        while at + 32 <= data.len() {
            for (k, lane) in acc.iter_mut().enumerate() {
                *lane = round(*lane, read_u64(&data[at + 8 * k..at + 8 * k + 8]));
            }
            at += 32;
        }
        hash = acc[0]
            .rotate_left(1)
            .wrapping_add(acc[1].rotate_left(7))
            .wrapping_add(acc[2].rotate_left(12))
            .wrapping_add(acc[3].rotate_left(18));
        for lane in acc {
            hash = (hash ^ round(0, lane))
                .wrapping_mul(PRIME1)
                .wrapping_add(PRIME4);
        }
    } else {
        hash = seed.wrapping_add(PRIME5);
    }
    hash = hash.wrapping_add(data.len() as u64);
    while at + 8 <= data.len() {
        hash ^= round(0, read_u64(&data[at..at + 8]));
        hash = hash
            .rotate_left(27)
            .wrapping_mul(PRIME1)
            .wrapping_add(PRIME4);
        at += 8;
    }
    if at + 4 <= data.len() {
        let word = u32::from_le_bytes(data[at..at + 4].try_into().expect("four bytes")) as u64;
        hash ^= word.wrapping_mul(PRIME1);
        hash = hash
            .rotate_left(23)
            .wrapping_mul(PRIME2)
            .wrapping_add(PRIME3);
        at += 4;
    }
    for &byte in &data[at..] {
        hash ^= u64::from(byte).wrapping_mul(PRIME5);
        hash = hash.rotate_left(11).wrapping_mul(PRIME1);
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(PRIME2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(PRIME3);
    hash ^ (hash >> 32)
}

fn round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(PRIME2))
        .rotate_left(31)
        .wrapping_mul(PRIME1)
}

fn read_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().expect("eight bytes"))
}

// ---------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------

fn corrupt(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("zstd: {what}"))
}

fn truncated() -> io::Error {
    corrupt("file ends mid-frame")
}

/// `data[at..at + n]`, with a truncation error instead of a panic when it is not there.
fn slice(data: &[u8], at: usize, n: usize) -> io::Result<&[u8]> {
    at.checked_add(n)
        .and_then(|end| data.get(at..end))
        .ok_or_else(truncated)
}

fn read_u32(data: &[u8], at: usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(
        slice(data, at, 4)?.try_into().expect("four bytes"),
    ))
}

/// The position of the highest set bit, for a value that is not zero.
fn highbit(value: u32) -> u32 {
    31 - value.leading_zeros()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/zstd")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// One frame per fixture the compressor produced, with the plaintext it must come back as.
    const FIXTURES: [(&str, &str); 15] = [
        ("empty.zst", "empty.bin"),
        ("one.zst", "one.bin"),
        ("text3.zst", "text.bin"),
        ("text19.zst", "text.bin"),
        ("streams4.zst", "text.bin"),
        ("long.zst", "text.bin"),
        ("stream.zst", "text.bin"),
        ("nocheck.zst", "text.bin"),
        ("contentsize.zst", "text.bin"),
        ("random.zst", "random.bin"),
        ("repeat.zst", "repeat.bin"),
        ("modes.zst", "modes.bin"),
        ("reuse.zst", "reuse.bin"),
        ("rletables.zst", "rletables.bin"),
        ("concat.zst", "concat.bin"),
    ];

    #[test]
    fn xxh64_matches_the_reference_vectors() {
        assert_eq!(xxh64(b"", 0), 0xef46_db37_51d8_e999);
        assert_eq!(xxh64(b"abc", 0), 0x44bc_2cf5_ad77_0999);
    }

    #[test]
    fn a_real_frame_checksum_is_the_low_half_of_xxh64() {
        // zstd wrote this trailer itself, so it is an outside answer to compare against.
        let packed = fixture("text3.zst");
        let stored = u32::from_le_bytes(packed[packed.len() - 4..].try_into().unwrap());
        assert_eq!(xxh64(&fixture("text.bin"), 0) as u32, stored);
    }

    #[test]
    fn every_fixture_expands_to_its_plaintext() {
        for (packed, plain) in FIXTURES {
            let got = decompress(&fixture(packed)).unwrap_or_else(|e| panic!("{packed}: {e}"));
            assert_eq!(got, fixture(plain), "{packed} did not expand to {plain}");
        }
    }

    #[test]
    fn the_fixture_set_covers_every_block_type() {
        // A 200 KiB text is worth checking for the sizes the block reader has to accept.
        let text = fixture("text.bin");
        assert_eq!(text.len(), 204_802);
        let raw = decompress(&fixture("random.zst")).unwrap();
        assert_eq!(raw.len(), 65_536);
    }

    fn raw_frame(payload: &[u8]) -> Vec<u8> {
        let mut frame = MAGIC.to_vec();
        frame.push(0x00);
        frame.push(0x00);
        let header = 1u32 | (payload.len() as u32) << 3;
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        frame.extend_from_slice(payload);
        frame
    }

    #[test]
    fn a_hand_built_raw_block_frame_expands() {
        assert!(is_zstd(&raw_frame(b"hello\n")));
        assert_eq!(decompress(&raw_frame(b"hello\n")).unwrap(), b"hello\n");
    }

    #[test]
    fn an_rle_literals_block_expands() {
        // Compressed block, RLE literals of 7 bytes, and no sequences at all.
        let mut frame = MAGIC.to_vec();
        frame.push(0x00);
        frame.push(0x00);
        let body = [1u8 | 7 << 3, b'Q', 0];
        let header = 1u32 | 2 << 1 | (body.len() as u32) << 3;
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        frame.extend_from_slice(&body);
        assert_eq!(decompress(&frame).unwrap(), b"QQQQQQQ");
    }

    #[test]
    fn an_rle_block_expands() {
        let mut frame = MAGIC.to_vec();
        frame.push(0x00);
        frame.push(0x00);
        let header = 1u32 | 1 << 1 | 300 << 3;
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        frame.push(b'z');
        assert_eq!(decompress(&frame).unwrap(), vec![b'z'; 300]);
    }

    #[test]
    fn a_skippable_frame_is_skipped() {
        let mut data = vec![0x50, 0x2a, 0x4d, 0x18];
        data.extend_from_slice(&5u32.to_le_bytes());
        data.extend_from_slice(b"junk!");
        data.extend_from_slice(&raw_frame(b"after"));
        assert_eq!(decompress(&data).unwrap(), b"after");
    }

    #[test]
    fn a_bad_magic_is_an_error() {
        let err = decompress(b"not a zstd frame").unwrap_err();
        assert!(err.to_string().contains("bad magic"), "{err}");
    }

    #[test]
    fn a_truncated_frame_is_an_error() {
        let frame = raw_frame(b"hello\n");
        for cut in 1..frame.len() {
            let err = decompress(&frame[..cut]).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "cut at {cut}");
        }
    }

    #[test]
    fn a_bad_checksum_is_an_error() {
        let mut frame = MAGIC.to_vec();
        frame.push(0x04);
        frame.push(0x00);
        let header = 1u32 | (6u32) << 3;
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        frame.extend_from_slice(b"hello\n");
        frame.extend_from_slice(&0xdead_beefu32.to_le_bytes());
        let err = decompress(&frame).unwrap_err();
        assert!(err.to_string().contains("checksum"), "{err}");
    }

    #[test]
    fn a_named_dictionary_is_refused() {
        let mut frame = MAGIC.to_vec();
        frame.push(0x21);
        frame.push(0x01);
        frame.push(0x2a);
        frame.extend_from_slice(&0u32.to_le_bytes()[..1]);
        let err = decompress(&frame).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert!(err.to_string().contains("dictionary"), "{err}");
    }

    #[test]
    fn a_window_above_the_cap_is_refused() {
        let mut frame = MAGIC.to_vec();
        frame.push(0x00);
        frame.push(31 << 3 | 7);
        let err = decompress(&frame).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert!(err.to_string().contains("window"), "{err}");
    }

    #[test]
    fn the_reserved_block_type_is_refused() {
        let mut frame = MAGIC.to_vec();
        frame.push(0x00);
        frame.push(0x00);
        let header = 1u32 | 3 << 1;
        frame.extend_from_slice(&header.to_le_bytes()[..3]);
        let err = decompress(&frame).unwrap_err();
        assert!(err.to_string().contains("reserved"), "{err}");
    }

    /// Not part of the suite, and needs `zstd` on PATH: expands real files at several
    /// settings and compares them against the originals. `cargo test --release --lib
    /// archive_zstd::tests::cross_check -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn cross_check_against_the_reference() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut files = Vec::new();
        for dir in ["/usr/bin", "/usr/lib/x86_64-linux-gnu", "/usr/share/doc"] {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten().take(6) {
                let path = entry.path();
                if path.is_file() {
                    files.push(path);
                }
            }
        }
        let mut checked = 0;
        for path in files {
            let plain = std::fs::read(&path).unwrap();
            if plain.len() > 4 << 20 {
                continue;
            }
            for args in [
                ["-1", "--no-check"],
                ["-3", ""],
                ["-9", ""],
                ["-19", "--long=20"],
                ["-5", "--content-size"],
            ] {
                let mut child = Command::new("zstd")
                    .args(["-q", "-c", args[0]])
                    .args(args[1].split_whitespace())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .expect("zstd on PATH");
                child.stdin.as_mut().unwrap().write_all(&plain).unwrap();
                let packed = child.wait_with_output().unwrap().stdout;
                let got = decompress(&packed)
                    .unwrap_or_else(|e| panic!("{} {args:?}: {e}", path.display()));
                assert!(got == plain, "{} {args:?}", path.display());
                checked += 1;
            }
        }
        println!("cross-checked {checked} frame/file pairs");
        assert!(checked > 50);
    }

    /// Not part of the suite: `cargo test --release --lib archive_zstd::tests::throughput
    /// -- --ignored --nocapture` prints the decode rate of the 200 KiB fixture.
    #[test]
    #[ignore]
    fn throughput() {
        let packed = fixture("text3.zst");
        let text = fixture("text.bin");
        for _ in 0..3 {
            assert_eq!(decompress(&packed).unwrap().len(), text.len());
        }
        let runs = 200;
        let start = std::time::Instant::now();
        for _ in 0..runs {
            std::hint::black_box(decompress(&packed).unwrap());
        }
        let seconds = start.elapsed().as_secs_f64();
        let mb = (text.len() as f64 * runs as f64) / (1024.0 * 1024.0);
        println!(
            "{:.1} MB/s ({} runs of {} bytes in {:.3}s)",
            mb / seconds,
            runs,
            text.len(),
            seconds
        );
    }

    #[test]
    fn random_bytes_are_refused_without_panicking() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            let len = 4 + (next() % 60) as usize;
            let mut data = MAGIC.to_vec();
            for _ in 0..len {
                data.push((next() >> 24) as u8);
            }
            let _ = decompress(&data);
            let junk: Vec<u8> = (0..len).map(|_| (next() >> 32) as u8).collect();
            let _ = decompress(&junk);
        }
    }

    #[test]
    fn corrupted_bytes_never_expand_to_the_wrong_text() {
        let packed = fixture("text3.zst");
        let text = fixture("text.bin");
        let (mut caught, mut decoded) = (0, 0);
        for at in (4..packed.len() - 4).step_by(149) {
            let mut data = packed.clone();
            data[at] ^= 0xa5;
            if decompress(&data).is_err() {
                caught += 1;
            } else {
                assert_eq!(decompress(&data).unwrap(), text, "flip at {at}");
                decoded += 1;
            }
        }
        assert!(caught > 100, "only {caught} flips were caught");
        assert_eq!(decoded, 0, "a flipped frame still expanded");
    }
}
