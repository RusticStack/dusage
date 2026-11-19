//! Batched directory enumeration via GetFileInformationByHandleEx. Owned by wave-1 lane `enumerate`.
//!
//! One syscall returns many entries (name, sizes, attributes, times, file id),
//! so no per-file open or `stat` is needed. Per-entry parse problems surface as
//! `io::ErrorKind::InvalidData`; a failed `read` may leave partial entries in `out`.

use super::sys::{self, HANDLE};
use crate::model::{ErrorCounts, Kind, Stat};
use std::collections::HashMap;
use std::ffi::{c_void, OsString};
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_PATH: usize = 260;
/// 100ns ticks between 1601-01-01 and 1970-01-01.
const EPOCH_DIFF_TICKS: u64 = 116_444_736_000_000_000;

/// FILE_ID_EXTD_DIR_INFO (winbase.h). Declared for layout verification; entries
/// are parsed field by field at `offset_of!` offsets, never by reading the whole
/// struct (the last entry's tail padding may lie outside the buffer).
#[repr(C)]
#[allow(dead_code)]
struct FileIdExtdDirInfo {
    next_entry_offset: u32,
    file_index: u32,
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    end_of_file: i64,
    allocation_size: i64,
    file_attributes: u32,
    file_name_length: u32,
    ea_size: u32,
    reparse_point_tag: u32,
    file_id: [u8; 16],
    file_name: [u16; 1],
}

/// FILE_ID_BOTH_DIR_INFO (winbase.h).
#[repr(C)]
#[allow(dead_code)]
struct FileIdBothDirInfo {
    next_entry_offset: u32,
    file_index: u32,
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    end_of_file: i64,
    allocation_size: i64,
    file_attributes: u32,
    file_name_length: u32,
    ea_size: u32,
    short_name_length: i8,
    short_name: [u16; 12],
    file_id: i64,
    file_name: [u16; 1],
}

/// WIN32_FIND_STREAM_DATA (fileapi.h).
#[repr(C)]
struct Win32FindStreamData {
    stream_size: i64,
    stream_name: [u16; MAX_PATH + 36],
}

// Layouts from the Microsoft Learn definitions in winbase.h / fileapi.h. Asserted at compile
// time, so the `x86_64-pc-windows-msvc` cross-check verifies them without a Windows host.
//   FILE_ID_EXTD_DIR_INFO: NextEntryOffset, FileIndex, CreationTime, LastAccessTime,
//     LastWriteTime, ChangeTime, EndOfFile, AllocationSize, FileAttributes, FileNameLength,
//     EaSize, ReparsePointTag, FileId (FILE_ID_128), FileName[1].
//   FILE_ID_BOTH_DIR_INFO: the same head, then ShortNameLength (CCHAR), ShortName[12],
//     FileId (LARGE_INTEGER), FileName[1].
//   WIN32_FIND_STREAM_DATA: StreamSize (LARGE_INTEGER), cStreamName[MAX_PATH + 36].
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(offset_of!(FileIdExtdDirInfo, next_entry_offset) == 0);
    assert!(offset_of!(FileIdExtdDirInfo, file_index) == 4);
    assert!(offset_of!(FileIdExtdDirInfo, creation_time) == 8);
    assert!(offset_of!(FileIdExtdDirInfo, last_access_time) == 16);
    assert!(offset_of!(FileIdExtdDirInfo, last_write_time) == 24);
    assert!(offset_of!(FileIdExtdDirInfo, change_time) == 32);
    assert!(offset_of!(FileIdExtdDirInfo, end_of_file) == 40);
    assert!(offset_of!(FileIdExtdDirInfo, allocation_size) == 48);
    assert!(offset_of!(FileIdExtdDirInfo, file_attributes) == 56);
    assert!(offset_of!(FileIdExtdDirInfo, file_name_length) == 60);
    assert!(offset_of!(FileIdExtdDirInfo, ea_size) == 64);
    assert!(offset_of!(FileIdExtdDirInfo, reparse_point_tag) == 68);
    assert!(offset_of!(FileIdExtdDirInfo, file_id) == 72);
    assert!(offset_of!(FileIdExtdDirInfo, file_name) == 88);
    assert!(size_of::<FileIdExtdDirInfo>() == 96);
    assert!(offset_of!(FileIdBothDirInfo, next_entry_offset) == 0);
    assert!(offset_of!(FileIdBothDirInfo, change_time) == 32);
    assert!(offset_of!(FileIdBothDirInfo, end_of_file) == 40);
    assert!(offset_of!(FileIdBothDirInfo, allocation_size) == 48);
    assert!(offset_of!(FileIdBothDirInfo, file_attributes) == 56);
    assert!(offset_of!(FileIdBothDirInfo, file_name_length) == 60);
    assert!(offset_of!(FileIdBothDirInfo, ea_size) == 64);
    assert!(offset_of!(FileIdBothDirInfo, short_name_length) == 68);
    assert!(offset_of!(FileIdBothDirInfo, short_name) == 70);
    assert!(offset_of!(FileIdBothDirInfo, file_id) == 96);
    assert!(offset_of!(FileIdBothDirInfo, file_name) == 104);
    assert!(size_of::<FileIdBothDirInfo>() == 112);
    assert!(offset_of!(Win32FindStreamData, stream_name) == 8);
    assert!(size_of::<Win32FindStreamData>() == 600);
};

/// Field offsets of one directory-info class, so one parser serves both.
struct Layout {
    next: usize,
    creation: usize,
    last_access: usize,
    last_write: usize,
    change: usize,
    eof: usize,
    alloc: usize,
    attrs: usize,
    name_len: usize,
    /// EaSize; for BOTH this doubles as the reparse tag on reparse points.
    ea: usize,
    /// Explicit tag field (EXTD only).
    tag: Option<usize>,
    id: usize,
    id_is_128: bool,
    name: usize,
}

macro_rules! off {
    ($t:ty, $f:ident) => {
        std::mem::offset_of!($t, $f)
    };
}

const EXTD: Layout = Layout {
    next: off!(FileIdExtdDirInfo, next_entry_offset),
    creation: off!(FileIdExtdDirInfo, creation_time),
    last_access: off!(FileIdExtdDirInfo, last_access_time),
    last_write: off!(FileIdExtdDirInfo, last_write_time),
    change: off!(FileIdExtdDirInfo, change_time),
    eof: off!(FileIdExtdDirInfo, end_of_file),
    alloc: off!(FileIdExtdDirInfo, allocation_size),
    attrs: off!(FileIdExtdDirInfo, file_attributes),
    name_len: off!(FileIdExtdDirInfo, file_name_length),
    ea: off!(FileIdExtdDirInfo, ea_size),
    tag: Some(off!(FileIdExtdDirInfo, reparse_point_tag)),
    id: off!(FileIdExtdDirInfo, file_id),
    id_is_128: true,
    name: off!(FileIdExtdDirInfo, file_name),
};

const BOTH: Layout = Layout {
    next: off!(FileIdBothDirInfo, next_entry_offset),
    creation: off!(FileIdBothDirInfo, creation_time),
    last_access: off!(FileIdBothDirInfo, last_access_time),
    last_write: off!(FileIdBothDirInfo, last_write_time),
    change: off!(FileIdBothDirInfo, change_time),
    eof: off!(FileIdBothDirInfo, end_of_file),
    alloc: off!(FileIdBothDirInfo, allocation_size),
    attrs: off!(FileIdBothDirInfo, file_attributes),
    name_len: off!(FileIdBothDirInfo, file_name_length),
    ea: off!(FileIdBothDirInfo, ea_size),
    tag: None,
    id: off!(FileIdBothDirInfo, file_id),
    id_is_128: false,
    name: off!(FileIdBothDirInfo, file_name),
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: OsString,
    pub attributes: u32,
    /// 0 unless FILE_ATTRIBUTE_REPARSE_POINT.
    pub reparse_tag: u32,
    /// EndOfFile.
    pub size: u64,
    /// AllocationSize.
    pub alloc: u64,
    /// 0 when unknown.
    pub file_id: u128,
    /// FILETIME as 100ns ticks since 1601.
    pub last_write: u64,
    pub change_time: u64,
    pub creation_time: u64,
    pub last_access_time: u64,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.attributes & sys::FILE_ATTRIBUTE_DIRECTORY != 0
    }

    /// Any reparse point (dir or file).
    pub fn is_reparse(&self) -> bool {
        self.attributes & sys::FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    pub fn modified(&self) -> SystemTime {
        filetime_to_system_time(self.last_write)
    }
}

/// Windows reports none of the POSIX owner, mode or link-count fields through a directory
/// record, and reading them would mean opening every file: `-perm`, `-uid`, `-links` and
/// friends answer from the zeros in [`Stat`] (`-perm` refuses, see `src/findexpr.rs`).
pub const POSIX_FIELDS: bool = false;

/// What kind of entry a directory record describes. A reparse point is reported as a
/// symlink: it is the one "do not follow" class here, junctions included.
fn kind_of(e: &Entry) -> Kind {
    if e.is_reparse() {
        Kind::Symlink
    } else if e.is_dir() {
        Kind::Dir
    } else {
        Kind::File
    }
}

/// The [`Stat`] a batched directory record already carries: size, allocation, times and
/// file id, with no syscall of its own.
fn stat_of(e: &Entry) -> Stat {
    Stat {
        kind: kind_of(e),
        size: e.size,
        // `st_blocks` is 512-byte units; NTFS allocation is in clusters.
        blocks: e.alloc / 512,
        mtime: filetime_to_system_time(e.last_write),
        atime: filetime_to_system_time(e.last_access_time),
        ctime: filetime_to_system_time(e.change_time),
        links: 0,
        inode: e.file_id as u64,
        dev: 0,
        mode: 0,
        uid: 0,
        gid: 0,
    }
}

/// A directory entry with its type already known and its inode data fetched on demand. The
/// batched query returns both, so nothing here costs an extra syscall; the shape matches the
/// Linux side so the walker can be written once.
#[derive(Debug, Clone)]
pub struct Lite {
    pub name: OsString,
    is_dir: bool,
    is_link: bool,
    stat: std::cell::Cell<Option<Stat>>,
}

impl Lite {
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    pub fn is_reparse(&self) -> bool {
        self.is_link
    }

    pub fn kind(&self) -> Kind {
        if self.is_link {
            Kind::Symlink
        } else if self.is_dir {
            Kind::Dir
        } else {
            Kind::File
        }
    }

    /// The record's own data. Unlike Linux there is nothing to defer: the batch already
    /// carried it, so a walk that never asks for a field pays nothing anyway.
    pub fn stat(&self) -> io::Result<Stat> {
        match self.stat.get() {
            Some(s) => Ok(s),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "entry has no inode data",
            )),
        }
    }
}

/// List `dir` into `out`, reusing the caller's vector.
pub fn list(dir: &Path, out: &mut Vec<Lite>, errs: &mut ErrorCounts) -> io::Result<()> {
    let mut ents = Vec::new();
    DirReader::new().read(dir, &mut ents, errs)?;
    out.clear();
    out.reserve(ents.len());
    for e in ents {
        out.push(Lite {
            is_dir: e.is_dir(),
            is_link: e.is_reparse(),
            stat: std::cell::Cell::new(Some(stat_of(&e))),
            name: e.name,
        });
    }
    Ok(())
}

fn filetime_to_system_time(ticks: u64) -> SystemTime {
    let dur = |t: u64| Duration::new(t / 10_000_000, (t % 10_000_000) as u32 * 100);
    if ticks >= EPOCH_DIFF_TICKS {
        UNIX_EPOCH + dur(ticks - EPOCH_DIFF_TICKS)
    } else {
        UNIX_EPOCH - dur(EPOCH_DIFF_TICKS - ticks)
    }
}

fn bad(msg: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Bounds-checked unaligned read of a `Copy` plain-integer type.
fn rd<T: Copy>(buf: &[u8], off: usize) -> io::Result<T> {
    match off.checked_add(std::mem::size_of::<T>()) {
        Some(end) if end <= buf.len() => {
            // SAFETY: `off..end` lies within `buf`; T is a plain integer/byte array so
            // any bit pattern is valid, and read_unaligned has no alignment requirement.
            Ok(unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const T) })
        }
        _ => Err(bad("directory info entry exceeds buffer")),
    }
}

/// Parse a buffer of chained entries into `out`, skipping "." and "..".
fn parse(buf: &[u8], l: &Layout, out: &mut Vec<Entry>) -> io::Result<()> {
    let mut off = 0usize;
    loop {
        let next: u32 = rd(buf, off + l.next)?;
        let attributes: u32 = rd(buf, off + l.attrs)?;
        let name_len: u32 = rd(buf, off + l.name_len)?;
        let name_len = name_len as usize;
        let name_off = off
            .checked_add(l.name)
            .ok_or_else(|| bad("offset overflow"))?;
        let name_end = name_off
            .checked_add(name_len)
            .ok_or_else(|| bad("name overflow"))?;
        if !name_len.is_multiple_of(2) || name_end > buf.len() {
            return Err(bad("file name exceeds buffer"));
        }
        // A record with no name would name the directory itself once joined (`dir.join("")`),
        // and the walk would descend into it under its own path; the kernel never emits one.
        if name_len == 0 {
            return Err(bad("directory entry with an empty file name"));
        }
        let wide: Vec<u16> = buf[name_off..name_end]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        let dots = |n: usize| wide.len() == n && wide.iter().all(|&c| c == b'.' as u16);
        if !(dots(1) || dots(2)) {
            let reparse = attributes & sys::FILE_ATTRIBUTE_REPARSE_POINT != 0;
            let reparse_tag = if !reparse {
                0
            } else {
                rd::<u32>(buf, off + l.tag.unwrap_or(l.ea))?
            };
            let file_id = if l.id_is_128 {
                u128::from_le_bytes(rd::<[u8; 16]>(buf, off + l.id)?)
            } else {
                rd::<i64>(buf, off + l.id)? as u64 as u128
            };
            out.push(Entry {
                name: OsString::from_wide(&wide),
                attributes,
                reparse_tag,
                size: rd::<i64>(buf, off + l.eof)?.max(0) as u64,
                alloc: rd::<i64>(buf, off + l.alloc)?.max(0) as u64,
                file_id,
                last_write: rd::<i64>(buf, off + l.last_write)?.max(0) as u64,
                change_time: rd::<i64>(buf, off + l.change)?.max(0) as u64,
                creation_time: rd::<i64>(buf, off + l.creation)?.max(0) as u64,
                last_access_time: rd::<i64>(buf, off + l.last_access)?.max(0) as u64,
            });
        }
        if next == 0 {
            return Ok(());
        }
        off = off
            .checked_add(next as usize)
            .ok_or_else(|| bad("next offset overflow"))?;
        if off >= buf.len() {
            return Err(bad("next entry offset exceeds buffer"));
        }
    }
}

pub struct DirReader {
    /// u64 elements keep the buffer 8-byte aligned.
    buf: Vec<u64>,
    /// Fell back to FILE_ID_BOTH_DIR_INFO (64-bit ids).
    both: bool,
}

impl Default for DirReader {
    fn default() -> Self {
        Self::new()
    }
}

impl DirReader {
    pub fn new() -> Self {
        Self::with_buffer(64 * 1024)
    }

    /// Buffer size is clamped to at least 4 KiB. Pass >= 64 KiB if names near the
    /// 32k-char limit must fit in a single entry.
    pub fn with_buffer(bytes: usize) -> Self {
        let bytes = bytes.max(4096);
        DirReader {
            buf: vec![0u64; bytes.div_ceil(8)],
            both: false,
        }
    }

    /// Enumerate one directory, skipping "." and "..". A batch-level failure is
    /// returned; there is no per-entry failure to record, so `errs` stays untouched.
    pub fn read(
        &mut self,
        dir: &Path,
        out: &mut Vec<Entry>,
        _errs: &mut ErrorCounts,
    ) -> io::Result<()> {
        let h = sys::open(
            dir,
            sys::FILE_LIST_DIRECTORY | sys::SYNCHRONIZE,
            sys::FILE_FLAG_BACKUP_SEMANTICS | sys::FILE_FLAG_OPEN_REPARSE_POINT,
        )?;
        let raw = h.as_raw_handle() as HANDLE;
        let mut first = true;
        loop {
            let (class, layout) = if self.both {
                (sys::FILE_ID_BOTH_DIRECTORY_INFO_CLASS, &BOTH)
            } else {
                (sys::FILE_ID_EXTD_DIRECTORY_INFO_CLASS, &EXTD)
            };
            let len = self.buf.len() * 8;
            // SAFETY: `raw` is a live directory handle; the buffer is `len` writable bytes.
            let ok = unsafe {
                sys::GetFileInformationByHandleEx(
                    raw,
                    class,
                    self.buf.as_mut_ptr() as *mut c_void,
                    len as u32,
                )
            };
            if ok == 0 {
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    Some(sys::ERROR_NO_MORE_FILES) => return Ok(()),
                    Some(
                        sys::ERROR_INVALID_PARAMETER
                        | sys::ERROR_INVALID_LEVEL
                        | sys::ERROR_NOT_SUPPORTED,
                    ) if first && !self.both => {
                        // Extended class unsupported on this filesystem; restart with the older one.
                        self.both = true;
                        continue;
                    }
                    _ => return Err(e),
                }
            }
            first = false;
            // SAFETY: the Vec<u64> owns `len` initialised bytes; u8 has no alignment needs.
            let bytes = unsafe { std::slice::from_raw_parts(self.buf.as_ptr() as *const u8, len) };
            parse(bytes, layout, out)?;
        }
    }
}

/// Volume serial number of the volume holding `path`.
pub fn volume_serial(path: &Path) -> io::Result<u64> {
    let h = sys::open(
        path,
        sys::FILE_READ_ATTRIBUTES,
        sys::FILE_FLAG_BACKUP_SEMANTICS | sys::FILE_FLAG_OPEN_REPARSE_POINT,
    )?;
    let mut serial = 0u32;
    // SAFETY: valid handle; only the serial out-pointer is non-null, name buffers have size 0.
    sys::check(unsafe {
        sys::GetVolumeInformationByHandleW(
            h.as_raw_handle() as HANDLE,
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        )
    })?;
    Ok(serial as u64)
}

#[link(name = "kernel32")]
extern "system" {
    fn FindFirstStreamW(name: *const u16, level: i32, data: *mut c_void, flags: u32) -> HANDLE;
    fn FindNextStreamW(h: HANDLE, data: *mut c_void) -> i32;
    fn FindClose(h: HANDLE) -> i32;
    fn GetVolumePathNameW(file: *const u16, buf: *mut u16, len: u32) -> i32;
    fn GetDiskFreeSpaceW(
        root: *const u16,
        sectors_per_cluster: *mut u32,
        bytes_per_sector: *mut u32,
        free_clusters: *mut u32,
        total_clusters: *mut u32,
    ) -> i32;
}

struct FindGuard(HANDLE);
impl Drop for FindGuard {
    fn drop(&mut self) {
        // SAFETY: the handle came from a successful FindFirstStreamW and is closed once.
        unsafe { FindClose(self.0) };
    }
}

/// Sum of named ($DATA other than the unnamed one) alternate stream sizes.
pub fn named_stream_bytes(path: &Path) -> io::Result<u64> {
    let w = sys::wide_verbatim(path)?;
    let mut data = Win32FindStreamData {
        stream_size: 0,
        stream_name: [0; MAX_PATH + 36],
    };
    // SAFETY: `w` is NUL-terminated; `data` is a valid WIN32_FIND_STREAM_DATA (level 0 = standard).
    let h = unsafe { FindFirstStreamW(w.as_ptr(), 0, &mut data as *mut _ as *mut c_void, 0) };
    if h == sys::INVALID_HANDLE_VALUE {
        let e = io::Error::last_os_error();
        return if e.raw_os_error() == Some(sys::ERROR_HANDLE_EOF) {
            Ok(0)
        } else {
            Err(e)
        };
    }
    let _g = FindGuard(h);
    let unnamed: Vec<u16> = "::$DATA".encode_utf16().collect();
    let mut total = 0u64;
    loop {
        let n = data
            .stream_name
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(data.stream_name.len());
        if data.stream_name[..n] != unnamed[..] {
            total += data.stream_size.max(0) as u64;
        }
        // SAFETY: `h` is a live find handle; `data` is a valid output struct.
        if unsafe { FindNextStreamW(h, &mut data as *mut _ as *mut c_void) } == 0 {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(sys::ERROR_HANDLE_EOF) {
                Ok(total)
            } else {
                Err(e)
            };
        }
    }
}

/// Cluster size in bytes of the volume holding `path`.
pub fn cluster_size(path: &Path) -> io::Result<u64> {
    let w = sys::wide_verbatim(path)?;
    let mut root = vec![0u16; 32 * 1024];
    // SAFETY: `w` is NUL-terminated; `root` is writable for `root.len()` u16s.
    sys::check(unsafe { GetVolumePathNameW(w.as_ptr(), root.as_mut_ptr(), root.len() as u32) })?;
    let (mut spc, mut bps, mut free, mut total) = (0u32, 0u32, 0u32, 0u32);
    // SAFETY: `root` was NUL-terminated by GetVolumePathNameW; out-pointers are valid u32s.
    sys::check(unsafe {
        GetDiskFreeSpaceW(root.as_ptr(), &mut spc, &mut bps, &mut free, &mut total)
    })?;
    Ok(spc as u64 * bps as u64)
}

/// FILE_ID_INFO (winbase.h).
#[repr(C)]
struct FileIdInfo {
    volume_serial_number: u64,
    file_id: [u8; 16],
}

// FILE_ID_INFO: VolumeSerialNumber (ULONGLONG), FileId (FILE_ID_128). 24 bytes.
const _: () = {
    assert!(std::mem::offset_of!(FileIdInfo, volume_serial_number) == 0);
    assert!(std::mem::offset_of!(FileIdInfo, file_id) == 8);
    assert!(std::mem::size_of::<FileIdInfo>() == 24);
};

/// Error for a tool root that is itself a reparse point (junction/symlink): listing it would
/// open the link object and see nothing, so callers refuse it instead of reporting zero.
/// `Ok(())` when `path` is not a reparse point; other failures are returned as-is.
pub fn refuse_reparse_root(path: &Path) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    let m = std::fs::symlink_metadata(path)?;
    if m.file_attributes() & sys::FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "root is a reparse point; pass its target ({})",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// 128-bit file id of `path` itself (reparse points are not followed). On NTFS the
/// low 48 bits are the MFT record index.
pub fn file_id_of(path: &Path) -> io::Result<u128> {
    let h = sys::open(
        path,
        sys::FILE_READ_ATTRIBUTES,
        sys::FILE_FLAG_BACKUP_SEMANTICS | sys::FILE_FLAG_OPEN_REPARSE_POINT,
    )?;
    let mut info = FileIdInfo {
        volume_serial_number: 0,
        file_id: [0; 16],
    };
    // SAFETY: valid handle; `info` is a writable FILE_ID_INFO of the size passed.
    sys::check(unsafe {
        sys::GetFileInformationByHandleEx(
            h.as_raw_handle() as HANDLE,
            sys::FILE_ID_INFO_CLASS,
            &mut info as *mut _ as *mut c_void,
            std::mem::size_of::<FileIdInfo>() as u32,
        )
    })?;
    Ok(u128::from_le_bytes(info.file_id))
}

#[cfg(test)]
mod stat_tests {
    use super::*;

    #[test]
    fn info_layouts_match_the_documented_structs() {
        // FILE_ID_EXTD_DIR_INFO: the four FILETIMEs follow the two u32 fields.
        assert_eq!(std::mem::offset_of!(FileIdExtdDirInfo, creation_time), 8);
        assert_eq!(
            std::mem::offset_of!(FileIdExtdDirInfo, last_access_time),
            16
        );
        assert_eq!(std::mem::offset_of!(FileIdExtdDirInfo, last_write_time), 24);
        assert_eq!(std::mem::offset_of!(FileIdExtdDirInfo, change_time), 32);
        // Attrs, name length, EA size and the reparse tag fill 56..72, then the id.
        assert_eq!(std::mem::offset_of!(FileIdExtdDirInfo, file_attributes), 56);
        assert_eq!(std::mem::offset_of!(FileIdExtdDirInfo, file_id), 72);
        // FILE_BASIC_INFO is 4 ticks and an attribute word, 40 bytes with its padding.
        assert_eq!(std::mem::offset_of!(BasicInfo, file_attributes), 32);
        assert_eq!(std::mem::size_of::<BasicInfo>(), 40);
        // FILE_STANDARD_INFO: two i64 sizes, then the link count and two flags.
        assert_eq!(std::mem::offset_of!(StandardInfo, number_of_links), 16);
        assert_eq!(std::mem::size_of::<StandardInfo>(), 24);
        assert_eq!(std::mem::size_of::<AttributeTagInfo>(), 8);
    }

    #[test]
    fn lstat_and_stat_describe_a_file() {
        let dir = std::env::temp_dir().join(format!("dusage_stat_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), vec![0u8; 5000]).unwrap();
        let s = lstat(&dir.join("a.txt")).unwrap();
        let d = lstat(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(s.kind, Kind::File);
        assert_eq!(s.size, 5000);
        assert_eq!(s.links, 1, "one name, so one link");
        assert!(s.inode != 0, "the handle knows the file id");
        assert!(s.blocks * 512 >= 5000, "allocation covers the file");
        assert!(s.mtime > UNIX_EPOCH);
        assert_eq!((d.kind, d.links), (Kind::Dir, 1));
        // The POSIX fields the platform does not have stay zero.
        assert_eq!((s.mode, s.uid, s.gid, s.dev), (0, 0, 0, 0));
    }

    #[test]
    fn a_record_stat_matches_the_handle() {
        let dir = std::env::temp_dir().join(format!("dusage_rec_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), vec![0u8; 5000]).unwrap();
        let mut ents = Vec::new();
        DirReader::new()
            .read(&dir, &mut ents, &mut ErrorCounts::default())
            .unwrap();
        let rec = stat_of(&ents[0]);
        let h = lstat(&dir.join("a.txt")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!((rec.kind, rec.size), (h.kind, h.size));
        assert_eq!(rec.blocks, h.blocks);
        assert_eq!(rec.inode, h.inode);
        assert_eq!(rec.mtime, h.mtime);
    }
}

#[cfg(test)]
mod file_id_tests {
    use super::*;

    #[test]
    fn file_id_info_layout() {
        assert_eq!(std::mem::offset_of!(FileIdInfo, file_id), 8);
        assert_eq!(std::mem::size_of::<FileIdInfo>(), 24);
    }

    #[test]
    fn file_id_matches_enumeration() {
        let dir = std::env::temp_dir().join(format!("dusage_fid_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        let mut v = Vec::new();
        DirReader::new()
            .read(&dir, &mut v, &mut ErrorCounts::default())
            .unwrap();
        let id = file_id_of(&dir.join("a.txt")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_ne!(id, 0);
        assert_eq!(v[0].file_id, id);
    }
}

/// Reparse tags with this bit are name surrogates (symlinks, mount points).
pub const NAME_SURROGATE: u32 = 0x2000_0000;

/// True for entries that are counted as links and never descended into: every
/// directory reparse point, and the name-surrogate file ones (symlinks).
pub fn is_skipped(e: &Entry) -> bool {
    e.is_reparse() && (e.is_dir() || e.reparse_tag & NAME_SURROGATE != 0)
}

/// The `readdir` backend: `std::fs::read_dir` with the find data std already
/// caches, so sizes are exact but allocation sizes and file ids are unknown.
pub fn read_plain(dir: &Path, out: &mut Vec<Entry>, errs: &mut ErrorCounts) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    for e in std::fs::read_dir(dir)? {
        let (e, md) = match e.and_then(|e| e.metadata().map(|m| (e, m))) {
            Ok(x) => x,
            Err(err) => {
                errs.record(&err);
                continue;
            }
        };
        let symlink = e.file_type().is_ok_and(|t| t.is_symlink());
        out.push(Entry {
            name: e.file_name(),
            attributes: md.file_attributes(),
            reparse_tag: if symlink { NAME_SURROGATE } else { 0 },
            size: md.len(),
            alloc: 0,
            file_id: 0,
            last_write: md.last_write_time(),
            change_time: 0,
            creation_time: md.creation_time(),
            last_access_time: md.last_access_time(),
        });
    }
    Ok(())
}

/// The filesystem rejected a per-directory detail we asked for.
pub fn is_unsupported(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(sys::ERROR_INVALID_PARAMETER | sys::ERROR_NOT_SUPPORTED | sys::ERROR_INVALID_LEVEL)
    )
}

pub fn is_denied(e: &io::Error) -> bool {
    e.raw_os_error() == Some(sys::ERROR_ACCESS_DENIED)
        || e.kind() == io::ErrorKind::PermissionDenied
}

/// FILE_BASIC_INFO (winbase.h): the four FILETIMEs and the attributes.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BasicInfo {
    creation_time: i64,
    last_access_time: i64,
    last_write_time: i64,
    change_time: i64,
    file_attributes: u32,
}

/// FILE_STANDARD_INFO (winbase.h): allocation, end of file and the link count.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct StandardInfo {
    allocation_size: i64,
    end_of_file: i64,
    number_of_links: u32,
    delete_pending: u8,
    directory: u8,
}

/// FILE_ATTRIBUTE_TAG_INFO (winbase.h).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AttributeTagInfo {
    file_attributes: u32,
    reparse_tag: u32,
}

// The three `GetFileInformationByHandleEx` out-structs, from winbase.h:
//   FILE_BASIC_INFO: four LARGE_INTEGER times, FileAttributes (ULONG), 4 bytes of padding.
//   FILE_STANDARD_INFO: AllocationSize, EndOfFile (LARGE_INTEGER), NumberOfLinks (ULONG),
//     DeletePending (BOOLEAN), Directory (BOOLEAN), 2 bytes of padding.
//   FILE_ATTRIBUTE_TAG_INFO: FileAttributes, ReparseTag.
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(offset_of!(BasicInfo, creation_time) == 0);
    assert!(offset_of!(BasicInfo, last_access_time) == 8);
    assert!(offset_of!(BasicInfo, last_write_time) == 16);
    assert!(offset_of!(BasicInfo, change_time) == 24);
    assert!(offset_of!(BasicInfo, file_attributes) == 32);
    assert!(size_of::<BasicInfo>() == 40);
    assert!(offset_of!(StandardInfo, allocation_size) == 0);
    assert!(offset_of!(StandardInfo, end_of_file) == 8);
    assert!(offset_of!(StandardInfo, number_of_links) == 16);
    assert!(offset_of!(StandardInfo, delete_pending) == 20);
    assert!(offset_of!(StandardInfo, directory) == 21);
    assert!(size_of::<StandardInfo>() == 24);
    assert!(offset_of!(AttributeTagInfo, file_attributes) == 0);
    assert!(offset_of!(AttributeTagInfo, reparse_tag) == 4);
    assert!(size_of::<AttributeTagInfo>() == 8);
};

const FILE_BASIC_INFO_CLASS: i32 = 0;
const FILE_STANDARD_INFO_CLASS: i32 = 1;

/// `lstat` one path: the handle is opened on the link itself, so a reparse point is
/// described, never followed.
pub fn lstat(path: &Path) -> io::Result<Stat> {
    stat_handle(path, true)
}

/// `stat` one path, following a reparse point (what `-follow` turns a link into).
pub fn stat(path: &Path) -> io::Result<Stat> {
    stat_handle(path, false)
}

/// One `GetFileInformationByHandle` worth of data on an opened handle: the basic times and
/// attributes, the standard sizes and link count, and the 128-bit file id.
///
/// `mode`, `uid` and `gid` stay 0 — Windows has none — and `dev` stays 0 too, so a path
/// stat and a batched record describe the same entry the same way. The link count is the
/// one field the handle knows and a directory record does not.
fn stat_handle(path: &Path, nofollow: bool) -> io::Result<Stat> {
    let flags = sys::FILE_FLAG_BACKUP_SEMANTICS
        | if nofollow {
            sys::FILE_FLAG_OPEN_REPARSE_POINT
        } else {
            0
        };
    let h = sys::open(path, sys::FILE_READ_ATTRIBUTES, flags)?;
    let raw = h.as_raw_handle() as HANDLE;
    let mut basic = BasicInfo::default();
    let mut standard = StandardInfo::default();
    let mut tag = AttributeTagInfo::default();
    let mut id = FileIdInfo {
        volume_serial_number: 0,
        file_id: [0; 16],
    };
    // SAFETY: `raw` is a live handle opened for FILE_READ_ATTRIBUTES, and each out-pointer
    // names a writable struct of exactly the size passed.
    unsafe {
        sys::check(sys::GetFileInformationByHandleEx(
            raw,
            FILE_BASIC_INFO_CLASS,
            &mut basic as *mut _ as *mut c_void,
            std::mem::size_of::<BasicInfo>() as u32,
        ))?;
        sys::check(sys::GetFileInformationByHandleEx(
            raw,
            FILE_STANDARD_INFO_CLASS,
            &mut standard as *mut _ as *mut c_void,
            std::mem::size_of::<StandardInfo>() as u32,
        ))?;
        sys::check(sys::GetFileInformationByHandleEx(
            raw,
            sys::FILE_ATTRIBUTE_TAG_INFO_CLASS,
            &mut tag as *mut _ as *mut c_void,
            std::mem::size_of::<AttributeTagInfo>() as u32,
        ))?;
        sys::check(sys::GetFileInformationByHandleEx(
            raw,
            sys::FILE_ID_INFO_CLASS,
            &mut id as *mut _ as *mut c_void,
            std::mem::size_of::<FileIdInfo>() as u32,
        ))?;
    }
    let ticks = |t: i64| t.max(0) as u64;
    let kind = if tag.file_attributes & sys::FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Kind::Symlink
    } else if standard.directory != 0 {
        Kind::Dir
    } else {
        Kind::File
    };
    Ok(Stat {
        kind,
        size: standard.end_of_file.max(0) as u64,
        blocks: standard.allocation_size.max(0) as u64 / 512,
        mtime: filetime_to_system_time(ticks(basic.last_write_time)),
        atime: filetime_to_system_time(ticks(basic.last_access_time)),
        ctime: filetime_to_system_time(ticks(basic.change_time)),
        links: u64::from(standard.number_of_links),
        inode: u128::from_le_bytes(id.file_id) as u64,
        dev: 0,
        mode: 0,
        uid: 0,
        gid: 0,
    })
}

/// Windows knows a filesystem name only per volume, and the walk carries no device for an
/// entry, so `-fstype` answers nothing here.
pub fn filesystem_types() -> HashMap<u64, String> {
    HashMap::new()
}

/// Device id for the `--one-filesystem` boundary; Windows mount points are reparse
/// points and already skipped, so there is nothing extra to compare.
pub fn dev_of(_path: &Path) -> io::Result<u64> {
    Ok(0)
}

pub fn other_filesystem(_e: &Entry, _root_dev: u64) -> bool {
    false
}

pub fn same_filesystem(_e: &Entry, _parent_dev: u64) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::mem::{offset_of, size_of};
    use std::os::windows::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Fresh error sink for the `read` signature.
    fn sink() -> ErrorCounts {
        ErrorCounts::default()
    }

    #[test]
    fn plain_listing_matches_batched() {
        let f = Fixture::new("plain");
        build(&f);
        let mut plain = Vec::new();
        read_plain(&f.0, &mut plain, &mut sink()).unwrap();
        let mut batched = Vec::new();
        DirReader::new()
            .read(&f.0, &mut batched, &mut sink())
            .unwrap();
        assert_eq!(plain.len(), batched.len());
        for e in &plain {
            let b = batched.iter().find(|b| b.name == e.name).unwrap();
            assert_eq!((e.size, e.is_dir()), (b.size, b.is_dir()));
            assert_eq!((e.alloc, e.file_id), (0, 0));
        }
        for e in &plain {
            let b = batched.iter().find(|b| b.name == e.name).unwrap();
            assert_eq!(is_skipped(e), is_skipped(b), "{:?}", e.name);
        }
    }

    #[test]
    fn struct_layouts() {
        assert_eq!(offset_of!(FileIdExtdDirInfo, end_of_file), 40);
        assert_eq!(offset_of!(FileIdExtdDirInfo, allocation_size), 48);
        assert_eq!(offset_of!(FileIdExtdDirInfo, file_attributes), 56);
        assert_eq!(offset_of!(FileIdExtdDirInfo, file_name_length), 60);
        assert_eq!(offset_of!(FileIdExtdDirInfo, ea_size), 64);
        assert_eq!(offset_of!(FileIdExtdDirInfo, reparse_point_tag), 68);
        assert_eq!(offset_of!(FileIdExtdDirInfo, file_id), 72);
        assert_eq!(offset_of!(FileIdExtdDirInfo, file_name), 88);
        assert_eq!(size_of::<FileIdExtdDirInfo>(), 96);
        assert_eq!(offset_of!(FileIdBothDirInfo, file_attributes), 56);
        assert_eq!(offset_of!(FileIdBothDirInfo, ea_size), 64);
        assert_eq!(offset_of!(FileIdBothDirInfo, short_name_length), 68);
        assert_eq!(offset_of!(FileIdBothDirInfo, short_name), 70);
        assert_eq!(offset_of!(FileIdBothDirInfo, file_id), 96);
        assert_eq!(offset_of!(FileIdBothDirInfo, file_name), 104);
        assert_eq!(size_of::<FileIdBothDirInfo>(), 112);
        assert_eq!(offset_of!(Win32FindStreamData, stream_name), 8);
        assert_eq!(size_of::<Win32FindStreamData>(), 600);
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(tag: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "dusage_enum_{tag}_{}_{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Fixture(p)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Remove a junction as a link first so its target is never traversed.
            let j = self.0.join("junc");
            let _ = fs::remove_dir(&j);
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn build(f: &Fixture) {
        fs::write(f.0.join("tiny.txt"), b"hi").unwrap();
        fs::write(f.0.join("big.bin"), vec![7u8; 200_000]).unwrap();
        fs::write(f.0.join("empty"), b"").unwrap();
        fs::write(f.0.join("üñí cödé.txt"), vec![1u8; 5000]).unwrap();
        fs::create_dir(f.0.join("sub")).unwrap();
        fs::write(f.0.join("sub").join("inner.txt"), b"x").unwrap();
    }

    fn read_all(dir: &Path) -> Vec<Entry> {
        let mut v = Vec::new();
        DirReader::new().read(dir, &mut v, &mut sink()).unwrap();
        v
    }

    fn compare_with_std(entries: &[Entry], dir: &Path) {
        let mut std_map = HashMap::new();
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            std_map.insert(e.file_name(), e.metadata().unwrap());
        }
        assert_eq!(entries.len(), std_map.len());
        for e in entries {
            let m = std_map
                .get(&e.name)
                .unwrap_or_else(|| panic!("extra {:?}", e.name));
            assert_eq!(e.is_dir(), m.is_dir(), "{:?}", e.name);
            if m.is_file() {
                assert_eq!(e.size, m.len(), "{:?}", e.name);
                assert!(e.alloc == 0 || e.alloc >= e.size, "{:?}", e.name);
            }
        }
    }

    #[test]
    fn matches_read_dir() {
        let f = Fixture::new("rd");
        build(&f);
        let v = read_all(&f.0);
        compare_with_std(&v, &f.0);
        let big = v.iter().find(|e| e.name == "big.bin").unwrap();
        assert_eq!(big.size, 200_000);
        assert!(big.alloc >= 200_000);
        assert!(v.iter().find(|e| e.name == "sub").unwrap().is_dir());
        assert!(v.iter().all(|e| !e.is_reparse()));
    }

    #[test]
    fn fallback_class_matches() {
        let f = Fixture::new("fb");
        build(&f);
        let mut r = DirReader::new();
        r.both = true;
        let mut v = Vec::new();
        r.read(&f.0, &mut v, &mut sink()).unwrap();
        compare_with_std(&v, &f.0);
        let mut w = Vec::new();
        DirReader::new().read(&f.0, &mut w, &mut sink()).unwrap();
        // 64-bit ids agree with the low 64 bits of the 128-bit ids on NTFS.
        for e in &v {
            let x = w.iter().find(|x| x.name == e.name).unwrap();
            assert_eq!(e.file_id as u64, x.file_id as u64);
            assert_eq!(e.size, x.size);
            assert_eq!(e.last_write, x.last_write);
        }
    }

    #[test]
    fn small_buffer_needs_multiple_calls() {
        let f = Fixture::new("many");
        for i in 0..300 {
            fs::write(
                f.0.join(format!("file_with_a_longish_name_{i:04}.dat")),
                b"a",
            )
            .unwrap();
        }
        let mut v = Vec::new();
        DirReader::with_buffer(4096)
            .read(&f.0, &mut v, &mut sink())
            .unwrap();
        assert_eq!(v.len(), 300);
        compare_with_std(&v, &f.0);
        // Reader is reusable across calls.
        let mut r = DirReader::new();
        let mut a = Vec::new();
        r.read(&f.0, &mut a, &mut sink()).unwrap();
        let mut b = Vec::new();
        r.read(&f.0, &mut b, &mut sink()).unwrap();
        assert_eq!(a.len(), b.len());
    }

    #[test]
    fn empty_dir_and_missing_dir() {
        let f = Fixture::new("empty");
        assert!(read_all(&f.0).is_empty());
        let mut v = Vec::new();
        assert!(DirReader::new()
            .read(&f.0.join("nope"), &mut v, &mut sink())
            .is_err());
    }

    #[test]
    fn junction_is_reparse() {
        let f = Fixture::new("junc");
        build(&f);
        let target = f.0.join("sub");
        let link = f.0.join("junc");
        let ok = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("skipped: could not create junction");
            return;
        }
        let v = read_all(&f.0);
        let j = v.iter().find(|e| e.name == "junc").unwrap();
        assert!(j.is_reparse());
        assert!(j.is_dir());
        assert_eq!(j.reparse_tag, sys::IO_REPARSE_TAG_MOUNT_POINT);
        let s = v.iter().find(|e| e.name == "sub").unwrap();
        assert!(!s.is_reparse());
        assert_eq!(s.reparse_tag, 0);
    }

    #[test]
    fn file_ids_and_hardlink() {
        let f = Fixture::new("ids");
        build(&f);
        fs::hard_link(f.0.join("big.bin"), f.0.join("big_link.bin")).unwrap();
        let v = read_all(&f.0);
        assert!(v.iter().all(|e| e.file_id != 0));
        let a = v.iter().find(|e| e.name == "big.bin").unwrap();
        let b = v.iter().find(|e| e.name == "big_link.bin").unwrap();
        assert_eq!(a.file_id, b.file_id);
        let mut ids: Vec<u128> = v
            .iter()
            .filter(|e| e.name != "big_link.bin")
            .map(|e| e.file_id)
            .collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n, "ids must be unique");
    }

    #[test]
    fn modified_matches_metadata() {
        let f = Fixture::new("mtime");
        build(&f);
        for e in read_all(&f.0) {
            let m = fs::metadata(f.0.join(&e.name)).unwrap().modified().unwrap();
            // NTFS refreshes a directory's timestamps in its parent index lazily, so only files must match exactly.
            if e.is_dir() {
                assert!(e.last_write > 0);
            } else {
                assert_eq!(e.modified(), m, "{:?}", e.name);
            }
        }
    }

    #[test]
    fn filetime_conversion() {
        assert_eq!(filetime_to_system_time(EPOCH_DIFF_TICKS), UNIX_EPOCH);
        assert_eq!(
            filetime_to_system_time(EPOCH_DIFF_TICKS + 10_000_000 + 5),
            UNIX_EPOCH + Duration::new(1, 500)
        );
        assert_eq!(
            filetime_to_system_time(EPOCH_DIFF_TICKS - 10_000_000),
            UNIX_EPOCH - Duration::from_secs(1)
        );
        let _ = filetime_to_system_time(0);
    }

    #[test]
    fn streams() {
        let f = Fixture::new("ads");
        let file = f.0.join("file.txt");
        fs::write(&file, b"hello").unwrap();
        assert_eq!(named_stream_bytes(&file).unwrap(), 0);
        if fs::write(f.0.join("file.txt:alt"), vec![0u8; 1234]).is_err() {
            eprintln!("skipped: no alternate streams on this filesystem");
            return;
        }
        fs::write(f.0.join("file.txt:b"), vec![0u8; 66]).unwrap();
        assert_eq!(named_stream_bytes(&file).unwrap(), 1234 + 66);
        assert_eq!(named_stream_bytes(&f.0).unwrap(), 0);
        assert!(named_stream_bytes(&f.0.join("missing")).is_err());
    }

    #[test]
    fn volume_and_cluster() {
        let f = Fixture::new("vol");
        let s1 = volume_serial(&f.0).unwrap();
        fs::write(f.0.join("a"), b"a").unwrap();
        assert_eq!(s1, volume_serial(&f.0.join("a")).unwrap());
        let c = cluster_size(&f.0).unwrap();
        assert!(c >= 512 && c.is_power_of_two(), "cluster {c}");
    }

    #[test]
    fn parser_rejects_corrupt_buffers() {
        let mut out = Vec::new();
        // Too short for a header.
        assert!(parse(&[0u8; 20], &EXTD, &mut out).is_err());
        // Name length overruns the buffer.
        let mut b = vec![0u8; 96];
        b[EXTD.name_len..EXTD.name_len + 4].copy_from_slice(&1000u32.to_le_bytes());
        assert!(parse(&b, &EXTD, &mut out).is_err());
        // Odd name length.
        let mut b = vec![0u8; 96];
        b[EXTD.name_len..EXTD.name_len + 4].copy_from_slice(&3u32.to_le_bytes());
        assert!(parse(&b, &EXTD, &mut out).is_err());
        // Next offset past the end.
        let mut b = vec![0u8; 96];
        b[0..4].copy_from_slice(&5000u32.to_le_bytes());
        assert!(parse(&b, &EXTD, &mut out).is_err());
        // Huge next offset must not overflow.
        let mut b = vec![0u8; 96];
        b[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse(&b, &EXTD, &mut out).is_err());
    }

    #[test]
    fn parser_skips_dots_and_reads_synthetic() {
        let entry = |name: &str, attrs: u32, eof: i64| {
            let mut b = vec![0u8; EXTD.name + name.len() * 2];
            let nb: Vec<u8> = name.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
            b[EXTD.name..].copy_from_slice(&nb);
            b[EXTD.attrs..EXTD.attrs + 4].copy_from_slice(&attrs.to_le_bytes());
            b[EXTD.name_len..EXTD.name_len + 4].copy_from_slice(&(nb.len() as u32).to_le_bytes());
            b[EXTD.eof..EXTD.eof + 8].copy_from_slice(&eof.to_le_bytes());
            b[EXTD.id..EXTD.id + 16].copy_from_slice(&0x0102u128.to_le_bytes());
            let t = EXTD.tag.unwrap();
            b[t..t + 4].copy_from_slice(&0xA000_0003u32.to_le_bytes());
            b
        };
        let mut buf = Vec::new();
        let items = [
            (".", 0x10, 0, false),
            ("..", 0x10, 0, false),
            ("a.txt", 0x20, 42, false),
            ("l", 0x410, 0, true),
        ];
        for (name, attrs, eof, last) in items {
            let mut e = entry(name, attrs, eof);
            let padded = e.len().div_ceil(8) * 8;
            e.resize(padded, 0);
            if !last {
                e[0..4].copy_from_slice(&(padded as u32).to_le_bytes());
            }
            buf.extend(e);
        }
        let mut out = Vec::new();
        parse(&buf, &EXTD, &mut out).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "a.txt");
        assert_eq!(out[0].size, 42);
        assert_eq!(out[0].file_id, 0x0102);
        assert_eq!(out[0].reparse_tag, 0);
        assert!(out[1].is_reparse() && out[1].is_dir());
        assert_eq!(out[1].reparse_tag, 0xA000_0003);
    }

    /// One synthetic `FILE_ID_*_DIR_INFO` record for `l`: `next` in NextEntryOffset, `name`
    /// as UTF-16 at `l.name`, and `name_len` in FileNameLength (which need not match the
    /// bytes staged). The slot is the header plus the name plus slack, zero-filled.
    fn rec(l: &Layout, next: u32, name: &str, name_len: u32) -> Vec<u8> {
        let nb: Vec<u8> = name.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
        let mut b = vec![0u8; l.name + nb.len() + 8];
        b[l.next..l.next + 4].copy_from_slice(&next.to_le_bytes());
        b[l.name_len..l.name_len + 4].copy_from_slice(&name_len.to_le_bytes());
        b[l.name..l.name + nb.len()].copy_from_slice(&nb);
        b
    }

    #[test]
    fn a_zero_length_name_is_rejected() {
        // `dir.join("")` names the directory itself, so a record that names nothing would make
        // the walk descend into it under its own path.
        for l in [&EXTD, &BOTH] {
            let mut out = Vec::new();
            let err = parse(&rec(l, 0, "", 0), l, &mut out).expect_err("empty name");
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
            assert!(out.is_empty());
        }
    }

    #[test]
    fn a_utf16_name_needs_no_terminator() {
        // FileNameLength, not a NUL, delimits the name: the UTF-16 need not end with 0.
        for l in [&EXTD, &BOTH] {
            let name = "üñí cödé.txt";
            let bytes = (name.encode_utf16().count() * 2) as u32;
            let mut out = Vec::new();
            parse(&rec(l, 0, name, bytes), l, &mut out).unwrap();
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].name, name);
            assert_eq!(out[0].name.encode_wide().count() * 2, bytes as usize);
        }
    }

    /// The Windows-only situation: these assertions run on a Windows host only (the parser is
    /// `cfg(windows)`), so the offsets they lean on are the ones the `const` layout assertions
    /// above already pinned at compile time — `file_name` at 88 for `FILE_ID_EXTD_DIR_INFO`
    /// and 104 for `FILE_ID_BOTH_DIR_INFO`, `end_of_file` at 40, `file_attributes` at 56.
    #[test]
    fn a_name_or_a_record_past_the_buffer_is_rejected() {
        for l in [&EXTD, &BOTH] {
            let mut out = Vec::new();
            let full = rec(l, 0, "ab", 4);
            // The same record parses while its name fits, so everything below is about the
            // bound: `name_end = file_name + FileNameLength` against the length of the batch.
            parse(&full, l, &mut out).unwrap();
            assert_eq!(out.len(), 1);
            out.clear();
            // FileNameLength claims 1000 bytes of name, far past the end of the batch.
            assert!(
                parse(&rec(l, 0, "ab", 1000), l, &mut out).is_err(),
                "name past the buffer"
            );
            // The batch ends in the middle of the name: 2 of the record's 4 name bytes.
            assert!(
                parse(&full[..l.name + 2], l, &mut out).is_err(),
                "truncated name"
            );
            // The batch ends in the middle of FileAttributes.
            assert!(
                parse(&full[..l.attrs + 2], l, &mut out).is_err(),
                "truncated header"
            );
            // FileNameLength ends between two UTF-16 code units.
            assert!(
                parse(&rec(l, 0, "a", 3), l, &mut out).is_err(),
                "odd length"
            );
            // No rejected record may leave an entry behind, named from bytes it does not own.
            assert!(out.is_empty(), "a rejected record appended {}", out.len());
        }
    }

    /// `NextEntryOffset` is kernel- and attacker-controlled: whatever it says, the walk has to
    /// come back, and every read it makes is a checked slice index, so a panic would be the
    /// out-of-bounds read this test is looking for.
    #[test]
    fn every_next_offset_and_name_length_terminates() {
        for next in [0u32, 1, 2, 4, 8, 88, 112, 255, 256, 1000, u32::MAX] {
            for name_len in [0u32, 1, 2, 4, 100, 1000, u32::MAX] {
                let mut b = vec![0u8; 256];
                b[EXTD.next..EXTD.next + 4].copy_from_slice(&next.to_le_bytes());
                b[EXTD.name..EXTD.name + 2].copy_from_slice(&(b'a' as u16).to_le_bytes());
                b[EXTD.name_len..EXTD.name_len + 4].copy_from_slice(&name_len.to_le_bytes());
                for l in [&EXTD, &BOTH] {
                    let mut out = Vec::new();
                    let _ = parse(&b, l, &mut out);
                }
            }
        }
    }

    /// `FileNameLength` is checked against the batch buffer, not against the record's own
    /// `NextEntryOffset`, so a record that understates its size can make its name borrow the
    /// next record's header. It cannot leave the buffer, which is why this is a correctness
    /// note and not a safety hole.
    #[test]
    fn a_name_may_borrow_the_next_record_but_never_leaves_the_buffer() {
        let mut b = vec![0u8; 224];
        b[EXTD.next..EXTD.next + 4].copy_from_slice(&112u32.to_le_bytes());
        // 40 bytes of name from offset 88: 24 inside record 1, 16 from record 2's header.
        b[EXTD.name_len..EXTD.name_len + 4].copy_from_slice(&40u32.to_le_bytes());
        for (i, c) in "ab".encode_utf16().enumerate() {
            b[EXTD.name + i * 2..EXTD.name + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
        let s = 112;
        b[s + EXTD.name_len..s + EXTD.name_len + 4].copy_from_slice(&4u32.to_le_bytes());
        for (i, c) in "cd".encode_utf16().enumerate() {
            b[s + EXTD.name + i * 2..s + EXTD.name + i * 2 + 2].copy_from_slice(&c.to_le_bytes());
        }
        let mut out = Vec::new();
        parse(&b, &EXTD, &mut out).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].name.encode_wide().count(),
            20,
            "borrowed past its own record"
        );
        assert_eq!(out[1].name, "cd");
    }

    #[test]
    fn rd_never_reads_past_the_buffer() {
        let buf = vec![0xFFu8; 24];
        for off in 0..=24 {
            assert_eq!(
                rd::<u32>(&buf, off).is_ok(),
                off + 4 <= buf.len(),
                "u32 at {off}"
            );
            assert_eq!(
                rd::<i64>(&buf, off).is_ok(),
                off + 8 <= buf.len(),
                "i64 at {off}"
            );
            assert_eq!(
                rd::<[u8; 16]>(&buf, off).is_ok(),
                off + 16 <= buf.len(),
                "[u8; 16] at {off}"
            );
        }
        assert!(rd::<u32>(&buf, usize::MAX).is_err(), "offset overflow");
    }
}
