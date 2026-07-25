//! Windows-only: the per-entry facts a directory handle can supply and
//! `Metadata` cannot.
//!
//! `std::fs::Metadata` on Windows exposes neither a file's **allocated** size
//! nor its identity, and the path-based alternatives are poor:
//! `GetCompressedFileSize` returns the logical length for anything that is not
//! compressed or sparse (so slack space is invisible), and reading a file id
//! means opening every file.
//!
//! `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)` answers both at once
//! for an entire directory from a **single handle**, in one or a few calls:
//! each [`FILE_ID_BOTH_DIR_INFO`] carries `AllocationSize` ("the number of bytes
//! that are allocated for the file... usually a multiple of the sector or
//! cluster size"), `EndOfFile`, and `FileId`. Microsoft documents that no
//! specific access rights are required for the query.
//!
//! That makes this strictly cheaper than what it replaces: `GetCompressedFileSize`
//! takes a *path*, so the kernel resolves and opens the file on every call — one
//! per file. Here it is one handle per directory.
//!
//! The handle itself comes from `std::fs::OpenOptions` with
//! `FILE_FLAG_BACKUP_SEMANTICS` (the flag that permits opening a directory), so
//! path length, error mapping and closing stay in safe `std` code and the
//! `unsafe` below is confined to the FFI calls and walking their output buffer.

use crate::walk::{EntryFacts, FileId};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ID_BOTH_DIR_INFO, FileIdBothDirectoryInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx,
};

/// Lets `OpenOptions::open` accept a directory instead of failing with
/// "Access is denied"; not exported by `std`.
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

/// The enumeration is finished, not broken.
const ERROR_NO_MORE_FILES: i32 = 18;

/// 64 KiB, as `u64`s so the allocation is 8-byte aligned — the API requires
/// each `FILE_ID_BOTH_DIR_INFO` to sit on a `DWORDLONG` boundary. A short
/// buffer is not an error: the enumeration simply resumes on the next call.
const BUFFER_U64S: usize = 8 * 1024;

/// Read every entry of `dir`, keyed by file name.
///
/// Returns an empty map rather than an error when the directory cannot be
/// opened or queried: the caller already has a working per-file fallback, and a
/// scan must never fail over a missing optimisation.
pub(crate) fn facts(dir: &Path) -> HashMap<OsString, EntryFacts> {
    read_facts(dir).unwrap_or_default()
}

#[allow(unsafe_code)]
fn read_facts(dir: &Path) -> io::Result<HashMap<OsString, EntryFacts>> {
    let handle = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)?;

    // A file id is only unique within a volume, so identity needs both. Every
    // *file* listed here lives on this directory's volume — a mount point would
    // be a directory, and directories are never given an identity.
    let volume = volume_serial(&handle)?;

    let mut out = HashMap::new();
    let mut buffer = vec![0u64; BUFFER_U64S];
    let capacity = mem::size_of_val(buffer.as_slice());

    loop {
        // SAFETY: `buffer` is live and `capacity` is its true size in bytes; the
        // callee only writes into that range. `FileIdBothDirectoryInfo` is the
        // class matching the `FILE_ID_BOTH_DIR_INFO` we parse below.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle.as_raw_handle() as HANDLE,
                FileIdBothDirectoryInfo,
                buffer.as_mut_ptr().cast(),
                capacity as u32,
            )
        };
        if ok == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(ERROR_NO_MORE_FILES) {
                return Ok(out);
            }
            return Err(err);
        }
        collect_batch(&buffer, capacity, volume, &mut out);
    }
}

/// Walk one filled buffer, following the `NextEntryOffset` chain.
#[allow(unsafe_code)]
fn collect_batch(
    buffer: &[u64],
    capacity: usize,
    volume: u64,
    out: &mut HashMap<OsString, EntryFacts>,
) {
    let base: *const u8 = buffer.as_ptr().cast();
    let header = mem::size_of::<FILE_ID_BOTH_DIR_INFO>();
    let name_offset = mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
    let mut offset = 0usize;

    loop {
        // The API reports how far the next entry sits but never how much of the
        // buffer it filled, so every read is bounds-checked against the capacity
        // we passed in rather than trusted.
        if offset + header > capacity {
            return;
        }
        // SAFETY: the bounds check above keeps the whole struct inside the
        // buffer. `read_unaligned` because the 8-byte alignment the API
        // guarantees is weaker than this struct's own requirement.
        let info = unsafe {
            base.add(offset)
                .cast::<FILE_ID_BOTH_DIR_INFO>()
                .read_unaligned()
        };

        let name_bytes = info.FileNameLength as usize;
        if offset + name_offset + name_bytes <= capacity {
            // SAFETY: bounds-checked above; the name is `FileNameLength` bytes
            // of UTF-16 immediately following the fixed part of the struct.
            let name = unsafe {
                std::slice::from_raw_parts(
                    base.add(offset + name_offset).cast::<u16>(),
                    name_bytes / 2,
                )
            };
            let name = OsString::from_wide(name);
            if name != "." && name != ".." {
                out.insert(
                    name,
                    EntryFacts {
                        // Both are `i64` in the API and non-negative in practice;
                        // clamp rather than wrap if a filesystem ever disagrees.
                        allocated: info.AllocationSize.max(0) as u64,
                        apparent: info.EndOfFile.max(0) as u64,
                        id: FileId {
                            device: volume,
                            inode: info.FileId as u64,
                        },
                    },
                );
            }
        }

        if info.NextEntryOffset == 0 {
            return;
        }
        offset += info.NextEntryOffset as usize;
    }
}

/// The volume serial number behind an open handle — the other half of a file's
/// identity.
#[allow(unsafe_code)]
fn volume_serial(handle: &File) -> io::Result<u64> {
    // SAFETY: `info` is a live, writable struct of the type the callee expects,
    // and `handle` is open for the duration of the call.
    let mut info = unsafe { mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(handle.as_raw_handle() as HANDLE, &mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.dwVolumeSerialNumber as u64)
}
