use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, ErrorCode, TransactionBehavior};

use crate::error::{Error, Result};
use crate::project::{GitInfo, Project};

use super::Store;

const CURRENT_VERSION: u32 = 1;

/// How long a writer waits for the database lock before giving up, matching
/// the JSON backend's `LOCK_TIMEOUT` so both feel alike under contention.
const BUSY_TIMEOUT: Duration = Duration::from_millis(500);

/// Jumps kept per project in the `usage` log; the oldest rows are pruned with
/// each new jump, so the log stays bounded.
const USAGE_KEEP: usize = 500;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS projects (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    path            TEXT NOT NULL UNIQUE,
    aliases         TEXT NOT NULL DEFAULT '[]',
    tags            TEXT NOT NULL DEFAULT '[]',
    created_at      TEXT NOT NULL,
    last_used_at    TEXT,
    use_count       INTEGER NOT NULL DEFAULT 0,
    pinned          INTEGER NOT NULL DEFAULT 0,
    git             INTEGER NOT NULL DEFAULT 0,
    git_remote_name TEXT,
    git_remote_url  TEXT
) STRICT;
CREATE TABLE IF NOT EXISTS usage (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    used_at    TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS usage_project_time ON usage(project_id, used_at);
";

const SELECT_PROJECTS: &str = "
SELECT id, name, path, aliases, tags, created_at, last_used_at, use_count,
       pinned, git, git_remote_name, git_remote_url
FROM projects
ORDER BY rowid
";

/// Upserts instead of `INSERT OR REPLACE` so a rewritten row keeps its
/// `usage` history (replace would delete and re-insert, cascading the log).
/// Used for both fresh inserts and rewrites: `ON CONFLICT` cannot fire on a
/// row that is not there yet.
const UPSERT_PROJECT: &str = "
INSERT INTO projects (
    id, name, path, aliases, tags, created_at, last_used_at, use_count,
    pinned, git, git_remote_name, git_remote_url
) VALUES (
    :id, :name, :path, :aliases, :tags, :created_at, :last_used_at,
    :use_count, :pinned, :git, :git_remote_name, :git_remote_url
)
ON CONFLICT(id) DO UPDATE SET
    name = excluded.name,
    path = excluded.path,
    aliases = excluded.aliases,
    tags = excluded.tags,
    created_at = excluded.created_at,
    last_used_at = excluded.last_used_at,
    use_count = excluded.use_count,
    pinned = excluded.pinned,
    git = excluded.git,
    git_remote_name = excluded.git_remote_name,
    git_remote_url = excluded.git_remote_url
";

const UPDATE_PROJECT: &str = "
UPDATE projects SET
    name = :name, path = :path, aliases = :aliases, tags = :tags,
    created_at = :created_at, last_used_at = :last_used_at,
    use_count = :use_count, pinned = :pinned, git = :git,
    git_remote_name = :git_remote_name, git_remote_url = :git_remote_url
WHERE id = :id
";

/// SQLite storage: WAL journal, one transaction per mutation.
///
/// `update` diffs the in-memory snapshot against the closure's result and
/// writes only the rows that changed, so a tracked jump costs one row update
/// instead of a whole-index rewrite. A missing database reads as an empty
/// index; the file is created with mode `0600` on Unix.
pub struct SqliteStore {
    path: PathBuf,
}

impl SqliteStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads the project set without creating the schema or taking the write
    /// lock; `doctor` uses this for an inactive database, so a foreign or
    /// empty file fails on the SELECT instead of being initialized.
    pub fn load_readonly(&self) -> Result<Vec<Project>> {
        crate::paths::refuse_symlink(&self.path, "SQLite database")?;
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let conn =
            Connection::open_with_flags(&self.path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|err| self.map_rusqlite(err))?;
        self.load_projects(&conn)
    }

    /// [`update`](Store::update) with a custom lock wait; used by tests.
    pub(crate) fn update_with_timeout<R>(
        &self,
        timeout: Duration,
        f: impl FnOnce(&mut Vec<Project>) -> Result<R>,
    ) -> Result<R> {
        let mut conn = self.open_with_timeout(timeout)?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|err| self.map_rusqlite(err))?;

        let before = self.load_projects(&transaction)?;
        let mut after = before.clone();
        let output = f(&mut after)?;
        self.apply_diff(&transaction, &before, &after)?;

        transaction.commit().map_err(|err| self.map_rusqlite(err))?;
        Ok(output)
    }

    /// Writes the whole snapshot in one transaction. Production callers go
    /// through [`Store::update`], which diffs; the migration and tests use
    /// this directly.
    pub(crate) fn save(&self, projects: &[Project]) -> Result<()> {
        let mut conn = self.open()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|err| self.map_rusqlite(err))?;

        // A temp table survives any project count without a huge `IN (...)`.
        transaction
            .execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS keep_ids (id TEXT PRIMARY KEY);\n\
                 DELETE FROM keep_ids;",
            )
            .map_err(|err| self.map_rusqlite(err))?;
        {
            let mut keep = transaction
                .prepare("INSERT OR IGNORE INTO keep_ids (id) VALUES (?1)")
                .map_err(|err| self.map_rusqlite(err))?;
            for project in projects {
                keep.execute([&project.id])
                    .map_err(|err| self.map_rusqlite(err))?;
            }
        }
        transaction
            .execute(
                "DELETE FROM projects WHERE id NOT IN (SELECT id FROM keep_ids)",
                [],
            )
            .map_err(|err| self.map_rusqlite(err))?;

        {
            let mut upsert = transaction
                .prepare(UPSERT_PROJECT)
                .map_err(|err| self.map_rusqlite(err))?;
            for project in projects {
                execute_project(&mut upsert, project).map_err(|err| self.map_rusqlite(err))?;
            }
        }

        transaction.commit().map_err(|err| self.map_rusqlite(err))?;
        Ok(())
    }

    fn open(&self) -> Result<Connection> {
        self.open_with_timeout(BUSY_TIMEOUT)
    }

    fn open_with_timeout(&self, timeout: Duration) -> Result<Connection> {
        if let Some(parent) = self.path.parent() {
            crate::paths::ensure_dir(parent)?;
        }
        crate::paths::refuse_symlink(&self.path, "SQLite database")?;
        create_private_file(&self.path)?;

        let conn = Connection::open(&self.path).map_err(|err| self.map_rusqlite(err))?;
        conn.busy_timeout(timeout)
            .map_err(|err| self.map_rusqlite(err))?;
        // Foreign keys are per-connection; the cascade from `projects` to
        // `usage` depends on it.
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|err| self.map_rusqlite(err))?;
        // Persistent once set; skip the write-privileged path when it is
        // already WAL so reads stay unblocked by other writers.
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .map_err(|err| self.map_rusqlite(err))?;
        if !mode.eq_ignore_ascii_case("wal") {
            let _mode: String = conn
                .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
                .map_err(|err| self.map_rusqlite(err))?;
        }

        self.ensure_schema(&conn)?;
        Ok(conn)
    }

    fn ensure_schema(&self, conn: &Connection) -> Result<()> {
        // Stay read-only when the schema already exists: running the DDL
        // (or the seed insert) on every open would take the write lock and
        // block readers behind another process's transaction.
        let has_meta: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
                [],
                |row| row.get(0),
            )
            .map_err(|err| self.map_rusqlite(err))?;
        if has_meta == 0 {
            conn.execute_batch(&format!(
                "BEGIN;\n{SCHEMA}\nINSERT OR IGNORE INTO meta (key, value) \
                 VALUES ('schema_version', '{CURRENT_VERSION}');\nCOMMIT;"
            ))
            .map_err(|err| self.map_rusqlite(err))?;
        }

        let version = self.read_schema_version(conn)?;
        if version > CURRENT_VERSION {
            return Err(self.storage_err(format!(
                "written by a newer version of overdosecd (schema version {version}, this build supports {CURRENT_VERSION})"
            )));
        }
        Ok(())
    }

    /// The stored schema version, parsed; an unparsable value is a storage
    /// error rather than a silent fallback.
    fn read_schema_version(&self, conn: &Connection) -> Result<u32> {
        let version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .map_err(|err| self.map_rusqlite(err))?;
        version
            .parse()
            .map_err(|_| self.storage_err(format!("unreadable schema version `{version}`")))
    }

    fn load_projects(&self, conn: &Connection) -> Result<Vec<Project>> {
        let mut statement = conn
            .prepare(SELECT_PROJECTS)
            .map_err(|err| self.map_rusqlite(err))?;
        let rows = statement
            .query_map([], row_to_project)
            .map_err(|err| self.map_rusqlite(err))?;

        let mut projects = Vec::new();
        for row in rows {
            projects.push(row.map_err(|err| self.map_rusqlite(err))?);
        }
        Ok(projects)
    }

    /// Writes the difference between `before` and `after`. Unchanged rows are
    /// left alone, and removed rows cascade their `usage` entries away.
    fn apply_diff(&self, conn: &Connection, before: &[Project], after: &[Project]) -> Result<()> {
        let after_ids: HashMap<&str, &Project> = after
            .iter()
            .map(|project| (project.id.as_str(), project))
            .collect();

        for stale in before
            .iter()
            .filter(|project| !after_ids.contains_key(project.id.as_str()))
        {
            conn.execute("DELETE FROM projects WHERE id = ?1", [&stale.id])
                .map_err(|err| self.map_rusqlite(err))?;
        }

        let before_ids: HashMap<&str, &Project> = before
            .iter()
            .map(|project| (project.id.as_str(), project))
            .collect();
        let mut update = conn
            .prepare(UPDATE_PROJECT)
            .map_err(|err| self.map_rusqlite(err))?;
        let mut insert = conn
            .prepare(UPSERT_PROJECT)
            .map_err(|err| self.map_rusqlite(err))?;

        for project in after {
            match before_ids.get(project.id.as_str()) {
                Some(old) if *old == project => {}
                Some(_) => {
                    execute_project(&mut update, project).map_err(|err| self.map_rusqlite(err))?;
                }
                None => {
                    execute_project(&mut insert, project).map_err(|err| self.map_rusqlite(err))?;
                }
            }
        }
        Ok(())
    }

    fn map_rusqlite(&self, err: rusqlite::Error) -> Error {
        if let rusqlite::Error::SqliteFailure(code, _) = &err
            && matches!(
                code.code,
                ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
            )
        {
            return Error::LockTimeout(self.path.clone());
        }
        self.storage_err(format!("could not access the database: {err}"))
    }

    fn storage_err(&self, message: String) -> Error {
        Error::Storage(format!("{}: {message}", self.path.display()))
    }
}

impl Store for SqliteStore {
    fn load(&self) -> Result<Vec<Project>> {
        crate::paths::refuse_symlink(&self.path, "SQLite database")?;
        // A missing database is an empty index; reads must not create files.
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let conn = self.open()?;
        self.load_projects(&conn)
    }

    fn update<R>(&self, f: impl FnOnce(&mut Vec<Project>) -> Result<R>) -> Result<R> {
        self.update_with_timeout(BUSY_TIMEOUT, f)
    }

    fn record_use(&self, id: &str, now: DateTime<Utc>) -> Result<()> {
        let mut conn = self.open()?;
        let transaction = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|err| self.map_rusqlite(err))?;

        let stamp = now.to_rfc3339_opts(SecondsFormat::AutoSi, true);
        let updated = transaction
            .execute(
                "UPDATE projects SET last_used_at = ?1, \
                 use_count = MIN(use_count + 1, 9223372036854775807) WHERE id = ?2",
                rusqlite::params![&stamp, id],
            )
            .map_err(|err| self.map_rusqlite(err))?;

        // A vanished id is ignored, matching the JSON backend's update path;
        // inserting for it would violate the foreign key anyway.
        if updated > 0 {
            transaction
                .execute(
                    "INSERT INTO usage (project_id, used_at) VALUES (?1, ?2)",
                    rusqlite::params![id, &stamp],
                )
                .map_err(|err| self.map_rusqlite(err))?;
            transaction
                .execute(
                    "DELETE FROM usage WHERE project_id = ?1 AND id NOT IN \
                     (SELECT id FROM usage WHERE project_id = ?1 \
                      ORDER BY used_at DESC, id DESC LIMIT ?2)",
                    rusqlite::params![id, USAGE_KEEP as i64],
                )
                .map_err(|err| self.map_rusqlite(err))?;
        }

        transaction.commit().map_err(|err| self.map_rusqlite(err))?;
        Ok(())
    }

    fn recent_uses(&self, id: &str, limit: usize) -> Result<Vec<DateTime<Utc>>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let conn = self.open()?;
        let mut statement = conn
            .prepare(
                "SELECT used_at FROM usage WHERE project_id = ?1 \
                 ORDER BY used_at DESC, id DESC LIMIT ?2",
            )
            .map_err(|err| self.map_rusqlite(err))?;
        let rows = statement
            .query_map(rusqlite::params![id, limit as i64], |row| {
                parse_time(&row.get::<_, String>(0)?, 0)
            })
            .map_err(|err| self.map_rusqlite(err))?;

        let mut times = Vec::new();
        for row in rows {
            times.push(row.map_err(|err| self.map_rusqlite(err))?);
        }
        Ok(times)
    }

    fn schema_version(&self) -> Result<Option<u32>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let conn = self.open()?;
        Ok(Some(self.read_schema_version(&conn)?))
    }
}

/// Binds one project to the named parameters every statement shares.
fn execute_project(
    statement: &mut rusqlite::Statement<'_>,
    project: &Project,
) -> rusqlite::Result<usize> {
    statement.execute(rusqlite::named_params! {
        ":id": &project.id,
        ":name": &project.name,
        ":path": project.path.to_string_lossy().into_owned(),
        ":aliases": serde_json::to_string(&project.aliases).unwrap_or_else(|_| "[]".to_owned()),
        ":tags": serde_json::to_string(&project.tags).unwrap_or_else(|_| "[]".to_owned()),
        ":created_at": project.created_at.to_rfc3339_opts(SecondsFormat::AutoSi, true),
        ":last_used_at": project
            .last_used_at
            .map(|when| when.to_rfc3339_opts(SecondsFormat::AutoSi, true)),
        ":use_count": i64::try_from(project.use_count).unwrap_or(i64::MAX),
        ":pinned": i64::from(project.pinned),
        ":git": i64::from(project.git.is_some()),
        ":git_remote_name": project.git.as_ref().and_then(|git| git.remote_name.as_deref()),
        ":git_remote_url": project.git.as_ref().and_then(|git| git.remote_url.as_deref()),
    })
}

fn row_to_project(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    let aliases: String = row.get(3)?;
    let tags: String = row.get(4)?;
    let created_at: String = row.get(5)?;
    let last_used_at: Option<String> = row.get(6)?;
    let use_count: i64 = row.get(7)?;
    let pinned: i64 = row.get(8)?;
    let git: i64 = row.get(9)?;

    Ok(Project {
        id: row.get(0)?,
        name: row.get(1)?,
        path: PathBuf::from(row.get::<_, String>(2)?),
        aliases: serde_json::from_str(&aliases).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(err))
        })?,
        tags: serde_json::from_str(&tags).map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(err))
        })?,
        created_at: parse_time(&created_at, 5)?,
        last_used_at: last_used_at
            .as_deref()
            .map(|raw| parse_time(raw, 6))
            .transpose()?,
        use_count: use_count.max(0) as u64,
        pinned: pinned != 0,
        git: if git != 0 {
            Some(GitInfo {
                remote_name: row.get(10)?,
                remote_url: row.get(11)?,
            })
        } else {
            None
        },
    })
}

fn parse_time(raw: &str, column: usize) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|when| when.with_timezone(&Utc))
        .map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(
                column,
                rusqlite::types::Type::Text,
                Box::new(err),
            )
        })
}

/// Creates the database file with mode `0600` up front, so SQLite never
/// creates it with wider permissions. The WAL and shared-memory files copy
/// the database's mode.
///
/// Every caller refuses a symlink on this path first (`refuse_symlink`), so
/// this only ever creates a regular file or steps aside for one.
fn create_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    match options.open(path) {
        Ok(_) => Ok(()),
        // Another process created it between the store's check and this open.
        Err(err) if err.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Project;
    use rusqlite::params;

    fn project(name: &str) -> Project {
        Project::for_test(name, &format!("/tmp/{name}"))
    }

    fn store(dir: &tempfile::TempDir) -> SqliteStore {
        SqliteStore::new(dir.path().join("projects.db"))
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(store(&dir).load().unwrap().is_empty());
    }

    #[test]
    fn round_trips_projects() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);

        let mut first = project("alpha");
        first.aliases = vec!["a".into()];
        first.tags = vec!["rust".into()];
        first.pinned = true;
        first.use_count = 7;
        first.last_used_at = Some(Utc::now());
        first.git = Some(GitInfo {
            remote_name: Some("origin".into()),
            remote_url: Some("git@github.com:o/alpha.git".into()),
        });
        let second = project("beta");

        store.save(&[first.clone(), second.clone()]).unwrap();
        let loaded = store.load().unwrap();

        assert_eq!(loaded, vec![first, second], "order must survive too");
    }

    #[test]
    fn newer_schema_version_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);
        store.save(&[project("alpha")]).unwrap();

        let conn = Connection::open(store.path()).unwrap();
        conn.execute(
            "UPDATE meta SET value = '99' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
        drop(conn);

        let err = store.load().unwrap_err();
        assert!(
            err.to_string().contains("newer version of overdosecd"),
            "error was: {err}"
        );
    }

    #[test]
    fn update_saves_and_returns_the_closure_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);

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
        let store = store(&dir);
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
    fn update_touches_only_changed_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);
        store.save(&[project("alpha"), project("bravo")]).unwrap();

        // A guard trigger proves the row is not rewritten: any UPDATE of
        // `bravo` aborts.
        let conn = Connection::open(store.path()).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER guard_bravo BEFORE UPDATE ON projects
             WHEN OLD.name = 'bravo'
             BEGIN SELECT RAISE(ABORT, 'bravo must not be rewritten'); END;",
        )
        .unwrap();
        drop(conn);

        let now = Utc::now();
        store
            .update(|projects| {
                projects
                    .iter_mut()
                    .find(|project| project.name == "alpha")
                    .unwrap()
                    .record_use(now);
                Ok(())
            })
            .unwrap();

        let loaded = store.load().unwrap();
        let alpha = loaded
            .iter()
            .find(|project| project.name == "alpha")
            .unwrap();
        assert_eq!(alpha.use_count, 1);
        assert_eq!(alpha.last_used_at, Some(now));

        let err = store
            .update(|projects| {
                projects
                    .iter_mut()
                    .find(|project| project.name == "bravo")
                    .unwrap()
                    .record_use(now);
                Ok(())
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("bravo must not be rewritten"),
            "error was: {err}"
        );
    }

    #[test]
    fn removing_a_project_cascades_its_usage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);
        let mut alpha = project("alpha");
        alpha.use_count = 1;
        store.save(&[alpha, project("bravo")]).unwrap();

        let conn = Connection::open(store.path()).unwrap();
        conn.execute(
            "INSERT INTO usage (project_id, used_at) VALUES (?1, ?2)",
            params!["alpha", "2026-09-30T00:00:00Z"],
        )
        .unwrap();
        drop(conn);

        store
            .update(|projects| {
                projects.retain(|project| project.id != "alpha");
                Ok(())
            })
            .unwrap();

        let conn = Connection::open(store.path()).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "delete must take the usage rows with it");
    }

    #[test]
    fn saving_keeps_usage_for_surviving_projects() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);
        store.save(&[project("alpha")]).unwrap();

        let conn = Connection::open(store.path()).unwrap();
        conn.execute(
            "INSERT INTO usage (project_id, used_at) VALUES (?1, ?2)",
            params!["alpha", "2026-09-30T00:00:00Z"],
        )
        .unwrap();
        drop(conn);

        let mut renamed = project("alpha");
        renamed.name = "alpha-renamed".into();
        store.save(&[renamed]).unwrap();

        let conn = Connection::open(store.path()).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "upsert must not cascade-delete the usage log");
    }

    #[test]
    fn update_times_out_when_the_lock_is_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);
        store.save(&[project("alpha")]).unwrap();

        let held = Connection::open(store.path()).unwrap();
        held.execute_batch("BEGIN IMMEDIATE").unwrap();

        let err = store
            .update_with_timeout(Duration::from_millis(50), |_| Ok(()))
            .unwrap_err();
        assert!(matches!(err, Error::LockTimeout(_)), "error was: {err:?}");

        held.execute_batch("ROLLBACK").unwrap();
        store
            .update(|_| Ok(()))
            .expect("the lock should be free after the rollback");
    }

    #[test]
    fn garbage_file_is_a_storage_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);
        fs::write(store.path(), "not a database").unwrap();

        let err = store.load().unwrap_err();
        assert_eq!(err.exit_code(), 3, "error was: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn database_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);
        store.save(&[project("alpha")]).unwrap();

        let mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the database must be private from creation");
    }

    #[test]
    fn record_use_appends_and_bumps() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);
        store.save(&[project("alpha")]).unwrap();

        let first = Utc::now();
        let second = first + chrono::Duration::seconds(5);
        store.record_use("alpha", first).unwrap();
        store.record_use("alpha", second).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded[0].use_count, 2);
        assert_eq!(loaded[0].last_used_at, Some(second));

        assert_eq!(
            store.recent_uses("alpha", 10).unwrap(),
            vec![second, first],
            "history is newest first"
        );
        assert_eq!(
            store.recent_uses("alpha", 1).unwrap(),
            vec![second],
            "the limit keeps the newest jumps"
        );
    }

    #[test]
    fn record_use_ignores_a_vanished_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);
        store.save(&[project("alpha")]).unwrap();

        store.record_use("ghost", Utc::now()).unwrap();

        let conn = Connection::open(store.path()).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "no usage row for a project that is not indexed");
    }

    #[test]
    fn usage_log_is_capped_per_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store(&dir);
        store.save(&[project("alpha")]).unwrap();

        let conn = Connection::open(store.path()).unwrap();
        conn.execute_batch("BEGIN").unwrap();
        for index in 0..USAGE_KEEP + 10 {
            conn.execute(
                "INSERT INTO usage (project_id, used_at) VALUES (?1, ?2)",
                params!["alpha", format!("2026-01-01T00:00:00.{index:09}Z")],
            )
            .unwrap();
        }
        conn.execute_batch("COMMIT").unwrap();
        drop(conn);

        store.record_use("alpha", Utc::now()).unwrap();

        let conn = Connection::open(store.path()).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, USAGE_KEEP as i64, "the oldest rows are pruned");
    }
}
