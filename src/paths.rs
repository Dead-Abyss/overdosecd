use std::io::Read;
use std::path::{Path, PathBuf};

use crate::config::StorageBackend;
use crate::error::{Error, Result};

/// Environment override for the data directory, used by tests and power users.
const ENV_DATA_DIR: &str = "OVERDOSECD_DATA_DIR";

/// Resolves the directory that holds `projects.json`.
///
/// Precedence: `--data-dir` flag, `$OVERDOSECD_DATA_DIR`, platform data dir.
///
/// A value from the flag or the environment must be absolute: a relative one
/// resolves against whatever directory the shell is in, so a planted `cd`
/// target could choose which index is read and written (`OVERDOSECD_DATA_DIR=.`
/// used to create the index in the current directory).
pub fn resolve_data_dir(cli_override: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = cli_override {
        return absolute("--data-dir", dir);
    }

    if let Some(value) = crate::dirs::os_env(ENV_DATA_DIR) {
        return absolute(
            "the data directory from $OVERDOSECD_DATA_DIR",
            Path::new(&value),
        );
    }

    let platform = crate::dirs::data_dir().ok_or_else(|| {
        Error::Storage("could not determine a data directory for overdosecd".into())
    })?;
    absolute("the platform data directory", &platform)
}

/// Requires an absolute path, naming where it came from in the error.
pub fn absolute(kind: &'static str, path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Err(Error::RelativePath {
        kind,
        value: path.to_string_lossy().into_owned(),
    })
}

/// Refuses a store file that is a symbolic link.
///
/// A link can point into a directory someone else controls (the index, the
/// cache, the visit counters) or at a file whose lock they own; the files
/// themselves are re-created atomically, so replacing a link is safe only
/// when the user asked for it. A missing file is fine.
pub fn refuse_symlink(path: &Path, kind: &'static str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::SymlinkedPath {
            kind,
            path: path.to_path_buf(),
        }),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// A sibling name `name.<tag>-<stamp>[-N]` that does not exist yet.
///
/// Quarantine (`projects.json.corrupt-…`), JSON migration
/// (`projects.json.migrated-…`), and the SQLite pre-migration copy
/// (`projects.db.v1-…`) all keep the original file aside; the timestamp has
/// one-second resolution, so a numeric suffix is what keeps a second event
/// from overwriting the first forensic copy.
pub fn sibling_backup(path: &Path, tag: &str) -> PathBuf {
    let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut candidate = path.with_file_name(format!("{name}.{tag}-{stamp}"));
    let mut suffix = 1;
    while candidate.exists() {
        candidate = path.with_file_name(format!("{name}.{tag}-{stamp}-{suffix}"));
        suffix += 1;
    }
    candidate
}

/// Reads a rebuildable JSON file through the hostile-file gate: a small
/// regular file, not a symlink, the size cap enforced by the read itself,
/// valid UTF-8 and JSON. `None` on any failure: the caller rebuilds.
pub fn read_rebuildable_json<T: serde::de::DeserializeOwned>(
    path: &Path,
    max_bytes: u64,
) -> Option<T> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    if metadata.len() > max_bytes {
        return None;
    }
    let mut raw = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(max_bytes + 1)
        .read_to_end(&mut raw)
        .ok()?;
    if raw.len() as u64 > max_bytes {
        return None;
    }
    let raw = String::from_utf8(raw).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Creates `path` and any missing parents, private (`0700`) on Unix.
///
/// The data directory holds indexed paths, jump history, and the
/// home-directory cache. A mode driven by the umask can leave it group- or
/// world-accessible, and with a permissive umask even writable, which lets
/// other local users enumerate it or plant the predictable temp and lock
/// names.
pub fn ensure_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    builder.mode(0o700);
    builder.create(path)
}

pub fn store_file(data_dir: &Path) -> PathBuf {
    data_dir.join("projects.json")
}

pub fn sqlite_file(data_dir: &Path) -> PathBuf {
    data_dir.join("projects.db")
}

/// The file the active backend reads and writes.
pub fn index_file(data_dir: &Path, backend: StorageBackend) -> PathBuf {
    match backend {
        StorageBackend::Json => store_file(data_dir),
        StorageBackend::Sqlite => sqlite_file(data_dir),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_overrides_win() {
        let dir = std::env::temp_dir().join("overdosecd-explicit");
        assert_eq!(
            resolve_data_dir(Some(&dir)).unwrap(),
            dir,
            "the --data-dir value should be used as-is"
        );
    }

    #[test]
    fn relative_overrides_are_refused() {
        let err = resolve_data_dir(Some(Path::new("relative/dir"))).expect_err("refused");
        assert!(matches!(err, Error::RelativePath { .. }), "{err:?}");
        assert_eq!(err.exit_code(), 1);
        // A relative `$OVERDOSECD_DATA_DIR` is the same hazard.
        let err = absolute("$OVERDOSECD_DATA_DIR", Path::new(".")).expect_err("refused");
        assert!(matches!(err, Error::RelativePath { .. }), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_store_files_are_refused() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let real = temp.path().join("real.json");
        std::fs::write(&real, "{}").expect("write");
        let link = temp.path().join("projects.json");
        symlink(&real, &link).expect("symlink");

        let err = refuse_symlink(&link, "JSON index").expect_err("refused");
        assert!(matches!(err, Error::SymlinkedPath { .. }), "{err:?}");
        assert_eq!(err.exit_code(), 3);

        // A regular file and a missing file are both fine.
        assert!(refuse_symlink(&real, "JSON index").is_ok());
        assert!(refuse_symlink(&temp.path().join("absent"), "JSON index").is_ok());
    }

    #[test]
    fn sibling_backups_never_clobber() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("projects.json");

        let first = sibling_backup(&path, "corrupt");
        std::fs::write(&first, "first").expect("write backup");
        let second = sibling_backup(&path, "corrupt");
        assert_ne!(first, second, "a second event gets its own name");
        std::fs::write(&second, "second").expect("write backup");

        assert_eq!(std::fs::read_to_string(&first).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "second");
        let name = second.file_name().unwrap().to_string_lossy();
        assert!(name.ends_with("-1"), "numeric suffix: {name}");
    }

    #[test]
    fn store_file_is_projects_json() {
        assert_eq!(
            store_file(Path::new("/data/overdosecd")),
            PathBuf::from("/data/overdosecd/projects.json")
        );
    }

    #[test]
    fn sqlite_file_is_projects_db() {
        assert_eq!(
            sqlite_file(Path::new("/data/overdosecd")),
            PathBuf::from("/data/overdosecd/projects.db")
        );
    }

    #[cfg(unix)]
    #[test]
    fn ensure_dir_creates_private_directories() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let data = temp.path().join("nested").join("overdosecd");
        ensure_dir(&data).expect("create");
        let mode = |path: &Path| {
            std::fs::metadata(path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(&data), 0o700, "the data directory is private");
        assert_eq!(mode(data.parent().unwrap()), 0o700, "parents too");

        ensure_dir(&data).expect("existing directories are accepted");
        assert_eq!(mode(&data), 0o700);
    }
}
