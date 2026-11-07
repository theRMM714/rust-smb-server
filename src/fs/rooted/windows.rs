//! Rooted directory operations on Windows.
//!
//! Every path is resolved one component at a time against an open directory
//! handle with `NtCreateFile`, so the layer never turns a handle back into a
//! path string and hands it to a path-based API. That is what lets the same
//! code work on a local disk and on a network share, whose handles carry a
//! verbatim UNC path that those APIs cannot reopen.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::windows::ffi::OsStringExt as _;
use std::os::windows::fs::OpenOptionsExt as _;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, RawHandle};
use std::path::{Component, Path};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
    FILE_OPEN_FOR_BACKUP_INTENT, FILE_OPEN_IF, FILE_OPEN_REPARSE_POINT, FILE_OVERWRITE,
    FILE_OVERWRITE_IF, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
    FileFullDirectoryInformation, FileRenameInformation, NtQueryDirectoryFile,
    NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{FILETIME, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_POSIX_SEMANTICS, FILE_DISPOSITION_INFO_EX, FILE_GENERIC_WRITE,
    FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_TRAVERSE, FILE_WRITE_DATA, FileDispositionInfoEx, GetFileInformationByHandle,
    SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use super::AmbientAuthority;
use super::nt::{self, STATUS_NO_MORE_FILES, STATUS_NO_SUCH_FILE};

/// The maximum number of reparse points one path resolution may follow.
const MAX_SYMLINKS: u32 = 40;

/// Opens the file or directory itself, rather than what it redirects to.
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// Lets an open reach a directory.
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

/// Opens the file for overlapped I/O.
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;

/// The access a walk from one directory to the next needs.
const STEP_ACCESS: u32 = FILE_READ_DATA | FILE_READ_ATTRIBUTES | FILE_TRAVERSE;

const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

/// ERROR_FILE_NOT_FOUND: the name does not exist.
const ERROR_FILE_NOT_FOUND: i32 = 2;

/// ERROR_PATH_NOT_FOUND: a directory of the path does not exist.
const ERROR_PATH_NOT_FOUND: i32 = 3;

/// ERROR_INVALID_PARAMETER: options that ask for nothing to open with.
const ERROR_INVALID_PARAMETER: i32 = 87;

/// ERROR_DIRECTORY: the name is not a directory.
const ERROR_DIRECTORY: i32 = 267;

/// ERROR_CANT_RESOLVE_FILENAME: the chain of reparse points is too long.
const ERROR_CANT_RESOLVE_FILENAME: i32 = 1921;

/// Bytes one call of a listing reads.
const LISTING: usize = 64 << 10;

/// Offsets in a FILE_FULL_DIR_INFORMATION entry.
const DIR_ATTRIBUTES: usize = 56;
const DIR_NAME_LENGTH: usize = 60;
const DIR_NAME: usize = 68;

/// Ticks between the Windows epoch and the Unix epoch.
const WINDOWS_EPOCH_IN_TICKS: u64 = 11_644_473_600 * 10_000_000;

/// An open directory handle that relative paths are resolved from.
pub struct Dir {
    file: fs::File,
}

impl Dir {
    /// Opens a path as a directory.
    ///
    /// # Arguments
    ///
    /// * `path` - Path of the directory to open.
    ///
    /// # Returns
    ///
    /// A handle to the directory.
    pub fn open_ambient_dir(
        path: impl AsRef<Path>,
        _authority: AmbientAuthority,
    ) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let (attributes, _) = nt::attribute_tag(&file)?;

        if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(not_a_directory());
        }

        Ok(Self { file })
    }

    /// Wraps an already opened directory handle.
    ///
    /// # Arguments
    ///
    /// * `file` - Handle that must refer to a directory.
    ///
    /// # Returns
    ///
    /// A handle to the directory.
    pub fn from_std_file(file: fs::File) -> Self {
        Self { file }
    }

    /// Duplicates the handle.
    ///
    /// # Returns
    ///
    /// An independent handle to the same directory.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
        })
    }

    /// Reads the metadata of the directory itself.
    ///
    /// # Returns
    ///
    /// Metadata of the directory behind this handle.
    pub fn dir_metadata(&self) -> io::Result<Metadata> {
        Metadata::from_file(&self.file)
    }

    /// Opens a path with explicit options.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to this directory.
    /// * `options` - Access mode, creation mode and extra flags.
    ///
    /// # Returns
    ///
    /// The opened file.
    pub fn open_with(&self, path: impl AsRef<Path>, options: &OpenOptions) -> io::Result<File> {
        let allows_creation = options.create || options.create_new;
        let (parent, name) = self.resolve(path.as_ref(), options.follow, allows_creation)?;

        if name == OsStr::new(".") {
            let access = options.access()?;
            return Ok(File(nt::reopen(&parent.file, access)?));
        }

        let (access, disposition, create_options) = options.nt()?;
        let (file, _) = nt::nt_open(
            parent.handle(),
            nt::wide(&name),
            access,
            SHARE_ALL,
            disposition,
            create_options,
        )?;

        Ok(File(file))
    }

    /// Reads metadata of a path, following a reparse point in the last
    /// component.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to this directory.
    ///
    /// # Returns
    ///
    /// Metadata of whatever the path refers to.
    pub fn metadata(&self, path: impl AsRef<Path>) -> io::Result<Metadata> {
        let (parent, name) = self.resolve(path.as_ref(), true, false)?;
        metadata_at(&parent, &name, true)
    }

    /// Reads metadata of a path without following a reparse point in the last
    /// component.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to this directory.
    ///
    /// # Returns
    ///
    /// Metadata of the entry itself.
    pub fn symlink_metadata(&self, path: impl AsRef<Path>) -> io::Result<Metadata> {
        let (parent, name) = self.resolve(path.as_ref(), false, false)?;
        metadata_at(&parent, &name, false)
    }

    /// Creates a directory.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to this directory.
    pub fn create_dir(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let (parent, name) = self.resolve(path.as_ref(), false, false)?;
        nt::nt_open(
            parent.handle(),
            nt::wide(&name),
            FILE_READ_DATA | FILE_READ_ATTRIBUTES,
            SHARE_ALL,
            FILE_CREATE,
            FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        )?;

        Ok(())
    }

    /// Removes a file without following a reparse point in the last
    /// component.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to this directory.
    pub fn remove_file(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let (parent, name) = self.resolve(path.as_ref(), false, false)?;
        let (file, _) = nt::nt_open(
            parent.handle(),
            nt::wide(&name),
            DELETE,
            SHARE_ALL,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        )?;

        delete(&file)
    }

    /// Removes an empty directory.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to this directory.
    pub fn remove_dir(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let (parent, name) = self.resolve(path.as_ref(), false, false)?;
        let (dir, _) = nt::nt_open(
            parent.handle(),
            nt::wide(&name),
            DELETE,
            SHARE_ALL,
            FILE_OPEN,
            FILE_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        )?;

        delete(&dir)
    }

    /// Renames a path.
    ///
    /// # Arguments
    ///
    /// * `from` - Source path relative to this directory.
    /// * `to_dir` - Directory the destination is relative to, which may be
    ///   another rooted directory.
    /// * `to` - Destination path relative to `to_dir`.
    pub fn rename(
        &self,
        from: impl AsRef<Path>,
        to_dir: &Dir,
        to: impl AsRef<Path>,
    ) -> io::Result<()> {
        let (from_parent, from_name) = self.resolve(from.as_ref(), false, false)?;
        let (to_parent, to_name) = to_dir.resolve(to.as_ref(), false, false)?;
        let (file, _) = nt::nt_open(
            from_parent.handle(),
            nt::wide(&from_name),
            DELETE,
            SHARE_ALL,
            FILE_OPEN,
            FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        )?;
        rename_handle(&file, &to_parent, &to_name)
    }

    /// Lists the entries of this directory.
    ///
    /// # Returns
    ///
    /// The entries, excluding `.` and `..`.
    pub fn entries(&self) -> io::Result<Entries> {
        let root = Arc::new(self.try_clone()?);
        let mut buf = nt::aligned(LISTING);
        let mut first = true;
        let mut entries = Vec::new();

        loop {
            let mut status_block = IO_STATUS_BLOCK::default();
            // SAFETY: `buf` is a live buffer of `LISTING` bytes, aligned to 8
            // as a `u64` buffer is; `status_block` is a live local. The call
            // is synchronous, and a null filter lists every name.
            let status = unsafe {
                NtQueryDirectoryFile(
                    root.handle(),
                    std::ptr::null_mut(),
                    None,
                    std::ptr::null(),
                    &mut status_block,
                    buf.as_mut_ptr().cast(),
                    LISTING as u32,
                    FileFullDirectoryInformation,
                    false,
                    std::ptr::null(),
                    first,
                )
            };
            first = false;

            match status {
                STATUS_NO_MORE_FILES | STATUS_NO_SUCH_FILE => break,
                s if s < 0 => return Err(nt::nt_error(s)),
                _ => {}
            }

            let listed = nt::bytes_of(&buf, status_block.Information.min(LISTING));
            let mut at = 0usize;

            while let Some(entry) = listed.get(at..at + DIR_NAME) {
                let field = |off: usize| {
                    u32::from_le_bytes(entry[off..off + 4].try_into().expect("four bytes"))
                };
                let next = field(0) as usize;
                let attributes = field(DIR_ATTRIBUTES);
                let name_len = field(DIR_NAME_LENGTH) as usize;

                if let Some(name) = listed.get(at + DIR_NAME..at + DIR_NAME + name_len) {
                    let units: Vec<u16> = name
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    let name = OsString::from_wide(&units);

                    if name.as_os_str() != OsStr::new(".") && name.as_os_str() != OsStr::new("..") {
                        let tag = if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                            tag_at(&root, &name).unwrap_or(0)
                        } else {
                            0
                        };

                        entries.push(DirEntry {
                            dir: Arc::clone(&root),
                            name,
                            file_type: FileType {
                                attributes,
                                reparse_tag: tag,
                            },
                        });
                    }
                }

                if next == 0 {
                    break;
                }

                at += next;
            }
        }

        Ok(Entries {
            inner: entries.into_iter(),
        })
    }

    /// The raw handle this directory is open on.
    fn handle(&self) -> HANDLE {
        self.file.as_raw_handle() as HANDLE
    }

    /// Splits a path into the directory handle that contains its last
    /// component and that component's name.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to this directory.
    /// * `follow_last` - Whether a reparse point in the last component is
    ///   followed.
    ///
    /// # Returns
    ///
    /// The containing directory and the final name; the name is `.` when the
    /// path refers to the directory itself.
    fn resolve(
        &self,
        path: &Path,
        follow_last: bool,
        allow_missing: bool,
    ) -> io::Result<(Dir, OsString)> {
        let mut todo = VecDeque::new();

        for component in path.components() {
            match component {
                Component::Normal(name) => todo.push_back(name.to_os_string()),
                Component::CurDir => {}
                Component::ParentDir => todo.push_back(OsString::from("..")),
                Component::RootDir | Component::Prefix(_) => return Err(escape_attempt()),
            }
        }

        let mut cur = self.try_clone()?;
        let mut stack: Vec<Dir> = Vec::new();
        let mut followed = 0;

        while let Some(name) = todo.pop_front() {
            if name == OsStr::new("..") {
                cur = match stack.pop() {
                    Some(parent) => parent,
                    None => return Err(escape_attempt()),
                };
                continue;
            }

            if todo.is_empty() && !follow_last {
                return Ok((cur, name));
            }

            let step = match step_at(&cur, &name) {
                Ok(step) => step,
                Err(error) if allow_missing && is_missing(&error) => {
                    if todo.is_empty() {
                        return Ok((cur, name));
                    }

                    return Err(error);
                }
                Err(error) => return Err(error),
            };

            match step {
                Step::Dir(next) => {
                    stack.push(cur);
                    cur = next;
                }
                Step::Link(target) => {
                    followed += 1;

                    if followed > MAX_SYMLINKS {
                        return Err(io::Error::from_raw_os_error(ERROR_CANT_RESOLVE_FILENAME));
                    }

                    push_target(&mut todo, target)?;
                }
                Step::File => {
                    if todo.is_empty() {
                        return Ok((cur, name));
                    }

                    return Err(not_a_directory());
                }
            }
        }

        Ok((cur, OsString::from(".")))
    }
}

impl std::fmt::Debug for Dir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dir")
            .field("handle", &self.file.as_raw_handle())
            .finish()
    }
}

impl AsHandle for Dir {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.file.as_handle()
    }
}

impl AsRawHandle for Dir {
    fn as_raw_handle(&self) -> RawHandle {
        self.file.as_raw_handle()
    }
}

/// A file opened through a `Dir`.
pub struct File(fs::File);

impl File {
    /// Returns the wrapped standard library file.
    ///
    /// # Returns
    ///
    /// The same file, as a `std::fs::File`.
    pub fn into_std(self) -> fs::File {
        self.0
    }
}

impl From<fs::File> for File {
    fn from(file: fs::File) -> Self {
        Self(file)
    }
}

impl std::ops::Deref for File {
    type Target = fs::File;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for File {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl AsHandle for File {
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

impl AsRawHandle for File {
    fn as_raw_handle(&self) -> RawHandle {
        self.0.as_raw_handle()
    }
}

impl std::fmt::Debug for File {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Options for opening a file through a `Dir`.
#[derive(Debug, Clone)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
    follow: bool,
    custom_flags: u32,
    access_mode: Option<u32>,
}

impl OpenOptions {
    /// Creates a set of options with everything disabled.
    pub fn new() -> Self {
        Self {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
            follow: true,
            custom_flags: 0,
            access_mode: None,
        }
    }

    /// Sets read access.
    ///
    /// # Arguments
    ///
    /// * `read` - Whether to grant read access.
    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    /// Sets write access.
    ///
    /// # Arguments
    ///
    /// * `write` - Whether to grant write access.
    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    /// Sets creation of a file that must not exist yet.
    ///
    /// # Arguments
    ///
    /// * `create_new` - Whether the open fails when the file exists.
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    /// Sets extra FILE_FLAG flags.
    ///
    /// # Arguments
    ///
    /// * `flags` - Flags such as FILE_FLAG_BACKUP_SEMANTICS.
    pub fn custom_flags(&mut self, flags: u32) -> &mut Self {
        self.custom_flags = flags;
        self
    }

    /// Sets the access mask, in place of the one the read, write and append
    /// modes ask for.
    ///
    /// # Arguments
    ///
    /// * `access` - The NT access mask to open with.
    pub fn access_mode(&mut self, access: u32) -> &mut Self {
        self.access_mode = Some(access);
        self
    }

    /// The access mask to open with.
    ///
    /// # Returns
    ///
    /// The mask the read, write and append modes ask for, or the one set
    /// through access_mode.
    fn access(&self) -> io::Result<u32> {
        if let Some(access) = self.access_mode {
            return Ok(access);
        }

        match (self.read, self.write, self.append) {
            (true, false, false) => Ok(GENERIC_READ),
            (false, true, false) => Ok(GENERIC_WRITE),
            (true, true, false) => Ok(GENERIC_READ | GENERIC_WRITE),
            (false, _, true) => Ok(FILE_GENERIC_WRITE & !FILE_WRITE_DATA),
            (true, _, true) => Ok(GENERIC_READ | (FILE_GENERIC_WRITE & !FILE_WRITE_DATA)),
            (false, false, false) => Err(invalid_parameter()),
        }
    }

    /// The access mask, disposition and create options to open with.
    ///
    /// # Returns
    ///
    /// The three arguments an NT open takes.
    fn nt(&self) -> io::Result<(u32, u32, u32)> {
        let access = self.access()? | FILE_READ_ATTRIBUTES;

        if !self.write && !self.append && (self.truncate || self.create || self.create_new) {
            return Err(invalid_parameter());
        }

        if self.append && self.truncate && !self.create_new {
            return Err(invalid_parameter());
        }

        let disposition = match (self.create, self.truncate, self.create_new) {
            (_, _, true) => FILE_CREATE,
            (true, true, false) => FILE_OVERWRITE_IF,
            (false, true, false) => FILE_OVERWRITE,
            (true, false, false) => FILE_OPEN_IF,
            (false, false, false) => FILE_OPEN,
        };
        let mut options = if self.custom_flags & FILE_FLAG_BACKUP_SEMANTICS != 0 {
            FILE_OPEN_FOR_BACKUP_INTENT
        } else {
            FILE_NON_DIRECTORY_FILE
        };

        if self.create_new || !self.follow || self.custom_flags & FILE_FLAG_OPEN_REPARSE_POINT != 0
        {
            options |= FILE_OPEN_REPARSE_POINT;
        }

        if self.custom_flags & FILE_FLAG_OVERLAPPED == 0 {
            options |= FILE_SYNCHRONOUS_IO_NONALERT;
        }

        Ok((access, disposition, options))
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// The kind of a directory entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileType {
    attributes: u32,
    reparse_tag: u32,
}

impl FileType {
    /// Reports whether this is a directory.
    pub fn is_dir(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    /// Reports whether this is a symbolic link or a junction.
    pub fn is_symlink(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            && nt::is_name_surrogate(self.reparse_tag)
    }
}

/// A directory entry produced by `Dir::entries`.
pub struct DirEntry {
    dir: Arc<Dir>,
    name: OsString,
    file_type: FileType,
}

impl DirEntry {
    /// Returns the name of the entry.
    pub fn file_name(&self) -> OsString {
        self.name.clone()
    }

    /// Returns the kind of the entry, without following a reparse point.
    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(self.file_type)
    }

    /// Reads metadata of the entry, without following a reparse point.
    ///
    /// # Returns
    ///
    /// Metadata of the entry itself.
    pub fn metadata(&self) -> io::Result<Metadata> {
        metadata_at(&self.dir, &self.name, false)
    }
}

impl std::fmt::Debug for DirEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirEntry")
            .field("name", &self.name)
            .field("file_type", &self.file_type)
            .finish()
    }
}

/// Iterator over the entries of a directory.
pub struct Entries {
    inner: std::vec::IntoIter<DirEntry>,
}

impl Iterator for Entries {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(Ok)
    }
}

/// Metadata of a file or directory.
#[derive(Clone)]
pub struct Metadata {
    attributes: u32,
    reparse_tag: u32,
    len: u64,
    created: SystemTime,
    accessed: SystemTime,
    modified: SystemTime,
    volume_serial_number: u32,
    file_index: u64,
    number_of_links: u32,
}

impl Metadata {
    /// Reads metadata of an open file.
    ///
    /// # Arguments
    ///
    /// * `file` - An open file handle.
    ///
    /// # Returns
    ///
    /// Metadata of the file behind the handle.
    pub fn from_file(file: &fs::File) -> io::Result<Self> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `info` is a live local of the size passed, and the handle is
        // open for the call.
        let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) };

        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        let (attributes, reparse_tag) =
            nt::attribute_tag(file).unwrap_or((info.dwFileAttributes, 0));

        Ok(Self {
            attributes,
            reparse_tag,
            len: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
            created: system_time(info.ftCreationTime)?,
            accessed: system_time(info.ftLastAccessTime)?,
            modified: system_time(info.ftLastWriteTime)?,
            volume_serial_number: info.dwVolumeSerialNumber,
            file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            number_of_links: info.nNumberOfLinks,
        })
    }

    /// Reports whether this is a directory.
    pub fn is_dir(&self) -> bool {
        self.file_type().is_dir()
    }

    /// Returns the size in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Returns the permission bits.
    pub fn permissions(&self) -> Permissions {
        Permissions {
            readonly: self.attributes & FILE_ATTRIBUTE_READONLY != 0,
        }
    }

    /// Returns the kind of the file.
    pub fn file_type(&self) -> FileType {
        FileType {
            attributes: self.attributes,
            reparse_tag: self.reparse_tag,
        }
    }

    /// Returns the last modification time.
    ///
    /// # Returns
    ///
    /// The modification time.
    pub fn modified(&self) -> io::Result<SystemTime> {
        Ok(self.modified)
    }

    /// Returns the last access time.
    ///
    /// # Returns
    ///
    /// The access time.
    pub fn accessed(&self) -> io::Result<SystemTime> {
        Ok(self.accessed)
    }

    /// Returns the creation time.
    ///
    /// # Returns
    ///
    /// The creation time.
    pub fn created(&self) -> io::Result<SystemTime> {
        Ok(self.created)
    }
}

impl std::fmt::Debug for Metadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Metadata")
            .field("attributes", &self.attributes)
            .field("len", &self.len)
            .field("file_index", &self.file_index)
            .field("number_of_links", &self.number_of_links)
            .finish()
    }
}

/// Platform specific extension methods for `Metadata`.
pub trait MetadataExt {
    /// Returns the FILE_ATTRIBUTE bits of the file.
    fn file_attributes(&self) -> u32;
}

impl MetadataExt for Metadata {
    fn file_attributes(&self) -> u32 {
        self.attributes
    }
}

/// The identity of a file on a volume, as the handle APIs report it.
pub trait WindowsByHandle {
    /// Returns the serial number of the volume holding the file.
    fn volume_serial_number(&self) -> Option<u32>;

    /// Returns the index of the file on its volume.
    fn file_index(&self) -> Option<u64>;

    /// Returns the number of hard links of the file.
    fn number_of_links(&self) -> Option<u32>;
}

impl WindowsByHandle for Metadata {
    fn volume_serial_number(&self) -> Option<u32> {
        Some(self.volume_serial_number)
    }

    fn file_index(&self) -> Option<u64> {
        Some(self.file_index)
    }

    fn number_of_links(&self) -> Option<u32> {
        Some(self.number_of_links)
    }
}

/// The permissions of a file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Permissions {
    readonly: bool,
}

impl Permissions {
    /// Reports whether the file is read-only.
    pub fn readonly(&self) -> bool {
        self.readonly
    }
}

/// One step of a path resolution.
enum Step {
    /// The name is a directory.
    Dir(Dir),
    /// The name is a reparse point that leads elsewhere.
    Link(nt::ReparseTarget),
    /// The name is a file.
    File,
}

/// Reads the entry `name` of `dir` through a handle, without following a
/// reparse point, and tells what it is.
///
/// # Arguments
///
/// * `dir` - The directory the name is relative to.
/// * `name` - The name to inspect.
///
/// # Returns
///
/// What the name is.
fn step_at(dir: &Dir, name: &OsStr) -> io::Result<Step> {
    let (file, _) = nt::nt_open(
        dir.handle(),
        nt::wide(name),
        STEP_ACCESS,
        SHARE_ALL,
        FILE_OPEN,
        FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
    )?;
    let (attributes, tag) = nt::attribute_tag(&file)?;

    if nt::is_name_surrogate(tag) {
        Ok(Step::Link(nt::reparse_target(&file)?))
    } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        Ok(Step::Dir(Dir { file }))
    } else {
        Ok(Step::File)
    }
}

/// Reads metadata of `name` in `dir`, following a reparse point when
/// `follow` is set.
///
/// # Arguments
///
/// * `dir` - The directory the name is relative to.
/// * `name` - The name to inspect.
/// * `follow` - Whether to follow a reparse point in `name`.
///
/// # Returns
///
/// Metadata of the name.
fn metadata_at(dir: &Dir, name: &OsStr, follow: bool) -> io::Result<Metadata> {
    if name == OsStr::new(".") {
        return Metadata::from_file(&dir.file);
    }

    let mut options = FILE_SYNCHRONOUS_IO_NONALERT;

    if !follow {
        options |= FILE_OPEN_REPARSE_POINT;
    }

    let (file, _) = nt::nt_open(
        dir.handle(),
        nt::wide(name),
        FILE_READ_ATTRIBUTES,
        SHARE_ALL,
        FILE_OPEN,
        options,
    )?;

    Metadata::from_file(&file)
}

/// The reparse tag of the entry `name` of `dir`.
///
/// # Arguments
///
/// * `dir` - The directory the name is relative to.
/// * `name` - The name of the entry.
///
/// # Returns
///
/// The tag the entry stores, or 0 when it is not a reparse point.
fn tag_at(dir: &Dir, name: &OsStr) -> io::Result<u32> {
    let (file, _) = nt::nt_open(
        dir.handle(),
        nt::wide(name),
        FILE_READ_ATTRIBUTES,
        SHARE_ALL,
        FILE_OPEN,
        FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
    )?;
    Ok(nt::attribute_tag(&file)?.1)
}

/// Prepends the components of a reparse point target to a path that is still
/// being resolved.
///
/// # Arguments
///
/// * `todo` - The components that still have to be resolved.
/// * `target` - The stored target of the reparse point.
///
/// # Returns
///
/// An error when the target is absolute.
fn push_target(todo: &mut VecDeque<OsString>, target: nt::ReparseTarget) -> io::Result<()> {
    if !target.relative {
        return Err(escape_attempt());
    }

    let path = nt::target_path(&target);
    let mut components = Vec::new();

    for component in Path::new(&path).components() {
        match component {
            Component::Normal(name) => components.push(name.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => components.push(OsString::from("..")),
            Component::RootDir | Component::Prefix(_) => return Err(escape_attempt()),
        }
    }

    for component in components.into_iter().rev() {
        todo.push_front(component);
    }

    Ok(())
}

/// Deletes `file`, and every name it has, at once.
///
/// The name disappears even while another handle of the file is open, which
/// is how Unix and SMB clients see a delete; `FILE_DELETE_ON_CLOSE` would
/// keep the name until the last handle closes and would drop the failure of a
/// directory that still has entries.
///
/// # Arguments
///
/// * `file` - Handle of the file to delete, opened with DELETE.
fn delete(file: &fs::File) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    };
    // SAFETY: `info` is a live local of the size passed, and `file` is an
    // open handle.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileDispositionInfoEx,
            (&raw const info).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO_EX>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Renames the file `file` is open on to `to` in `to_dir`, replacing an
/// existing destination.
///
/// # Arguments
///
/// * `file` - Handle of the file to rename, opened with DELETE.
/// * `to_dir` - Directory the destination is relative to.
/// * `to` - Destination name.
fn rename_handle(file: &fs::File, to_dir: &Dir, to: &OsStr) -> io::Result<()> {
    let name = nt::wide(to);
    let offset = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
    // The name and a NUL, which the structure holds one unit of.
    let size = offset + (name.len() + 1) * 2;
    // u64 units keep the structure aligned.
    let mut buf = vec![0u64; size.div_ceil(8)];
    let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    // SAFETY: `buf` holds `size` bytes, aligned for FILE_RENAME_INFORMATION:
    // the fixed part, then name.len() UTF-16 units from FileName on.
    unsafe {
        (*info).Anonymous.ReplaceIfExists = true;
        (*info).RootDirectory = to_dir.handle();
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            (&raw mut (*info).FileName).cast::<u16>(),
            name.len(),
        );
    }
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: `buf` is live and holds `size` bytes of the structure, and
    // `file` is an open handle.
    let status = unsafe {
        NtSetInformationFile(
            file.as_raw_handle() as HANDLE,
            &mut status_block,
            buf.as_ptr().cast(),
            size as u32,
            FileRenameInformation,
        )
    };
    if status < 0 {
        return Err(nt::nt_error(status));
    }
    Ok(())
}

/// Converts a Windows timestamp to a `SystemTime`.
///
/// # Arguments
///
/// * `time` - Ticks since 1601, in units of 100 ns.
///
/// # Returns
///
/// The matching `SystemTime`.
fn system_time(time: FILETIME) -> io::Result<SystemTime> {
    let ticks = (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    let (secs, nanos) = match ticks.checked_sub(WINDOWS_EPOCH_IN_TICKS) {
        Some(since) => (since / 10_000_000, (since % 10_000_000) * 100),
        None => {
            let before = WINDOWS_EPOCH_IN_TICKS - ticks;
            let time = UNIX_EPOCH
                .checked_sub(Duration::new(
                    before / 10_000_000,
                    ((before % 10_000_000) * 100) as u32,
                ))
                .ok_or_else(out_of_range)?;
            return Ok(time);
        }
    };
    let time = UNIX_EPOCH
        .checked_add(Duration::new(secs, nanos as u32))
        .ok_or_else(out_of_range)?;

    Ok(time)
}

/// The error for a timestamp that a `SystemTime` cannot hold.
fn out_of_range() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "timestamp is out of range")
}

/// The error for a name that is not a directory.
fn not_a_directory() -> io::Error {
    io::Error::from_raw_os_error(ERROR_DIRECTORY)
}

/// True if `error` reports a name that does not exist, which a creation
/// expects.
fn is_missing(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(ERROR_FILE_NOT_FOUND) | Some(ERROR_PATH_NOT_FOUND)
    )
}

/// The error for open options that ask for nothing to open with.
fn invalid_parameter() -> io::Error {
    io::Error::from_raw_os_error(ERROR_INVALID_PARAMETER)
}

/// Creates the error used when a path would leave the root.
fn escape_attempt() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "path escapes the rooted directory",
    )
}
