//! Rooted directory operations.
//!
//! A `Dir` is an open directory handle that every path in this module is
//! resolved from. Paths are walked one component at a time with the `*at`
//! system calls and are never turned back into an absolute string, so an
//! operation on a share root behaves the same whether that root sits on a
//! local disk or on a network mount.
//!
//! Symlinks are followed only while their target stays below the root: an
//! absolute target is refused, and so is a `..` that would climb above the
//! root.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Component, Path};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, Dir as RawDir, FileType as RawFileType, Mode, OFlags};
use rustix::io::Errno;

use crate::fs::rooted::AmbientAuthority;

/// The maximum number of symlinks one path resolution may follow.
const MAX_SYMLINKS: u32 = 40;

/// An open directory handle that relative paths are resolved from.
pub struct Dir {
    file: fs::File,
}

impl Dir {
    /// Opens a path as a directory.
    ///
    /// # Arguments
    ///
    /// * `path` - Absolute path of the directory to open.
    ///
    /// # Returns
    ///
    /// A handle to the directory.
    pub fn open_ambient_dir(
        path: impl AsRef<Path>,
        _authority: AmbientAuthority,
    ) -> io::Result<Self> {
        let file = fs::File::open(path)?;

        if !file.metadata()?.is_dir() {
            return Err(io::Error::from_raw_os_error(Errno::NOTDIR.raw_os_error()));
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
    #[cfg(target_os = "linux")]
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

    /// Opens a path for reading.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to this directory.
    ///
    /// # Returns
    ///
    /// The opened file or directory.
    pub fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        self.open_with(path, OpenOptions::new().read(true))
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
        let (parent, name) = self.resolve(path.as_ref(), options.follow)?;
        let flags = options.to_oflags()?;
        let fd = rustix::fs::openat(&parent.file, &name, flags, Mode::from_bits_truncate(0o666))?;

        Ok(File(fs::File::from(fd)))
    }

    /// Opens a path as a directory.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to this directory.
    ///
    /// # Returns
    ///
    /// A handle to the directory.
    pub fn open_dir(&self, path: impl AsRef<Path>) -> io::Result<Self> {
        let (parent, name) = self.resolve(path.as_ref(), true)?;
        open_dir_at(&parent, &name)
    }

    /// Reads metadata of a path, following a symlink in the last component.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to this directory.
    ///
    /// # Returns
    ///
    /// Metadata of whatever the path refers to.
    pub fn metadata(&self, path: impl AsRef<Path>) -> io::Result<Metadata> {
        let (parent, name) = self.resolve(path.as_ref(), true)?;
        metadata_at(&parent, &name, AtFlags::empty())
    }

    /// Reads metadata of a path without following a symlink in the last
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
        let (parent, name) = self.resolve(path.as_ref(), false)?;
        metadata_at(&parent, &name, AtFlags::SYMLINK_NOFOLLOW)
    }

    /// Creates a directory.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to this directory.
    pub fn create_dir(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let (parent, name) = self.resolve(path.as_ref(), false)?;
        rustix::fs::mkdirat(&parent.file, &name, Mode::from_bits_truncate(0o777))?;
        Ok(())
    }

    /// Removes a file without following a symlink in the last component.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to this directory.
    pub fn remove_file(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let (parent, name) = self.resolve(path.as_ref(), false)?;
        rustix::fs::unlinkat(&parent.file, &name, AtFlags::empty())?;
        Ok(())
    }

    /// Removes an empty directory.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to this directory.
    pub fn remove_dir(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let (parent, name) = self.resolve(path.as_ref(), false)?;
        rustix::fs::unlinkat(&parent.file, &name, AtFlags::REMOVEDIR)?;
        Ok(())
    }

    /// Removes a directory and everything below it.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to this directory.
    #[cfg(target_os = "linux")]
    pub fn remove_dir_all(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let (parent, name) = self.resolve(path.as_ref(), false)?;
        remove_tree(&parent.open_dir(&name)?)?;
        rustix::fs::unlinkat(&parent.file, &name, AtFlags::REMOVEDIR)?;
        Ok(())
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
        let (from_parent, from_name) = self.resolve(from.as_ref(), false)?;
        let (to_parent, to_name) = to_dir.resolve(to.as_ref(), false)?;
        rustix::fs::renameat(&from_parent.file, &from_name, &to_parent.file, &to_name)?;
        Ok(())
    }

    /// Lists the entries of this directory.
    ///
    /// # Returns
    ///
    /// The entries, excluding `.` and `..`.
    pub fn entries(&self) -> io::Result<Entries> {
        let dir = Arc::new(self.try_clone()?);
        let mut reader = RawDir::read_from(&self.file)?;
        let mut entries = Vec::new();

        for entry in reader.by_ref() {
            let entry = entry?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes()).to_os_string();

            if name == OsStr::new(".") || name == OsStr::new("..") {
                continue;
            }

            let mut file_type = FileType {
                raw: entry.file_type(),
            };

            if file_type.raw == RawFileType::Unknown {
                file_type = metadata_at(self, &name, AtFlags::SYMLINK_NOFOLLOW)?.file_type();
            }

            entries.push(DirEntry {
                dir: Arc::clone(&dir),
                name,
                file_type,
            });
        }

        Ok(Entries {
            inner: entries.into_iter(),
        })
    }

    /// Splits a path into the directory handle that contains its last
    /// component and that component's name.
    ///
    /// # Arguments
    ///
    /// * `path` - Path relative to this directory.
    /// * `follow_last` - Whether a symlink in the last component is followed.
    ///
    /// # Returns
    ///
    /// The containing directory and the final name; the name is `.` when the
    /// path refers to the directory itself.
    fn resolve(&self, path: &Path, follow_last: bool) -> io::Result<(Dir, OsString)> {
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

            let is_last = todo.is_empty();

            match open_dir_at(&cur, &name) {
                Ok(next) => {
                    stack.push(cur);
                    cur = next;
                }
                Err(error) if is_not_a_directory(&error) || (is_last && is_missing(&error)) => {
                    let metadata = match metadata_at(&cur, &name, AtFlags::SYMLINK_NOFOLLOW) {
                        Ok(metadata) => metadata,
                        // A name that does not exist is handed to the caller,
                        // which is the only place that may create it.
                        Err(error) if is_last && is_missing(&error) => return Ok((cur, name)),
                        Err(error) => return Err(error),
                    };

                    if metadata.is_symlink() {
                        followed += 1;

                        if followed > MAX_SYMLINKS {
                            return Err(io::Error::from_raw_os_error(Errno::LOOP.raw_os_error()));
                        }

                        let target = read_link_at(&cur, &name)?;
                        push_target(&mut todo, target)?;
                    } else if is_last {
                        return Ok((cur, name));
                    } else {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }

        Ok((cur, OsString::from(".")))
    }
}

impl fmt::Debug for Dir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dir")
            .field("fd", &self.file.as_raw_fd())
            .finish()
    }
}

impl AsFd for Dir {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl AsRawFd for Dir {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
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

impl AsFd for File {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl AsRawFd for File {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl fmt::Debug for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
    custom_flags: i32,
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

    /// Sets truncation of an existing file.
    ///
    /// # Arguments
    ///
    /// * `truncate` - Whether to truncate the file on open.
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
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

    /// Sets extra `O_*` flags.
    ///
    /// # Arguments
    ///
    /// * `flags` - Flags such as `O_PATH` or `O_NONBLOCK`.
    pub fn custom_flags(&mut self, flags: i32) -> &mut Self {
        self.custom_flags = flags;
        self
    }

    /// Converts the options to `openat` flags.
    ///
    /// # Returns
    ///
    /// The flags to pass to `openat`.
    fn to_oflags(&self) -> io::Result<OFlags> {
        let mut flags = OFlags::CLOEXEC;

        match (self.read, self.write, self.append) {
            (_, true, true) => flags |= OFlags::RDWR | OFlags::APPEND,
            (true, false, true) => flags |= OFlags::RDWR | OFlags::APPEND,
            (false, false, true) => flags |= OFlags::WRONLY | OFlags::APPEND,
            (true, true, false) => flags |= OFlags::RDWR,
            (false, true, false) => flags |= OFlags::WRONLY,
            (true, false, false) => flags |= OFlags::RDONLY,
            (false, false, false) => {
                if !self.create && !self.create_new && !self.truncate {
                    return Err(io::Error::from_raw_os_error(Errno::INVAL.raw_os_error()));
                }

                flags |= OFlags::RDONLY;
            }
        }

        if self.create_new {
            flags |= OFlags::CREATE | OFlags::EXCL;
        } else if self.create {
            flags |= OFlags::CREATE;
        }

        if self.truncate {
            flags |= OFlags::TRUNC;
        }

        if !self.follow {
            flags |= OFlags::NOFOLLOW;
        }

        flags |= OFlags::from_bits_retain(self.custom_flags.cast_unsigned());

        Ok(flags)
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
    raw: RawFileType,
}

impl FileType {
    /// Reports whether this is a directory.
    pub fn is_dir(&self) -> bool {
        self.raw.is_dir()
    }

    /// Reports whether this is a regular file.
    pub fn is_file(&self) -> bool {
        self.raw.is_file()
    }

    /// Reports whether this is a symlink.
    pub fn is_symlink(&self) -> bool {
        self.raw.is_symlink()
    }

    /// Reports whether this is a FIFO.
    pub fn is_fifo(&self) -> bool {
        self.raw.is_fifo()
    }

    /// Reports whether this is a socket.
    pub fn is_socket(&self) -> bool {
        self.raw.is_socket()
    }

    /// Reports whether this is a character device.
    pub fn is_char_device(&self) -> bool {
        self.raw.is_char_device()
    }

    /// Reports whether this is a block device.
    pub fn is_block_device(&self) -> bool {
        self.raw.is_block_device()
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

    /// Returns the kind of the entry, without following a symlink.
    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(self.file_type)
    }

    /// Reads metadata of the entry, without following a symlink.
    ///
    /// # Returns
    ///
    /// Metadata of the entry itself.
    pub fn metadata(&self) -> io::Result<Metadata> {
        metadata_at(&self.dir, &self.name, AtFlags::SYMLINK_NOFOLLOW)
    }
}

impl fmt::Debug for DirEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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
    file_type: FileType,
    len: u64,
    permissions: Permissions,
    atime: (i64, i64),
    mtime: (i64, i64),
    birth: Option<(i64, i64)>,
    dev: u64,
    ino: u64,
    nlink: u64,
    mode: u32,
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
        #[cfg(target_os = "linux")]
        {
            use rustix::fs::{StatxFlags, statx};

            match statx(
                file,
                "",
                AtFlags::EMPTY_PATH,
                StatxFlags::BASIC_STATS | StatxFlags::BTIME,
            ) {
                Ok(statx) => return Self::from_statx(&statx),
                Err(Errno::NOSYS | Errno::PERM) => {}
                Err(error) => return Err(error.into()),
            }
        }

        Self::from_stat(&rustix::fs::fstat(file)?)
    }

    /// Builds metadata from a `stat` result.
    ///
    /// # Arguments
    ///
    /// * `stat` - The result of `statat` or `fstat`.
    ///
    /// # Returns
    ///
    /// The matching metadata.
    // The fields of `Stat` are narrower on some Unix hosts than the types
    // used here, so the casts are needed there and redundant on Linux.
    #[allow(clippy::unnecessary_cast)]
    fn from_stat(stat: &rustix::fs::Stat) -> io::Result<Self> {
        Ok(Self {
            file_type: FileType {
                raw: RawFileType::from_raw_mode(stat.st_mode as _),
            },
            len: stat.st_size as u64,
            permissions: Permissions {
                mode: stat.st_mode as u32,
            },
            atime: (stat.st_atime as i64, stat.st_atime_nsec as i64),
            mtime: (stat.st_mtime as i64, stat.st_mtime_nsec as i64),
            birth: None,
            dev: stat.st_dev as u64,
            ino: stat.st_ino as u64,
            nlink: stat.st_nlink as u64,
            mode: stat.st_mode as u32,
        })
    }

    /// Builds metadata from a `statx` result.
    ///
    /// # Arguments
    ///
    /// * `statx` - The result of `statx`.
    ///
    /// # Returns
    ///
    /// The matching metadata.
    #[cfg(target_os = "linux")]
    fn from_statx(statx: &rustix::fs::Statx) -> io::Result<Self> {
        use rustix::fs::StatxFlags;

        let birth = (statx.stx_mask & StatxFlags::BTIME.bits() != 0)
            .then_some((statx.stx_btime.tv_sec, i64::from(statx.stx_btime.tv_nsec)));

        Ok(Self {
            file_type: FileType {
                raw: RawFileType::from_raw_mode(u32::from(statx.stx_mode) as _),
            },
            len: statx.stx_size,
            permissions: Permissions {
                mode: u32::from(statx.stx_mode),
            },
            atime: (statx.stx_atime.tv_sec, i64::from(statx.stx_atime.tv_nsec)),
            mtime: (statx.stx_mtime.tv_sec, i64::from(statx.stx_mtime.tv_nsec)),
            birth,
            dev: rustix::fs::makedev(statx.stx_dev_major, statx.stx_dev_minor),
            ino: statx.stx_ino,
            nlink: u64::from(statx.stx_nlink),
            mode: u32::from(statx.stx_mode),
        })
    }

    /// Reports whether this is a directory.
    pub fn is_dir(&self) -> bool {
        self.file_type.is_dir()
    }

    /// Reports whether this is a regular file.
    pub fn is_file(&self) -> bool {
        self.file_type.is_file()
    }

    /// Reports whether this is a symlink.
    pub fn is_symlink(&self) -> bool {
        self.file_type.is_symlink()
    }

    /// Returns the size in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Returns the permission bits.
    pub fn permissions(&self) -> Permissions {
        self.permissions
    }

    /// Returns the kind of the file.
    pub fn file_type(&self) -> FileType {
        self.file_type
    }

    /// Returns the last modification time.
    ///
    /// # Returns
    ///
    /// The modification time, or an error when the file system did not report
    /// a representable one.
    pub fn modified(&self) -> io::Result<SystemTime> {
        system_time(self.mtime)
    }

    /// Returns the last access time.
    ///
    /// # Returns
    ///
    /// The access time, or an error when the file system did not report a
    /// representable one.
    pub fn accessed(&self) -> io::Result<SystemTime> {
        system_time(self.atime)
    }

    /// Returns the creation time.
    ///
    /// # Returns
    ///
    /// The creation time, or an error when the file system does not record
    /// it.
    pub fn created(&self) -> io::Result<SystemTime> {
        match self.birth {
            Some(birth) => system_time(birth),
            None => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the file system does not record creation times",
            )),
        }
    }
}

impl fmt::Debug for Metadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Metadata")
            .field("file_type", &self.file_type)
            .field("len", &self.len)
            .field("mode", &self.mode)
            .field("dev", &self.dev)
            .field("ino", &self.ino)
            .field("nlink", &self.nlink)
            .finish()
    }
}

/// Platform specific extension methods for `Metadata`.
pub trait MetadataExt {
    /// Returns the device of the file.
    fn dev(&self) -> u64;

    /// Returns the inode of the file.
    fn ino(&self) -> u64;

    /// Returns the raw `st_mode` of the file.
    fn mode(&self) -> u32;

    /// Returns the number of hard links.
    fn nlink(&self) -> u64;
}

impl MetadataExt for Metadata {
    fn dev(&self) -> u64 {
        self.dev
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn mode(&self) -> u32 {
        self.mode
    }

    fn nlink(&self) -> u64 {
        self.nlink
    }
}

/// The permission bits of a file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Permissions {
    mode: u32,
}

impl Permissions {
    /// Reports whether no write bit is set.
    pub fn readonly(&self) -> bool {
        self.mode & 0o222 == 0
    }
}

/// Opens a directory relative to another one, without following a symlink.
///
/// # Arguments
///
/// * `dir` - The directory the name is relative to.
/// * `name` - The name of the directory to open.
///
/// # Returns
///
/// A handle to the opened directory.
fn open_dir_at(dir: &Dir, name: &OsStr) -> io::Result<Dir> {
    let fd = rustix::fs::openat(
        &dir.file,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;

    Ok(Dir {
        file: fs::File::from(fd),
    })
}

/// Reads metadata relative to a directory.
///
/// # Arguments
///
/// * `dir` - The directory the name is relative to.
/// * `name` - The name to inspect.
/// * `flags` - Whether to follow a symlink in `name`.
///
/// # Returns
///
/// Removes every entry of `dir`, leaving the directory itself.
///
/// # Arguments
///
/// * `dir` - The directory to empty.
///
/// # Returns
///
/// The error of the first entry that could not be removed.
#[cfg(target_os = "linux")]
fn remove_tree(dir: &Dir) -> io::Result<()> {
    for entry in dir.entries()? {
        let entry = entry?;
        let name = entry.file_name();
        let ty = entry.file_type()?;

        if ty.is_dir() {
            remove_tree(&dir.open_dir(&name)?)?;
            rustix::fs::unlinkat(&dir.file, &name, AtFlags::REMOVEDIR)?;
        } else {
            rustix::fs::unlinkat(&dir.file, &name, AtFlags::empty())?;
        }
    }

    Ok(())
}

/// Metadata of the name.
fn metadata_at(dir: &Dir, name: &OsStr, flags: AtFlags) -> io::Result<Metadata> {
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{StatxFlags, statx};

        match statx(
            &dir.file,
            name,
            flags,
            StatxFlags::BASIC_STATS | StatxFlags::BTIME,
        ) {
            Ok(statx) => return Metadata::from_statx(&statx),
            Err(Errno::NOSYS | Errno::PERM) => {}
            Err(error) => return Err(error.into()),
        }
    }

    Metadata::from_stat(&rustix::fs::statat(&dir.file, name, flags)?)
}

/// Reads a symlink target relative to a directory.
///
/// # Arguments
///
/// * `dir` - The directory the name is relative to.
/// * `name` - The name of the symlink.
///
/// # Returns
///
/// The target of the symlink.
fn read_link_at(dir: &Dir, name: &OsStr) -> io::Result<OsString> {
    let target = rustix::fs::readlinkat(&dir.file, name, Vec::new())?;
    Ok(OsString::from_vec(target.into_bytes()))
}

/// Prepends the components of a symlink target to a path that is still being
/// resolved.
///
/// # Arguments
///
/// * `todo` - The components that still have to be resolved.
/// * `target` - The target of the symlink.
///
/// # Returns
///
/// An error when the target is absolute.
fn push_target(todo: &mut VecDeque<OsString>, target: OsString) -> io::Result<()> {
    let mut components = Vec::new();

    for component in Path::new(&target).components() {
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

/// Reports whether an `openat` error means the name is not a directory,
/// which is how a symlink shows up when `O_DIRECTORY` is set.
///
/// # Arguments
///
/// * `error` - The error returned by `openat`.
fn is_not_a_directory(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code == Errno::NOTDIR.raw_os_error() || code == Errno::LOOP.raw_os_error()
    )
}

/// Reports whether an error means the name does not exist.
fn is_missing(error: &io::Error) -> bool {
    error.raw_os_error() == Some(Errno::NOENT.raw_os_error())
}

/// Creates the error used when a path would leave the root.
fn escape_attempt() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "path escapes the rooted directory",
    )
}

/// Converts a platform timestamp to a `SystemTime`.
///
/// # Arguments
///
/// * `(sec, nsec)` - Seconds and nanoseconds since the Unix epoch.
///
/// # Returns
///
/// The matching `SystemTime`.
fn system_time((sec, nsec): (i64, i64)) -> io::Result<SystemTime> {
    let nanos = u32::try_from(nsec.clamp(0, 999_999_999)).unwrap_or(0);
    let time = if sec >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(sec.unsigned_abs(), nanos))
    } else {
        UNIX_EPOCH.checked_sub(Duration::new(sec.unsigned_abs(), 0))
    };

    time.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "timestamp is out of range"))
}
