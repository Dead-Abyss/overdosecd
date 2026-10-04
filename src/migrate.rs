use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::paths;
use crate::store::Store;
use crate::store::json::JsonStore;
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
    let json_path = paths::store_file(data_dir);
    let db_path = paths::sqlite_file(data_dir);

    if db_path.exists() {
        if !json_path.exists() {
            return Ok(Outcome::AlreadyMigrated { db: db_path });
        }
        if !SqliteStore::new(db_path.clone()).load()?.is_empty() {
            return Err(Error::MigrationClash {
                json: json_path,
                db: db_path,
            });
        }
    } else if !json_path.exists() {
        return Err(Error::NothingToMigrate(json_path));
    }

    let json = JsonStore::new(json_path.clone());
    let projects = json.load()?;
    let count = projects.len();

    SqliteStore::new(db_path).save(&projects)?;

    let backup = paths::sibling_backup(&json_path, "migrated");
    fs::rename(&json_path, &backup)?;
    Ok(Outcome::Migrated {
        projects: count,
        backup,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Project;

    fn directory() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn project(name: &str) -> Project {
        Project::for_test(name, &format!("/tmp/{name}"))
    }

    fn write_json(dir: &Path, projects: &[Project]) {
        JsonStore::new(paths::store_file(dir))
            .save(projects)
            .expect("write json");
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
