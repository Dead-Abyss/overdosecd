use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use fd_lock::RwLock;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::project::Project;

use super::Store;

pub const CURRENT_VERSION: u32 = 1;

/// Sidecar lock file. It lives next to the data file but is never renamed:
/// atomic saves replace the data file's inode, so a lock held on it would not
/// serialize a second writer.
pub const LOCK_FILE: &str = "projects.lock";

/// How long writers wait for the index lock before giving up.
pub const LOCK_TIMEOUT: Duration = Duration::from_millis(500);

const LOCK_RETRY: Duration = Duration::from_millis(20);

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

/// JSON-file storage with atomic writes.
///
/// A missing file reads as an empty index. An unparsable file is moved to
/// `projects.json.corrupt-<timestamp>` before the error is reported, so a
/// broken index is never silently discarded.
pub struct JsonStore {
    path: PathBuf,
}

impl JsonStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock_path(&self) -> PathBuf {
        self.path.with_file_name(LOCK_FILE)
    }

    fn ensure_parent(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            crate::paths::ensure_dir(parent)?;
        }
        Ok(())
    }

    /// [`update`](Store::update) with a custom lock timeout; used by tests.
    pub(crate) fn update_with_timeout<R>(
        &self,
        timeout: Duration,
        f: impl FnOnce(&mut Vec<Project>) -> Result<R>,
    ) -> Result<R> {
        self.ensure_parent()?;
        crate::paths::refuse_symlink(&self.lock_path(), "index lock file")?;
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(self.lock_path())?;
        let mut lock = RwLock::new(file);
        let deadline = Instant::now() + timeout;

        // The guard is scoped to the loop arm; returning it from a helper
        // would trip the borrow checker's handling of mutable borrows.
        loop {
            match lock.try_write() {
                Ok(_guard) => {
                    let mut projects = self.load()?;
                    let output = f(&mut projects)?;
                    self.save(&projects)?;
                    return Ok(output);
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(Error::LockTimeout(self.lock_path()));
                    }
                    thread::sleep(LOCK_RETRY);
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    fn quarantine(&self) -> Result<PathBuf> {
        // The suffix keeps a second corruption in the same second from
        // overwriting the first backup (the timestamp has one-second
        // resolution).
        let backup = crate::paths::sibling_backup(&self.path, "corrupt");
        fs::rename(&self.path, &backup)?;
        Ok(backup)
    }

    /// Writes the whole index atomically. Production callers go through
    /// [`Store::update`], which holds the sidecar lock; tests and the
    /// migration use this directly.
    pub(crate) fn save(&self, projects: &[Project]) -> Result<()> {
        crate::paths::refuse_symlink(&self.path, "JSON index")?;
        let mut body = serde_json::to_string_pretty(&StoreFile::new(projects.to_vec()))?;
        body.push('\n');
        write_private(&self.path, body.as_bytes())
    }
}

impl Store for JsonStore {
    fn update<R>(&self, f: impl FnOnce(&mut Vec<Project>) -> Result<R>) -> Result<R> {
        self.update_with_timeout(LOCK_TIMEOUT, f)
    }

    fn record_use(&self, id: &str, now: DateTime<Utc>) -> Result<()> {
        self.update(|projects| {
            if let Some(project) = projects.iter_mut().find(|project| project.id == id) {
                project.record_use(now);
            }
            Ok(())
        })
    }

    fn recent_uses(&self, _id: &str, _limit: usize) -> Result<Vec<DateTime<Utc>>> {
        // The JSON index keeps only the aggregate counters, never a jump log.
        Ok(Vec::new())
    }

    fn schema_version(&self) -> Result<Option<u32>> {
        // The JSON format has no separate schema-version metadata; its
        // `version` field is validated by `load`.
        Ok(None)
    }

    fn load(&self) -> Result<Vec<Project>> {
        crate::paths::refuse_symlink(&self.path, "JSON index")?;
        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };

        let file: StoreFile = match serde_json::from_str(&raw) {
            Ok(file) => file,
            Err(err) => {
                let backup = self.quarantine()?;
                return Err(Error::Storage(format!(
                    "could not parse {}: {err}\nthe unreadable file was moved to {}",
                    self.path.display(),
                    backup.display()
                )));
            }
        };

        if file.version > CURRENT_VERSION {
            return Err(Error::Storage(format!(
                "{} was written by a newer version of withercd (file version {}, this build supports {})",
                self.path.display(),
                file.version,
                CURRENT_VERSION
            )));
        }

        Ok(file.projects)
    }
}

/// Writes `body` to `path` atomically and privately: a sibling tmp file with
/// mode `0600`, fsync, then rename. Shared by the JSON index and the home
/// discovery cache.
pub(crate) fn write_private(path: &Path, body: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        crate::paths::ensure_dir(parent)?;
    }

    // A pid-suffixed name plus `create_new`: a crashed write can leave a
    // leftover behind, and a planted file or symlink (in a data directory
    // someone else can write) must never be opened and truncated.
    // `remove_file` unlinks the name itself, it does not follow a symlink.
    let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Both files contain local paths; keep them private from the start.
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(body)?;
    file.sync_all()?;
    drop(file);

    fs::rename(&tmp, path)?;

    // fsync the parent so the rename itself survives a crash, not just the
    // file contents; best effort, since the sync can fail on some filesystems.
    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Project;

    fn project(name: &str) -> Project {
        Project::for_test(name, &format!("/tmp/{name}"))
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JsonStore::new(dir.path().join("projects.json"));
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn round_trips_projects() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JsonStore::new(dir.path().join("nested/projects.json"));

        let mut first = project("alpha");
        first.aliases = vec!["a".into()];
        first.tags = vec!["rust".into()];
        first.pinned = true;
        let second = project("beta");

        store.save(&[first.clone(), second.clone()]).unwrap();
        let loaded = store.load().unwrap();

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0], first);
        assert_eq!(loaded[1], second);

        let raw = fs::read_to_string(store.path()).unwrap();
        assert!(raw.contains("\"version\": 1"));
        assert!(raw.ends_with('\n'));
    }

    #[test]
    fn corrupt_file_is_quarantined_not_lost() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("projects.json");
        fs::write(&path, "{ not json").unwrap();

        let store = JsonStore::new(path.clone());
        let err = store.load().unwrap_err();
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

        let store = JsonStore::new(path);
        let err = store.load().unwrap_err();
        assert!(err.to_string().contains("newer version of withercd"));
    }

    #[cfg(unix)]
    #[test]
    fn write_private_does_not_follow_a_planted_tmp_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target");
        fs::write(&target, "untouched").expect("write target");
        let path = dir.path().join("projects.json");
        let tmp = path.with_extension(format!("json.tmp-{}", std::process::id()));
        symlink(&target, &tmp).expect("plant symlink");

        write_private(&path, b"index").expect("write");

        assert_eq!(
            fs::read_to_string(&target).expect("read target"),
            "untouched",
            "the planted symlink's target is never opened"
        );
        assert_eq!(fs::read_to_string(&path).expect("read index"), "index");
        assert!(!tmp.exists(), "the tmp name is unlinked, not reused");
    }

    #[test]
    fn update_saves_and_returns_the_closure_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JsonStore::new(dir.path().join("projects.json"));

        let output = store
            .update(|projects| {
                projects.push(project("alpha"));
                Ok(42)
            })
            .unwrap();

        assert_eq!(output, 42);
        assert_eq!(store.load().unwrap().len(), 1);
    }

    #[test]
    fn update_skips_the_save_when_the_closure_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JsonStore::new(dir.path().join("projects.json"));
        store.save(&[project("existing")]).unwrap();

        let result: Result<()> = store.update(|projects| {
            projects.clear();
            Err(Error::NoMatch("x".into()))
        });

        assert!(matches!(result.unwrap_err(), Error::NoMatch(_)));
        assert_eq!(
            store.load().unwrap().len(),
            1,
            "a failing closure must not persist changes"
        );
    }

    #[test]
    fn update_times_out_when_the_lock_is_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = JsonStore::new(dir.path().join("projects.json"));
        let lock_path = dir.path().join(LOCK_FILE);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        let mut held = RwLock::new(file);
        let guard = held.try_write().expect("test should hold the lock");

        let err = store
            .update_with_timeout(Duration::from_millis(50), |_| Ok(()))
            .unwrap_err();
        assert!(matches!(err, Error::LockTimeout(_)));

        drop(guard);
        store
            .update(|_| Ok(()))
            .expect("the lock should be free after the guard drops");
    }
}
