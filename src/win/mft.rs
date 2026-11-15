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

use std::ffi::{c_void, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::AsRawHandle;
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
const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;

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

/// Parse one fixed-up FILE record. `Ok(None)` for records that are not in use,
/// extension records (non-zero base reference) and records with no usable name.
pub fn parse_record(record: &[u8], index: u64) -> Result<Option<MftEntry>, String> {
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
    if u64_at(record, 0x20).ok_or("short header")? & 0x0000_FFFF_FFFF_FFFF != 0 {
        return Ok(None);
    }
    let seq = u16_at(record, 0x10).ok_or("short header")? as u64;
    let is_dir = flags & FLAG_DIR != 0;

    let mut best: Option<(u64, OsString, bool)> = None; // parent, name, is_win32
    let mut links = 0u16;
    let mut data: Option<(u64, u64)> = None;
    let mut has_list = false;
    let mut reparse = false;

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
                links = links.saturating_add(1);
                let win32 = ns != 0;
                if best.as_ref().is_none_or(|b| win32 && !b.2) {
                    let wide: Vec<u16> = (0..nlen)
                        .map(|i| u16::from_le_bytes([raw[2 * i], raw[2 * i + 1]]))
                        .collect();
                    let parent = u64_at(v, 0).ok_or("short $FILE_NAME")?;
                    best = Some((parent, OsString::from_wide(&wide), win32));
                }
            }
            ATTR_DATA if a.name_len == 0 && data.is_none() => {
                if a.non_resident {
                    if a.start_vcn() == 0 {
                        data = Some((a.data_size(), a.disk_alloc()));
                    }
                } else {
                    let v = a.resident_value().ok_or("bad resident $DATA")?;
                    data = Some((v.len() as u64, 0));
                }
            }
            ATTR_ATTRIBUTE_LIST => has_list = true,
            ATTR_REPARSE_POINT => reparse = true,
            _ => {}
        }
    }

    let Some((parent, name, _)) = best else {
        return Ok(None);
    };
    let (size, alloc) = if is_dir {
        (0, 0)
    } else {
        data.unwrap_or((0, 0))
    };
    Ok(Some(MftEntry {
        frn: (index & 0x0000_FFFF_FFFF_FFFF) | (seq << 48),
        parent,
        name,
        is_dir,
        size,
        alloc,
        links,
        reparse,
        approx: has_list && !is_dir && data.is_none(),
    }))
}

// ---- volume scan --------------------------------------------------------

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn read_exact_at(f: &mut File, off: u64, buf: &mut [u8]) -> io::Result<()> {
    f.seek(SeekFrom::Start(off))?;
    f.read_exact(buf)
}

/// Read `buf.len()` bytes at logical `offset` of a file described by `runs`.
fn read_mapped(
    f: &mut File,
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
    f: &mut File,
    rec0: &[u8],
    bpc: u64,
    rec_size: u64,
) -> io::Result<(Vec<(i64, u64)>, u64)> {
    let mut runs = Vec::new();
    let mut size = None;
    let mut list: Option<Vec<u8>> = None;
    for a in AttrIter::new(rec0).map_err(invalid)? {
        let a = a.map_err(invalid)?;
        if a.ty == ATTR_DATA && a.name_len == 0 && a.non_resident && a.start_vcn() == 0 {
            runs = a.runlist().map_err(invalid)?;
            size = Some(a.data_size());
        } else if a.ty == ATTR_ATTRIBUTE_LIST {
            list = Some(
                a.resident_value()
                    .ok_or_else(|| invalid("non-resident $MFT attribute list"))?
                    .to_vec(),
            );
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
            if ty == ATTR_DATA && list[o + 6] == 0 && vcn > 0 && fref != 0 {
                exts.push((vcn, fref));
            }
            o += len;
        }
        exts.sort_unstable();
        exts.dedup();
        for (vcn, idx) in exts {
            let mut rec = vec![0u8; rec_size as usize];
            read_mapped(f, &runs, bpc, idx * rec_size, &mut rec)?;
            apply_fixup(&mut rec, FIXUP_STRIDE).map_err(invalid)?;
            let mut found = false;
            for a in AttrIter::new(&rec).map_err(invalid)? {
                let a = a.map_err(invalid)?;
                if a.ty == ATTR_DATA && a.name_len == 0 && a.non_resident && a.start_vcn() == vcn {
                    runs.extend(a.runlist().map_err(invalid)?);
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
    Ok((runs, size))
}

struct Scanner<'a> {
    f: &'a mut dyn FnMut(MftEntry),
    stats: MftStats,
    scratch: Vec<u8>,
}

impl Scanner<'_> {
    fn process(&mut self, rec: &[u8], index: u64) {
        self.stats.records += 1;
        if rec[..4] == [0, 0, 0, 0] {
            return; // never-used slot
        }
        self.scratch.clear();
        self.scratch.extend_from_slice(rec);
        let r = apply_fixup(&mut self.scratch, FIXUP_STRIDE)
            .and_then(|_| parse_record(&self.scratch, index));
        match r {
            Ok(Some(e)) => {
                self.stats.in_use += 1;
                if e.approx {
                    self.stats.approx_records += 1;
                }
                (self.f)(e);
            }
            Ok(None) => {}
            Err(_) => self.stats.parse_errors += 1,
        }
    }
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
    let handle = sys::open_wide(
        &sys::wide_plain(OsStr::new(&dev)),
        sys::GENERIC_READ,
        FILE_FLAG_SEQUENTIAL_SCAN,
    )?;

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

    let mut file = File::from(handle);
    let mut rec0 = vec![0u8; rec_size as usize];
    read_exact_at(&mut file, v.mft_start_lcn as u64 * bpc, &mut rec0)?;
    apply_fixup(&mut rec0, FIXUP_STRIDE).map_err(|e| invalid(format!("$MFT record 0: {e}")))?;
    let (runs, data_size) = mft_extents(&mut file, &rec0, bpc, rec_size)?;

    let mut sc = Scanner {
        f,
        stats: MftStats::default(),
        scratch: Vec::with_capacity(rec_size as usize),
    };
    let rs = rec_size as usize;
    let mut buf = vec![0u8; CHUNK];
    let mut pending: Vec<u8> = Vec::new();
    let mut remaining = data_size;
    let mut index = 0u64;
    for (lcn, clusters) in runs {
        if remaining == 0 {
            break;
        }
        let mut take = (clusters * bpc).min(remaining);
        remaining -= take;
        if lcn < 0 {
            // Sparse MFT runs hold no records; skip their slots.
            pending.clear();
            index += take / rec_size;
            continue;
        }
        let mut pos = lcn as u64 * bpc;
        while take > 0 {
            let n = (take as usize).min(CHUNK);
            // Round the read up to a whole cluster (sector aligned); trailing bytes
            // beyond `take` are ignored.
            let rd = (n as u64).next_multiple_of(bpc) as usize;
            if rd > buf.len() {
                buf.resize(rd, 0);
            }
            read_exact_at(&mut file, pos, &mut buf[..rd])?;
            let mut data = &buf[..n];
            if !pending.is_empty() {
                let k = (rs - pending.len()).min(data.len());
                pending.extend_from_slice(&data[..k]);
                data = &data[k..];
                if pending.len() == rs {
                    let p = std::mem::take(&mut pending);
                    sc.process(&p, index);
                    index += 1;
                    pending = p;
                    pending.clear();
                }
            }
            while data.len() >= rs {
                sc.process(&data[..rs], index);
                index += 1;
                data = &data[rs..];
            }
            pending.extend_from_slice(data);
            pos += n as u64;
            take -= n as u64;
        }
    }
    sc.stats.elapsed = start.elapsed();
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
        let mut f = File::open(&p).unwrap();
        let runs = [(10i64, 2u64), (-1, 1), (2, 1)];
        let mut b = [0u8; 8];
        read_mapped(&mut f, &runs, 16, 24, &mut b).unwrap();
        assert_eq!(b, [11; 8]);
        assert!(read_mapped(&mut f, &runs, 16, 28, &mut b).is_err()); // runs into sparse
        let mut c = [0u8; 16];
        assert!(read_mapped(&mut f, &runs, 16, 32, &mut c).is_err()); // sparse
        read_mapped(&mut f, &runs, 16, 48, &mut c).unwrap();
        assert_eq!(c, [2u8; 16]);
        let adj = [(10i64, 2u64), (2, 1)];
        read_mapped(&mut f, &adj, 16, 28, &mut b).unwrap();
        assert_eq!(b, [11, 11, 11, 11, 2, 2, 2, 2]);
        assert!(read_mapped(&mut f, &runs, 16, 60, &mut [0u8; 16]).is_err()); // past the end
        let _ = std::fs::remove_dir_all(&dir);
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
