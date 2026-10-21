//! The private files next to the database that Harness Desktop and the server share: the run
//! requests ([`crate::mcp`] script approvals), the save guards and the held items
//! ([`crate::held`]).
//!
//! On Unix the folders must be folders of this user that no one else may write to (`0700` when
//! the server makes them), the files are `0600`, written to a temporary file and renamed, and read
//! without following a symbolic link. Elsewhere the same calls skip the owner and mode checks.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::Env;

/// The file Harness Desktop keeps touching while it runs, next to the database.
pub const ALIVE_FILE: &str = "desktop.alive";

/// How fresh [`ALIVE_FILE`] must be for the desktop to count as running.
const ALIVE_FOR: Duration = Duration::from_secs(15);

/// The folder that holds the database, where Harness Desktop and the server meet.
pub fn data_folder(env: &Env) -> Option<PathBuf> {
    crate::database_path(env).and_then(|path| path.parent().map(Path::to_path_buf))
}

/// True when [`ALIVE_FILE`] in `folder` was touched less than 15 seconds ago (or is dated at most
/// that far ahead, for a clock set back).
pub fn desktop_alive(folder: &Path) -> bool {
    let Ok(touched) = std::fs::metadata(folder.join(ALIVE_FILE)).and_then(|metadata| metadata.modified()) else {
        return false;
    };

    match SystemTime::now().duration_since(touched) {
        Ok(age) => age < ALIVE_FOR,
        Err(ahead) => ahead.duration() < ALIVE_FOR,
    }
}

/// Creates `folder` (shown as `name` in errors) when missing and makes it `0700` on Unix
/// (failing for a folder another user owns).
pub fn private_folder(folder: &Path, name: &str) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);

    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);

    builder
        .create(folder)
        .map_err(|error| format!("creating the {name} failed: {error}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("securing the {name} failed: {error}"))?;
    }

    Ok(())
}

/// Fails unless each of `folders` (a path and its name in errors) is a folder (not a symbolic
/// link) owned by this user that no one else may write to, so no other user can read or swap a
/// file in it.
#[cfg(unix)]
pub fn check_folders(folders: &[(&Path, &str)]) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    let user = nix::unistd::getuid().as_raw();

    for (folder, name) in folders {
        let metadata =
            std::fs::symlink_metadata(folder).map_err(|error| format!("the {name} cannot be read: {error}"))?;

        if !metadata.file_type().is_dir() {
            return Err(format!("the {name} is not a folder"));
        }

        if metadata.uid() != user {
            return Err(format!("the {name} belongs to another user"));
        }

        if metadata.mode() & 0o022 != 0 {
            return Err(format!("other users may write to the {name}"));
        }
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn check_folders(_folders: &[(&Path, &str)]) -> Result<(), String> {
    Ok(())
}

/// Writes `text` to `path` through a temporary file `<prefix>*.tmp` in `folder` (`0600` on Unix)
/// renamed into place, so a reader never sees half a file. `what` names the file in errors.
pub fn write_atomic(folder: &Path, path: &Path, text: &[u8], prefix: &str, what: &str) -> Result<(), String> {
    let failed = |error: std::io::Error| format!("writing the {what} failed: {error}");

    let mut file = tempfile::Builder::new()
        .prefix(prefix)
        .suffix(".tmp")
        .tempfile_in(folder)
        .map_err(failed)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(failed)?;
    }

    file.write_all(text).and_then(|()| file.flush()).map_err(failed)?;
    file.persist(path).map_err(|error| failed(error.error))?;

    Ok(())
}

/// The bytes of the regular file `path`, at most `max` of them; `Ok(None)` when there is no such
/// file. Fails for a symbolic link or anything but a regular file, more than `max` bytes, and on
/// Unix a file of another user or one other users may write to.
pub fn read_private(path: &Path, max: usize) -> Result<Option<Vec<u8>>, String> {
    let shown = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{shown} cannot be read: {error}")),
    };

    if !metadata.file_type().is_file() {
        return Err(format!("{shown} is not a regular file"));
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true);

    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, nix::libc::O_NOFOLLOW);

    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{shown} cannot be read: {error}")),
    };

    check_owner(&file, &shown)?;

    let mut bytes = Vec::new();
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("{shown} cannot be read: {error}"))?;

    if bytes.len() > max {
        return Err(format!("{shown} is larger than {max} bytes"));
    }

    Ok(Some(bytes))
}

/// Fails unless the opened `file` is a regular file of this user that no one else may write to.
#[cfg(unix)]
fn check_owner(file: &std::fs::File, shown: &str) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file
        .metadata()
        .map_err(|error| format!("{shown} cannot be read: {error}"))?;

    if !metadata.file_type().is_file() {
        return Err(format!("{shown} is not a regular file"));
    }

    if metadata.uid() != nix::unistd::getuid().as_raw() {
        return Err(format!("{shown} belongs to another user"));
    }

    if metadata.mode() & 0o022 != 0 {
        return Err(format!("other users may write to {shown}"));
    }

    Ok(())
}

#[cfg(not(unix))]
fn check_owner(_file: &std::fs::File, _shown: &str) -> Result<(), String> {
    Ok(())
}

/// A random version 4 UUID in lowercase, from the operating system's random source; `None` when it
/// fails.
pub fn random_uuid() -> Option<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

/// True for a lowercase UUID: `8-4-4-4-12` lowercase hex digits.
pub fn is_lowercase_uuid(raw: &str) -> bool {
    let groups: Vec<&str> = raw.split('-').collect();
    let lengths = [8, 4, 4, 4, 12];

    groups.len() == lengths.len()
        && groups.iter().zip(lengths).all(|(group, length)| {
            group.len() == length
                && group
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_lowercase_version_4_and_checked() {
        let first = random_uuid().expect("random");
        let second = random_uuid().expect("random");

        assert!(is_lowercase_uuid(&first), "{first}");
        assert_eq!(&first[14..15], "4");
        assert_ne!(first, second);

        for bad in [
            "",
            "not-a-uuid",
            "0F0E0D0C-0000-4000-8000-000000000000",
            "0f0e0d0c-0000-4000-8000-00000000000",
            "0f0e0d0c00004000800000000000000000",
            "../../../etc/passwd",
            "0f0e0d0c-0000-4000-8000-000000000000 ",
        ] {
            assert!(!is_lowercase_uuid(bad), "{bad:?}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn private_files_are_read_without_links_or_other_writers() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("a.json");
        assert_eq!(read_private(&path, 16), Ok(None));

        write_atomic(dir.path(), &path, b"{}", ".x-", "file").expect("written");
        assert_eq!(read_private(&path, 16), Ok(Some(b"{}".to_vec())));
        assert!(read_private(&path, 1).is_err());

        let link = dir.path().join("b.json");
        std::os::unix::fs::symlink(&path, &link).expect("link");
        assert!(read_private(&link, 16).is_err());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).expect("chmod");
        assert!(read_private(&path, 16).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn private_folders_need_this_user_and_no_other_writer() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp");
        let data = dir.path().join("data");
        let inner = data.join("inner");
        std::fs::create_dir_all(&data).expect("mkdir");
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        private_folder(&inner, "inner folder").expect("created");
        assert_eq!(
            check_folders(&[(&data, "data folder"), (&inner, "inner folder")]),
            Ok(())
        );

        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        assert_eq!(
            check_folders(&[(&data, "data folder")]),
            Err("other users may write to the data folder".to_owned())
        );
    }
}
