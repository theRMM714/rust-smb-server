//! Rooted filesystem operations.
//!
//! A [`Dir`] is an open directory handle, and every path handled here is
//! resolved from one. Paths are walked one component at a time against the
//! handle and are never turned back into an absolute string, so the layer
//! behaves the same on a local disk and on a network share.
//!
//! A symbolic link is followed only while its target stays below the root: an
//! absolute target is refused, and so is a `..` that would climb above the
//! root.

#[cfg(windows)]
pub(crate) mod nt;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{Dir, DirEntry, File, FileType, Metadata, MetadataExt, OpenOptions};
#[cfg(test)]
mod tests;

#[cfg(windows)]
pub use windows::{
    Dir, DirEntry, File, FileType, Metadata, MetadataExt, OpenOptions, WindowsByHandle,
};

/// Authority to open a path this process can already reach.
#[derive(Clone, Copy, Debug)]
pub struct AmbientAuthority;

/// Returns the authority to open a path the process can already reach.
pub fn ambient_authority() -> AmbientAuthority {
    AmbientAuthority
}
