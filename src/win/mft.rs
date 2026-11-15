//! Raw NTFS MFT reader (admin only). Owned by wave-1 lane `mft`.
//!
//! On-disk layouts (little endian), see MS Learn `FILE_RECORD_SEGMENT_HEADER`,
//! `MULTI_SECTOR_HEADER`, `ATTRIBUTE_RECORD_HEADER` and flatcap's NTFS docs:
//!
//! ```text
//! FILE record: 0x00 "FILE" | 0x04 usa_off u16 | 0x06 usa_count u16 | 0x10 seq u16
//!              0x12 link count u16 | 0x14 first attr off u16 | 0x16 flags u16 (1 in use, 2 dir)
//!              0x18 used size u32 | 0x1C alloc size u32 | 0x20 base record ref u64
//! attr header: 0x00 type u32 | 0x04 len u32 | 0x08 non-resident u8 | 0x09 name len u8
//!              resident:     0x10 value len u32 | 0x14 value off u16
//!              non-resident: 0x10 start vcn | 0x20 runlist off u16 | 0x22 compression unit u16
//!                            0x28 alloc size u64 | 0x30 data size u64 | 0x40 compressed size u64
//! $FILE_NAME:  0x00 parent ref u64 | 0x38 flags u32 | 0x40 name len u8 | 0x41 namespace u8 | 0x42 name
//! ```
//!
//! Approximations (counted in [`MftStats::approx_records`], flagged with
//! [`MftEntry::approx`]): a file whose `$ATTRIBUTE_LIST` is present and whose
//! unnamed `$DATA` first extent lives in an extension record gets size 0.
//! Files with a `$ATTRIBUTE_LIST` but a `$DATA` first extent in the base record
//! are sized exactly (only the first extent carries the total sizes). Records
//! whose `$FILE_NAME` lives only in an extension record are not emitted.
//! Resident `$DATA` reports `alloc = 0` (the bytes live inside the MFT record).

use std::collections::HashMap;
use std::ffi::{c_void, OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::FileExt;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::sys;

pub struct MftEntry {
    /// Record index | sequence number << 48.
    pub frn: u64,
    /// Parent reference exactly as stored in `$FILE_NAME` (index | seq << 48).
    pub parent: u64,
    pub name: OsString,
    pub is_dir: bool,
    pub size: u64,
    pub alloc: u64,
    /// Number of non-DOS `$FILE_NAME` attributes found in the base record.
    pub links: u16,
    pub reparse: bool,
    /// True when size/links may be incomplete because of an `$ATTRIBUTE_LIST`.
    pub approx: bool,
    /// Further hardlink names (parent reference, name) beyond `parent`/`name`.
    pub other_names: Vec<(u64, OsString)>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct MftStats {
    /// Record slots examined (including empty ones).
    pub records: u64,
    /// Entries delivered to the callback.
    pub in_use: u64,
    pub parse_errors: u64,
    /// Entries delivered with `approx = true`.
    pub approx_records: u64,
    pub elapsed: Duration,
}

const FILE_MAGIC: &[u8; 4] = b"FILE";
const FLAG_IN_USE: u16 = 0x1;
const FLAG_DIR: u16 = 0x2;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
const ATTR_DATA: u32 = 0x80;
const ATTR_REPARSE_POINT: u32 = 0xC0;
const ATTR_END: u32 = 0xFFFF_FFFF;
const NS_DOS: u8 = 2;
/// NTFS fixup stride is always 512 bytes, independent of the device sector size.
const FIXUP_STRIDE: usize = 512;
const CHUNK: usize = 4 << 20;

const FSCTL_GET_NTFS_VOLUME_DATA: u32 = ctl_code(9, 25, 0, 0);
/// Bypass the cache: the MFT is read once, and cached reads cap throughput.
/// Offsets, lengths and buffer addresses must then be sector aligned.
const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;

const fn ctl_code(device: u32, function: u32, method: u32, access: u32) -> u32 {
    (device << 16) | (access << 14) | (function << 2) | method
}

/// `NTFS_VOLUME_DATA_BUFFER` (winioctl.h); the extended tail is not needed.
#[repr(C)]
#[derive(Clone, Copy)]
struct NtfsVolumeData {
    volume_serial_number: i64,
    number_sectors: i64,
    total_clusters: i64,
    free_clusters: i64,
    total_reserved: i64,
    bytes_per_sector: u32,
    bytes_per_cluster: u32,
    bytes_per_file_record_segment: u32,
    clusters_per_file_record_segment: u32,
    mft_valid_data_length: i64,
    mft_start_lcn: i64,
    mft2_start_lcn: i64,
    mft_zone_start: i64,
    mft_zone_end: i64,
}

// ---- byte readers -------------------------------------------------------

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        b.get(o..o.checked_add(2)?)?.try_into().ok()?,
    ))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        b.get(o..o.checked_add(4)?)?.try_into().ok()?,
    ))
}
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        b.get(o..o.checked_add(8)?)?.try_into().ok()?,
    ))
}

// ---- fixup --------------------------------------------------------------

/// Undo the multi-sector update sequence protection in place. Each
/// `bytes_per_sector` stride must end with the update sequence number; the
/// original bytes are restored from the update sequence array. A mismatch means
/// a torn write.
pub fn apply_fixup(record: &mut [u8], bytes_per_sector: usize) -> Result<(), String> {
    if bytes_per_sector < 4 || record.len() < 8 {
        return Err("record too small".into());
    }
    let usa_off = u16_at(record, 4).ok_or("short header")? as usize;
    let usa_count = u16_at(record, 6).ok_or("short header")? as usize;
    if usa_count < 2 {
        return Err("bad update sequence count".into());
    }
    let strides = usa_count - 1;
    if strides * bytes_per_sector != record.len() {
        return Err(format!(
            "update sequence count {usa_count} does not match record size {}",
            record.len()
        ));
    }
    if usa_off < 8 || usa_off + usa_count * 2 > bytes_per_sector.min(record.len()) {
        return Err("update sequence array out of bounds".into());
    }
    let usn = [record[usa_off], record[usa_off + 1]];
    for i in 0..strides {
        let end = (i + 1) * bytes_per_sector;
        if record[end - 2..end] != usn {
            return Err(format!("torn record: stride {i} does not end with the USN"));
        }
        let src = usa_off + 2 * (i + 1);
        record[end - 2] = record[src];
        record[end - 1] = record[src + 1];
    }
    Ok(())
}

// ---- runlist ------------------------------------------------------------

/// Decode a non-resident runlist into `(lcn, clusters)` pairs; `lcn == -1`
/// marks a sparse run. Stops at the 0 terminator (or the end of `data`).
pub fn decode_runlist(data: &[u8]) -> Result<Vec<(i64, u64)>, String> {
    let mut runs = Vec::new();
    let mut pos = 0usize;
    let mut lcn: i64 = 0;
    while pos < data.len() {
        let h = data[pos];
        if h == 0 {
            break;
        }
        pos += 1;
        let len_sz = (h & 0xF) as usize;
        let off_sz = (h >> 4) as usize;
        if len_sz == 0 || len_sz > 8 || off_sz > 8 {
            return Err(format!("bad runlist header byte {h:#04x}"));
        }
        let end = pos + len_sz + off_sz;
        if end > data.len() {
            return Err("truncated runlist".into());
        }
        let mut len = 0u64;
        for (i, &b) in data[pos..pos + len_sz].iter().enumerate() {
            len |= (b as u64) << (8 * i);
        }
        if len == 0 {
            return Err("zero-length run".into());
        }
        pos += len_sz;
        if off_sz == 0 {
            runs.push((-1, len));
        } else {
            let mut raw = 0u64;
            for (i, &b) in data[pos..pos + off_sz].iter().enumerate() {
                raw |= (b as u64) << (8 * i);
            }
            let shift = 64 - 8 * off_sz as u32;
            let delta = ((raw << shift) as i64) >> shift;
            lcn = lcn.checked_add(delta).ok_or("runlist LCN overflow")?;
            if lcn < 0 {
                return Err("negative LCN in runlist".into());
            }
            runs.push((lcn, len));
        }
        pos = end;
    }
    Ok(runs)
}

// ---- attribute walking --------------------------------------------------

struct Attr<'a> {
    ty: u32,
    non_resident: bool,
    name_len: u8,
    raw: &'a [u8],
}

impl<'a> Attr<'a> {
    fn resident_value(&self) -> Option<&'a [u8]> {
        if self.non_resident {
            return None;
        }
        let len = u32_at(self.raw, 0x10)? as usize;
        let off = u16_at(self.raw, 0x14)? as usize;
        self.raw.get(off..off.checked_add(len)?)
    }
    fn start_vcn(&self) -> u64 {
        u64_at(self.raw, 0x10).unwrap_or(0)
    }
    fn alloc_size(&self) -> u64 {
        u64_at(self.raw, 0x28).unwrap_or(0)
    }
    fn data_size(&self) -> u64 {
        u64_at(self.raw, 0x30).unwrap_or(0)
    }
    /// On-disk size of a compressed/sparse stream (0x40), else the allocated size.
    fn disk_alloc(&self) -> u64 {
        let unit = u16_at(self.raw, 0x22).unwrap_or(0);
        if unit != 0 && self.raw.len() >= 0x48 {
            u64_at(self.raw, 0x40).unwrap_or(0)
        } else {
            self.alloc_size()
        }
    }
    fn runlist(&self) -> Result<Vec<(i64, u64)>, String> {
        let off = u16_at(self.raw, 0x20).ok_or("short attribute")? as usize;
        if off < 0x40 || off > self.raw.len() {
            return Err("bad runlist offset".into());
        }
        decode_runlist(&self.raw[off..])
    }
}

struct AttrIter<'a> {
    rec: &'a [u8],
    off: usize,
    used: usize,
    done: bool,
}

impl<'a> AttrIter<'a> {
    fn new(rec: &'a [u8]) -> Result<Self, String> {
        if rec.len() < 0x30 || &rec[..4] != FILE_MAGIC {
            return Err("bad magic".into());
        }
        let used = (u32_at(rec, 0x18).ok_or("short header")? as usize).min(rec.len());
        let off = u16_at(rec, 0x14).ok_or("short header")? as usize;
        if off < 0x18 || off > used {
            return Err("bad first attribute offset".into());
        }
        Ok(AttrIter {
            rec,
            off,
            used,
            done: false,
        })
    }
}

impl<'a> Iterator for AttrIter<'a> {
    type Item = Result<Attr<'a>, String>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let fail = |s: &mut Self, m: &str| {
            s.done = true;
            Some(Err(m.to_string()))
        };
        let Some(ty) = u32_at(self.rec, self.off).filter(|_| self.off + 4 <= self.used) else {
            return fail(self, "attributes not terminated");
        };
        if ty == ATTR_END {
            self.done = true;
            return None;
        }
        let len = u32_at(self.rec, self.off + 4).unwrap_or(0) as usize;
        if len < 0x10 || !len.is_multiple_of(8) || self.off + len > self.used {
            return fail(self, "bad attribute length");
        }
        let raw = &self.rec[self.off..self.off + len];
        let non_resident = raw[8] != 0;
        if (non_resident && len < 0x40) || (!non_resident && len < 0x18) {
            return fail(self, "attribute shorter than its header");
        }
        self.off += len;
        Some(Ok(Attr {
            ty,
            non_resident,
            name_len: raw[9],
            raw,
        }))
    }
}

// ---- record parsing -----------------------------------------------------

/// Attributes of one in-use FILE record that matter for sizing. A file whose
/// attributes overflow its base record keeps an `$ATTRIBUTE_LIST` there and spills
/// further attributes (extra hardlink names, the `$DATA` header) into extension
/// records, which point back at the base through `base`.
struct Parts {
    seq: u64,
    /// Base record index for an extension record, 0 for a base record.
    base: u64,
    is_dir: bool,
    names: Vec<(u64, OsString, bool)>, // parent, name, is_win32
    data: Option<(u64, u64)>,          // size, allocated
    has_list: bool,
    reparse: bool,
}

impl Parts {
    /// Fold an extension record's attributes into its base record.
    fn merge(&mut self, ext: Parts) {
        self.names.extend(ext.names);
        if self.data.is_none() {
            self.data = ext.data;
        }
        self.reparse |= ext.reparse;
    }
}

fn parse_parts(record: &[u8]) -> Result<Option<Parts>, String> {
    if record.len() < 0x30 {
        return Err("record too small".into());
    }
    if &record[..4] != FILE_MAGIC {
        return Err(format!("bad magic {:02x?}", &record[..4]));
    }
    let flags = u16_at(record, 0x16).ok_or("short header")?;
    if flags & FLAG_IN_USE == 0 {
        return Ok(None);
    }
    let mut p = Parts {
        seq: u16_at(record, 0x10).ok_or("short header")? as u64,
        base: u64_at(record, 0x20).ok_or("short header")? & 0x0000_FFFF_FFFF_FFFF,
        is_dir: flags & FLAG_DIR != 0,
        names: Vec::new(),
        data: None,
        has_list: false,
        reparse: false,
    };
    for a in AttrIter::new(record)? {
        let a = a?;
        match a.ty {
            ATTR_FILE_NAME => {
                let v = a.resident_value().ok_or("non-resident $FILE_NAME")?;
                let nlen = *v.get(0x40).ok_or("short $FILE_NAME")? as usize;
                let ns = *v.get(0x41).ok_or("short $FILE_NAME")?;
                let raw = v
                    .get(0x42..0x42 + nlen * 2)
                    .ok_or("truncated $FILE_NAME name")?;
                if ns == NS_DOS {
                    continue;
                }
                // Every non-DOS name is a separate hardlink (a WIN32 name's 8.3 alias
                // is the DOS-only attribute skipped above).
                let wide: Vec<u16> = (0..nlen)
                    .map(|i| u16::from_le_bytes([raw[2 * i], raw[2 * i + 1]]))
                    .collect();
                let parent = u64_at(v, 0).ok_or("short $FILE_NAME")?;
                p.names.push((parent, OsString::from_wide(&wide), ns != 0));
            }
            ATTR_DATA if a.name_len == 0 && p.data.is_none() => {
                if a.non_resident {
                    // Only the first extent (VCN 0) carries the stream's sizes.
                    if a.start_vcn() == 0 {
                        p.data = Some((a.data_size(), a.disk_alloc()));
                    }
                } else {
                    let v = a.resident_value().ok_or("bad resident $DATA")?;
                    p.data = Some((v.len() as u64, 0));
                }
            }
            ATTR_ATTRIBUTE_LIST => p.has_list = true,
            ATTR_REPARSE_POINT => p.reparse = true,
            _ => {}
        }
    }
    Ok(Some(p))
}

/// Turn a (merged) base record into an entry; `None` when it has no usable name.
fn build_entry(mut p: Parts, index: u64) -> Option<MftEntry> {
    // Primary name: the first WIN32 one, else the first POSIX one.
    let first = p
        .names
        .iter()
        .position(|n| n.2)
        .or((!p.names.is_empty()).then_some(0))?;
    let links = p.names.len().min(u16::MAX as usize) as u16;
    let (parent, name, _) = p.names.swap_remove(first);
    let other_names = p.names.into_iter().map(|(pr, n, _)| (pr, n)).collect();
    let (size, alloc) = if p.is_dir {
        (0, 0)
    } else {
        p.data.unwrap_or((0, 0))
    };
    Some(MftEntry {
        frn: (index & 0x0000_FFFF_FFFF_FFFF) | (p.seq << 48),
        parent,
        name,
        is_dir: p.is_dir,
        size,
        alloc,
        links,
        reparse: p.reparse,
        approx: p.has_list && !p.is_dir && p.data.is_none(),
        other_names,
    })
}

/// Parse one fixed-up FILE record on its own. `Ok(None)` for records that are not in
/// use, extension records (non-zero base reference) and records with no usable name.
/// Attributes that live in extension records are only merged by [`scan_volume`].
pub fn parse_record(record: &[u8], index: u64) -> Result<Option<MftEntry>, String> {
    let Some(p) = parse_parts(record)? else {
        return Ok(None);
    };
    if p.base != 0 {
        return Ok(None);
    }
    Ok(build_entry(p, index))
}

// ---- volume scan --------------------------------------------------------

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Raw volume reads must start and end on sector boundaries; 4 KiB covers both
/// 512-byte and 4Kn sectors. Unaligned requests go through an aligned bounce buffer
/// (a short read at the end of a plain file is fine as long as `buf` is covered).
const IO_ALIGN: u64 = 4096;

fn read_exact_at(f: &File, off: u64, buf: &mut [u8]) -> io::Result<()> {
    let end = off + buf.len() as u64;
    if off.is_multiple_of(IO_ALIGN)
        && end.is_multiple_of(IO_ALIGN)
        && (buf.as_ptr() as usize).is_multiple_of(IO_ALIGN as usize)
    {
        let n = read_at(f, off, buf)?;
        return if n == buf.len() {
            Ok(())
        } else {
            Err(io::Error::from(io::ErrorKind::UnexpectedEof))
        };
    }
    let start = off / IO_ALIGN * IO_ALIGN;
    let mut store = Vec::new();
    let tmp = aligned_mut(
        &mut store,
        (end.next_multiple_of(IO_ALIGN) - start) as usize,
    );
    let got = read_at(f, start, tmp)?;
    let lo = (off - start) as usize;
    if got < lo + buf.len() {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
    }
    buf.copy_from_slice(&tmp[lo..lo + buf.len()]);
    Ok(())
}

/// A `len`-byte window of `v` that starts on an `IO_ALIGN` boundary.
fn aligned_mut(v: &mut Vec<u8>, len: usize) -> &mut [u8] {
    let a = IO_ALIGN as usize;
    v.resize(len + a, 0);
    let skip = (a - v.as_ptr() as usize % a) % a;
    &mut v[skip..skip + len]
}

/// Positional read until `buf` is full or EOF; returns the bytes read.
fn read_at(f: &File, mut off: u64, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match f.seek_read(&mut buf[got..], off) {
            Ok(0) => break,
            Ok(n) => {
                got += n;
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// Read `buf.len()` bytes at logical `offset` of a file described by `runs`.
fn read_mapped(
    f: &File,
    runs: &[(i64, u64)],
    bpc: u64,
    mut offset: u64,
    mut buf: &mut [u8],
) -> io::Result<()> {
    for &(lcn, clusters) in runs {
        let run_bytes = clusters * bpc;
        if offset >= run_bytes {
            offset -= run_bytes;
            continue;
        }
        if lcn < 0 {
            return Err(invalid("read from sparse MFT run"));
        }
        let n = ((run_bytes - offset) as usize).min(buf.len());
        let (head, tail) = buf.split_at_mut(n);
        read_exact_at(f, lcn as u64 * bpc + offset, head)?;
        buf = tail;
        offset = 0;
        if buf.is_empty() {
            return Ok(());
        }
    }
    Err(invalid("read beyond the mapped MFT runs"))
}

/// Runs and data size of `$MFT`'s unnamed `$DATA`, gathered from record 0 and,
/// when `$ATTRIBUTE_LIST` is present, from the extension records it names.
fn mft_extents(
    f: &File,
    rec0: &[u8],
    bpc: u64,
    rec_size: u64,
) -> io::Result<(Vec<(i64, u64)>, u64)> {
    // Unnamed $DATA extents keyed by start VCN; joined in VCN order at the end.
    let mut extents: Vec<(u64, Vec<(i64, u64)>)> = Vec::new();
    let mut size = None;
    let mut list: Option<Vec<u8>> = None;
    for a in AttrIter::new(rec0).map_err(invalid)? {
        let a = a.map_err(invalid)?;
        if a.ty == ATTR_DATA && a.name_len == 0 && a.non_resident {
            if a.start_vcn() == 0 {
                size = Some(a.data_size());
            }
            extents.push((a.start_vcn(), a.runlist().map_err(invalid)?));
        } else if a.ty == ATTR_ATTRIBUTE_LIST {
            list = Some(match a.resident_value() {
                Some(v) => v.to_vec(),
                // Large or fragmented volumes keep $MFT's attribute list in its own
                // clusters; raw volume reads must be sector multiples, so read whole
                // clusters and truncate to the list's data size.
                None => read_non_resident(f, &a, bpc)?,
            });
        }
    }
    let size = size.ok_or_else(|| invalid("$MFT has no non-resident $DATA"))?;
    if let Some(list) = list {
        let mut exts: Vec<(u64, u64)> = Vec::new(); // (start vcn, record index)
        let mut o = 0usize;
        while o + 0x1A <= list.len() {
            let ty = u32_at(&list, o).unwrap_or(0);
            let len = u16_at(&list, o + 4).unwrap_or(0) as usize;
            if len < 0x1A || o + len > list.len() {
                return Err(invalid("bad $ATTRIBUTE_LIST entry"));
            }
            let vcn = u64_at(&list, o + 8).unwrap_or(0);
            let fref = u64_at(&list, o + 0x10).unwrap_or(0) & 0x0000_FFFF_FFFF_FFFF;
            // fref 0 is record 0 itself, whose extents were collected above.
            if ty == ATTR_DATA && list[o + 6] == 0 && vcn > 0 && fref != 0 {
                exts.push((vcn, fref));
            }
            o += len;
        }
        exts.sort_unstable();
        exts.dedup();
        for (vcn, idx) in exts {
            if extents.iter().any(|(v, _)| *v == vcn) {
                continue;
            }
            // Extension records sit in already-known extents (low VCNs come first).
            let known = join_extents(&mut extents.clone())?;
            let mut rec = vec![0u8; rec_size as usize];
            read_mapped(f, &known, bpc, idx * rec_size, &mut rec)?;
            apply_fixup(&mut rec, FIXUP_STRIDE).map_err(invalid)?;
            let mut found = false;
            for a in AttrIter::new(&rec).map_err(invalid)? {
                let a = a.map_err(invalid)?;
                if a.ty == ATTR_DATA && a.name_len == 0 && a.non_resident && a.start_vcn() == vcn {
                    extents.push((vcn, a.runlist().map_err(invalid)?));
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(invalid(
                    "$MFT extension record lacks the listed $DATA extent",
                ));
            }
        }
    }
    Ok((join_extents(&mut extents)?, size))
}

/// Concatenate extents in VCN order, checking that each starts where the last ended.
fn join_extents(extents: &mut [(u64, Vec<(i64, u64)>)]) -> io::Result<Vec<(i64, u64)>> {
    extents.sort_by_key(|(vcn, _)| *vcn);
    let mut runs = Vec::new();
    let mut next_vcn = 0u64;
    for (vcn, r) in extents.iter() {
        if *vcn != next_vcn {
            return Err(invalid(format!(
                "$MFT extents not contiguous: expected VCN {next_vcn}, found {vcn}"
            )));
        }
        next_vcn += r.iter().map(|&(_, c)| c).sum::<u64>();
        runs.extend_from_slice(r);
    }
    Ok(runs)
}

/// Read a (small) non-resident attribute's value, e.g. `$MFT`'s `$ATTRIBUTE_LIST`.
fn read_non_resident(f: &File, a: &Attr<'_>, bpc: u64) -> io::Result<Vec<u8>> {
    const MAX: u64 = 16 << 20;
    let size = a.data_size();
    if size > MAX {
        return Err(invalid("implausibly large $MFT attribute list"));
    }
    let runs = a.runlist().map_err(invalid)?;
    let mapped: u64 = runs.iter().map(|&(_, c)| c * bpc).sum();
    let whole = size.next_multiple_of(bpc);
    if whole > mapped {
        return Err(invalid("$MFT attribute list larger than its runs"));
    }
    let mut v = vec![0u8; whole as usize];
    read_mapped(f, &runs, bpc, 0, &mut v)?;
    v.truncate(size as usize);
    Ok(v)
}

/// Per-worker parse state; workers' results are folded together with `absorb`.
#[derive(Default)]
struct Scanner {
    stats: MftStats,
    out: Vec<MftEntry>,
    /// Base records with an `$ATTRIBUTE_LIST`, held until every extension is read.
    held: HashMap<u64, Parts>,
    /// Extension records by base index (an extension may precede its base).
    exts: HashMap<u64, Vec<Parts>>,
}

impl Scanner {
    /// Parse one raw record in place (the fixup rewrites it).
    fn process(&mut self, rec: &mut [u8], index: u64) {
        self.stats.records += 1;
        if rec[..4] == [0, 0, 0, 0] {
            return; // never-used slot
        }
        match apply_fixup(rec, FIXUP_STRIDE).and_then(|_| parse_parts(rec)) {
            Ok(Some(p)) if p.base != 0 => self.exts.entry(p.base).or_default().push(p),
            Ok(Some(p)) if p.has_list => {
                self.held.insert(index, p);
            }
            Ok(Some(p)) => self.emit(p, index),
            Ok(None) => {}
            Err(_) => self.stats.parse_errors += 1,
        }
    }

    fn emit(&mut self, p: Parts, index: u64) {
        if let Some(e) = build_entry(p, index) {
            self.stats.in_use += 1;
            if e.approx {
                self.stats.approx_records += 1;
            }
            self.out.push(e);
        }
    }

    fn absorb(&mut self, o: Scanner) {
        self.stats.records += o.stats.records;
        self.stats.in_use += o.stats.in_use;
        self.stats.parse_errors += o.stats.parse_errors;
        self.stats.approx_records += o.stats.approx_records;
        if self.out.is_empty() {
            self.out = o.out;
        } else {
            self.out.extend(o.out);
        }
        self.held.extend(o.held);
        for (k, v) in o.exts {
            self.exts.entry(k).or_default().extend(v);
        }
    }

    /// Merge held base records with their extension records and emit them.
    /// Extensions whose base is not in use are orphans and are dropped.
    fn finish(&mut self) {
        let mut held: Vec<(u64, Parts)> = std::mem::take(&mut self.held).into_iter().collect();
        held.sort_unstable_by_key(|(i, _)| *i);
        for (index, mut p) in held {
            for ext in self.exts.remove(&index).unwrap_or_default() {
                p.merge(ext);
            }
            self.emit(p, index);
        }
        self.exts.clear();
    }
}

/// Threads that read and parse `$MFT` chunks.
fn parse_workers() -> usize {
    std::thread::available_parallelism()
        .map_or(4, |n| n.get())
        .clamp(1, 16)
}

/// Logical `(offset, len)` reads covering every whole record of the `$MFT` stream:
/// record aligned, at most `CHUNK` bytes, never touching a sparse run (a record
/// split by one is dropped, as it cannot be read).
fn read_jobs(runs: &[(i64, u64)], bpc: u64, data_size: u64, rec_size: u64) -> Vec<(u64, usize)> {
    let step = (CHUNK as u64 / rec_size).max(1) * rec_size;
    let mut jobs = Vec::new();
    let mut split = |a: u64, b: u64| {
        let (mut a, b) = (a.next_multiple_of(rec_size), b / rec_size * rec_size);
        while a < b {
            let n = step.min(b - a);
            jobs.push((a, n as usize));
            a += n;
        }
    };
    let (mut logical, mut range_start) = (0u64, None);
    for &(lcn, clusters) in runs {
        if logical >= data_size {
            break;
        }
        let end = (logical + clusters * bpc).min(data_size);
        match (lcn < 0, range_start) {
            (true, Some(a)) => {
                split(a, logical);
                range_start = None;
            }
            (false, None) => range_start = Some(logical),
            _ => {}
        }
        logical = end;
    }
    if let Some(a) = range_start {
        split(a, logical);
    }
    jobs
}

/// Read every in-use record of the volume's `$MFT`. Needs admin. Per-record
/// parse errors are counted in the stats, I/O errors abort.
pub fn scan_volume(letter: char, f: &mut dyn FnMut(MftEntry)) -> io::Result<MftStats> {
    if !letter.is_ascii_alphabetic() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "drive letter expected",
        ));
    }
    let start = Instant::now();
    let dev = format!(r"\\.\{}:", letter.to_ascii_uppercase());
    let dev = sys::wide_plain(OsStr::new(&dev));
    let open = || sys::open_wide(&dev, sys::GENERIC_READ, FILE_FLAG_NO_BUFFERING);
    let handle = open()?;

    let mut vd = [0u64; 16]; // 128 bytes, 8-aligned, >= sizeof(NtfsVolumeData)
    let mut ret = 0u32;
    // SAFETY: handle is a valid volume handle; the out buffer is 128 writable bytes
    // and the size passed matches it; no input buffer or overlapped is used.
    let ok = unsafe {
        sys::DeviceIoControl(
            handle.as_raw_handle() as sys::HANDLE,
            FSCTL_GET_NTFS_VOLUME_DATA,
            std::ptr::null(),
            0,
            vd.as_mut_ptr() as *mut c_void,
            std::mem::size_of_val(&vd) as u32,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    sys::check(ok)?;
    if (ret as usize) < std::mem::size_of::<NtfsVolumeData>() {
        return Err(invalid("short NTFS_VOLUME_DATA_BUFFER"));
    }
    // SAFETY: the buffer is 8-aligned, at least size_of::<NtfsVolumeData>() bytes, and
    // the kernel initialised `ret` >= that many bytes; every field is a plain integer.
    let v: NtfsVolumeData = unsafe { std::ptr::read(vd.as_ptr() as *const NtfsVolumeData) };

    let bpc = v.bytes_per_cluster as u64;
    let rec_size = v.bytes_per_file_record_segment as u64;
    if !bpc.is_power_of_two() || !rec_size.is_power_of_two() || !(512..=65536).contains(&rec_size) {
        return Err(invalid("implausible NTFS geometry"));
    }
    if v.mft_start_lcn < 0 {
        return Err(invalid("negative MftStartLcn"));
    }

    let file = File::from(handle);
    let mut rec0 = vec![0u8; rec_size as usize];
    let rec0_off = v.mft_start_lcn as u64 * bpc;
    read_exact_at(&file, rec0_off, &mut rec0).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("reading $MFT record 0 ({rec_size} bytes at {rec0_off}, cluster {bpc}): {e}"),
        )
    })?;
    apply_fixup(&mut rec0, FIXUP_STRIDE).map_err(|e| invalid(format!("$MFT record 0: {e}")))?;
    let (runs, data_size) = mft_extents(&file, &rec0, bpc, rec_size)
        .map_err(|e| io::Error::new(e.kind(), format!("mapping $MFT extents: {e}")))?;

    let rs = rec_size as usize;
    let jobs = read_jobs(&runs, bpc, data_size, rec_size);
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let failed: Mutex<Option<io::Error>> = Mutex::new(None);
    // Each worker reads record-aligned chunks through its own handle (synchronous I/O
    // on one handle is serialised) and parses them, so reads overlap.
    let mut files = vec![file];
    for _ in 1..parse_workers() {
        files.push(File::from(open()?));
    }
    let (jobs, runs, next, stop, failed) = (&jobs, &runs, &next, &stop, &failed);
    let done: Vec<Scanner> = std::thread::scope(|s| {
        let handles: Vec<_> = files
            .into_iter()
            .map(|file| {
                s.spawn(move || {
                    let mut sc = Scanner::default();
                    let mut store = Vec::new();
                    while !stop.load(Relaxed) {
                        let Some(&(off, len)) = jobs.get(next.fetch_add(1, Relaxed)) else {
                            break;
                        };
                        let buf = aligned_mut(&mut store, len);
                        if let Err(e) = read_mapped(&file, runs, bpc, off, buf) {
                            stop.store(true, Relaxed);
                            let e = io::Error::new(
                                e.kind(),
                                format!("reading $MFT at offset {off} ({len} bytes): {e}"),
                            );
                            // Poisoning is impossible: nothing panics while holding it.
                            failed
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .get_or_insert(e);
                            break;
                        }
                        let first = off / rec_size;
                        for (i, rec) in buf.chunks_exact_mut(rs).enumerate() {
                            sc.process(rec, first + i as u64);
                        }
                    }
                    sc
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
            .collect()
    });
    if let Some(e) = failed.lock().unwrap_or_else(|p| p.into_inner()).take() {
        return Err(e);
    }
    let mut sc = Scanner::default();
    for w in done {
        sc.absorb(w);
    }
    sc.finish();
    sc.stats.elapsed = start.elapsed();
    for e in std::mem::take(&mut sc.out) {
        f(e);
    }
    Ok(sc.stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    const REC: usize = 1024;

    fn pad8(v: &mut Vec<u8>) {
        while !v.len().is_multiple_of(8) {
            v.push(0);
        }
    }

    fn resident(ty: u32, value: &[u8]) -> Vec<u8> {
        let mut a = vec![0u8; 0x18];
        a[0..4].copy_from_slice(&ty.to_le_bytes());
        a[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        a[0x14..0x16].copy_from_slice(&0x18u16.to_le_bytes());
        a.extend_from_slice(value);
        pad8(&mut a);
        let len = a.len() as u32;
        a[4..8].copy_from_slice(&len.to_le_bytes());
        a
    }

    fn non_resident(ty: u32, runlist: &[u8], alloc: u64, size: u64, comp: Option<u64>) -> Vec<u8> {
        let hdr = if comp.is_some() { 0x48 } else { 0x40 };
        let mut a = vec![0u8; hdr];
        a[0..4].copy_from_slice(&ty.to_le_bytes());
        a[8] = 1;
        a[0x20..0x22].copy_from_slice(&(hdr as u16).to_le_bytes());
        if let Some(c) = comp {
            a[0x22..0x24].copy_from_slice(&4u16.to_le_bytes());
            a[0x40..0x48].copy_from_slice(&c.to_le_bytes());
        }
        a[0x28..0x30].copy_from_slice(&alloc.to_le_bytes());
        a[0x30..0x38].copy_from_slice(&size.to_le_bytes());
        a[0x38..0x40].copy_from_slice(&size.to_le_bytes());
        a.extend_from_slice(runlist);
        pad8(&mut a);
        let len = a.len() as u32;
        a[4..8].copy_from_slice(&len.to_le_bytes());
        a
    }

    fn file_name(parent: u64, name: &str, ns: u8) -> Vec<u8> {
        let w: Vec<u16> = name.encode_utf16().collect();
        let mut v = vec![0u8; 0x42];
        v[0..8].copy_from_slice(&parent.to_le_bytes());
        v[0x40] = w.len() as u8;
        v[0x41] = ns;
        for c in w {
            v.extend_from_slice(&c.to_le_bytes());
        }
        resident(ATTR_FILE_NAME, &v)
    }

    fn build(flags: u16, seq: u16, base: u64, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut r = vec![0u8; REC];
        r[0..4].copy_from_slice(b"FILE");
        r[4..6].copy_from_slice(&0x30u16.to_le_bytes());
        r[6..8].copy_from_slice(&3u16.to_le_bytes());
        r[0x10..0x12].copy_from_slice(&seq.to_le_bytes());
        r[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes());
        r[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        r[0x1C..0x20].copy_from_slice(&(REC as u32).to_le_bytes());
        r[0x20..0x28].copy_from_slice(&base.to_le_bytes());
        let mut o = 0x38;
        for a in attrs {
            r[o..o + a.len()].copy_from_slice(a);
            o += a.len();
        }
        r[o..o + 4].copy_from_slice(&ATTR_END.to_le_bytes());
        o += 8;
        r[0x18..0x1C].copy_from_slice(&(o as u32).to_le_bytes());
        // protect: USN 0x0007
        r[0x30..0x32].copy_from_slice(&7u16.to_le_bytes());
        for i in 0..2 {
            let end = (i + 1) * 512;
            let (a, b) = (r[end - 2], r[end - 1]);
            r[0x32 + 2 * i] = a;
            r[0x33 + 2 * i] = b;
            r[end - 2..end].copy_from_slice(&7u16.to_le_bytes());
        }
        r
    }

    fn parse(mut r: Vec<u8>) -> Result<Option<MftEntry>, String> {
        apply_fixup(&mut r, 512)?;
        parse_record(&r, 42)
    }

    #[test]
    fn struct_layout_matches_winioctl() {
        assert_eq!(size_of::<NtfsVolumeData>(), 96);
        assert_eq!(offset_of!(NtfsVolumeData, bytes_per_sector), 40);
        assert_eq!(offset_of!(NtfsVolumeData, bytes_per_cluster), 44);
        assert_eq!(
            offset_of!(NtfsVolumeData, bytes_per_file_record_segment),
            48
        );
        assert_eq!(
            offset_of!(NtfsVolumeData, clusters_per_file_record_segment),
            52
        );
        assert_eq!(offset_of!(NtfsVolumeData, mft_valid_data_length), 56);
        assert_eq!(offset_of!(NtfsVolumeData, mft_start_lcn), 64);
        assert_eq!(offset_of!(NtfsVolumeData, mft_zone_end), 88);
        assert_eq!(FSCTL_GET_NTFS_VOLUME_DATA, 0x0009_0064);
    }

    #[test]
    fn fixup_ok_restores_bytes() {
        let mut r = build(1, 1, 0, &[file_name(5, "a", 1)]);
        // Original tail bytes were zero; plant markers and re-protect by hand.
        r[510] = 7;
        r[1022] = 7;
        apply_fixup(&mut r, 512).unwrap();
        assert_eq!(&r[510..512], &[0, 0]);
        assert_eq!(&r[1022..1024], &[0, 0]);
    }

    #[test]
    fn fixup_torn_and_bad_geometry() {
        let mut r = build(1, 1, 0, &[]);
        r[1022] = 9;
        assert!(apply_fixup(&mut r, 512).unwrap_err().contains("torn"));
        let mut r = build(1, 1, 0, &[]);
        r[6] = 5; // wrong count
        assert!(apply_fixup(&mut r, 512).is_err());
        let mut r = build(1, 1, 0, &[]);
        r[4] = 0xFF;
        r[5] = 0xFF;
        assert!(apply_fixup(&mut r, 512).is_err());
        assert!(apply_fixup(&mut [0u8; 4], 512).is_err());
    }

    #[test]
    fn bad_magic_and_not_in_use() {
        let mut r = build(1, 1, 0, &[file_name(5, "a", 1)]);
        r[0..4].copy_from_slice(b"BAAD");
        assert!(parse(r).is_err());
        assert!(parse(build(0, 1, 0, &[file_name(5, "a", 1)]))
            .unwrap()
            .is_none());
    }

    #[test]
    fn resident_data() {
        let e = parse(build(
            1,
            3,
            0,
            &[
                file_name(5 | (2 << 48), "hello.txt", 1),
                resident(ATTR_DATA, b"12345"),
            ],
        ))
        .unwrap()
        .unwrap();
        assert_eq!(e.name, "hello.txt");
        assert_eq!(e.size, 5);
        assert_eq!(e.alloc, 0);
        assert!(!e.is_dir && !e.reparse && !e.approx);
        assert_eq!(e.parent, 5 | (2 << 48));
        assert_eq!(e.frn, 42 | (3 << 48));
        assert_eq!(e.links, 1);
    }

    #[test]
    fn non_resident_data_and_compressed() {
        let e = parse(build(
            1,
            1,
            0,
            &[
                file_name(5, "big.bin", 3),
                non_resident(ATTR_DATA, &[0x11, 0x10, 0x20, 0], 1 << 20, 1_000_000, None),
            ],
        ))
        .unwrap()
        .unwrap();
        assert_eq!((e.size, e.alloc), (1_000_000, 1 << 20));
        let e = parse(build(
            1,
            1,
            0,
            &[
                file_name(5, "c.bin", 1),
                non_resident(ATTR_DATA, &[0], 1 << 20, 1_000_000, Some(65536)),
            ],
        ))
        .unwrap()
        .unwrap();
        assert_eq!((e.size, e.alloc), (1_000_000, 65536));
    }

    #[test]
    fn dos_name_skipped_and_win32_preferred() {
        let e = parse(build(
            1,
            1,
            0,
            &[
                file_name(5, "LONGNA~1.TXT", 2),
                file_name(9, "LongName.txt", 1),
            ],
        ))
        .unwrap()
        .unwrap();
        assert_eq!(e.name, "LongName.txt");
        assert_eq!(e.parent, 9);
        assert_eq!(e.links, 1);
        // POSIX first then Win32: Win32 wins; two names -> links 2.
        let e = parse(build(
            1,
            1,
            0,
            &[file_name(4, "posix", 0), file_name(6, "win", 1)],
        ))
        .unwrap()
        .unwrap();
        assert_eq!((e.name.to_str().unwrap(), e.parent, e.links), ("win", 6, 2));
        // DOS-only: no usable name.
        assert!(parse(build(1, 1, 0, &[file_name(5, "X~1", 2)]))
            .unwrap()
            .is_none());
    }

    #[test]
    fn dir_reparse_and_unicode() {
        let e = parse(build(
            3,
            1,
            0,
            &[
                file_name(5, "d\u{e9}\u{1F600}", 1),
                resident(ATTR_REPARSE_POINT, &[0; 8]),
            ],
        ))
        .unwrap()
        .unwrap();
        assert!(e.is_dir && e.reparse);
        assert_eq!((e.size, e.alloc), (0, 0));
        assert_eq!(e.name, "d\u{e9}\u{1F600}");
    }

    #[test]
    fn extension_record_is_none() {
        assert!(parse(build(1, 1, 99, &[file_name(5, "x", 1)]))
            .unwrap()
            .is_none());
    }

    #[test]
    fn attribute_list_is_flagged() {
        let list = resident(ATTR_ATTRIBUTE_LIST, &[0; 32]);
        let e = parse(build(1, 1, 0, &[file_name(5, "frag", 1), list.clone()]))
            .unwrap()
            .unwrap();
        assert!(e.approx);
        let e = parse(build(
            1,
            1,
            0,
            &[
                file_name(5, "frag", 1),
                list,
                non_resident(ATTR_DATA, &[0], 4096, 100, None),
            ],
        ))
        .unwrap()
        .unwrap();
        assert!(!e.approx);
        assert_eq!(e.size, 100);
    }

    #[test]
    fn named_data_stream_ignored() {
        let mut named = resident(ATTR_DATA, b"zzzzzzzzzz");
        named[9] = 1; // name length != 0 => alternate stream
        let e = parse(build(
            1,
            1,
            0,
            &[file_name(5, "s", 1), named, resident(ATTR_DATA, b"ab")],
        ))
        .unwrap()
        .unwrap();
        assert_eq!(e.size, 2);
    }

    #[test]
    fn malformed_attributes_error_not_panic() {
        let mut r = build(1, 1, 0, &[file_name(5, "a", 1)]);
        r[0x3C..0x40].copy_from_slice(&0xFFFF_0000u32.to_le_bytes()); // huge length
        assert!(parse(r).is_err());
        let mut r = build(1, 1, 0, &[file_name(5, "a", 1)]);
        r[0x38 + 0x18 + 0x40] = 255; // name length beyond value
        assert!(parse(r).is_err());
    }

    #[test]
    fn runlist_cases() {
        // 0x21: 1-byte len, 2-byte offset
        assert_eq!(
            decode_runlist(&[0x21, 0x18, 0x34, 0x56, 0]).unwrap(),
            vec![(0x5634, 0x18)]
        );
        // positive then negative delta
        assert_eq!(
            decode_runlist(&[0x11, 0x04, 0x10, 0x11, 0x02, 0xF8, 0]).unwrap(),
            vec![(16, 4), (8, 2)]
        );
        // sparse run between
        assert_eq!(
            decode_runlist(&[0x11, 0x01, 0x20, 0x01, 0x05, 0x11, 0x03, 0x04, 0]).unwrap(),
            vec![(32, 1), (-1, 5), (36, 3)]
        );
        // 2-byte length
        assert_eq!(
            decode_runlist(&[0x12, 0x00, 0x01, 0x10, 0]).unwrap(),
            vec![(16, 256)]
        );
        assert_eq!(decode_runlist(&[0]).unwrap(), vec![]);
        // errors
        assert!(decode_runlist(&[0x21, 0x01]).is_err());
        assert!(decode_runlist(&[0x01, 0x00]).is_err()); // zero length
        assert!(decode_runlist(&[0x0F, 0x01]).is_err()); // len_sz 15
        assert!(decode_runlist(&[0x11, 0x01, 0xF0, 0]).is_err()); // negative LCN
    }

    #[test]
    fn mapped_read_over_file() {
        let dir = std::env::temp_dir().join(format!("dusage_mft_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("img.bin");
        let img: Vec<u8> = (0..64u32).flat_map(|i| [i as u8; 16]).collect(); // 1024 bytes, 16-byte "clusters"
        std::fs::write(&p, &img).unwrap();
        let f = File::open(&p).unwrap();
        let runs = [(10i64, 2u64), (-1, 1), (2, 1)];
        let mut b = [0u8; 8];
        read_mapped(&f, &runs, 16, 24, &mut b).unwrap();
        assert_eq!(b, [11; 8]);
        assert!(read_mapped(&f, &runs, 16, 28, &mut b).is_err()); // runs into sparse
        let mut c = [0u8; 16];
        assert!(read_mapped(&f, &runs, 16, 32, &mut c).is_err()); // sparse
        read_mapped(&f, &runs, 16, 48, &mut c).unwrap();
        assert_eq!(c, [2u8; 16]);
        let adj = [(10i64, 2u64), (2, 1)];
        read_mapped(&f, &adj, 16, 28, &mut b).unwrap();
        assert_eq!(b, [11, 11, 11, 11, 2, 2, 2, 2]);
        assert!(read_mapped(&f, &runs, 16, 60, &mut [0u8; 16]).is_err()); // past the end
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hardlink_names_are_all_kept() {
        // Two WIN32 names in different directories plus a DOS alias: two links.
        let r = build(
            FLAG_IN_USE,
            3,
            0,
            &[
                file_name(100, "LONGNAME.TXT", NS_DOS),
                file_name(100, "long name.txt", 1),
                file_name(200, "other.txt", 1),
                resident(ATTR_DATA, b"hello"),
            ],
        );
        let e = parse(r).unwrap().unwrap();
        assert_eq!(e.links, 2);
        assert_eq!((e.parent, e.name.to_str().unwrap()), (100, "long name.txt"));
        assert_eq!(e.other_names.len(), 1);
        assert_eq!(e.other_names[0].0, 200);
        assert_eq!(e.other_names[0].1.to_str().unwrap(), "other.txt");
        assert_eq!(e.size, 5);
    }

    #[test]
    fn aligned_window() {
        let mut v = Vec::new();
        for len in [1, 4096, 10_000] {
            let w = aligned_mut(&mut v, len);
            assert_eq!(w.len(), len);
            assert!((w.as_ptr() as usize).is_multiple_of(IO_ALIGN as usize));
        }
    }

    #[test]
    fn read_jobs_skip_sparse_and_align_to_records() {
        // 4 KiB clusters, 1 KiB records: 2 clusters, a sparse cluster, 1 cluster.
        let runs = [(10i64, 2u64), (-1, 1), (20, 1)];
        assert_eq!(
            read_jobs(&runs, 4096, 16384, 1024),
            vec![(0, 8192), (12288, 4096)]
        );
        // data_size clips the last run and adjacent runs read as one range.
        assert_eq!(
            read_jobs(&[(1, 1), (5, 1)], 4096, 6144, 1024),
            vec![(0, 6144)]
        );
        // 512-byte clusters with 1 KiB records: a record split by a hole is dropped.
        assert_eq!(
            read_jobs(&[(1, 3), (-1, 1), (9, 4)], 512, 4096, 1024),
            vec![(0, 1024), (2048, 2048)]
        );
        // Long ranges are split into CHUNK-sized reads.
        let big = read_jobs(&[(1, (CHUNK as u64 / 4096) * 3)], 4096, u64::MAX, 1024);
        assert_eq!(big.len(), 3);
        assert!(big.iter().all(|&(_, n)| n == CHUNK));
    }

    #[test]
    fn extension_records_merge_into_base() {
        let list = resident(ATTR_ATTRIBUTE_LIST, &[0; 32]);
        let base = build(FLAG_IN_USE, 4, 0, &[file_name(100, "a.bin", 1), list]);
        // The extension (base reference = record 10, seq 4) carries a second hardlink
        // and the $DATA header; it is read before its base here.
        let ext = build(
            FLAG_IN_USE,
            1,
            10 | (4 << 48),
            &[
                file_name(200, "b.bin", 1),
                non_resident(ATTR_DATA, &[0], 8192, 5000, None),
            ],
        );
        let lone = build(FLAG_IN_USE, 1, 0, &[file_name(100, "c", 1)]);
        // Base and extension land in different workers; absorb joins them.
        let (mut w1, mut w2) = (Scanner::default(), Scanner::default());
        w1.process(&mut ext.clone(), 11);
        w2.process(&mut base.clone(), 10);
        w2.process(&mut lone.clone(), 12);
        w1.process(
            &mut build(FLAG_IN_USE, 1, 77, &[file_name(1, "orphan", 1)]),
            13,
        );
        let mut sc = Scanner::default();
        sc.absorb(w1);
        sc.absorb(w2);
        sc.finish();
        let (stats, got) = (sc.stats, sc.out);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name.to_str().unwrap(), "c");
        assert!(!got[0].approx); // no list, no $DATA: an empty file, not approximate
        let e = &got[1];
        assert_eq!(e.frn, 10 | (4 << 48));
        assert_eq!((e.size, e.alloc, e.links, e.approx), (5000, 8192, 2, false));
        assert_eq!(e.other_names[0].0, 200);
        assert_eq!(
            (stats.in_use, stats.approx_records, stats.records),
            (2, 0, 4)
        );
    }

    fn list_entry(ty: u32, vcn: u64, fref: u64) -> Vec<u8> {
        let mut e = vec![0u8; 0x20];
        e[0..4].copy_from_slice(&ty.to_le_bytes());
        e[4..6].copy_from_slice(&0x20u16.to_le_bytes());
        e[8..16].copy_from_slice(&vcn.to_le_bytes());
        e[0x10..0x18].copy_from_slice(&fref.to_le_bytes());
        e
    }

    #[test]
    fn mft_extents_follow_non_resident_attribute_list() {
        // Fake volume, 1 KiB clusters = 1 KiB records:
        //   cluster 0-1: $MFT records 0 and 1 (first extent, VCN 0-1)
        //   cluster 3:   $MFT's non-resident $ATTRIBUTE_LIST
        //   cluster 5:   $MFT record 2 (second extent, VCN 2, described in record 1)
        let bpc = REC as u64;
        let mut list = list_entry(ATTR_DATA, 0, 0);
        list.extend(list_entry(ATTR_DATA, 2, 1));
        let list_attr = non_resident(
            ATTR_ATTRIBUTE_LIST,
            &[0x11, 1, 3, 0],
            bpc,
            list.len() as u64,
            None,
        );
        let data0 = non_resident(ATTR_DATA, &[0x11, 2, 0, 0], 3 * bpc, 3 * bpc, None);
        let rec0 = build(FLAG_IN_USE, 1, 0, &[list_attr, data0]);
        let mut data2 = non_resident(ATTR_DATA, &[0x11, 1, 5, 0], bpc, bpc, None);
        data2[0x10..0x18].copy_from_slice(&2u64.to_le_bytes()); // start VCN 2
        let rec1 = build(FLAG_IN_USE, 1, 0, &[data2]);

        let mut img = vec![0u8; 6 * REC];
        img[..REC].copy_from_slice(&rec0);
        img[REC..2 * REC].copy_from_slice(&rec1);
        img[3 * REC..3 * REC + list.len()].copy_from_slice(&list);
        let dir = std::env::temp_dir().join(format!("dusage_mftlist_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("vol.bin");
        std::fs::write(&p, &img).unwrap();
        let f = File::open(&p).unwrap();
        let mut r0 = rec0.clone();
        apply_fixup(&mut r0, 512).unwrap();
        let (runs, size) = mft_extents(&f, &r0, bpc, bpc).unwrap();
        assert_eq!(runs, vec![(0, 2), (5, 1)]);
        assert_eq!(size, 3 * bpc);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn join_extents_rejects_gaps() {
        let mut ok = vec![(2u64, vec![(9i64, 1u64)]), (0, vec![(4, 2)])];
        assert_eq!(join_extents(&mut ok).unwrap(), vec![(4, 2), (9, 1)]);
        let mut gap = vec![(0u64, vec![(4i64, 2u64)]), (3, vec![(9, 1)])];
        assert!(join_extents(&mut gap).is_err());
    }

    #[test]
    fn live_scan_needs_admin() {
        let mut n = 0u64;
        let mut dirs = 0u64;
        match scan_volume('C', &mut |e| {
            n += 1;
            dirs += e.is_dir as u64;
        }) {
            Ok(st) => {
                assert!(st.records > 0);
                assert_eq!(st.in_use, n);
                assert!(dirs > 0);
            }
            Err(e) if matches!(e.raw_os_error(), Some(5) | Some(1) | Some(1314)) => {
                eprintln!("skipped: needs admin ({e})");
            }
            Err(e) => panic!("live scan failed: {e}"),
        }
    }
}
