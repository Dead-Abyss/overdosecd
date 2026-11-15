use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::paths;
use crate::project::Project;
use crate::store::json::read_legacy;
use crate::store::sqlite::SqliteStore;

/// What [`run`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The JSON index moved into the database; the original is kept as
    /// `backup`.
    Migrated { projects: usize, backup: PathBuf },

    /// A database was already present, with no JSON index left beside it.
    AlreadyMigrated { db: PathBuf },
}

/// Moves `projects.json` into `projects.db`.
///
/// The JSON original is renamed to `projects.json.migrated-<timestamp>` and
/// never deleted. Running twice is fine: the second run reports
/// [`Outcome::AlreadyMigrated`]. A database that already holds projects is
/// refused instead of overwritten; an empty one (schema only) is reused.
pub fn run(data_dir: &Path) -> Result<Outcome> {
    run_with(data_dir, &|db, projects| {
        SqliteStore::new(db).save(projects)
    })
}

/// [`run`] with an injectable save step, so a failed database write can be
/// exercised without a read-only filesystem.
fn run_with(data_dir: &Path, save: &dyn Fn(PathBuf, &[Project]) -> Result<()>) -> Result<Outcome> {
    let json_path = paths::store_file(data_dir);
    let db_path = paths::sqlite_file(data_dir);

    if db_path.exists() {
        if !json_path.exists() {
            return Ok(Outcome::AlreadyMigrated { db: db_path });
        }
        if !SqliteStore::new(db_path.clone()).is_empty()? {
            return Err(Error::MigrationClash {
                json: json_path,
                db: db_path,
            });
        }
    } else if !json_path.exists() {
        return Err(Error::NothingToMigrate(json_path));
    }

    let projects = read_legacy(&json_path)?;
    let count = projects.len();

    // Move the JSON aside *before* writing the database. The old order
    // populated the database first, so a failed rename left "populated DB +
    // live JSON" behind, which every later command reports as
    // `MigrationClash` — permanently. Now a failed save simply puts the JSON
    // back and the migration is retryable.
    //
    // The marker narrows what a crash can hide: it exists only while the JSON
    // is renamed aside and the database is not yet populated, so startup can
    // tell that state apart from a legitimately empty index.
    let marker = marker_path(data_dir);
    fs::write(&marker, b"migrating\n")?;

    let backup = paths::sibling_backup(&json_path, "migrated");
    if let Err(err) = fs::rename(&json_path, &backup) {
        let _ = fs::remove_file(&marker);
        return Err(err.into());
    }

    if let Err(err) = save(db_path, &projects) {
        // Best effort: restore the original name so the next run retries.
        // A failed restore is reported alongside the save error.
        if let Err(restore) = fs::rename(&backup, &json_path) {
            return Err(Error::Storage(format!(
                "{err}; the legacy index could not be moved back from {}: {restore}",
                backup.display()
            )));
        }
        let _ = fs::remove_file(&marker);
        return Err(err);
    }

    // The database is populated; the marker has served its purpose. A failed
    // removal is harmless: the recovery check only fires for an *empty* index.
    let _ = fs::remove_file(&marker);
    Ok(Outcome::Migrated {
        projects: count,
        backup,
    })
}

/// The transient marker written during `run`'s rename window.
fn marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join("projects.json.migrating")
}

/// The startup one-shot: migrates a `projects.json` left by 0.2.0 before any
/// command touches the index. `None` when there is nothing to migrate.
///
/// A corrupt legacy file is quarantined by [`read_legacy`] and the error is
/// returned; a clash with a populated database is [`Error::MigrationClash`].
///
/// A crash while the migration marker exists leaves no live JSON and an empty
/// (or missing) database — which used to read as an empty index, silently.
/// That state is reported instead, so the backup is findable.
pub fn auto(data_dir: &Path) -> Result<Option<Outcome>> {
    if !paths::store_file(data_dir).exists() {
        if marker_path(data_dir).exists()
            && let Some(backup) = interrupted_backup(data_dir)
        {
            let db = paths::sqlite_file(data_dir);
            let empty = !db.exists() || SqliteStore::new(db).is_empty().unwrap_or(true);
            if empty {
                return Err(Error::Storage(format!(
                    "an interrupted migration left no index; {} holds a migrated copy — rename it back to {} to recover (or remove it to start empty)",
                    backup.display(),
                    paths::store_file(data_dir).display()
                )));
            }
        }
        return Ok(None);
    }
    run(data_dir).map(Some)
}

/// The newest `projects.json.migrated-*` sibling, if one exists.
fn interrupted_backup(data_dir: &Path) -> Option<PathBuf> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(data_dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("projects.json.migrated-") {
            continue;
        }
        let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(when, _)| modified > *when) {
            newest = Some((modified, entry.path()));
        }
    }
    newest.map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Project;
    use crate::store::json::StoreFile;

    fn directory() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn project(name: &str) -> Project {
        Project::for_test(name, &format!("/tmp/{name}"))
    }

    fn write_json(dir: &Path, projects: &[Project]) {
        let body =
            serde_json::to_string_pretty(&StoreFile::new(projects.to_vec())).expect("encode index");
        crate::paths::write_private(&paths::store_file(dir), body.as_bytes()).expect("write json");
    }

    #[test]
    fn auto_does_nothing_without_a_legacy_index() {
        let dir = directory();
        assert!(
            auto(dir.path()).expect("auto is quiet").is_none(),
            "no json, no migration"
        );
    }

    #[test]
    fn an_interrupted_migration_is_reported_instead_of_silently_empty() {
        let dir = directory();
        let backup = paths::sibling_backup(&paths::store_file(dir.path()), "migrated");
        fs::write(&backup, "[]").expect("write backup");
        // The crash state: the marker exists, the JSON was moved aside, and
        // the database is absent (or schema-only).
        fs::write(marker_path(dir.path()), b"migrating\n").expect("write marker");
        SqliteStore::new(paths::sqlite_file(dir.path()))
            .save(&[])
            .expect("schema-only database");

        let err = auto(dir.path()).expect_err("an interrupted migration is loud");
        let rendered = err.to_string();
        assert!(
            rendered.contains(&backup.display().to_string()),
            "the message must name the backup to restore: {rendered}"
        );
        assert!(
            rendered.contains("interrupted"),
            "the message must say what happened: {rendered}"
        );

        // Without the marker it is just a (legitimately) empty index.
        fs::remove_file(marker_path(dir.path())).expect("remove marker");
        assert!(
            auto(dir.path()).expect("quiet again").is_none(),
            "an empty index with an old backup is not an interrupted migration"
        );
    }

    #[test]
    fn a_successful_migration_clears_the_marker() {
        let dir = directory();
        write_json(dir.path(), &[project("alpha")]);
        run(dir.path()).expect("migrate");
        assert!(
            !marker_path(dir.path()).exists(),
            "the transient marker must not survive a successful migration"
        );
        assert!(auto(dir.path()).expect("quiet").is_none());
    }

    #[test]
    fn auto_migrates_a_legacy_index_once() {
        let dir = directory();
        write_json(dir.path(), &[project("alpha"), project("beta")]);

        let outcome = auto(dir.path())
            .expect("auto migrates")
            .expect("an outcome");
        assert!(matches!(outcome, Outcome::Migrated { projects: 2, .. }));
        assert!(
            !paths::store_file(dir.path()).exists(),
            "the live json moves aside"
        );
        assert!(
            auto(dir.path()).expect("second run").is_none(),
            "the second run has nothing to do"
        );
        let loaded = SqliteStore::new(paths::sqlite_file(dir.path()))
            .load()
            .expect("load migrated database");
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn migrates_projects_and_keeps_the_original() {
        let dir = directory();
        write_json(dir.path(), &[project("alpha"), project("beta")]);

        let outcome = run(dir.path()).expect("migrate");
        let Outcome::Migrated { projects, backup } = outcome else {
            panic!("expected a migration");
        };
        assert_eq!(projects, 2);
        assert!(backup.exists(), "the original must be kept");
        assert!(
            !paths::store_file(dir.path()).exists(),
            "the live json must move aside"
        );

        let loaded = SqliteStore::new(paths::sqlite_file(dir.path()))
            .load()
            .expect("load migrated database");
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn running_twice_reports_already_migrated() {
        let dir = directory();
        write_json(dir.path(), &[project("alpha")]);
        run(dir.path()).expect("first migrate");

        let outcome = run(dir.path()).expect("second migrate");
        assert!(matches!(outcome, Outcome::AlreadyMigrated { .. }));
    }

    #[test]
    fn refuses_to_overwrite_a_database_with_projects() {
        let dir = directory();
        write_json(dir.path(), &[project("alpha")]);
        run(dir.path()).expect("first migrate");

        // A new JSON index appears while the database holds data.
        write_json(dir.path(), &[project("beta")]);
        let err = run(dir.path()).unwrap_err();
        assert!(
            matches!(err, Error::MigrationClash { .. }),
            "error was: {err:?}"
        );
        assert!(
            paths::store_file(dir.path()).exists(),
            "a refused migration must leave the json in place"
        );
    }

    #[test]
    fn nothing_to_migrate_is_an_error() {
        let dir = directory();
        let err = run(dir.path()).unwrap_err();
        assert!(matches!(err, Error::NothingToMigrate(_)));
    }

    #[test]
    fn a_failed_save_restores_the_json_and_stays_retryable() {
        let dir = directory();
        write_json(dir.path(), &[project("alpha")]);

        let err = run_with(dir.path(), &|_db, _projects| {
            Err(Error::Storage("simulated write failure".to_owned()))
        })
        .expect_err("the save fails");
        assert!(err.to_string().contains("simulated"), "error was: {err}");
        assert!(
            paths::store_file(dir.path()).exists(),
            "the legacy index must be back in place, not renamed aside"
        );
        // A real `save` may have created the schema before its transaction
        // fails; what matters is that no project data landed.
        assert!(
            SqliteStore::new(paths::sqlite_file(dir.path()))
                .is_empty()
                .unwrap_or(true),
            "the failed save must not leave a populated database"
        );

        // The next run (with a working save) goes through.
        let outcome = run(dir.path()).expect("retry succeeds");
        assert!(matches!(outcome, Outcome::Migrated { projects: 1, .. }));
    }

    #[test]
    fn an_empty_database_is_reused() {
        let dir = directory();
        SqliteStore::new(paths::sqlite_file(dir.path()))
            .save(&[])
            .expect("create empty database");
        write_json(dir.path(), &[project("alpha")]);

        let outcome = run(dir.path()).expect("migrate into the empty database");
        assert!(
            matches!(outcome, Outcome::Migrated { projects: 1, .. }),
            "expected a one-project migration, got {outcome:?}"
        );
    }

    #[test]
    fn backups_never_overwrite_each_other() {
        let dir = directory();
        write_json(dir.path(), &[project("alpha")]);
        let Outcome::Migrated { backup: first, .. } = run(dir.path()).unwrap() else {
            panic!("expected the first migration");
        };

        // Empty the database again so a second migration is allowed.
        SqliteStore::new(paths::sqlite_file(dir.path()))
            .save(&[])
            .expect("empty the database");
        write_json(dir.path(), &[project("beta")]);
        let Outcome::Migrated { backup: second, .. } = run(dir.path()).unwrap() else {
            panic!("expected the second migration");
        };

        assert_ne!(first, second, "backup names must not collide");
        assert!(first.exists() && second.exists());
    }
}
