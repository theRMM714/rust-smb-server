//! Tests of the rooted directory layer.

use std::io::{Read as _, Write as _};
use std::time::{Duration, SystemTime};

use tempfile::TempDir;

use super::{Dir, OpenOptions, ambient_authority};

/// The root of a new directory.
fn root(td: &TempDir) -> Dir {
    Dir::open_ambient_dir(td.path(), ambient_authority()).unwrap()
}

/// The options that read an existing file.
fn read_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    options
}

/// The options that create a new file and write it.
fn create_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    options
}

/// The names the entries of `dir` hold, sorted.
fn names(dir: &Dir) -> Vec<String> {
    let mut names: Vec<String> = dir
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// `names` as a vector of strings.
fn expected(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

#[test]
fn creates_writes_lists_renames_and_removes() {
    let td = tempfile::tempdir().unwrap();
    let root = root(&td);

    root.create_dir("d").unwrap();
    assert_eq!(names(&root), expected(&["d"]));

    let dir = Dir::open_ambient_dir(td.path().join("d"), ambient_authority()).unwrap();
    let mut file = root.open_with("d/f", &create_options()).unwrap();
    file.write_all(b"hello").unwrap();
    drop(file);
    assert_eq!(names(&dir), expected(&["f"]));

    let mut file = root.open_with("d/f", &read_options()).unwrap();
    let mut body = String::new();
    file.read_to_string(&mut body).unwrap();
    assert_eq!(body, "hello");
    drop(file);

    let metadata = root.metadata("d/f").unwrap();
    assert!(!metadata.file_type().is_dir());
    assert_eq!(metadata.len(), 5);

    root.rename("d/f", &root, "d/g").unwrap();
    assert_eq!(names(&dir), expected(&["g"]));
    assert!(root.metadata("d/f").is_err());

    root.remove_file("d/g").unwrap();
    assert!(names(&dir).is_empty());
    drop(dir);
    root.remove_dir("d").unwrap();
    assert!(names(&root).is_empty());
}

#[test]
fn removes_a_name_while_it_is_open() {
    let td = tempfile::tempdir().unwrap();
    let root = root(&td);
    root.create_dir("d").unwrap();
    let open = Dir::open_ambient_dir(td.path().join("d"), ambient_authority()).unwrap();

    root.remove_dir("d").unwrap();
    assert!(!td.path().join("d").exists());
    assert!(root.metadata("d").is_err());
    assert!(names(&root).is_empty());
    drop(open);
}

#[test]
fn creates_a_file_only_once() {
    let td = tempfile::tempdir().unwrap();
    let root = root(&td);

    root.open_with("f", &create_options()).unwrap();
    let again = root.open_with("f", &create_options());
    assert_eq!(again.unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
}

#[test]
fn refuses_a_path_above_the_root() {
    let td = tempfile::tempdir().unwrap();
    let root = root(&td);

    let err = root.open_with("../outside", &create_options()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    assert!(!td.path().parent().unwrap().join("outside").exists());

    root.create_dir("d").unwrap();
    let err = root.metadata("d/../../outside").unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn reports_metadata_of_a_file() {
    let td = tempfile::tempdir().unwrap();
    let root = root(&td);
    root.open_with("f", &create_options()).unwrap();

    let metadata = root.symlink_metadata("f").unwrap();
    assert_eq!(metadata.len(), 0);
    assert!(!metadata.permissions().readonly());

    let modified = metadata.modified().unwrap();
    let now = SystemTime::now();
    assert!(modified <= now + Duration::from_secs(60));
    assert!(modified + Duration::from_secs(60) >= now);
}

#[cfg(unix)]
mod unix {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn follows_a_symlink_below_the_root() {
        let td = tempfile::tempdir().unwrap();
        let root = root(&td);
        let mut file = root.open_with("f", &create_options()).unwrap();
        file.write_all(b"hello").unwrap();
        drop(file);
        symlink("f", td.path().join("link")).unwrap();

        let mut file = root.open_with("link", &read_options()).unwrap();
        let mut body = String::new();
        file.read_to_string(&mut body).unwrap();
        assert_eq!(body, "hello");
        assert_eq!(root.metadata("link").unwrap().len(), 5);
        assert!(root.symlink_metadata("link").unwrap().is_symlink());
    }

    #[test]
    fn refuses_an_absolute_symlink_target() {
        let td = tempfile::tempdir().unwrap();
        let root = root(&td);
        symlink("/etc/passwd", td.path().join("link")).unwrap();

        let err = root.open_with("link", &read_options()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            root.metadata("link").unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(root.symlink_metadata("link").unwrap().is_symlink());
    }

    #[test]
    fn refuses_a_symlink_target_above_the_root() {
        let td = tempfile::tempdir().unwrap();
        let root = root(&td);
        symlink("../outside", td.path().join("link")).unwrap();

        let err = root.open_with("link", &read_options()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn removes_a_tree() {
        let td = tempfile::tempdir().unwrap();
        let root = root(&td);
        root.create_dir("d").unwrap();
        let dir = root.open_dir("d").unwrap();
        dir.create_dir("e").unwrap();
        dir.open_with("e/f", &create_options()).unwrap();
        dir.open_with("g", &create_options()).unwrap();

        root.remove_dir_all("d").unwrap();
        assert!(names(&root).is_empty());
    }
}

#[cfg(windows)]
mod windows {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::*;

    /// `path` named through the administrative share of its volume, as a
    /// UNC path. None when the share is out of reach.
    fn unc(path: &Path) -> Option<PathBuf> {
        let mut components = path.components();
        let std::path::Component::Prefix(prefix) = components.next()? else {
            return None;
        };
        let letter = prefix.as_os_str().to_string_lossy();
        let drive = letter.strip_suffix(":").filter(|d| d.len() == 1)?;
        let share = PathBuf::from(format!(r"\localhost{drive}$"));
        if !share.is_dir() {
            return None;
        }
        let rest: PathBuf = components.collect();
        Some(share.join(rest))
    }

    #[test]
    fn works_through_a_network_share() {
        let td = tempfile::tempdir().unwrap();
        let Some(shared) = unc(td.path()) else {
            eprintln!("skipped: the administrative share is out of reach");
            return;
        };
        let root = Dir::open_ambient_dir(&shared, ambient_authority()).unwrap();

        root.create_dir("d").unwrap();
        assert!(td.path().join("d").is_dir());
        let mut file = root.open_with("d/f", &create_options()).unwrap();
        file.write_all(b"hello").unwrap();
        drop(file);
        assert_eq!(
            std::fs::read(td.path().join("d").join("f")).unwrap(),
            b"hello"
        );
        assert_eq!(root.metadata("d/f").unwrap().len(), 5);
        assert_eq!(names(&root), expected(&["d"]));

        root.rename("d/f", &root, "d/g").unwrap();
        assert!(td.path().join("d").join("g").is_file());
        root.remove_file("d/g").unwrap();
        assert!(!td.path().join("d").join("g").exists());
        root.remove_dir("d").unwrap();
        assert!(!td.path().join("d").exists());
    }

    #[test]
    fn refuses_a_junction_outside_the_root() {
        let td = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("f"), b"hello").unwrap();
        let link = td.path().join("j");
        let made = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");

        let root = root(&td);
        assert!(root.symlink_metadata("j").unwrap().file_type().is_symlink());
        let err = root.metadata("j").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        let err = root.open_with("j/f", &read_options()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }
}
