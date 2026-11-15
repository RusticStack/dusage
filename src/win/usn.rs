//! USN change journal: query, read, enumerate, resolve file references.
//! Owned by wave-1 lane `usn`.
//!
//! Layouts follow winioctl.h / Microsoft Learn (USN_JOURNAL_DATA_V0,
//! READ_USN_JOURNAL_DATA_V1, MFT_ENUM_DATA_V1, USN_RECORD_V2/V3).
#![allow(non_camel_case_types, clippy::upper_case_acronyms)]

use super::sys::{self, HANDLE};
use std::ffi::{c_void, OsString};
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Component, Path, PathBuf, Prefix};

/// `CTL_CODE` macro from winioctl.h.
pub const fn ctl_code(device: u32, function: u32, method: u32, access: u32) -> u32 {
    (device << 16) | (access << 14) | (function << 2) | method
}

const FILE_DEVICE_FILE_SYSTEM: u32 = 9;
const METHOD_BUFFERED: u32 = 0;
const METHOD_NEITHER: u32 = 3;
const FILE_ANY_ACCESS: u32 = 0;

pub const FSCTL_QUERY_USN_JOURNAL: u32 = ctl_code(
    FILE_DEVICE_FILE_SYSTEM,
    61,
    METHOD_BUFFERED,
    FILE_ANY_ACCESS,
);
pub const FSCTL_READ_USN_JOURNAL: u32 =
    ctl_code(FILE_DEVICE_FILE_SYSTEM, 46, METHOD_NEITHER, FILE_ANY_ACCESS);
pub const FSCTL_ENUM_USN_DATA: u32 =
    ctl_code(FILE_DEVICE_FILE_SYSTEM, 44, METHOD_NEITHER, FILE_ANY_ACCESS);

const FILE_ID_TYPE: i32 = 0;
const EXTENDED_FILE_ID_TYPE: i32 = 2;
const IO_BUF_BYTES: usize = 256 * 1024;

/// USN_JOURNAL_DATA_V0 (56 bytes).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct USN_JOURNAL_DATA_V0 {
    usn_journal_id: u64,
    first_usn: i64,
    next_usn: i64,
    lowest_valid_usn: i64,
    max_usn: i64,
    maximum_size: u64,
    allocation_delta: u64,
}

/// READ_USN_JOURNAL_DATA_V1 (48 bytes with tail padding).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct READ_USN_JOURNAL_DATA_V1 {
    start_usn: i64,
    reason_mask: u32,
    return_only_on_close: u32,
    timeout: u64,
    bytes_to_wait_for: u64,
    usn_journal_id: u64,
    min_major_version: u16,
    max_major_version: u16,
}

/// MFT_ENUM_DATA_V1 (32 bytes with tail padding).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct MFT_ENUM_DATA_V1 {
    start_file_reference_number: u64,
    low_usn: i64,
    high_usn: i64,
    min_major_version: u16,
    max_major_version: u16,
}

/// FILE_ID_DESCRIPTOR (24 bytes): size, type, 16-byte union (8-aligned).
#[repr(C)]
#[derive(Clone, Copy)]
struct FILE_ID_DESCRIPTOR {
    size: u32,
    kind: i32,
    id: [u64; 2],
}

#[link(name = "kernel32")]
extern "system" {
    fn OpenFileById(
        hint: HANDLE,
        id: *const FILE_ID_DESCRIPTOR,
        access: u32,
        share: u32,
        security: *mut c_void,
        flags: u32,
    ) -> HANDLE;
    fn GetFinalPathNameByHandleW(h: HANDLE, buf: *mut u16, len: u32, flags: u32) -> u32;
}

const USN_REC_MIN_V2: usize = 60;
const USN_REC_MIN_V3: usize = 76;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalInfo {
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
    pub max_usn: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsnRecord {
    pub file_ref: u128,
    pub parent_ref: u128,
    pub usn: i64,
    pub reason: u32,
    pub attributes: u32,
    pub name: OsString,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
fn u128_at(b: &[u8], o: usize) -> u128 {
    u128::from_le_bytes(b[o..o + 16].try_into().unwrap())
}

fn parse_one(rec: &[u8], major: u16) -> Option<UsnRecord> {
    // (min header size, file ref offset, parent ref offset, usn offset, 128-bit refs)
    let (min, fo, po, uo, wide_ref) = match major {
        2 => (USN_REC_MIN_V2, 8, 16, 24, false),
        3 => (USN_REC_MIN_V3, 8, 24, 40, true),
        _ => return None,
    };
    if rec.len() < min {
        return None;
    }
    let (file_ref, parent_ref) = if wide_ref {
        (u128_at(rec, fo), u128_at(rec, po))
    } else {
        (u64_at(rec, fo) as u128, u64_at(rec, po) as u128)
    };
    let usn = u64_at(rec, uo) as i64;
    // After the 8-byte timestamp: reason, source info, security id, attributes, name len, name off.
    let base = uo + 16;
    let reason = u32_at(rec, base);
    let attributes = u32_at(rec, base + 12);
    let name_len = u16_at(rec, base + 16) as usize;
    let name_off = u16_at(rec, base + 18) as usize;
    let end = name_off.checked_add(name_len)?;
    if end > rec.len() || !name_len.is_multiple_of(2) || name_off < min {
        return None;
    }
    let wide: Vec<u16> = rec[name_off..end]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    Some(UsnRecord {
        file_ref,
        parent_ref,
        usn,
        reason,
        attributes,
        name: OsString::from_wide(&wide),
    })
}

/// Parse a run of USN_RECORD_V2/V3 records (the part of an output buffer after the
/// leading 8-byte next-USN/FRN). Stops at the first record whose length is invalid
/// or out of bounds; malformed or unknown-version records are skipped using their
/// `RecordLength`.
pub fn parse_records(buf: &[u8]) -> Vec<UsnRecord> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while buf.len().saturating_sub(off) >= 8 {
        let len = u32_at(buf, off) as usize;
        let major = u16_at(buf, off + 4);
        if len < 8 || len > buf.len() - off {
            break;
        }
        if let Some(r) = parse_one(&buf[off..off + len], major) {
            out.push(r);
        }
        // Records are 8-byte aligned in the buffer.
        off += (len + 7) & !7;
    }
    out
}

/// Prefix cleanup for GetFinalPathNameByHandleW output.
fn strip_verbatim(s: &[u16]) -> Vec<u16> {
    let unc: Vec<u16> = r"\\?\UNC\".encode_utf16().collect();
    let pre: Vec<u16> = r"\\?\".encode_utf16().collect();
    if s.starts_with(&unc) {
        let mut v = vec![b'\\' as u16, b'\\' as u16];
        v.extend_from_slice(&s[unc.len()..]);
        v
    } else if s.starts_with(&pre) {
        s[pre.len()..].to_vec()
    } else {
        s.to_vec()
    }
}

fn ioctl(
    h: HANDLE,
    code: u32,
    input: *const c_void,
    in_size: usize,
    out: &mut [u64],
) -> io::Result<usize> {
    let mut ret = 0u32;
    // SAFETY: `h` is a live handle; `input` points to `in_size` readable bytes (or is
    // null with size 0); `out` is a writable buffer of exactly `out.len()*8` bytes.
    let ok = unsafe {
        sys::DeviceIoControl(
            h,
            code,
            input,
            in_size as u32,
            out.as_mut_ptr() as *mut c_void,
            (out.len() * 8) as u32,
            &mut ret,
            std::ptr::null_mut(),
        )
    };
    sys::check(ok)?;
    Ok(ret as usize)
}

fn bytes_of(buf: &[u64], n: usize) -> &[u8] {
    // SAFETY: a u64 slice is validly viewed as bytes; `n` is clamped to its byte length.
    let all = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len() * 8) };
    &all[..n.min(all.len())]
}

/// An open `\\.\X:` volume handle.
pub struct Volume {
    handle: OwnedHandle,
}

impl Volume {
    /// Opens `\\.\X:` read-only; needs admin.
    pub fn open(letter: char) -> io::Result<Volume> {
        if !letter.is_ascii_alphabetic() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bad drive letter",
            ));
        }
        let path = format!(r"\\.\{}:", letter.to_ascii_uppercase());
        let w = sys::wide_plain(std::ffi::OsStr::new(&path));
        let handle = sys::open_wide(&w, sys::GENERIC_READ, 0)?;
        Ok(Volume { handle })
    }

    /// Volume containing `p` (drive-letter paths only).
    pub fn for_path(p: &Path) -> io::Result<Volume> {
        let abs = std::path::absolute(p)?;
        if let Some(Component::Prefix(pc)) = abs.components().next() {
            if let Prefix::Disk(d) | Prefix::VerbatimDisk(d) = pc.kind() {
                return Self::open(d as char);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path has no drive-letter volume",
        ))
    }

    fn raw(&self) -> HANDLE {
        self.handle.as_raw_handle()
    }

    /// FSCTL_QUERY_USN_JOURNAL.
    pub fn query(&self) -> io::Result<JournalInfo> {
        let mut out = vec![0u64; 16];
        let n = ioctl(
            self.raw(),
            FSCTL_QUERY_USN_JOURNAL,
            std::ptr::null(),
            0,
            &mut out,
        )?;
        if n < std::mem::size_of::<USN_JOURNAL_DATA_V0>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short journal data",
            ));
        }
        let b = bytes_of(&out, n);
        Ok(JournalInfo {
            journal_id: u64_at(b, 0),
            first_usn: u64_at(b, 8) as i64,
            next_usn: u64_at(b, 16) as i64,
            lowest_valid_usn: u64_at(b, 24) as i64,
            max_usn: u64_at(b, 32) as i64,
        })
    }

    /// FSCTL_READ_USN_JOURNAL from `start` until no further records; returns the
    /// records and the next start USN. A purged start USN surfaces as
    /// ERROR_JOURNAL_ENTRY_DELETED (1181); callers treat any error as "full rescan".
    pub fn read(&self, journal_id: u64, start: i64) -> io::Result<(Vec<UsnRecord>, i64)> {
        let mut recs = Vec::new();
        let mut next = start;
        let mut out = vec![0u64; IO_BUF_BYTES / 8];
        loop {
            let req = READ_USN_JOURNAL_DATA_V1 {
                start_usn: next,
                reason_mask: u32::MAX,
                return_only_on_close: 0,
                timeout: 0,
                bytes_to_wait_for: 0,
                usn_journal_id: journal_id,
                min_major_version: 2,
                max_major_version: 3,
            };
            let n = ioctl(
                self.raw(),
                FSCTL_READ_USN_JOURNAL,
                &req as *const _ as *const c_void,
                std::mem::size_of_val(&req),
                &mut out,
            )?;
            if n < 8 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "short journal read",
                ));
            }
            let b = bytes_of(&out, n);
            let new_next = u64_at(b, 0) as i64;
            let before = recs.len();
            recs.extend(parse_records(&b[8..]));
            let advanced = new_next > next;
            if advanced {
                next = new_next;
            }
            if n == 8 || recs.len() == before || !advanced {
                break;
            }
        }
        Ok((recs, next))
    }

    /// FSCTL_ENUM_USN_DATA over the whole MFT (names + parent refs, no sizes).
    pub fn enumerate(&self, f: &mut dyn FnMut(UsnRecord)) -> io::Result<()> {
        self.enumerate_while(&mut |r| {
            f(r);
            true
        })
    }

    /// Like `enumerate`, but stops early when `f` returns false.
    fn enumerate_while(&self, f: &mut dyn FnMut(UsnRecord) -> bool) -> io::Result<()> {
        let mut req = MFT_ENUM_DATA_V1 {
            start_file_reference_number: 0,
            low_usn: 0,
            high_usn: i64::MAX,
            min_major_version: 2,
            max_major_version: 3,
        };
        let mut out = vec![0u64; IO_BUF_BYTES / 8];
        loop {
            let res = ioctl(
                self.raw(),
                FSCTL_ENUM_USN_DATA,
                &req as *const _ as *const c_void,
                std::mem::size_of_val(&req),
                &mut out,
            );
            let n = match res {
                Ok(n) => n,
                Err(e) if e.raw_os_error() == Some(sys::ERROR_HANDLE_EOF) => return Ok(()),
                Err(e) => return Err(e),
            };
            if n < 8 {
                return Ok(());
            }
            let b = bytes_of(&out, n);
            req.start_file_reference_number = u64_at(b, 0);
            for r in parse_records(&b[8..]) {
                if !f(r) {
                    return Ok(());
                }
            }
        }
    }

    /// Resolve a file reference to a full path (OpenFileById + GetFinalPathNameByHandleW).
    pub fn path_of(&self, file_ref: u128) -> io::Result<PathBuf> {
        path_of_with_hint(self.raw(), file_ref)
    }
}

fn path_of_with_hint(hint: HANDLE, file_ref: u128) -> io::Result<PathBuf> {
    let (kind, id) = if file_ref >> 64 == 0 {
        (FILE_ID_TYPE, [file_ref as u64, 0])
    } else {
        (
            EXTENDED_FILE_ID_TYPE,
            [file_ref as u64, (file_ref >> 64) as u64],
        )
    };
    let desc = FILE_ID_DESCRIPTOR {
        size: std::mem::size_of::<FILE_ID_DESCRIPTOR>() as u32,
        kind,
        id,
    };
    // SAFETY: `desc` is a valid FILE_ID_DESCRIPTOR; `hint` is a live handle on the volume.
    let h = unsafe {
        OpenFileById(
            hint,
            &desc,
            sys::FILE_READ_ATTRIBUTES,
            sys::FILE_SHARE_ALL,
            std::ptr::null_mut(),
            sys::FILE_FLAG_BACKUP_SEMANTICS,
        )
    };
    if h == sys::INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `h` is a fresh, valid handle we exclusively own.
    let owned = unsafe { OwnedHandle::from_raw_handle(h) };
    let mut buf = vec![0u16; 1024];
    loop {
        // SAFETY: `buf` is writable for `buf.len()` u16s; the handle is live. Flags 0 =
        // FILE_NAME_NORMALIZED | VOLUME_NAME_DOS.
        let n = unsafe {
            GetFinalPathNameByHandleW(owned.as_raw_handle(), buf.as_mut_ptr(), buf.len() as u32, 0)
        } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n >= buf.len() {
            buf.resize(n + 1, 0);
            continue;
        }
        let s = strip_verbatim(&buf[..n]);
        return Ok(PathBuf::from(OsString::from_wide(&s)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    fn name16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect()
    }

    fn v2(frn: u64, parent: u64, usn: i64, reason: u32, attrs: u32, name: &str) -> Vec<u8> {
        let nb = name16(name);
        let len = (USN_REC_MIN_V2 + nb.len() + 7) & !7;
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&frn.to_le_bytes());
        r[16..24].copy_from_slice(&parent.to_le_bytes());
        r[24..32].copy_from_slice(&usn.to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        r[52..56].copy_from_slice(&attrs.to_le_bytes());
        r[56..58].copy_from_slice(&(nb.len() as u16).to_le_bytes());
        r[58..60].copy_from_slice(&(USN_REC_MIN_V2 as u16).to_le_bytes());
        r[60..60 + nb.len()].copy_from_slice(&nb);
        r
    }

    fn v3(frn: u128, parent: u128, usn: i64, reason: u32, attrs: u32, name: &str) -> Vec<u8> {
        let nb = name16(name);
        let len = (USN_REC_MIN_V3 + nb.len() + 7) & !7;
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&3u16.to_le_bytes());
        r[8..24].copy_from_slice(&frn.to_le_bytes());
        r[24..40].copy_from_slice(&parent.to_le_bytes());
        r[40..48].copy_from_slice(&usn.to_le_bytes());
        r[56..60].copy_from_slice(&reason.to_le_bytes());
        r[68..72].copy_from_slice(&attrs.to_le_bytes());
        r[72..74].copy_from_slice(&(nb.len() as u16).to_le_bytes());
        r[74..76].copy_from_slice(&(USN_REC_MIN_V3 as u16).to_le_bytes());
        r[76..76 + nb.len()].copy_from_slice(&nb);
        r
    }

    #[test]
    fn ctl_codes() {
        assert_eq!(ctl_code(9, 61, 0, 0), 0x0009_00f4);
        assert_eq!(FSCTL_QUERY_USN_JOURNAL, 0x0009_00f4);
        assert_eq!(FSCTL_READ_USN_JOURNAL, 0x0009_00bb);
        assert_eq!(FSCTL_ENUM_USN_DATA, 0x0009_00b3);
        // FSCTL_GET_NTFS_VOLUME_DATA = CTL_CODE(9, 25, BUFFERED, ANY) = 0x90064
        assert_eq!(ctl_code(9, 25, 0, 0), 0x0009_0064);
    }

    #[test]
    fn struct_layouts() {
        assert_eq!(size_of::<USN_JOURNAL_DATA_V0>(), 56);
        assert_eq!(offset_of!(USN_JOURNAL_DATA_V0, max_usn), 32);
        assert_eq!(offset_of!(USN_JOURNAL_DATA_V0, allocation_delta), 48);
        assert_eq!(size_of::<READ_USN_JOURNAL_DATA_V1>(), 48);
        assert_eq!(offset_of!(READ_USN_JOURNAL_DATA_V1, reason_mask), 8);
        assert_eq!(offset_of!(READ_USN_JOURNAL_DATA_V1, timeout), 16);
        assert_eq!(offset_of!(READ_USN_JOURNAL_DATA_V1, bytes_to_wait_for), 24);
        assert_eq!(offset_of!(READ_USN_JOURNAL_DATA_V1, usn_journal_id), 32);
        assert_eq!(offset_of!(READ_USN_JOURNAL_DATA_V1, min_major_version), 40);
        assert_eq!(offset_of!(READ_USN_JOURNAL_DATA_V1, max_major_version), 42);
        assert_eq!(size_of::<MFT_ENUM_DATA_V1>(), 32);
        assert_eq!(offset_of!(MFT_ENUM_DATA_V1, high_usn), 16);
        assert_eq!(offset_of!(MFT_ENUM_DATA_V1, min_major_version), 24);
        assert_eq!(offset_of!(MFT_ENUM_DATA_V1, max_major_version), 26);
        assert_eq!(size_of::<FILE_ID_DESCRIPTOR>(), 24);
        assert_eq!(align_of::<FILE_ID_DESCRIPTOR>(), 8);
        assert_eq!(offset_of!(FILE_ID_DESCRIPTOR, kind), 4);
        assert_eq!(offset_of!(FILE_ID_DESCRIPTOR, id), 8);
    }

    #[test]
    fn parse_v2_multiple() {
        let mut buf = v2(5, 5, 100, 0x100, 0x10, "alpha.txt");
        buf.extend(v2(0x0001_0000_0000_0042, 5, 200, 0x2, 0x20, "b\u{e9}ta"));
        let r = parse_records(&buf);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].file_ref, 5);
        assert_eq!(r[0].parent_ref, 5);
        assert_eq!(r[0].usn, 100);
        assert_eq!(r[0].reason, 0x100);
        assert_eq!(r[0].attributes, 0x10);
        assert_eq!(r[0].name, OsString::from("alpha.txt"));
        assert_eq!(r[1].file_ref, 0x0001_0000_0000_0042);
        assert_eq!(r[1].name, OsString::from("b\u{e9}ta"));
    }

    #[test]
    fn parse_v3_and_mixed() {
        let big = (0xabcdu128 << 64) | 0x1234;
        let mut buf = v3(big, 7, -9, 1, 0x20, "v3file");
        buf.extend(v2(9, 8, 3, 4, 0x80, "v2"));
        let r = parse_records(&buf);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].file_ref, big);
        assert_eq!(r[0].parent_ref, 7);
        assert_eq!(r[0].usn, -9);
        assert_eq!(r[0].reason, 1);
        assert_eq!(r[0].attributes, 0x20);
        assert_eq!(r[0].name, OsString::from("v3file"));
        assert_eq!(r[1].file_ref, 9);
    }

    #[test]
    fn parse_bounds() {
        assert!(parse_records(&[]).is_empty());
        assert!(parse_records(&[0u8; 7]).is_empty());
        let good = v2(1, 2, 3, 4, 5, "x");
        // Truncated buffer: record length exceeds the buffer.
        assert!(parse_records(&good[..good.len() - 1]).is_empty());
        // Zero record length must terminate.
        assert!(parse_records(&[0u8; 64]).is_empty());
        // Name range beyond the record is rejected.
        let mut bad = good.clone();
        bad[56..58].copy_from_slice(&200u16.to_le_bytes());
        assert!(parse_records(&bad).is_empty());
        // Odd name length rejected.
        let mut odd = good.clone();
        odd[56..58].copy_from_slice(&1u16.to_le_bytes());
        assert!(parse_records(&odd).is_empty());
        // Name offset pointing into the header rejected.
        let mut low = good.clone();
        low[58..60].copy_from_slice(&4u16.to_le_bytes());
        assert!(parse_records(&low).is_empty());
        // Unknown major version is skipped, following record still parsed.
        let mut unk = good.clone();
        unk[4..6].copy_from_slice(&4u16.to_le_bytes());
        unk.extend(v2(10, 11, 12, 13, 14, "ok"));
        let r = parse_records(&unk);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].file_ref, 10);
        // V3 record shorter than its minimum header is dropped.
        let mut short = vec![0u8; 64];
        short[0..4].copy_from_slice(&64u32.to_le_bytes());
        short[4..6].copy_from_slice(&3u16.to_le_bytes());
        assert!(parse_records(&short).is_empty());
    }

    #[test]
    fn strip_prefixes() {
        let s = |x: &str| -> Vec<u16> { x.encode_utf16().collect() };
        let back = |v: Vec<u16>| String::from_utf16(&v).unwrap();
        assert_eq!(back(strip_verbatim(&s(r"\\?\C:\a\b"))), r"C:\a\b");
        assert_eq!(back(strip_verbatim(&s(r"\\?\UNC\srv\sh\a"))), r"\\srv\sh\a");
        assert_eq!(back(strip_verbatim(&s(r"C:\a"))), r"C:\a");
    }

    #[test]
    fn open_rejects_bad_input() {
        assert!(Volume::open('1').is_err());
        assert!(Volume::for_path(Path::new(r"\\srv\share\x")).is_err());
    }

    #[test]
    fn path_of_by_id_roundtrip() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("dusage_usn_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("probe.txt");
        std::fs::File::create(&file)
            .unwrap()
            .write_all(b"x")
            .unwrap();
        let h = sys::open(&file, sys::FILE_READ_ATTRIBUTES, 0).unwrap();
        // FILE_ID_INFO: u64 volume serial + 16-byte file id.
        let mut info = [0u64; 3];
        // SAFETY: `info` is 24 writable bytes, matching FILE_ID_INFO.
        let ok = unsafe {
            sys::GetFileInformationByHandleEx(
                h.as_raw_handle(),
                sys::FILE_ID_INFO_CLASS,
                info.as_mut_ptr() as *mut c_void,
                24,
            )
        };
        let result = if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            let id = (info[1] as u128) | ((info[2] as u128) << 64);
            // A file handle on the same volume serves as the hint.
            path_of_with_hint(h.as_raw_handle(), id)
        };
        drop(h);
        let want = std::fs::canonicalize(&file);
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);
        match result {
            Ok(p) => {
                let want = want.unwrap();
                let w: Vec<u16> =
                    std::os::windows::ffi::OsStrExt::encode_wide(want.as_os_str()).collect();
                let want = PathBuf::from(OsString::from_wide(&strip_verbatim(&w)));
                assert!(
                    p.to_string_lossy()
                        .eq_ignore_ascii_case(&want.to_string_lossy()),
                    "{p:?} vs {want:?}"
                );
            }
            // Some filesystems (or policies) do not support open-by-id.
            Err(e) => eprintln!("skipped: open by id unsupported here: {e}"),
        }
    }

    fn open_c() -> Option<Volume> {
        match Volume::open('C') {
            Ok(v) => Some(v),
            Err(e) if e.raw_os_error() == Some(sys::ERROR_ACCESS_DENIED) => {
                eprintln!("skipped: needs admin");
                None
            }
            Err(e) => {
                eprintln!("skipped: cannot open C: {e}");
                None
            }
        }
    }

    #[test]
    fn live_query_and_read() {
        let Some(v) = open_c() else { return };
        let info = match v.query() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("skipped: journal unavailable: {e}");
                return;
            }
        };
        assert!(info.next_usn >= info.first_usn);
        assert!(info.max_usn > 0);
        let start = (info.next_usn - 4096).max(info.lowest_valid_usn);
        let (recs, next) = v.read(info.journal_id, start).unwrap();
        assert!(next >= start);
        assert!(recs.iter().all(|r| r.usn >= start));
    }

    #[test]
    fn live_enumerate_few() {
        let Some(v) = open_c() else { return };
        let mut got: Vec<UsnRecord> = Vec::new();
        let res = v.enumerate_while(&mut |r| {
            got.push(r);
            got.len() < 5
        });
        if let Err(e) = res {
            eprintln!("skipped: enumerate failed: {e}");
            return;
        }
        assert!(!got.is_empty());
        assert!(got.iter().all(|r| !r.name.is_empty()));
        for r in got.iter().take(3) {
            // Files may vanish or be inaccessible; only exercise the call.
            let _ = v.path_of(r.file_ref);
        }
    }
}
