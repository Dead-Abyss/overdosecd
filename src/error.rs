use std::fmt;
use std::path::PathBuf;

use colored::Colorize;

use crate::matcher::Match;
use crate::output::shorten_home;
use crate::sanitize;

/// Errors that reach the user, mapped to exit codes by [`Error::exit_code`].
///
/// Values that can come from a planted index, a cloned repository, or an
/// environment variable are escaped at this boundary ([`sanitize`]), so
/// `error: …` is always one line a terminal can print as text.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("directory does not exist: {}", sanitize::path(.0))]
    MissingDirectory(PathBuf),

    #[error("not a directory: {}", sanitize::path(.0))]
    NotADirectory(PathBuf),

    #[error(
        "directory no longer exists: {} (project `{}`)",
        sanitize::path(.path),
        sanitize::text(.name)
    )]
    StaleDirectory { name: String, path: PathBuf },

    #[error("no project matched query: {}", sanitize::text(.0))]
    NoMatch(String),

    #[error("no projects indexed yet. run `overdosecd add <path>` to get started")]
    EmptyIndex,

    #[error("timed out waiting for the index lock: {}", sanitize::path(.0))]
    LockTimeout(PathBuf),

    #[error("multiple projects matched query: {}\n{candidates}", sanitize::text(.query))]
    Ambiguous {
        query: String,
        candidates: Candidates,
    },

    #[error(
        "no confident match for `{}` (best: {}, {reason})",
        sanitize::text(.query),
        sanitize::text(.name)
    )]
    WeakMatch {
        query: String,
        name: String,
        reason: &'static str,
    },

    #[error(
        "project already indexed as `{}`: {}\nuse --force to update it",
        sanitize::text(.name),
        sanitize::path(.path)
    )]
    Duplicate { name: String, path: PathBuf },

    #[error("{}", sanitize::text(.0))]
    InvalidValue(String),

    /// A path given through a flag or environment variable must be absolute:
    /// a relative one resolves against whatever directory the shell happens
    /// to be in, so a planted directory could choose the index for you.
    #[error("{kind} must be an absolute path: {}", sanitize::text(.value))]
    RelativePath { kind: &'static str, value: String },

    /// Unix paths can carry bytes that are not UTF-8; the index stores
    /// strings, and a lossy round-trip would silently point somewhere else.
    #[error("path is not valid UTF-8: {}", sanitize::path(.0))]
    NonUtf8Path(PathBuf),

    /// A store file that is a symbolic link could point into a directory the
    /// attacker controls, or hold a lock on a file they own.
    #[error(
        "refusing to use a symbolic link for the {kind}: {} (move it aside and let overdosecd recreate it)",
        sanitize::path(.path)
    )]
    SymlinkedPath { kind: &'static str, path: PathBuf },

    #[error("invalid {kind}: must not contain control or invisible characters: {value:?}")]
    ControlInValue { kind: &'static str, value: String },

    /// A path that cannot be printed as machine output: `goto`'s stdout is
    /// captured by `cd "$(…)"`, so an escaped path would be worse than none.
    #[error("refusing to print a path with control or invisible characters: {}", sanitize::text(.0))]
    UnsafePath(String),

    /// The output funnel caught a terminal control sequence that is not one of
    /// overdosecd's own colour changes: a renderer let a value through.
    #[error("internal error: refusing to write an unsafe terminal control sequence")]
    UnsafeOutput,

    /// A third-party store could not be imported: missing, unreadable, or a
    /// format this build does not understand.
    #[error(
        "cannot import from {}: {}",
        sanitize::path(.path),
        sanitize::text(.message)
    )]
    ImportSource { path: PathBuf, message: String },

    #[error(
        "a project is already named `{}` ({}); pass --force to rename anyway",
        sanitize::text(.name),
        sanitize::path(.path)
    )]
    NameTaken { name: String, path: PathBuf },

    #[error(
        "{kind} `{}` is not set on `{}`",
        sanitize::text(.value),
        sanitize::text(.project)
    )]
    TermNotSet {
        kind: &'static str,
        value: String,
        project: String,
    },

    #[error("project `{}` is no longer in the index (another process may have removed it)", sanitize::text(.0))]
    NoLongerIndexed(String),

    #[error(
        "refusing to repair in a non-interactive shell; run `overdosecd doctor --fix` from a terminal"
    )]
    FixRequiresTerminal,

    #[error("another project already uses this path: {}", sanitize::path(.0))]
    PathTaken(PathBuf),

    #[error("refusing to remove without confirmation in a non-interactive shell; pass --yes")]
    ConfirmationRequired,

    #[error("the picker needs a terminal; run it from a shell or use `ocd <query>` instead")]
    NoTerminal,

    #[error("nothing to migrate: {} does not exist", sanitize::path(.0))]
    NothingToMigrate(PathBuf),

    #[error(
        "refusing to overwrite {db}: it already holds projects; move or delete it to migrate {json}",
        db = sanitize::path(.db),
        json = sanitize::path(.json)
    )]
    MigrationClash { json: PathBuf, db: PathBuf },

    #[error(
        "invalid config {}: {}",
        sanitize::text(.origin),
        sanitize::text(.message)
    )]
    Config { origin: String, message: String },

    #[error(
        "config file not found: {} (from OVERDOSECD_CONFIG)",
        sanitize::path(.0)
    )]
    ConfigNotFound(PathBuf),

    #[error("{}", sanitize::text(.0))]
    Storage(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// Exit code for the CLI: user errors are `1`, storage problems are `3`.
    /// Clap reports usage errors with `2` on its own.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Io(_)
            | Error::Json(_)
            | Error::Storage(_)
            | Error::LockTimeout(_)
            | Error::Config { .. }
            | Error::ConfigNotFound(_)
            | Error::SymlinkedPath { .. }
            | Error::UnsafeOutput => 3,
            _ => 1,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Renders ambiguous-match candidates as an indented list.
#[derive(Debug, Clone)]
pub struct Candidates(pub Vec<Match>);

impl fmt::Display for Candidates {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let home = crate::project::home_dir();
        for (index, candidate) in self.0.iter().enumerate() {
            if index > 0 {
                writeln!(formatter)?;
            }
            write!(
                formatter,
                "  - {} -> {} ({})",
                sanitize::text(&candidate.name).bold(),
                shorten_home(&candidate.path, home.as_deref()).dimmed(),
                candidate.reason.label().dimmed()
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::Reason;
    use std::path::PathBuf;

    #[test]
    fn exit_codes_follow_the_documented_table() {
        assert_eq!(Error::NoMatch("x".into()).exit_code(), 1);
        assert_eq!(
            Error::Ambiguous {
                query: "x".into(),
                candidates: Candidates(Vec::new())
            }
            .exit_code(),
            1
        );
        assert_eq!(Error::Storage("boom".into()).exit_code(), 3);
        assert_eq!(Error::EmptyIndex.exit_code(), 1);
        assert_eq!(
            Error::LockTimeout(PathBuf::from("/tmp/projects.lock")).exit_code(),
            3
        );
        assert_eq!(
            Error::NameTaken {
                name: "x".into(),
                path: PathBuf::from("/x")
            }
            .exit_code(),
            1
        );
        assert_eq!(
            Error::TermNotSet {
                kind: "alias",
                value: "w".into(),
                project: "x".into()
            }
            .exit_code(),
            1
        );
        assert_eq!(Error::NoLongerIndexed("x".into()).exit_code(), 1);
        assert_eq!(Error::InvalidValue("boom".into()).exit_code(), 1);
        assert_eq!(
            Error::ControlInValue {
                kind: "alias",
                value: "a\nb".into()
            }
            .exit_code(),
            1
        );
        assert_eq!(
            Error::ImportSource {
                path: PathBuf::from("/x/db.zo"),
                message: "not found".into()
            }
            .exit_code(),
            1
        );
        assert_eq!(Error::FixRequiresTerminal.exit_code(), 1);
        assert_eq!(Error::NoTerminal.exit_code(), 1);
        assert_eq!(
            Error::WeakMatch {
                query: "wthr".into(),
                name: "overdosecd".into(),
                reason: "fuzzy name",
            }
            .exit_code(),
            1
        );
        assert_eq!(Error::PathTaken(PathBuf::from("/x")).exit_code(), 1);
        assert_eq!(
            Error::NothingToMigrate(PathBuf::from("/x/projects.json")).exit_code(),
            1
        );
        assert_eq!(
            Error::MigrationClash {
                json: PathBuf::from("/x/projects.json"),
                db: PathBuf::from("/x/projects.db")
            }
            .exit_code(),
            1
        );
        assert_eq!(
            Error::Config {
                origin: "/x/config.toml".into(),
                message: "boom".into()
            }
            .exit_code(),
            3
        );
        assert_eq!(
            Error::ConfigNotFound(PathBuf::from("/x/config.toml")).exit_code(),
            3
        );
        assert_eq!(Error::Io(std::io::Error::other("io")).exit_code(), 3);
    }

    #[test]
    fn control_errors_render_escaped_values() {
        let rendered = Error::ControlInValue {
            kind: "alias",
            value: "a\nb".into(),
        }
        .to_string();
        assert_eq!(
            rendered,
            r#"invalid alias: must not contain control or invisible characters: "a\nb""#
        );
        assert_eq!(rendered.lines().count(), 1);

        let rendered = Error::ControlInValue {
            kind: "name",
            value: "a\u{1b}]0;title".into(),
        }
        .to_string();
        assert!(
            rendered.contains(r#"\u{1b}]0;title"#),
            "the escape renders escaped, got: {rendered}"
        );
        assert_eq!(rendered.lines().count(), 1);

        // Bidi overrides are rejected and reported escaped, one line.
        let rendered = Error::ControlInValue {
            kind: "path",
            value: "safe\u{202e}txt".into(),
        }
        .to_string();
        assert!(rendered.contains(r"\u{202e}"), "got: {rendered}");
        assert_eq!(rendered.lines().count(), 1);
    }

    #[test]
    fn hostile_values_stay_one_escaped_line() {
        let escaped = "\u{1b}]52;c;cGF5bG9hZA==\u{7}";
        for rendered in [
            Error::MissingDirectory(PathBuf::from(format!("/tmp/{escaped}"))).to_string(),
            Error::StaleDirectory {
                name: format!("name{escaped}"),
                path: PathBuf::from("/tmp/x"),
            }
            .to_string(),
            Error::NoMatch(format!("q{escaped}")).to_string(),
            Error::NameTaken {
                name: format!("n{escaped}"),
                path: PathBuf::from("/tmp/x"),
            }
            .to_string(),
            Error::TermNotSet {
                kind: "alias",
                value: format!("v{escaped}"),
                project: "p".into(),
            }
            .to_string(),
            Error::NoLongerIndexed(format!("n{escaped}")).to_string(),
            Error::ImportSource {
                path: PathBuf::from(format!("/tmp/{escaped}")),
                message: format!("m{escaped}"),
            }
            .to_string(),
            Error::Config {
                origin: format!("o{escaped}"),
                message: format!("m{escaped}"),
            }
            .to_string(),
            Error::ConfigNotFound(PathBuf::from(format!("/tmp/{escaped}"))).to_string(),
            Error::Storage(format!("s{escaped}")).to_string(),
            Error::LockTimeout(PathBuf::from(format!("/tmp/{escaped}"))).to_string(),
            Error::NothingToMigrate(PathBuf::from(format!("/tmp/{escaped}"))).to_string(),
            Error::MigrationClash {
                json: PathBuf::from("/tmp/j"),
                db: PathBuf::from(format!("/tmp/{escaped}")),
            }
            .to_string(),
            Error::PathTaken(PathBuf::from(format!("/tmp/{escaped}"))).to_string(),
            Error::UnsafePath(format!("/tmp/{escaped}")).to_string(),
            Error::WeakMatch {
                query: format!("q{escaped}"),
                name: format!("n{escaped}"),
                reason: "fuzzy name",
            }
            .to_string(),
        ] {
            assert_eq!(rendered.lines().count(), 1, "one line: {rendered:?}");
            assert!(
                crate::sanitize::foreign_escape(&rendered).is_none(),
                "no terminal escapes may survive error rendering: {rendered:?}"
            );
        }

        // The duplicate message keeps its own template newline, nothing more.
        let rendered = Error::Duplicate {
            name: format!("n{escaped}"),
            path: PathBuf::from(format!("/tmp/{escaped}")),
        }
        .to_string();
        assert_eq!(rendered.lines().count(), 2, "template newline only");
        assert!(
            crate::sanitize::foreign_escape(&rendered).is_none(),
            "no terminal escapes: {rendered:?}"
        );
    }

    #[test]
    fn unsafe_variants_carry_the_documented_exit_codes() {
        assert_eq!(Error::UnsafePath("/x".into()).exit_code(), 1);
        assert_eq!(Error::UnsafeOutput.exit_code(), 3);
    }

    #[test]
    fn candidates_render_one_line_each() {
        let candidates = Candidates(vec![
            Match::for_test("app-one", "/code/app-one", 2_000, Reason::NamePrefix),
            Match::for_test("app-two", "/code/app-two", 1_990, Reason::NamePrefix),
        ]);
        let rendered = candidates.to_string();
        assert!(rendered.contains("  - app-one -> /code/app-one (name prefix)"));
        assert!(rendered.contains("  - app-two -> /code/app-two (name prefix)"));
        assert_eq!(rendered.lines().count(), 2);
    }
}
