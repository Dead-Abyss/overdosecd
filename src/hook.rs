//! The opt-in `cd` hook: record jumps into indexed projects, and count visits
//! to the ones that are not indexed.
//!
//! `overdosecd hook` runs on every `cd` when the wrapper is installed, so it
//! must be cheap, silent on success, and it must *never* fail the `cd`. The
//! visit counters live in `hook-visits.json` next to the index: private,
//! atomic, rebuildable (a broken file is ignored, never quarantined), capped,
//! and decaying.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Bumped when the file's shape changes; a different version is ignored.
const VISITS_VERSION: u32 = 1;
/// Suggest indexing after this many visits to the same directory.
const MIN_VISITS: u32 = 3;
/// After a hint, stay quiet about that directory for this long.
const HINT_COOLDOWN_DAYS: i64 = 14;
/// Visit records older than this are dropped.
const ENTRY_TTL_DAYS: i64 = 30;
/// Most recent entries kept; the rest are pruned.
const MAX_ENTRIES: usize = 500;
/// Lock retries before the hook gives up and skips persisting (≈5 ms).
const LOCK_ATTEMPTS: usize = 5;
/// A visit file larger than this is not a visit file; it is ignored and
/// rebuilt from the next visit (the hook must stay cheap on every `cd`).
const MAX_VISITS_BYTES: u64 = 4 << 20;

/// One directory's visit record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Visit {
    pub path: PathBuf,
    pub count: u32,
    pub last_seen: DateTime<Utc>,
    pub last_hint: Option<DateTime<Utc>>,
}

/// The visit counters file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Visits {
    pub version: u32,
    pub(crate) entries: Vec<Visit>,
}

impl Default for Visits {
    fn default() -> Self {
        Self {
            version: VISITS_VERSION,
            entries: Vec::new(),
        }
    }
}

/// Where the counters live, next to the index.
pub fn file_path(data_dir: &Path) -> PathBuf {
    data_dir.join("hook-visits.json")
}

/// Takes an exclusive, non-blocking lock on the visit-store read-modify-write.
///
/// Two shells can `cd` at once; without a lock the later `save` overwrote the
/// earlier one's count. The hook runs on every `cd`, so waiting is worse than
/// losing a visit: acquisition retries for a few milliseconds and then gives
/// up (`None`), and the caller skips persisting. The lock lives on its own
/// file because [`crate::paths::write_private`] replaces the data file's
/// inode on every save, which would strand a lock taken on it.
pub fn lock(data_dir: &Path) -> Option<std::fs::File> {
    use std::os::unix::io::AsRawFd;

    let path = data_dir.join("hook-visits.lock");
    // The hook may run before any command created the data directory.
    crate::paths::ensure_dir(data_dir).ok()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .ok()?;
    for _ in 0..LOCK_ATTEMPTS {
        // SAFETY: `flock` only inspects the descriptor, which `file` keeps
        // open for the life of the returned handle.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Some(file);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock
            && error.kind() != std::io::ErrorKind::Interrupted
        {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    None
}

impl Visits {
    /// Reads the file; a broken or differently-versioned file is ignored
    /// (rebuildable, like the home cache — never quarantined).
    ///
    /// Timestamps from the future (clock skew, a hand-edited file) are
    /// clamped to now: a future `last_seen` never expired and a future
    /// `last_hint` never ended its cooldown, so one bad value could pin an
    /// entry forever.
    pub fn load(path: &Path) -> Option<Self> {
        let mut visits: Self = crate::paths::read_rebuildable_json(path, MAX_VISITS_BYTES)?;
        if visits.version != VISITS_VERSION {
            return None;
        }
        let now = Utc::now();
        for entry in &mut visits.entries {
            if entry.last_seen > now {
                entry.last_seen = now;
            }
            if entry.last_hint.is_some_and(|when| when > now) {
                entry.last_hint = Some(now);
            }
        }
        Some(visits)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut body = serde_json::to_vec_pretty(self)?;
        body.push(b'\n');
        crate::paths::write_private(path, &body)
    }

    /// Bumps the counter for `path` and returns the new count.
    ///
    /// `None` for a path the JSON store cannot represent: `serde_json` writes
    /// a `PathBuf` through its UTF-8 form, and one unserializable entry would
    /// poison the whole document, so a single non-UTF-8 `cd` used to stop
    /// every later visit from persisting.
    pub fn record_visit(&mut self, path: &Path, now: DateTime<Utc>) -> Option<u32> {
        // One unserializable entry would poison the whole document.
        path.to_str()?;
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.path == path) {
            entry.count = entry.count.saturating_add(1);
            entry.last_seen = now;
            return Some(entry.count);
        }
        self.entries.push(Visit {
            path: path.to_path_buf(),
            count: 1,
            last_seen: now,
            last_hint: None,
        });
        // The cap belongs to the write path, not only to `prune`: a shell
        // that only ever opens new directories must not grow the file until
        // the next TTL sweep.
        if self.entries.len() > MAX_ENTRIES {
            self.prune(now);
        }
        Some(1)
    }

    /// True when the directory has been visited enough and the last hint (if
    /// any) is older than the cooldown.
    pub fn hint_due(&self, path: &Path, now: DateTime<Utc>) -> bool {
        let Some(entry) = self.entries.iter().find(|entry| entry.path == path) else {
            return false;
        };
        if entry.count < MIN_VISITS {
            return false;
        }
        match entry.last_hint {
            None => true,
            Some(when) => now - when >= Duration::days(HINT_COOLDOWN_DAYS),
        }
    }

    /// Remembers that a hint was shown, starting the cooldown.
    pub fn mark_hinted(&mut self, path: &Path, now: DateTime<Utc>) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.path == path) {
            entry.last_hint = Some(now);
        }
    }

    /// Drops entries older than the TTL and keeps only the most recent.
    pub fn prune(&mut self, now: DateTime<Utc>) {
        let ttl = Duration::days(ENTRY_TTL_DAYS);
        self.entries.retain(|entry| now - entry.last_seen < ttl);
        if self.entries.len() > MAX_ENTRIES {
            self.entries
                .sort_by_key(|entry| std::cmp::Reverse(entry.last_seen));
            self.entries.truncate(MAX_ENTRIES);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).expect("epoch")
    }

    #[test]
    fn counts_visits_and_stamps_the_time() {
        let mut visits = Visits::default();
        let dir = Path::new("/code/alpha");
        assert_eq!(visits.record_visit(dir, now()), Some(1));
        assert_eq!(
            visits.record_visit(dir, now() + Duration::hours(1)),
            Some(2)
        );
        let entry = &visits.entries[0];
        assert_eq!(entry.count, 2);
        assert_eq!(entry.last_seen, now() + Duration::hours(1));
        assert_eq!(entry.last_hint, None);
    }

    #[test]
    fn hints_need_three_visits_and_respect_the_cooldown() {
        let mut visits = Visits::default();
        let dir = Path::new("/code/alpha");
        visits.record_visit(dir, now());
        visits.record_visit(dir, now());
        assert!(!visits.hint_due(dir, now()), "two visits stay quiet");
        visits.record_visit(dir, now());
        assert!(visits.hint_due(dir, now()));

        visits.mark_hinted(dir, now());
        assert!(!visits.hint_due(dir, now() + Duration::days(13)));
        assert!(visits.hint_due(dir, now() + Duration::days(14)));

        assert!(!visits.hint_due(Path::new("/elsewhere"), now()));
    }

    #[test]
    fn prune_drops_old_entries_and_caps_the_file() {
        let mut visits = Visits::default();
        visits.record_visit(Path::new("/code/fresh"), now());
        visits.record_visit(Path::new("/code/stale"), now() - Duration::days(31));
        visits.prune(now());
        assert_eq!(visits.len(), 1);
        assert_eq!(visits.entries[0].path, PathBuf::from("/code/fresh"));

        let mut visits = Visits::default();
        for index in 0..(MAX_ENTRIES + 5) {
            let path = PathBuf::from(format!("/code/{index:04}"));
            let when = now() + Duration::seconds(index as i64);
            visits.record_visit(&path, when);
        }
        visits.prune(now() + Duration::seconds(MAX_ENTRIES as i64));
        assert_eq!(visits.len(), MAX_ENTRIES);
        // The most recent entries survive the cap.
        let expected = format!("/code/{:04}", MAX_ENTRIES + 4);
        assert!(
            visits
                .entries
                .iter()
                .any(|entry| entry.path == Path::new(&expected))
        );
    }

    #[test]
    fn load_ignores_broken_and_older_versions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = file_path(temp.path());

        assert!(Visits::load(&path).is_none(), "missing file");
        std::fs::write(&path, "not json").expect("write");
        assert!(Visits::load(&path).is_none(), "broken file is rebuildable");
        std::fs::write(&path, r#"{"version":0,"entries":[]}"#).expect("write");
        assert!(Visits::load(&path).is_none(), "other versions are ignored");

        let mut visits = Visits::default();
        visits.record_visit(Path::new("/code/alpha"), now());
        visits.save(&path).expect("save");
        let loaded = Visits::load(&path).expect("load");
        assert_eq!(loaded, visits);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "the counters are private");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_visit_is_skipped_not_stored() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let mut visits = Visits::default();
        let hostile = Path::new(OsStr::from_bytes(b"/code/\xff"));
        assert_eq!(visits.record_visit(hostile, now()), None);
        assert!(visits.entries.is_empty(), "nothing unserializable is kept");

        // The rest of the store keeps working: the next clean visit saves.
        assert_eq!(
            visits.record_visit(Path::new("/code/alpha"), now()),
            Some(1)
        );
        let temp = tempfile::tempdir().expect("tempdir");
        let path = file_path(temp.path());
        visits.save(&path).expect("save");
        assert_eq!(Visits::load(&path).expect("load"), visits);
    }

    #[test]
    fn future_timestamps_are_clamped_on_load() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = file_path(temp.path());
        let body = format!(
            r#"{{"version":{VISITS_VERSION},"entries":[{{"path":"/code/future","count":3,"last_seen":"2999-01-01T00:00:00Z","last_hint":"2999-01-01T00:00:00Z"}}]}}"#
        );
        std::fs::write(&path, body).expect("write");
        let visits = Visits::load(&path).expect("load clamps");
        let entry = &visits.entries[0];
        assert!(entry.last_seen <= Utc::now());
        assert!(
            entry.last_hint.is_some_and(|when| when <= Utc::now()),
            "a future hint cooldown must not outlive now"
        );
        // A clamped entry still expires and can be hinted about.
        assert!(!visits.hint_due(Path::new("/code/future"), Utc::now()));
        assert!(visits.hint_due(
            Path::new("/code/future"),
            Utc::now() + Duration::days(HINT_COOLDOWN_DAYS + 1)
        ));
    }

    #[test]
    fn the_entry_cap_holds_at_record_time() {
        let mut visits = Visits::default();
        let base = Utc::now();
        for index in 0..(MAX_ENTRIES + 2) {
            visits.record_visit(Path::new(&format!("/code/{index:04}")), base);
        }
        assert_eq!(visits.len(), MAX_ENTRIES);
    }
}
