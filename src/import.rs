//! Import an existing directory-jumper store into the index.
//!
//! Sources are third-party files and are treated as hostile: reads are
//! capped, every length prefix is bounds-checked, malformed records are
//! counted instead of aborting, and nothing is ever written back to the
//! source. Imported entries carry counters only — no `usage` rows are
//! fabricated — so `info` history and `list --since` never claim jumps that
//! did not happen on this machine.

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::cli::ImportSource;
use crate::error::{Error, Result};
use crate::project::{self, Project};

/// Refuse stores larger than this; it is zoxide's own read limit.
const MAX_STORE: u64 = 32 << 20;

/// One entry as the source stored it, before normalization.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub path: String,
    pub score: f64,
    pub visited: Option<DateTime<Utc>>,
}

/// The records a source yielded, plus what did not parse.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Parsed {
    pub records: Vec<Record>,
    /// Records the source itself marks as not wanted (autojump blacklist).
    pub excluded: usize,
    pub malformed: usize,
}

/// An entry the import would add, after normalization and classification.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NewProject {
    pub path: PathBuf,
    pub use_count: u64,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// What an import (dry-run or real) does, by category.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Plan {
    pub to_add: Vec<NewProject>,
    pub existing: usize,
    pub missing: usize,
    /// Dropped on request or by the source: blacklist, zero score, filters,
    /// the `--limit` overflow, and duplicates within the source itself.
    pub excluded: usize,
    pub malformed: usize,
}

/// The store location for `source`, honouring the tool's own override.
pub fn resolve_path(source: ImportSource) -> Result<PathBuf> {
    match source {
        ImportSource::Zoxide => {
            let dir = match std::env::var_os("_ZO_DATA_DIR") {
                Some(value) if !value.is_empty() => PathBuf::from(value),
                _ => crate::dirs::data_local_dir()
                    .ok_or_else(|| Error::Storage("could not determine the data directory".into()))?
                    .join("zoxide"),
            };
            Ok(dir.join("db.zo"))
        }
        ImportSource::Autojump => {
            let dir = match std::env::var_os("AUTOJUMP_DATA_DIR") {
                Some(value) if !value.is_empty() => PathBuf::from(value),
                _ => {
                    let base = std::env::var_os("XDG_DATA_HOME")
                        .filter(|value| !value.is_empty())
                        .map(PathBuf::from)
                        .or_else(|| project::home_dir().map(|home| home.join(".local/share")))
                        .ok_or_else(|| {
                            Error::Storage("could not determine the data directory".into())
                        })?;
                    base.join("autojump")
                }
            };
            Ok(dir.join("autojump.txt"))
        }
        ImportSource::ZshZ => {
            if let Some(value) = std::env::var_os("ZSHZ_DATA")
                && !value.is_empty()
            {
                return Ok(PathBuf::from(value));
            }
            project::home_dir()
                .map(|home| home.join(".z"))
                .ok_or_else(|| Error::Storage("could not determine the home directory".into()))
        }
    }
}

fn oversized_message() -> String {
    format!("larger than the {} MiB import limit", MAX_STORE >> 20)
}

/// Parses the store at `path` without touching it.
pub fn parse_at(source: ImportSource, path: &Path) -> Result<Parsed> {
    let source_err = |message: String| Error::ImportSource {
        path: path.to_path_buf(),
        message,
    };

    // The store is hostile input. Check the name *before* opening so a FIFO
    // cannot block the command and a device cannot stream forever
    // (`metadata()` follows symlinks, so the target type is what decides),
    // then read through the handle with the cap enforced by construction: a
    // file that grows after the check still cannot exceed it.
    let meta = fs::metadata(path).map_err(|err| {
        source_err(match err.kind() {
            std::io::ErrorKind::NotFound => "not found".to_owned(),
            _ => err.to_string(),
        })
    })?;
    if !meta.is_file() {
        return Err(source_err("not a regular file".to_owned()));
    }
    if meta.len() > MAX_STORE {
        return Err(source_err(oversized_message()));
    }

    let file = fs::File::open(path).map_err(|err| source_err(err.to_string()))?;
    let handle = file.metadata().map_err(|err| source_err(err.to_string()))?;
    if !handle.is_file() {
        return Err(source_err("not a regular file".to_owned()));
    }
    if handle.len() > MAX_STORE {
        return Err(source_err(oversized_message()));
    }

    let mut bytes = Vec::new();
    file.take(MAX_STORE + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| source_err(err.to_string()))?;
    if bytes.len() as u64 > MAX_STORE {
        return Err(source_err(oversized_message()));
    }

    match source {
        ImportSource::Zoxide => parse_zoxide(&bytes, path),
        ImportSource::Autojump => Ok(parse_autojump(&bytes)),
        ImportSource::ZshZ => Ok(parse_zshz(&bytes)),
    }
}

/// zoxide's `db.zo` is a bincode 1.x (legacy: fixint, little-endian) stream:
/// `u32` format version, `u64` entry count, then per entry a `u64` path
/// length, the UTF-8 path, an `f64` rank, and a `u64` last-accessed epoch.
///
/// The layout is parsed by hand to avoid a runtime dependency on `bincode`;
/// the crate is a dev-dependency that generates the fixtures, so the tests
/// use the same serializer zoxide does.
fn parse_zoxide(bytes: &[u8], path: &Path) -> Result<Parsed> {
    let corrupted = |detail: &str| Error::ImportSource {
        path: path.to_path_buf(),
        message: format!("corrupted zoxide database: {detail}"),
    };

    if bytes.starts_with(b"SQLite format 3\0") {
        return Err(Error::ImportSource {
            path: path.to_path_buf(),
            message: "a legacy SQLite zoxide database (pre-0.6); run zoxide once to migrate it"
                .to_owned(),
        });
    }

    let mut at = 0usize;
    let version = take_u32(bytes, &mut at).ok_or_else(|| corrupted("missing version"))?;
    if version != 3 {
        return Err(Error::ImportSource {
            path: path.to_path_buf(),
            message: format!("unsupported zoxide database version {version} (this build reads 3)"),
        });
    }
    let count = take_u64(bytes, &mut at).ok_or_else(|| corrupted("missing entry count"))?;
    // Every entry occupies at least its length prefix, rank, and epoch.
    if count > ((bytes.len() - at) / 24) as u64 {
        return Err(corrupted("entry count exceeds the buffer"));
    }

    let mut parsed = Parsed::default();
    for _ in 0..count {
        let len = take_u64(bytes, &mut at).ok_or_else(|| corrupted("missing entry length"))?;
        if len > (bytes.len() - at) as u64 {
            return Err(corrupted("entry length exceeds the buffer"));
        }
        let raw = take(bytes, &mut at, len as usize).expect("bounds checked above");
        let rank = take_f64(bytes, &mut at).ok_or_else(|| corrupted("missing rank"))?;
        let last = take_u64(bytes, &mut at).ok_or_else(|| corrupted("missing timestamp"))?;
        match std::str::from_utf8(raw) {
            Ok(path) if !path.is_empty() && rank.is_finite() => parsed.records.push(Record {
                path: path.to_owned(),
                score: rank,
                visited: epoch_to_time(last),
            }),
            _ => parsed.malformed += 1,
        }
    }
    Ok(parsed)
}

/// autojump's `autojump.txt`: one `weight\tpath` per line; negative weights
/// mark a blacklisted directory.
fn parse_autojump(bytes: &[u8]) -> Parsed {
    let mut parsed = Parsed::default();
    for line in String::from_utf8_lossy(bytes).lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Some((weight, path)) = line.split_once('\t') else {
            parsed.malformed += 1;
            continue;
        };
        let Ok(weight) = weight.trim().parse::<f64>() else {
            parsed.malformed += 1;
            continue;
        };
        if !weight.is_finite() {
            parsed.malformed += 1;
        } else if weight < 0.0 {
            parsed.excluded += 1;
        } else {
            parsed.records.push(Record {
                path: path.to_owned(),
                score: weight,
                visited: None,
            });
        }
    }
    parsed
}

/// zsh-z (and rupa/z): one `path|rank|epoch` per line, parsed from the right
/// so a path containing `|` survives.
fn parse_zshz(bytes: &[u8]) -> Parsed {
    let mut parsed = Parsed::default();
    for line in String::from_utf8_lossy(bytes).lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut parts = line.rsplitn(3, '|');
        let (Some(epoch), Some(rank), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            parsed.malformed += 1;
            continue;
        };
        let (Ok(epoch), Ok(rank)) = (epoch.parse::<u64>(), rank.parse::<f64>()) else {
            parsed.malformed += 1;
            continue;
        };
        if path.is_empty() || !rank.is_finite() {
            parsed.malformed += 1;
            continue;
        }
        parsed.records.push(Record {
            path: path.to_owned(),
            score: rank,
            visited: epoch_to_time(epoch),
        });
    }
    parsed
}

/// Classifies every record against the index: what would be added (sorted by
/// score, then path, and capped by `--limit`) and what is skipped and why.
pub fn plan(
    parsed: &Parsed,
    projects: &[Project],
    home: Option<&Path>,
    min_score: Option<f64>,
    limit: Option<usize>,
) -> Plan {
    let mut plan = Plan {
        excluded: parsed.excluded,
        malformed: parsed.malformed,
        ..Plan::default()
    };
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut candidates: Vec<NewProject> = Vec::new();

    for record in &parsed.records {
        let use_count = use_count_for(record.score);
        if use_count == 0 || min_score.is_some_and(|min| (use_count as f64) < min) {
            plan.excluded += 1;
            continue;
        }
        let raw = project::expand_tilde_with(&record.path, home);
        // A relative entry would resolve against the directory the import
        // happens to run in; every supported source stores absolute paths, so
        // a relative one is malformed input, not a candidate.
        if !raw.is_absolute() {
            plan.malformed += 1;
            continue;
        }
        if !raw.is_dir() {
            plan.missing += 1;
            continue;
        }
        let Ok(canonical) = project::normalize(&raw) else {
            plan.missing += 1;
            continue;
        };
        if project::reject_control_chars_path(&canonical).is_err() {
            plan.malformed += 1;
            continue;
        }
        if projects.iter().any(|project| project.path == canonical) {
            plan.existing += 1;
            continue;
        }
        if !seen.insert(canonical.clone()) {
            plan.excluded += 1;
            continue;
        }
        candidates.push(NewProject {
            path: canonical,
            use_count,
            last_used_at: record.visited,
        });
    }

    candidates.sort_by(|a, b| {
        b.use_count
            .cmp(&a.use_count)
            .then_with(|| a.path.cmp(&b.path))
    });
    if let Some(limit) = limit
        && candidates.len() > limit
    {
        plan.excluded += candidates.len() - limit;
        candidates.truncate(limit);
    }
    plan.to_add = candidates;
    plan
}

/// Frecency → `use_count`: a rounded jump count, at least 1. Autojump weights
/// and z ranks are on their own scales; rounding is the honest approximation,
/// and the ranking saturates long before large weights matter.
fn use_count_for(score: f64) -> u64 {
    if !score.is_finite() || score <= 0.0 {
        return 0;
    }
    score.round().min(u64::MAX as f64) as u64
}

fn epoch_to_time(seconds: u64) -> Option<DateTime<Utc>> {
    if seconds == 0 {
        return None;
    }
    DateTime::from_timestamp(i64::try_from(seconds).ok()?, 0)
}

fn take<'a>(bytes: &'a [u8], at: &mut usize, len: usize) -> Option<&'a [u8]> {
    let end = at.checked_add(len)?;
    let slice = bytes.get(*at..end)?;
    *at = end;
    Some(slice)
}

fn take_u32(bytes: &[u8], at: &mut usize) -> Option<u32> {
    take(bytes, at, 4).map(|raw| u32::from_le_bytes(raw.try_into().unwrap()))
}

fn take_u64(bytes: &[u8], at: &mut usize) -> Option<u64> {
    take(bytes, at, 8).map(|raw| u64::from_le_bytes(raw.try_into().unwrap()))
}

fn take_f64(bytes: &[u8], at: &mut usize) -> Option<f64> {
    take(bytes, at, 8).map(|raw| f64::from_le_bytes(raw.try_into().unwrap()))
}

/// The entries as table rows, for `--dry-run`; no filesystem is touched.
pub fn preview(plan: &Plan) -> Result<Vec<Project>> {
    plan.to_add
        .iter()
        .map(|entry| {
            Ok(Project {
                id: String::new(),
                name: project::default_name(&entry.path)?,
                path: entry.path.clone(),
                aliases: Vec::new(),
                tags: Vec::new(),
                created_at: Utc::now(),
                last_used_at: entry.last_used_at,
                use_count: entry.use_count,
                pinned: false,
                git: None,
                // `--dry-run` promises no filesystem work, so no marker scan.
                kind: project::Kind::Unknown,
            })
        })
        .collect()
}

/// Applies a plan under the store lock, re-checking every path; returns
/// `(imported, raced)` where `raced` entries were added by someone else
/// between planning and the lock.
pub fn apply(projects: &mut Vec<Project>, to_add: &[NewProject]) -> (usize, usize) {
    let (mut imported, mut raced) = (0, 0);
    for entry in to_add {
        if projects.iter().any(|project| project.path == entry.path) {
            raced += 1;
            continue;
        }
        if project::insert(
            projects,
            entry.path.clone(),
            None,
            Vec::new(),
            Vec::new(),
            false,
        )
        .is_err()
        {
            raced += 1;
            continue;
        }
        if let Some(project) = projects
            .iter_mut()
            .find(|project| project.path == entry.path)
        {
            project.use_count = entry.use_count;
            project.last_used_at = entry.last_used_at;
        }
        imported += 1;
    }
    (imported, raced)
}

/// One line for stdout: what happened (or would), and the skip counters.
pub fn summary(plan: &Plan, dry_run: bool) -> String {
    let noun = crate::output::plural(plan.to_add.len(), "project", "projects");
    let mut line = if plan.to_add.is_empty() {
        "nothing to import".to_owned()
    } else if dry_run {
        format!("dry run: would import {} {noun}", plan.to_add.len())
    } else {
        format!("imported {} {noun}", plan.to_add.len())
    };
    let mut skipped = Vec::new();
    if plan.existing > 0 {
        skipped.push(format!("{} already indexed", plan.existing));
    }
    if plan.missing > 0 {
        skipped.push(format!("{} missing", plan.missing));
    }
    if plan.excluded > 0 {
        skipped.push(format!("{} excluded", plan.excluded));
    }
    if plan.malformed > 0 {
        skipped.push(format!("{} malformed", plan.malformed));
    }
    if !skipped.is_empty() {
        line.push_str(&format!(" (skipped {})", skipped.join(", ")));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    /// The shape zoxide serializes; `bincode` 1.x's free functions use the
    /// fixint encoding zoxide reads back, so these fixtures are byte-exact.
    #[derive(Serialize)]
    struct ZDir {
        path: String,
        rank: f64,
        last_accessed: u64,
    }

    fn zoxide_bytes(dirs: &[(&str, f64, u64)]) -> Vec<u8> {
        let dirs: Vec<ZDir> = dirs
            .iter()
            .map(|(path, rank, last_accessed)| ZDir {
                path: (*path).to_owned(),
                rank: *rank,
                last_accessed: *last_accessed,
            })
            .collect();
        let mut buffer = Vec::new();
        bincode::serialize_into(&mut buffer, &3u32).expect("version");
        bincode::serialize_into(&mut buffer, &dirs).expect("dirs");
        buffer
    }

    fn project(name: &str, path: &Path) -> Project {
        Project::for_test(name, path.to_str().expect("utf-8 path"))
    }

    #[test]
    fn zoxide_records_round_trip_and_corruption_is_an_error() {
        let dirs: Vec<(String, f64, u64)> = vec![
            ("/tmp/one".to_owned(), 12.0, 1_700_000_000),
            ("/tmp/two".to_owned(), 3.5, 0),
        ];
        let dirs_ref: Vec<(&str, f64, u64)> = dirs
            .iter()
            .map(|(path, rank, last)| (path.as_str(), *rank, *last))
            .collect();
        let parsed = parse_zoxide(&zoxide_bytes(&dirs_ref), Path::new("db.zo")).expect("parse");
        assert_eq!(parsed.malformed, 0);
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.records[0].path, "/tmp/one");
        assert_eq!(parsed.records[0].score, 12.0);
        assert!(parsed.records[0].visited.is_some());
        assert_eq!(parsed.records[1].visited, None, "epoch 0 means unknown");

        // A non-UTF-8 path is counted, not fatal; framing stays aligned.
        let mut buffer = Vec::new();
        bincode::serialize_into(&mut buffer, &3u32).expect("version");
        bincode::serialize_into(&mut buffer, &2u64).expect("count");
        bincode::serialize_into(&mut buffer, &3u64).expect("len");
        buffer.extend_from_slice(&[0xff, 0xfe, 0xfd]);
        bincode::serialize_into(&mut buffer, &1.0f64).expect("rank");
        bincode::serialize_into(&mut buffer, &0u64).expect("epoch");
        bincode::serialize_into(&mut buffer, &4u64).expect("len");
        buffer.extend_from_slice(b"/tmp");
        bincode::serialize_into(&mut buffer, &2.0f64).expect("rank");
        bincode::serialize_into(&mut buffer, &1u64).expect("epoch");
        let parsed = parse_zoxide(&buffer, Path::new("db.zo")).expect("parse");
        assert_eq!(parsed.records.len(), 1, "the malformed record is skipped");
        assert_eq!(parsed.malformed, 1);

        // Version, truncation, and legacy SQLite are distinct errors.
        let err = parse_zoxide(&zoxide_bytes(&[]), Path::new("db.zo"));
        assert!(err.is_ok(), "an empty database parses to zero records");
        let mut wrong = zoxide_bytes(&[]);
        wrong[0] = 9;
        let err = parse_zoxide(&wrong, Path::new("db.zo")).expect_err("version");
        assert!(
            err.to_string()
                .contains("unsupported zoxide database version 9")
        );
        let err = parse_zoxide(b"\x03\x00\x00\x00", Path::new("db.zo")).expect_err("truncated");
        assert!(err.to_string().contains("corrupted"));
        let err = parse_zoxide(b"SQLite format 3\0rest", Path::new("db.zo")).expect_err("legacy");
        assert!(err.to_string().contains("legacy SQLite"));
    }

    #[test]
    fn autojump_parses_weights_and_blacklists() {
        let text = b"12.5\t/tmp/alpha\n-1\t/tmp/blacklisted\n\nnot-a-line\nx\t/tmp/bad-weight\n2\t/tmp/beta\n";
        let parsed = parse_autojump(text);
        assert_eq!(parsed.excluded, 1, "negative weight is a blacklist marker");
        assert_eq!(parsed.malformed, 2, "a missing tab and a bad weight");
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.records[0].score, 12.5);
        assert_eq!(parsed.records[1].path, "/tmp/beta");
        assert_eq!(parsed.records[1].visited, None);
    }

    #[test]
    fn zshz_parses_from_the_right_and_keeps_pipes() {
        let text = b"/tmp/od|d|dir|4.2|1700000000\n/tmp/plain|1|0\nbroken|2\n4.2|/tmp/wrong\n";
        let parsed = parse_zshz(text);
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.records[0].path, "/tmp/od|d|dir");
        assert_eq!(parsed.records[0].score, 4.2);
        assert!(parsed.records[0].visited.is_some());
        assert_eq!(parsed.records[1].path, "/tmp/plain");
        assert_eq!(parsed.records[1].visited, None);
        assert_eq!(parsed.malformed, 2);
    }

    #[test]
    fn plan_classifies_existing_missing_and_limits() {
        let temp = tempfile::tempdir().expect("tempdir");
        let kept = temp.path().join("kept");
        let also = temp.path().join("also");
        let indexed = temp.path().join("indexed");
        for path in [&kept, &also, &indexed] {
            fs::create_dir_all(path).expect("create");
        }
        let record = |path: &Path, score: f64| Record {
            path: path.to_str().unwrap().to_owned(),
            score,
            visited: None,
        };
        let parsed = Parsed {
            records: vec![
                record(&kept, 10.0),
                record(&also, 2.4),
                record(&indexed, 8.0),
                record(&temp.path().join("gone"), 7.0),
                Record {
                    path: "~/never".to_owned(),
                    score: 1.0,
                    visited: None,
                },
                record(&kept, 10.0),
                Record {
                    path: "/tmp/zero".to_owned(),
                    score: 0.4,
                    visited: None,
                },
            ],
            excluded: 1,
            malformed: 2,
        };
        let existing = vec![project(
            "indexed",
            &project::normalize(&indexed).expect("normalize"),
        )];
        let home = temp.path().join("home");

        let planned = plan(&parsed, &existing, Some(&home), None, None);
        let names: Vec<&str> = planned
            .to_add
            .iter()
            .map(|entry| entry.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, ["kept", "also"], "sorted by score, then path");
        assert_eq!(planned.to_add[0].use_count, 10);
        assert_eq!(planned.to_add[1].use_count, 2, "weights round");
        assert_eq!(planned.existing, 1);
        assert_eq!(planned.missing, 2, "no such directory and unexpanded tilde");
        assert_eq!(
            planned.excluded, 3,
            "source blacklist, duplicate, zero score"
        );
        assert_eq!(planned.malformed, 2);

        // Filters and the cap only shrink `to_add`.
        let planned = plan(&parsed, &existing, Some(&home), Some(3.0), None);
        let names: Vec<&str> = planned
            .to_add
            .iter()
            .map(|entry| entry.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, ["kept"], "min score drops the 2.4 weight");
        let planned = plan(&parsed, &existing, Some(&home), None, Some(1));
        assert_eq!(planned.to_add.len(), 1);
        assert_eq!(
            planned.excluded, 4,
            "the capped candidate counts as excluded"
        );
    }

    #[test]
    fn apply_sets_counters_and_respects_races() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join("alpha");
        fs::create_dir_all(&dir).expect("create");
        let canonical = project::normalize(&dir).expect("normalize");
        let entry = NewProject {
            path: canonical.clone(),
            use_count: 7,
            last_used_at: None,
        };

        let mut projects = Vec::new();
        assert_eq!(apply(&mut projects, std::slice::from_ref(&entry)), (1, 0));
        assert_eq!(projects[0].use_count, 7);
        assert_eq!(projects[0].last_used_at, None);

        assert_eq!(
            apply(&mut projects, &[entry]),
            (0, 1),
            "re-verified under the lock"
        );
        assert_eq!(projects.len(), 1);
    }

    #[test]
    fn summaries_name_every_nonzero_counter() {
        let mut plan = Plan {
            to_add: vec![NewProject {
                path: PathBuf::from("/tmp/one"),
                use_count: 1,
                last_used_at: None,
            }],
            existing: 2,
            missing: 1,
            excluded: 0,
            malformed: 3,
        };
        assert_eq!(
            summary(&plan, false),
            "imported 1 project (skipped 2 already indexed, 1 missing, 3 malformed)"
        );
        assert_eq!(
            summary(&plan, true),
            "dry run: would import 1 project (skipped 2 already indexed, 1 missing, 3 malformed)"
        );
        plan.to_add.clear();
        assert_eq!(
            summary(&plan, false),
            "nothing to import (skipped 2 already indexed, 1 missing, 3 malformed)"
        );
    }

    #[test]
    fn preview_rows_look_like_real_projects() {
        let plan = Plan {
            to_add: vec![NewProject {
                path: PathBuf::from("/tmp/notes"),
                use_count: 4,
                last_used_at: None,
            }],
            ..Plan::default()
        };
        let rows = preview(&plan).expect("preview");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "notes");
        assert_eq!(rows[0].use_count, 4);
        assert!(rows[0].git.is_none());
    }

    #[test]
    fn parse_at_reports_a_missing_store_cleanly() {
        let err = parse_at(
            ImportSource::Autojump,
            Path::new("/tmp/definitely-not-there/autojump.txt"),
        )
        .expect_err("missing source");
        let rendered = err.to_string();
        assert!(
            rendered.contains("/tmp/definitely-not-there/autojump.txt"),
            "{rendered}"
        );
        assert!(rendered.contains("not found"), "{rendered}");
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn parse_at_reads_stores_through_the_public_entry_point() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = temp.path().join("autojump.txt");
        fs::write(&store, "3\t/tmp/alpha\n").expect("write");
        let parsed = parse_at(ImportSource::Autojump, &store).expect("parse");
        assert_eq!(parsed.records.len(), 1);
        assert_eq!(parsed.records[0].score, 3.0);
    }

    use proptest::prelude::*;

    proptest! {
        /// Arbitrary bytes are hostile input by definition: the text parsers
        /// must count every line into exactly one bucket and never panic.
        #[test]
        fn text_parsers_account_for_every_line(data in prop::collection::vec(any::<u8>(), 0..512)) {
            let autojump = parse_autojump(&data);
            prop_assert!(
                autojump.records.len() + autojump.malformed + autojump.excluded <= data.len() + 1
            );
            let zshz = parse_zshz(&data);
            prop_assert!(zshz.records.len() + zshz.malformed + zshz.excluded <= data.len() + 1);
        }

        /// The bincode parser either rejects the frame with a clear error or
        /// stays inside the buffer's bounds; no arbitrary bytes may panic.
        #[test]
        fn zoxide_parser_stays_inside_the_buffer(data in prop::collection::vec(any::<u8>(), 0..512)) {
            if let Ok(parsed) = parse_zoxide(&data, Path::new("db.zo")) {
                prop_assert!(parsed.records.len() + parsed.malformed <= data.len());
            }
        }
    }
}
