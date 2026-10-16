//! Project folder validation.

use std::path::{Path, PathBuf};

use super::{Env, Error};

/// Validates a project directory: after `~` expansion it must be absolute, exist, be a directory
/// and not be a filesystem root. Returns the canonical path (no `\\?\` prefix on Windows).
pub fn validate_project_dir(env: &Env, path: &Path) -> Result<PathBuf, Error> {
    let expanded = match path.to_str() {
        Some(raw) => env.expand_home(raw),
        None => path.to_path_buf(),
    };

    if !expanded.is_absolute() {
        return Err(Error::InvalidPath(format!(
            "project path must be absolute: {}",
            path.display()
        )));
    }

    let canonical =
        dunce::canonicalize(&expanded).map_err(|error| Error::io(format!("resolving {}", path.display()), error))?;

    if !canonical.is_dir() {
        return Err(Error::InvalidPath(format!(
            "project path is not a directory: {}",
            path.display()
        )));
    }

    if canonical.parent().is_none() {
        return Err(Error::InvalidPath("a filesystem root cannot be a project".to_owned()));
    }

    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn a_project_is_an_existing_absolute_non_root_directory() {
        let home = TempDir::new().expect("temp");
        let project = home.path().join("proj");
        std::fs::create_dir_all(&project).expect("mkdir");
        let file = home.path().join("file.txt");
        std::fs::write(&file, "").expect("write");
        let env = Env::for_tests(home.path());

        let expected = dunce::canonicalize(&project).expect("canonical");
        assert_eq!(validate_project_dir(&env, &project).expect("valid"), expected);
        assert_eq!(
            validate_project_dir(&env, Path::new("~/proj")).expect("valid"),
            expected
        );

        let relative = validate_project_dir(&env, Path::new("proj"));
        let not_dir = validate_project_dir(&env, &file);
        let missing = validate_project_dir(&env, &home.path().join("nope"));
        let root = validate_project_dir(&env, Path::new(if cfg!(windows) { "C:\\" } else { "/" }));

        assert!(matches!(relative, Err(Error::InvalidPath(_))));
        assert!(matches!(not_dir, Err(Error::InvalidPath(_))));
        assert!(matches!(missing, Err(Error::Io { source, .. }) if source.kind() == ErrorKind::NotFound));
        assert!(matches!(root, Err(Error::InvalidPath(_))));
    }
}
