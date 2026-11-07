//! NT primitives shared by the rooted directory layer and the code that
//! reaches named streams and extended attributes.
//!
//! Every call takes a handle and a path relative to it, so a caller that
//! holds a directory handle never has to turn it back into a string.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
};
use windows_sys::Win32::Foundation::{
    HANDLE, NTSTATUS, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_TAG_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FileAttributeTagInfo, GetFileInformationByHandleEx, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::{DeviceIoControl, IO_STATUS_BLOCK};
use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;

/// A reparse point that redirects to another path, a junction.
pub(crate) const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;

/// A reparse point that redirects to another path, a symbolic link.
pub(crate) const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

/// The tag of a reparse point that is a path, as opposed to data of another
/// filter.
pub(crate) fn is_name_surrogate(tag: u32) -> bool {
    tag == IO_REPARSE_TAG_SYMLINK || tag == IO_REPARSE_TAG_MOUNT_POINT
}

/// STATUS_NO_MORE_FILES: a directory listing reached the end.
pub(crate) const STATUS_NO_MORE_FILES: NTSTATUS = 0x8000_0006_u32 as NTSTATUS;

/// STATUS_NO_SUCH_FILE: a directory listing of an empty name.
pub(crate) const STATUS_NO_SUCH_FILE: NTSTATUS = 0xC000_000F_u32 as NTSTATUS;

/// The flag of a symbolic link that stores a relative target.
const SYMBOLIC_LINK_FLAG_RELATIVE: u32 = 1;

/// Bytes a reparse point may hold (MAXIMUM_REPARSE_DATA_BUFFER_SIZE).
const MAX_REPARSE: usize = 16 * 1024;

/// The error an NTSTATUS stands for.
pub(crate) fn nt_error(status: NTSTATUS) -> io::Error {
    // SAFETY: a pure conversion of a status code.
    let code = unsafe { RtlNtStatusToDosError(status) };
    io::Error::from_raw_os_error(code as i32)
}

/// The UTF-16 units of a name, as an NT object name is made of.
pub(crate) fn wide(name: &OsStr) -> Vec<u16> {
    name.encode_wide().collect()
}

/// Opens `name` relative to the open handle `root` with `access`,
/// `share`, `disposition` and `options`. Returns the handle and what
/// the open did.
pub(crate) fn nt_open(
    root: HANDLE,
    mut name: Vec<u16>,
    access: u32,
    share: u32,
    disposition: u32,
    options: u32,
) -> io::Result<(fs::File, usize)> {
    let bytes = u16::try_from(name.len() * 2).map_err(|_| io::ErrorKind::InvalidFilename)?;
    let unicode = UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes,
        Buffer: name.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: root,
        ObjectName: &unicode,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle: HANDLE = std::ptr::null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: every pointer refers to a live local that outlives the call:
    // `attributes` names `unicode`, whose buffer is `name`, and the root
    // is an open handle the caller holds. The call writes only `handle`
    // and `status_block`.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attributes,
            &mut status_block,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            share,
            disposition,
            options,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(nt_error(status));
    }
    // SAFETY: the call succeeded, so `handle` is an open handle that
    // nothing else owns.
    let file = unsafe { fs::File::from_raw_handle(handle as _) };
    Ok((file, status_block.Information))
}

/// A second handle of the file or directory `file` is open on, with
/// `access`.
pub(crate) fn reopen(file: &fs::File, access: u32) -> io::Result<fs::File> {
    let empty = UNICODE_STRING {
        Length: 0,
        MaximumLength: 0,
        Buffer: std::ptr::null_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: file.as_raw_handle() as HANDLE,
        ObjectName: &empty,
        Attributes: 0,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle: HANDLE = std::ptr::null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: `attributes` is a live local naming the live local `empty`,
    // and its root is the open handle `file` holds; the call writes only
    // `handle` and `status_block`.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access | SYNCHRONIZE,
            &attributes,
            &mut status_block,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(nt_error(status));
    }
    // SAFETY: the call succeeded, so `handle` is an open handle that
    // nothing else owns.
    Ok(unsafe { fs::File::from_raw_handle(handle as _) })
}

/// The attributes and the reparse tag of `file`.
pub(crate) fn attribute_tag(file: &fs::File) -> io::Result<(u32, u32)> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: `info` is a live local of the size passed, and the handle is
    // open for the call.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileAttributeTagInfo,
            (&raw mut info).cast(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((info.FileAttributes, info.ReparseTag))
}

/// Where a name surrogate reparse point leads.
#[derive(Debug)]
pub(crate) struct ReparseTarget {
    /// The path the reparse point stores, in UTF-16.
    pub(crate) name: Vec<u16>,
    /// The stored path is relative to the directory holding the reparse
    /// point, rather than absolute.
    pub(crate) relative: bool,
}

/// The stored path of the name surrogate reparse point `file` is open on.
pub(crate) fn reparse_target(file: &fs::File) -> io::Result<ReparseTarget> {
    let (_, tag) = attribute_tag(file)?;
    let mut buf = aligned(MAX_REPARSE);
    let mut returned = 0u32;
    // SAFETY: `buf` is a live buffer of `MAX_REPARSE` bytes, aligned to 8
    // as a `u64` buffer is; `returned` is a live local. No input buffer
    // is passed.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as HANDLE,
            FSCTL_GET_REPARSE_POINT,
            std::ptr::null(),
            0,
            buf.as_mut_ptr().cast(),
            MAX_REPARSE as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let bytes = bytes_of(&buf, returned as usize);
    let invalid = || io::Error::from(io::ErrorKind::InvalidData);
    let field = |at: usize| -> io::Result<u16> {
        bytes
            .get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .ok_or_else(invalid)
    };
    let (path_at, relative) = match tag {
        IO_REPARSE_TAG_SYMLINK => {
            let flags = bytes
                .get(16..20)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .ok_or_else(invalid)?;
            (20, flags & SYMBOLIC_LINK_FLAG_RELATIVE != 0)
        }
        IO_REPARSE_TAG_MOUNT_POINT => (16, false),
        _ => return Err(invalid()),
    };
    let offset = usize::from(field(8)?);
    let length = usize::from(field(10)?);
    let start = path_at + offset;
    let path = bytes.get(start..start + length).ok_or_else(invalid)?;
    let name = path
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(ReparseTarget { name, relative })
}

/// `target` as a path, refusing the NT path prefixes that name a volume.
pub(crate) fn target_path(target: &ReparseTarget) -> OsString {
    let mut units = target.name.clone();
    if !target.relative {
        let prefix: Vec<u16> = r"\??\".encode_utf16().collect();
        if units.starts_with(&prefix) {
            units.drain(..prefix.len());
        }
    }
    OsString::from_wide(&units)
}

/// A buffer of `len` bytes, aligned for the structures NT fills.
pub(crate) fn aligned(len: usize) -> Vec<u64> {
    vec![0; len.div_ceil(8)]
}

/// The first `len` bytes of `words`.
pub(crate) fn bytes_of(words: &[u64], len: usize) -> Vec<u8> {
    words
        .iter()
        .flat_map(|word| word.to_ne_bytes())
        .take(len)
        .collect()
}
