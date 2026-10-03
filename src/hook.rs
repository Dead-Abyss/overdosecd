//! The opt-in `cd` hook: record jumps into indexed projects, and count visits
//! to the ones that are not indexed.
//!
//! `overdosecd hook` runs on every `cd` when the wrapper is installed, so it
//! must be cheap, silent on success, and it must *never* fail the `cd`. The
//! visit counters live in `hook-visits.json` next to the index: private,
//! atomic, rebuildable (a broken file is ignored, never quarantined), capped,
//! and decaying.

use std::io::Read;
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
/// A visit file larger than this is not a visit file; it is ignored and
/// rebuilt from the next visit (the hook must stay cheap on every `cd`).
const MAX_VISITS_BYTES: u64 = 4 << 20;

/// One directory's visit record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Visit {
    pub path: PathBuf,
    pub count: u32,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub last_hint: Option<DateTime<Utc>>,
}

/// The visit counters file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Visits {
    pub version: u32,
    pub entries: Vec<Visit>,
}

impl Default for Visits {
    fn default() -> Self {
        Self::new()
    }
}

/// Where the counters live, next to the index.
pub fn file_path(data_dir: &Path) -> PathBuf {
    data_dir.join("hook-visits.json")
}

impl Visits {
    pub fn new() -> Self {
        Self {
            version: VISITS_VERSION,
            entries: Vec::new(),
        }
    }

    /// Reads the file; a broken or differently-versioned file is ignored
    /// (rebuildable, like the home cache — never quarantined).
    pub fn load(path: &Path) -> Option<Self> {
        // Rebuildable counters: a symlink is treated as an empty file, and a
        // file that is not a small regular one is not worth reading on every
        // `cd`.
        let metadata = std::fs::symlink_metadata(path).ok()?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return None;
        }
        if metadata.len() > MAX_VISITS_BYTES {
            return None;
        }
        let mut raw = Vec::new();
        std::fs::File::open(path)
            .ok()?
            .take(MAX_VISITS_BYTES + 1)
            .read_to_end(&mut raw)
            .ok()?;
        if raw.len() as u64 > MAX_VISITS_BYTES {
            return None;
        }
        let raw = String::from_utf8(raw).ok()?;
        let visits: Self = serde_json::from_str(&raw).ok()?;
        (visits.version == VISITS_VERSION).then_some(visits)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut body = serde_json::to_vec_pretty(self)?;
        body.push(b'\n');
        crate::store::json::write_private(path, &body)
    }

    /// Bumps the counter for `path` and returns the new count.
    pub fn record_visit(&mut self, path: &Path, now: DateTime<Utc>) -> u32 {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.path == path) {
            entry.count = entry.count.saturating_add(1);
            entry.last_seen = now;
            return entry.count;
        }
        self.entries.push(Visit {
            path: path.to_path_buf(),
            count: 1,
            first_seen: now,
            last_seen: now,
            last_hint: None,
        });
        1
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
    pub fn len(&self) -> usize {
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
    fn counts_visits_and_keeps_first_seen() {
        let mut visits = Visits::new();
        let dir = Path::new("/code/alpha");
        assert_eq!(visits.record_visit(dir, now()), 1);
        assert_eq!(visits.record_visit(dir, now() + Duration::hours(1)), 2);
        let entry = &visits.entries[0];
        assert_eq!(entry.count, 2);
        assert_eq!(entry.first_seen, now());
        assert_eq!(entry.last_seen, now() + Duration::hours(1));
        assert_eq!(entry.last_hint, None);
    }

    #[test]
    fn hints_need_three_visits_and_respect_the_cooldown() {
        let mut visits = Visits::new();
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
        let mut visits = Visits::new();
        visits.record_visit(Path::new("/code/fresh"), now());
        visits.record_visit(Path::new("/code/stale"), now() - Duration::days(31));
        visits.prune(now());
        assert_eq!(visits.len(), 1);
        assert_eq!(visits.entries[0].path, PathBuf::from("/code/fresh"));

        let mut visits = Visits::new();
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

        let mut visits = Visits::new();
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
}
