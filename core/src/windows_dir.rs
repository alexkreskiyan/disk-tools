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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// Seconds between the FILETIME epoch (1601-01-01) and the Unix one.
const FILETIME_EPOCH_OFFSET: u64 = 11_644_473_600;

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
                        modified: filetime(info.LastWriteTime),
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

/// Convert a FILETIME — 100-nanosecond ticks since 1601-01-01 — to a
/// [`SystemTime`].
///
/// `None` rather than a guess whenever the value cannot be trusted: `0` is the
/// documented "not recorded" marker, a negative tick count is meaningless, and
/// an addition that would overflow means the value is not a real timestamp
/// either. The scan reports "unknown" for such an entry, which the age rule then
/// declines to match — absence of evidence, not evidence of age.
///
/// Pre-1970 timestamps are ordinary here (the epoch is 1601), so they subtract
/// from [`UNIX_EPOCH`] rather than being rejected.
fn filetime(ticks: i64) -> Option<SystemTime> {
    let ticks = u64::try_from(ticks).ok()?;
    if ticks == 0 {
        return None;
    }

    let secs = ticks / 10_000_000;
    let nanos = (ticks % 10_000_000) as u32 * 100;

    match secs.checked_sub(FILETIME_EPOCH_OFFSET) {
        Some(since_epoch) => UNIX_EPOCH.checked_add(Duration::new(since_epoch, nanos)),
        // Before 1970: step back the whole seconds, then forward by the
        // sub-second remainder, which still belongs after that instant.
        None => UNIX_EPOCH
            .checked_sub(Duration::from_secs(FILETIME_EPOCH_OFFSET - secs))
            .and_then(|t| t.checked_add(Duration::from_nanos(nanos as u64))),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The epoch offset is the one number in the conversion that a typo would
    /// silently shift by decades, so it is checked against the exact tick count
    /// Microsoft documents for 1970-01-01.
    #[test]
    fn the_unix_epoch_converts_to_the_unix_epoch() {
        const UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;

        assert_eq!(filetime(UNIX_EPOCH_TICKS), Some(UNIX_EPOCH));
    }

    #[test]
    fn sub_second_ticks_survive() {
        const UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;
        // One second and a half past the Unix epoch.
        let ticks = UNIX_EPOCH_TICKS + 15_000_000;

        assert_eq!(
            filetime(ticks),
            Some(UNIX_EPOCH + Duration::new(1, 500_000_000)),
            "the conversion must keep the 100 ns remainder, not truncate to seconds"
        );
    }

    /// The FILETIME epoch is 1601, so a timestamp before 1970 is an ordinary
    /// value here and must come back as a real instant rather than `None`.
    #[test]
    fn a_pre_1970_timestamp_is_representable() {
        // Exactly one second before the Unix epoch.
        let ticks = 116_444_736_000_000_000 - 10_000_000;

        assert_eq!(filetime(ticks), Some(UNIX_EPOCH - Duration::from_secs(1)));
    }

    #[test]
    fn unusable_values_are_none() {
        assert_eq!(
            filetime(0),
            None,
            "0 is the documented 'not recorded' marker"
        );
        assert_eq!(filetime(-1), None, "a negative tick count is meaningless");
    }

    /// Whether `SystemTime` can hold a year-30000 instant is platform-specific,
    /// so this pins the two things that matter regardless: the conversion does
    /// not panic, and an absurd input cannot wrap into a plausible date.
    #[test]
    fn an_absurd_tick_count_never_becomes_a_believable_time() {
        const A_CENTURY: Duration = Duration::from_secs(100 * 365 * 24 * 3600);

        if let Some(instant) = filetime(i64::MAX) {
            assert!(
                instant > UNIX_EPOCH + A_CENTURY,
                "an overflowing tick count must not land near the present day"
            );
        }
    }
}
