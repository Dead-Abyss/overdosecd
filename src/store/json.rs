//! The legacy `projects.json` shape: since 0.3.0 it is the one-shot
//! migration's source and the `list --json` output format, never a store.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::project::Project;

const CURRENT_VERSION: u32 = 1;

/// On-disk shape of `projects.json`.
#[derive(Debug, Serialize, Deserialize)]
pub struct StoreFile {
    #[serde(default = "current_version")]
    pub version: u32,
    #[serde(default)]
    pub projects: Vec<Project>,
}

impl StoreFile {
    pub fn new(projects: Vec<Project>) -> Self {
        Self {
            version: CURRENT_VERSION,
            projects,
        }
    }
}

fn current_version() -> u32 {
    CURRENT_VERSION
}

/// Largest legacy index this tool will read; a bigger file is not a
/// `projects.json` this version wrote, and must not be slurped unbounded.
const MAX_LEGACY_BYTES: u64 = 64 << 20;

/// Reads a legacy JSON index for the one-shot migration.
///
/// A missing file reads as an empty index. An unparsable file is moved to
/// `projects.json.corrupt-<timestamp>` before the error is reported, so a
/// broken index is never silently discarded — `doctor` keeps reporting those
/// quarantine files.
pub fn read_legacy(path: &Path) -> Result<Vec<Project>> {
    use std::io::Read;

    crate::paths::refuse_symlink(path, "legacy JSON index")?;
    let file = match crate::paths::open_regular_io(path, MAX_LEGACY_BYTES) {
        Ok(Some(file)) => file,
        Ok(None) => {
            // Not a regular file, or over the cap: move it aside like the
            // corrupt branch does, so one bad file cannot block every later
            // command (`migrate::auto` runs at startup).
            let destination = quarantine(path);
            return Err(Error::Storage(format!(
                "{} is not a readable regular file within the {MAX_LEGACY_BYTES} byte legacy index cap; {destination}",
                path.display()
            )));
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => {
            return Err(Error::Storage(format!(
                "could not open {}: {err}",
                path.display()
            )));
        }
    };
    let mut raw = Vec::new();
    // The cap is enforced by the read itself: a file that grows after the
    // handle check still cannot exceed it. Bytes first, then the size check:
    // a cut through a multi-byte character inside `read_to_string` would
    // report "invalid UTF-8" for what is really an oversized file.
    file.take(MAX_LEGACY_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(|err| Error::Storage(format!("could not read {}: {err}", path.display())))?;
    if raw.len() as u64 > MAX_LEGACY_BYTES {
        let destination = quarantine(path);
        return Err(Error::Storage(format!(
            "{} is larger than the {MAX_LEGACY_BYTES} byte legacy index cap; {destination}",
            path.display()
        )));
    }
    let raw = match String::from_utf8(raw) {
        Ok(raw) => raw,
        Err(err) => {
            // Undecodable is as broken as unparsable: leaving it in place
            // would fail every later command at startup.
            let destination = quarantine(path);
            return Err(Error::Storage(format!(
                "{} is not valid UTF-8: {err}; {destination}",
                path.display()
            )));
        }
    };

    let file: StoreFile = match serde_json::from_str(&raw) {
        Ok(file) => file,
        Err(err) => {
            let destination = quarantine(path);
            return Err(Error::Storage(format!(
                "could not parse {}: {err}; {destination}",
                path.display()
            )));
        }
    };

    if file.version > CURRENT_VERSION {
        return Err(Error::Storage(format!(
            "{} was written by a newer version of overdosecd (file version {}, this build supports {})",
            path.display(),
            file.version,
            CURRENT_VERSION
        )));
    }

    Ok(file.projects)
}

/// Moves an unreadable legacy index to its quarantined sibling name and
/// returns the diagnostic tail (including the rename failure reason when the
/// move itself failed).
fn quarantine(path: &Path) -> String {
    let backup = crate::paths::sibling_backup(path, "corrupt");
    match fs::rename(path, &backup) {
        Ok(()) => format!("the unreadable file was moved to {}", backup.display()),
        Err(err) => format!(
            "the unreadable file could not be moved to {}: {err}",
            backup.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Project;

    fn project(name: &str) -> Project {
        Project::for_test(name, &format!("/tmp/{name}"))
    }

    /// Writes an index through the same private writer the sandboxes use.
    fn write_index(path: &Path, projects: &[Project]) {
        let body =
            serde_json::to_string_pretty(&StoreFile::new(projects.to_vec())).expect("encode index");
        crate::paths::write_private(path, body.as_bytes()).expect("write index");
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            read_legacy(&dir.path().join("projects.json"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn reads_back_what_the_legacy_format_holds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("projects.json");

        let mut first = project("alpha");
        first.aliases = vec!["a".into()];
        first.tags = vec!["rust".into()];
        first.pinned = true;
        let second = project("beta");
        write_index(&path, &[first.clone(), second.clone()]);

        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"version\": 1"));
        assert_eq!(read_legacy(&path).unwrap(), vec![first, second]);
    }

    #[test]
    fn corrupt_file_is_quarantined_not_lost() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("projects.json");
        fs::write(&path, "{ not json").unwrap();

        let err = read_legacy(&path).unwrap_err();
        assert!(err.to_string().contains("moved to"));
        assert!(!path.exists(), "the corrupt file should be moved aside");

        let quarantined: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("projects.json.corrupt-"))
            .collect();
        assert_eq!(
            quarantined.len(),
            1,
            "expected exactly one backup: {quarantined:?}"
        );
    }

    #[test]
    fn an_undecodable_file_is_quarantined_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("projects.json");
        // Invalid UTF-8: as broken as unparsable JSON, and it used to stay in
        // place and fail every later startup.
        fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();

        let err = read_legacy(&path).unwrap_err();
        assert!(err.to_string().contains("moved to"), "error: {err}");
        assert!(!path.exists(), "the undecodable file is moved aside");
        let quarantined = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("projects.json.corrupt-")
            })
            .count();
        assert_eq!(quarantined, 1, "one forensic copy");
        assert!(read_legacy(&path).is_ok(), "the next run starts clean");
    }

    #[test]
    fn a_directory_at_the_legacy_path_is_quarantined() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("projects.json");
        fs::create_dir(&path).unwrap();
        let err = read_legacy(&path).unwrap_err();
        assert!(err.to_string().contains("moved to"), "error: {err}");
        assert!(!path.exists(), "the directory is moved aside, not read");
    }

    #[test]
    fn newer_file_version_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("projects.json");
        fs::write(&path, r#"{"version": 99, "projects": []}"#).unwrap();

        let err = read_legacy(&path).unwrap_err();
        assert!(err.to_string().contains("newer version of overdosecd"));
        assert!(path.exists(), "a newer file is left alone, not quarantined");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_index_is_refused() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real.json");
        fs::write(&real, "{}").expect("write");
        let link = dir.path().join("projects.json");
        symlink(&real, &link).expect("symlink");

        let err = read_legacy(&link).expect_err("refused");
        assert!(matches!(err, Error::SymlinkedPath { .. }), "{err:?}");
    }
}
