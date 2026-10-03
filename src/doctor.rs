use std::collections::HashSet;
use std::fs;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use crate::config::StorageBackend;
use crate::error::{Error, Result};
use crate::paths;
use crate::project::{self, GitInfo, Project};
use crate::store::json::StoreFile;
use crate::store::sqlite::SqliteStore;

/// A problem found by [`inspect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Issue {
    /// The project's directory no longer exists.
    StalePath { name: String, path: PathBuf },

    /// Two or more projects share a name (case-insensitively).
    DuplicateName { name: String, count: usize },

    /// A quarantined index file is lying next to the live one.
    QuarantineFile(PathBuf),

    /// SQLite sidecar files (`-wal`, `-shm`, `-journal`) exist without their
    /// database, usually after a crash.
    StrayFile(PathBuf),

    /// `projects.json` and `projects.db` both exist but do not hold the same
    /// project ids.
    BackendDisagreement {
        json: usize,
        db: usize,
        differing: usize,
    },

    /// The index file that is not the active backend cannot be read.
    InactiveIndexUnreadable { path: PathBuf, message: String },

    /// The index file has no write permission.
    IndexNotWritable(PathBuf),

    /// The index file mode is wider than `0600` (Unix only).
    #[cfg_attr(not(unix), allow(dead_code))]
    IndexPermissions { path: PathBuf, mode: u32 },

    /// The home-discovery cache mode is wider than `0600` (Unix only).
    #[cfg_attr(not(unix), allow(dead_code))]
    CachePermissions { path: PathBuf, mode: u32 },

    /// An indexed name, alias, tag, or path contains control characters a
    /// terminal would interpret (entries written before the guard existed).
    ControlInValue {
        project: String,
        field: &'static str,
        value: String,
    },

    /// The data directory has no write permission.
    DataDirNotWritable(PathBuf),

    /// The data directory mode is wider than `0700` (Unix only).
    #[cfg_attr(not(unix), allow(dead_code))]
    DataDirPermissions { path: PathBuf, mode: u32 },

    /// The config file exists but could not be read or parsed.
    ConfigProblem { path: PathBuf, message: String },

    /// A store file (index, cache, counters, lock) is a symbolic link, which
    /// overdosecd refuses: the target could belong to someone else.
    SymlinkedFile(PathBuf),
}

/// Everything [`inspect`] found, including nothing.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub issues: Vec<Issue>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty()
    }
}

/// Runs the read-only checks against an already-loaded index.
///
/// The permission checks target the active backend's file; the other file is
/// still inspected (quarantine leftovers, a disagreeing pair, orphaned SQLite
/// sidecars), but nothing is written or quarantined here.
pub fn inspect(data_dir: &Path, backend: StorageBackend, projects: &[Project]) -> Report {
    let mut issues = Vec::new();

    for project in projects {
        if !project.path.exists() {
            issues.push(Issue::StalePath {
                name: project.name.clone(),
                path: project.path.clone(),
            });
        }
    }

    for project in projects {
        let mut report = |field: &'static str, value: &str| {
            if crate::sanitize::contains_dangerous(value) {
                issues.push(Issue::ControlInValue {
                    project: project.name.clone(),
                    field,
                    value: value.to_owned(),
                });
            }
        };
        report("name", &project.name);
        for alias in &project.aliases {
            report("alias", alias);
        }
        for tag in &project.tags {
            report("tag", tag);
        }
        let path = project.path.to_string_lossy();
        report("path", &path);
        // Git metadata is read from a repository's own `.git` files, so it is
        // attacker-controlled by construction. `add` refuses to store a dirty
        // remote; this catches rows written before the guard or by a planted
        // index, and the same check covers the legacy control-character class.
        if let Some(git) = &project.git {
            if let Some(name) = &git.remote_name {
                report("git remote name", name);
            }
            if let Some(url) = &git.remote_url {
                report("git remote URL", url);
            }
        }
    }

    let mut names: Vec<(String, String, usize)> = Vec::new();
    for project in projects {
        let key = project.name.to_lowercase();
        match names.iter().position(|(seen, _, _)| *seen == key) {
            Some(index) => names[index].2 += 1,
            None => names.push((key, project.name.clone(), 1)),
        }
    }
    for (_, name, count) in names {
        if count > 1 {
            issues.push(Issue::DuplicateName { name, count });
        }
    }

    let json_path = paths::store_file(data_dir);
    let db_path = paths::sqlite_file(data_dir);

    // A link where a store file belongs can point into a directory someone
    // else controls; every open refuses it, and this is where it is reported.
    let active = match backend {
        StorageBackend::Json => json_path.clone(),
        StorageBackend::Sqlite => db_path.clone(),
    };
    for path in [
        json_path.clone(),
        db_path.clone(),
        crate::discovery::cache_path(data_dir),
        crate::hook::file_path(data_dir),
        json_path.with_file_name("projects.lock"),
    ] {
        // The active index is refused by the store already and reported by
        // `cmd_doctor`'s load arm; listing it here too would double it.
        if path == active {
            continue;
        }
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && metadata.file_type().is_symlink()
        {
            issues.push(Issue::SymlinkedFile(path));
        }
    }
    let index_path = match backend {
        StorageBackend::Json => json_path.clone(),
        StorageBackend::Sqlite => db_path.clone(),
    };

    // `store::json` quarantines corrupt indexes as `<file>.corrupt-<stamp>`;
    // only the JSON backend ever creates those, whichever backend is active.
    if let Ok(entries) = fs::read_dir(data_dir)
        && let Some(prefix) = json_path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| format!("{name}.corrupt-"))
    {
        let mut quarantined: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&prefix))
            })
            .collect();
        quarantined.sort();
        issues.extend(quarantined.into_iter().map(Issue::QuarantineFile));
    }

    if !db_path.exists() {
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut name = db_path.clone().into_os_string();
            name.push(suffix);
            let stray = PathBuf::from(name);
            if stray.exists() {
                issues.push(Issue::StrayFile(stray));
            }
        }
    }

    if json_path.exists() && db_path.exists() {
        let inactive = match backend {
            StorageBackend::Json => SqliteStore::new(db_path.clone())
                .load_readonly()
                .map_err(|err| err.to_string()),
            StorageBackend::Sqlite => read_json_projects(&json_path),
        };
        match inactive {
            Ok(inactive_projects) => {
                let active_ids: HashSet<&str> =
                    projects.iter().map(|project| project.id.as_str()).collect();
                let inactive_ids: HashSet<&str> = inactive_projects
                    .iter()
                    .map(|project| project.id.as_str())
                    .collect();
                let differing = active_ids.symmetric_difference(&inactive_ids).count();
                if differing > 0 {
                    let (json, db) = match backend {
                        StorageBackend::Json => (projects.len(), inactive_projects.len()),
                        StorageBackend::Sqlite => (inactive_projects.len(), projects.len()),
                    };
                    issues.push(Issue::BackendDisagreement {
                        json,
                        db,
                        differing,
                    });
                }
            }
            Err(message) => {
                let path = match backend {
                    StorageBackend::Json => db_path,
                    StorageBackend::Sqlite => json_path,
                };
                issues.push(Issue::InactiveIndexUnreadable { path, message });
            }
        }
    }

    if let Ok(metadata) = fs::metadata(&index_path) {
        if metadata.permissions().readonly() {
            issues.push(Issue::IndexNotWritable(index_path.clone()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode() & 0o777;
            if mode != 0o600 {
                issues.push(Issue::IndexPermissions {
                    path: index_path,
                    mode,
                });
            }
        }
    }
    if let Ok(metadata) = fs::metadata(data_dir) {
        if metadata.permissions().readonly() {
            issues.push(Issue::DataDirNotWritable(data_dir.to_path_buf()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                issues.push(Issue::DataDirPermissions {
                    path: data_dir.to_path_buf(),
                    mode,
                });
            }
        }
    }
    let cache_path = crate::discovery::cache_path(data_dir);
    if let Ok(metadata) = fs::metadata(&cache_path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode() & 0o777;
            if mode != 0o600 {
                issues.push(Issue::CachePermissions {
                    path: cache_path,
                    mode,
                });
            }
        }
    }

    Report { issues }
}

/// Parses a `projects.json` without the quarantine side effect `JsonStore`
/// has, since `doctor` must stay read-only.
fn read_json_projects(path: &Path) -> std::result::Result<Vec<Project>, String> {
    let raw = fs::read_to_string(path).map_err(|err| err.to_string())?;
    let file: StoreFile = serde_json::from_str(&raw).map_err(|err| err.to_string())?;
    Ok(file.projects)
}

/// A change the user chose for a stale project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Repair {
    Relocate { id: String, path: PathBuf },
    Remove { id: String },
}

/// Walks the stale projects, asking what to do with each.
///
/// Answers come from `input`; prompts and per-answer notes go to `output`, so
/// callers decide which streams to use. No index lock is held here — the
/// caller applies the returned repairs later, under the lock, with
/// [`apply_repairs`].
pub fn fix_session(
    projects: &[Project],
    home: Option<&Path>,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Vec<Repair>> {
    let mut repairs = Vec::new();

    for project in projects.iter().filter(|project| !project.path.exists()) {
        writeln!(
            output,
            "stale project `{}` -> {}",
            crate::sanitize::text(&project.name),
            crate::sanitize::path(&project.path)
        )?;

        loop {
            write!(output, "  [s]kip, [r]emove, [q]uit, or a new path: ")?;
            output.flush()?;
            let Some(answer) = read_answer(input)? else {
                writeln!(output, "  input ended; stopping")?;
                return Ok(repairs);
            };

            match classify(&answer) {
                Answer::Skip => break,
                Answer::Quit => return Ok(repairs),
                Answer::Invalid => {
                    writeln!(output, "  expected [s]kip, [r]emove, [q]uit, or a path")?;
                }
                Answer::Remove => {
                    writeln!(output, "  marked for removal")?;
                    repairs.push(Repair::Remove {
                        id: project.id.clone(),
                    });
                    break;
                }
                Answer::Path(raw) => match validate_target(projects, &project.id, &raw, home) {
                    Ok(path) => {
                        writeln!(output, "  -> {}", path.display())?;
                        repairs.push(Repair::Relocate {
                            id: project.id.clone(),
                            path,
                        });
                        break;
                    }
                    Err(err) => writeln!(output, "  {err}")?,
                },
            }
        }
    }

    Ok(repairs)
}

/// Applies collected repairs in place, re-verifying every `id`.
///
/// Returns how many repairs were applied. Targets that vanished, or whose new
/// path another process took meanwhile, are skipped rather than failing the
/// batch. Relocated projects get their git metadata re-detected.
pub fn apply_repairs(projects: &mut Vec<Project>, repairs: &[Repair]) -> usize {
    let mut applied = 0;

    for repair in repairs {
        match repair {
            Repair::Remove { id } => {
                if let Some(index) = projects.iter().position(|project| project.id == *id) {
                    projects.remove(index);
                    applied += 1;
                }
            }
            Repair::Relocate { id, path } => {
                let taken = projects
                    .iter()
                    .any(|project| project.id != *id && project.path == *path);
                if taken {
                    continue;
                }
                if let Some(project) = projects.iter_mut().find(|project| project.id == *id) {
                    project.path = path.clone();
                    project.git = GitInfo::detect(path);
                    applied += 1;
                }
            }
        }
    }

    applied
}

enum Answer {
    Skip,
    Remove,
    Quit,
    Invalid,
    Path(String),
}

fn classify(answer: &str) -> Answer {
    let answer = answer.trim();
    if answer.is_empty() {
        return Answer::Invalid;
    }
    if answer.eq_ignore_ascii_case("s") || answer.eq_ignore_ascii_case("skip") {
        return Answer::Skip;
    }
    if answer.eq_ignore_ascii_case("r") || answer.eq_ignore_ascii_case("remove") {
        return Answer::Remove;
    }
    if answer.eq_ignore_ascii_case("q") || answer.eq_ignore_ascii_case("quit") {
        return Answer::Quit;
    }
    Answer::Path(answer.to_owned())
}

fn read_answer(input: &mut impl BufRead) -> Result<Option<String>> {
    let mut line = String::new();
    if input.read_line(&mut line)? == 0 {
        Ok(None)
    } else {
        Ok(Some(line))
    }
}

/// Expands and validates a replacement path for `id`: it must exist, be a
/// directory, and not belong to another project. Used by `doctor --fix` and
/// the picker's health view.
pub(crate) fn validate_target(
    projects: &[Project],
    id: &str,
    raw: &str,
    home: Option<&Path>,
) -> Result<PathBuf> {
    let expanded = project::expand_tilde_with(raw, home);
    if !expanded.exists() {
        return Err(Error::MissingDirectory(expanded));
    }
    if !expanded.is_dir() {
        return Err(Error::NotADirectory(expanded));
    }

    let canonical = project::normalize(&expanded)?;
    project::reject_control_chars_path(&canonical)?;
    if projects
        .iter()
        .any(|project| project.id != id && project.path == canonical)
    {
        return Err(Error::PathTaken(canonical));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Creates a fresh index file with private permissions so only the
    /// check under test reports an issue.
    fn write_index(path: &Path) {
        fs::create_dir_all(path.parent().expect("index has a parent")).expect("create data dir");
        fs::write(path, "{}").expect("write index");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("chmod index");
        }
    }

    fn project(name: &str, path: &Path) -> Project {
        Project::for_test(name, path.to_str().expect("utf-8 path"))
    }

    /// A data directory with the mode `paths::ensure_dir` creates; the
    /// process umask can otherwise leave a plain `tempdir` group-readable,
    /// which would be reported by the check under test.
    fn private_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).expect("chmod 0700");
        }
        dir
    }

    #[test]
    fn clean_index_reports_nothing() {
        let dir = private_dir();
        let project_dir = dir.path().join("project");
        fs::create_dir_all(&project_dir).expect("create project dir");
        let index = dir.path().join("projects.json");
        write_index(&index);

        let report = inspect(
            dir.path(),
            StorageBackend::Json,
            &[project("alpha", &project_dir)],
        );
        assert!(report.is_clean(), "unexpected issues: {report:?}");
    }

    #[cfg(unix)]
    #[test]
    fn loose_data_directory_mode_is_reported() {
        use std::os::unix::fs::PermissionsExt;

        let dir = private_dir();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).expect("chmod 0755");

        let report = inspect(dir.path(), StorageBackend::Json, &[]);
        assert!(
            report.issues.contains(&Issue::DataDirPermissions {
                path: dir.path().to_path_buf(),
                mode: 0o755,
            }),
            "issues: {:?}",
            report.issues
        );

        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).expect("restore 0700");
        assert!(
            inspect(dir.path(), StorageBackend::Json, &[])
                .issues
                .is_empty()
        );
    }

    #[test]
    fn control_characters_are_reported() {
        let dir = private_dir();
        let good = dir.path().join("good");
        fs::create_dir_all(&good).expect("create project dir");
        let mut one = project("bad\u{1b}name", &good);
        one.tags.push("bad\ttag".into());
        let bad_path = dir.path().join("bad\u{7f}path");
        let mut two = project("plain", &bad_path);
        two.aliases.push("bad\u{9b}alias".into());

        let report = inspect(dir.path(), StorageBackend::Json, &[one, two]);
        let rendered: Vec<String> = report
            .issues
            .iter()
            .filter_map(|issue| match issue {
                Issue::ControlInValue { field, value, .. } => Some(format!("{field}={value:?}")),
                _ => None,
            })
            .collect();
        assert_eq!(
            rendered,
            vec![
                r#"name="bad\u{1b}name""#.to_owned(),
                r#"tag="bad\ttag""#.to_owned(),
                r#"alias="bad\u{9b}alias""#.to_owned(),
                format!("path={:?}", bad_path.to_string_lossy()),
            ]
        );
    }

    #[test]
    fn stale_paths_are_reported() {
        let dir = private_dir();
        let index = dir.path().join("projects.json");
        write_index(&index);
        let missing = dir.path().join("gone");

        let report = inspect(
            dir.path(),
            StorageBackend::Json,
            &[project("gone", &missing)],
        );
        assert_eq!(
            report.issues,
            vec![Issue::StalePath {
                name: "gone".to_owned(),
                path: missing,
            }]
        );
    }

    #[test]
    fn duplicate_names_are_reported_case_insensitively() {
        let dir = private_dir();
        let index = dir.path().join("projects.json");
        write_index(&index);
        let one = dir.path().join("one");
        let two = dir.path().join("two");
        fs::create_dir_all(&one).expect("create one");
        fs::create_dir_all(&two).expect("create two");

        let report = inspect(
            dir.path(),
            StorageBackend::Json,
            &[project("App", &one), project("app", &two)],
        );
        assert_eq!(
            report.issues,
            vec![Issue::DuplicateName {
                name: "App".to_owned(),
                count: 2,
            }]
        );
    }

    #[test]
    fn quarantine_files_are_reported() {
        let dir = private_dir();
        let index = dir.path().join("projects.json");
        write_index(&index);
        let quarantined = dir.path().join("projects.json.corrupt-20260101000000");
        fs::write(&quarantined, "{ not json").expect("write quarantine file");

        let report = inspect(dir.path(), StorageBackend::Json, &[]);
        assert_eq!(report.issues, vec![Issue::QuarantineFile(quarantined)]);
    }

    #[cfg(unix)]
    #[test]
    fn loose_index_permissions_are_reported() {
        use std::os::unix::fs::PermissionsExt;

        let dir = private_dir();
        let index = dir.path().join("projects.json");
        write_index(&index);

        fs::set_permissions(&index, fs::Permissions::from_mode(0o644)).expect("chmod 0644");
        let report = inspect(dir.path(), StorageBackend::Json, &[]);
        assert_eq!(
            report.issues,
            vec![Issue::IndexPermissions {
                path: index.clone(),
                mode: 0o644,
            }]
        );

        fs::set_permissions(&index, fs::Permissions::from_mode(0o600)).expect("chmod 0600");
        assert!(inspect(dir.path(), StorageBackend::Json, &[]).is_clean());

        fs::set_permissions(&index, fs::Permissions::from_mode(0o400)).expect("chmod 0400");
        let report = inspect(dir.path(), StorageBackend::Json, &[]);
        assert!(report.issues.contains(&Issue::IndexNotWritable(index)));
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_data_dir_is_reported() {
        use std::os::unix::fs::PermissionsExt;

        let dir = private_dir();
        let data = dir.path().join("data");
        let index = data.join("projects.json");
        write_index(&index);

        fs::set_permissions(&data, fs::Permissions::from_mode(0o500)).expect("chmod 0500");
        let report = inspect(&data, StorageBackend::Json, &[]);
        assert_eq!(report.issues, vec![Issue::DataDirNotWritable(data.clone())]);

        // Restore write access so the tempdir can be cleaned up.
        fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    }

    #[test]
    fn fix_session_collects_relocations_and_skips() {
        let dir = private_dir();
        let replacement = dir.path().join("replacement");
        fs::create_dir_all(&replacement).expect("create replacement");
        let projects = [
            project("one", &dir.path().join("missing-one")),
            project("two", &dir.path().join("missing-two")),
        ];

        let mut input = Cursor::new(format!("skip\n{}\n", replacement.display()));
        let mut output = Vec::new();
        let repairs = fix_session(&projects, None, &mut input, &mut output).expect("session");

        let canonical = project::normalize(&replacement).expect("canonicalize");
        assert_eq!(
            repairs,
            vec![Repair::Relocate {
                id: "two".to_owned(),
                path: canonical,
            }]
        );
        let log = String::from_utf8(output).expect("utf-8 log");
        assert!(log.contains("stale project `one`"));
        assert!(log.contains("stale project `two`"));
    }

    #[test]
    fn fix_session_removes_and_quits_early() {
        let dir = private_dir();
        let projects = [
            project("one", &dir.path().join("missing-one")),
            project("two", &dir.path().join("missing-two")),
            project("three", &dir.path().join("missing-three")),
        ];

        let mut input = Cursor::new("r\nq\ns\n");
        let mut output = Vec::new();
        let repairs = fix_session(&projects, None, &mut input, &mut output).expect("session");

        assert_eq!(
            repairs,
            vec![Repair::Remove {
                id: "one".to_owned(),
            }]
        );
    }

    #[test]
    fn fix_session_retries_after_invalid_answers() {
        let dir = private_dir();
        let healthy = dir.path().join("healthy");
        let replacement = dir.path().join("replacement");
        fs::create_dir_all(&healthy).expect("create healthy");
        fs::create_dir_all(&replacement).expect("create replacement");
        // Stored paths are canonical, so a raw tempdir path (under a
        // symlinked `/tmp`, for example) would not match the normalized
        // candidate.
        let projects = [
            project("stale", &dir.path().join("missing")),
            project("healthy", &project::normalize(&healthy).expect("normalize")),
        ];

        let input_text = format!(
            "\nno-such-directory\n{}\n{}\n",
            healthy.display(),
            replacement.display()
        );
        let mut input = Cursor::new(input_text);
        let mut output = Vec::new();
        let repairs = fix_session(&projects, None, &mut input, &mut output).expect("session");

        let canonical = project::normalize(&replacement).expect("canonicalize");
        assert_eq!(
            repairs,
            vec![Repair::Relocate {
                id: "stale".to_owned(),
                path: canonical,
            }]
        );
        let log = String::from_utf8(output).expect("utf-8 log");
        assert!(log.contains("expected [s]kip"));
        assert!(log.contains("directory does not exist"));
        assert!(log.contains("already uses this path"));
    }

    #[test]
    fn fix_session_expands_tilde_against_home() {
        let dir = private_dir();
        let replacement = dir.path().join("replacement");
        fs::create_dir_all(&replacement).expect("create replacement");
        let projects = [project("one", &dir.path().join("missing"))];

        let mut input = Cursor::new("~/replacement\n");
        let mut output = Vec::new();
        let repairs =
            fix_session(&projects, Some(dir.path()), &mut input, &mut output).expect("session");

        let canonical = project::normalize(&replacement).expect("canonicalize");
        assert_eq!(
            repairs,
            vec![Repair::Relocate {
                id: "one".to_owned(),
                path: canonical,
            }]
        );
    }

    #[test]
    fn fix_session_stops_at_input_end() {
        let dir = private_dir();
        let projects = [project("one", &dir.path().join("missing"))];

        let mut input = Cursor::new("");
        let mut output = Vec::new();
        let repairs = fix_session(&projects, None, &mut input, &mut output).expect("session");

        assert!(repairs.is_empty());
        let log = String::from_utf8(output).expect("utf-8 log");
        assert!(log.contains("input ended"));
    }

    #[test]
    fn apply_repairs_skips_vanished_and_taken_targets() {
        let dir = private_dir();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        let taken = dir.path().join("taken");
        for path in [&first, &second, &taken] {
            fs::create_dir_all(path).expect("create dir");
        }
        let mut projects = vec![
            project("first", &first),
            project("second", &second),
            project("taken", &taken),
        ];

        let repairs = vec![
            Repair::Relocate {
                id: "first".to_owned(),
                path: taken.clone(),
            },
            Repair::Remove {
                id: "vanished".to_owned(),
            },
            Repair::Remove {
                id: "second".to_owned(),
            },
        ];
        let applied = apply_repairs(&mut projects, &repairs);

        assert_eq!(applied, 1, "only the removal of `second` applies");
        assert_eq!(projects.len(), 2);
        assert_eq!(projects[0].path, first, "the taken path must not be reused");
    }

    #[test]
    fn apply_repairs_redetects_git_metadata() {
        let dir = private_dir();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        fs::create_dir_all(&old).expect("create old");
        fs::create_dir_all(&new).expect("create new");
        fs::create_dir_all(new.join(".git")).expect("create .git");

        let mut projects = vec![project("moving", &old)];
        let repairs = vec![Repair::Relocate {
            id: "moving".to_owned(),
            path: new.clone(),
        }];
        let applied = apply_repairs(&mut projects, &repairs);

        assert_eq!(applied, 1);
        assert_eq!(projects[0].path, new);
        assert!(
            projects[0].git.is_some(),
            "the new location is a repository"
        );
    }
}
