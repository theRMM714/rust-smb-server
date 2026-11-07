//! Local-filesystem [`ShareBackend`] for `smb-server`, sandboxed by a rooted
//! directory layer.

#[cfg(target_os = "linux")]
mod eas;
#[cfg(target_os = "linux")]
mod inotify;
mod local;
#[cfg(target_os = "macos")]
mod macos_streams;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod open_streams;
mod rooted;
#[cfg(target_os = "linux")]
mod streams;
#[cfg(windows)]
mod windows_eas;
#[cfg(windows)]
mod windows_streams;

pub use local::LocalFsBackend;

mod volume;
