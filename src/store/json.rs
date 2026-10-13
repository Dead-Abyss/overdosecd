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

/// Reads a legacy JSON index for the one-shot migration.
///
/// A missing file reads as an empty index. An unparsable file is moved to
/// `projects.json.corrupt-<timestamp>` before the error is reported, so a
/// broken index is never silently discarded — `doctor` keeps reporting those
/// quarantine files.
pub fn read_legacy(path: &Path) -> Result<Vec<Project>> {
    crate::paths::refuse_symlink(path, "legacy JSON index")?;
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };

    let file: StoreFile = match serde_json::from_str(&raw) {
        Ok(file) => file,
        Err(err) => {
            let backup = crate::paths::sibling_backup(path, "corrupt");
            fs::rename(path, &backup)?;
            return Err(Error::Storage(format!(
                "could not parse {}: {err}\nthe unreadable file was moved to {}",
                path.display(),
                backup.display()
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
