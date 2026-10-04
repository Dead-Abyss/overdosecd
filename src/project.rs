use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// A single indexed directory.
///
/// The `id` is a short stable handle (useful for future rename/alias
/// commands); the canonical `path` is the unique key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub use_count: u64,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub git: Option<GitInfo>,
}

/// Git metadata captured when a project is added.
///
/// The remote is stable enough to index (it feeds matching); the branch is
/// volatile and read live by `info` instead. `Some` means "is a working tree",
/// even when no remote is configured.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitInfo {
    #[serde(default)]
    pub remote_name: Option<String>,
    #[serde(default)]
    pub remote_url: Option<String>,
}

impl GitInfo {
    pub fn detect(path: &Path) -> Option<Self> {
        crate::git::git_dir(path)?;
        let remote = crate::git::read_remote(path);
        Some(Self {
            remote_name: remote.as_ref().map(|remote| remote.name.clone()),
            remote_url: remote.map(|remote| remote.url),
        })
    }
}

impl Project {
    /// Records a successful jump: bumps the count and stamps the time.
    pub fn record_use(&mut self, now: DateTime<Utc>) {
        self.last_used_at = Some(now);
        self.use_count = self.use_count.saturating_add(1);
    }

    /// Creates a fresh, unpinned project with no aliases or tags.
    #[cfg(test)]
    pub(crate) fn for_test(name: &str, path: &str) -> Self {
        Self {
            id: name.to_owned(),
            name: name.to_owned(),
            path: PathBuf::from(path),
            aliases: Vec::new(),
            tags: Vec::new(),
            created_at: Utc::now(),
            last_used_at: None,
            use_count: 0,
            pinned: false,
            git: None,
        }
    }
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The user's home directory, if one can be determined.
pub fn home_dir() -> Option<PathBuf> {
    crate::dirs::home_dir()
}

/// Expands a leading `~` or `~/` using the current home directory.
pub fn expand_tilde(input: &str) -> PathBuf {
    expand_tilde_with(input, home_dir().as_deref())
}

/// Testable variant of [`expand_tilde`] with an explicit home directory.
pub fn expand_tilde_with(input: &str, home: Option<&Path>) -> PathBuf {
    match home {
        Some(home) if input == "~" => home.to_path_buf(),
        Some(home) => match input.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(input),
        },
        None => PathBuf::from(input),
    }
}

/// Canonicalizes a path: resolves symlinks, `.`, and `..`.
pub fn normalize(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path)
        .map_err(|err| Error::Storage(format!("could not resolve {}: {err}", path.display())))
}

/// Derives a default project name from the final path component.
pub fn default_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::Storage(format!(
                "could not derive a project name from {}; pass --name",
                path.display()
            ))
        })
}

/// The default display order: pinned first, then most recently used, then
/// name (case-insensitively). Shared by `list` and the picker so both agree.
fn display_order(a: &Project, b: &Project) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    b.pinned
        .cmp(&a.pinned)
        .then_with(|| match (a.last_used_at, b.last_used_at) {
            (Some(a_time), Some(b_time)) => b_time.cmp(&a_time),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        })
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
}

/// The comparator behind every sort order `list --sort` and the picker's sort
/// cycle offer. [`SortBy::Used`] is [`display_order`]; the explicit orders
/// ignore pinning.
pub fn sort_key(a: &Project, b: &Project, sort: crate::cli::SortBy) -> std::cmp::Ordering {
    match sort {
        crate::cli::SortBy::Used => display_order(a, b),
        crate::cli::SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        crate::cli::SortBy::Created => b
            .created_at
            .cmp(&a.created_at)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
    }
}

/// Inserts a project for the canonical `path`, or (with `force`) updates the
/// existing entry for it. Returns the user-facing message. Shared by
/// `overdosecd add` and the picker's add prompt.
pub fn insert(
    projects: &mut Vec<Project>,
    path: PathBuf,
    name: Option<String>,
    aliases: Vec<String>,
    tags: Vec<String>,
    force: bool,
) -> Result<String> {
    if let Some(existing) = projects.iter_mut().find(|project| project.path == path) {
        if !force {
            return Err(Error::Duplicate {
                name: existing.name.clone(),
                path: path.clone(),
            });
        }
        if let Some(name) = name {
            existing.name = name;
        }
        existing.git = GitInfo::detect(&path);
        for alias in &aliases {
            push_unique(&mut existing.aliases, alias);
        }
        for tag in &tags {
            push_unique(&mut existing.tags, tag);
        }
        return Ok(format!(
            "updated `{}` -> {}",
            crate::sanitize::text(&existing.name),
            crate::sanitize::path(&path)
        ));
    }

    let name = match name {
        Some(name) => name,
        None => default_name(&path)?,
    };
    let project = Project {
        id: new_id(projects),
        name,
        path: path.clone(),
        aliases,
        tags,
        created_at: Utc::now(),
        last_used_at: None,
        use_count: 0,
        pinned: false,
        git: GitInfo::detect(&path),
    };
    let name = project.name.clone();
    projects.push(project);
    Ok(format!(
        "added `{}` -> {}",
        crate::sanitize::text(&name),
        crate::sanitize::path(&path)
    ))
}

/// Generates a short random id that does not collide with `existing`.
fn new_id(existing: &[Project]) -> String {
    loop {
        let id = random_id();
        if !existing.iter().any(|project| project.id == id) {
            return id;
        }
    }
}

fn random_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let mut hasher = RandomState::new().build_hasher();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    hasher.write_u128(now.as_nanos());
    hasher.write_u64(ID_COUNTER.fetch_add(1, Ordering::Relaxed));
    format!("{:08x}", (hasher.finish() & 0xffff_ffff) as u32)
}

/// Rejects characters that must not reach a terminal or machine output in
/// names, aliases, tags, and paths.
///
/// The set lives in [`crate::sanitize::is_dangerous`]: control characters
/// would break table rows and `cd "$(overdosecd goto …)"`, and a terminal would
/// interpret escape sequences (OSC 52 among them) instead of showing them — a
/// path is attacker-controlled as soon as a repository you cloned brought its
/// own directory names. Bidi overrides and never-visible characters are
/// refused for the same reason: the displayed value must be the stored one.
pub fn reject_control_chars(kind: &'static str, value: &str) -> Result<()> {
    if crate::sanitize::contains_dangerous(value) {
        return Err(Error::ControlInValue {
            kind,
            value: value.to_owned(),
        });
    }
    Ok(())
}

/// [`reject_control_chars`] for a filesystem path.
///
/// Also refuses a path that is not valid UTF-8: the index stores strings, and
/// a lossy `U+FFFD` round-trip would silently point somewhere else (the
/// SQLite backend used to accept one and store a row it could never load).
pub fn reject_control_chars_path(path: &Path) -> Result<()> {
    let Some(value) = path.to_str() else {
        return Err(Error::NonUtf8Path(path.to_path_buf()));
    };
    reject_control_chars("path", value)
}

/// Appends a trimmed value unless it is empty or already present
/// (case-insensitively).
pub fn push_unique(list: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    if !list
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(value))
    {
        list.push(value.to_owned());
    }
}

/// Removes a value case-insensitively (after trimming); returns whether it
/// was present.
fn remove_unique(list: &mut Vec<String>, value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() {
        return false;
    }
    match list
        .iter()
        .position(|existing| existing.eq_ignore_ascii_case(value))
    {
        Some(index) => {
            list.remove(index);
            true
        }
        None => false,
    }
}

/// Which term list an alias/tag mutation touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermKind {
    Alias,
    Tag,
}

impl TermKind {
    /// The user-facing noun, used in messages and errors.
    pub fn label(self) -> &'static str {
        match self {
            TermKind::Alias => "alias",
            TermKind::Tag => "tag",
        }
    }

    fn values(self, project: &Project) -> &[String] {
        match self {
            TermKind::Alias => &project.aliases,
            TermKind::Tag => &project.tags,
        }
    }

    fn values_mut(self, project: &mut Project) -> &mut Vec<String> {
        match self {
            TermKind::Alias => &mut project.aliases,
            TermKind::Tag => &mut project.tags,
        }
    }
}

/// The one rule behind every user-supplied name, alias, and tag: trim, refuse
/// control and invisible characters, refuse empty.
pub fn sanitize_value(what: &'static str, value: &str) -> Result<String> {
    let value = value.trim().to_owned();
    reject_control_chars(what, &value)?;
    if value.is_empty() {
        return Err(Error::InvalidValue(format!("{what} must not be empty")));
    }
    Ok(value)
}

/// Adds or removes one alias/tag on `project`, returning the message both the
/// CLI and the picker print. The project name is escaped here: the CLI prints
/// the message as-is, and the picker's renderer leaves already-escaped text
/// alone.
pub fn set_term(project: &mut Project, kind: TermKind, value: &str, add: bool) -> Result<String> {
    let name = crate::sanitize::text(&project.name);
    if add {
        if kind
            .values(project)
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(value))
        {
            return Ok(format!(
                "{} `{value}` is already set on `{name}`",
                kind.label()
            ));
        }
        kind.values_mut(project).push(value.to_owned());
        Ok(format!("added {} `{value}` to `{name}`", kind.label()))
    } else {
        if !remove_unique(kind.values_mut(project), value) {
            return Err(Error::TermNotSet {
                kind: kind.label(),
                value: value.to_owned(),
                project: project.name.clone(),
            });
        }
        Ok(format!("removed {} `{value}` from `{name}`", kind.label()))
    }
}

/// Re-resolves a project by id inside [`Store::update`](crate::store::Store::update).
/// Mutations resolve their target before taking the lock and re-verify here,
/// since another process may have removed it; `label` names the missing
/// project in the error.
pub fn revalidate<'a>(
    projects: &'a mut [Project],
    id: &str,
    label: String,
) -> Result<&'a mut Project> {
    projects
        .iter_mut()
        .find(|project| project.id == id)
        .ok_or(Error::NoLongerIndexed(label))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_tilde_only_at_the_start() {
        let home = Path::new("/home/user");
        assert_eq!(
            expand_tilde_with("~/code", Some(home)),
            PathBuf::from("/home/user/code")
        );
        assert_eq!(
            expand_tilde_with("~", Some(home)),
            PathBuf::from("/home/user")
        );
        assert_eq!(
            expand_tilde_with("~other/code", Some(home)),
            PathBuf::from("~other/code"),
            "~user is not supported"
        );
        assert_eq!(expand_tilde_with("code", Some(home)), PathBuf::from("code"));
        assert_eq!(
            expand_tilde_with("~/code", None),
            PathBuf::from("~/code"),
            "no home means no expansion"
        );
    }

    #[test]
    fn default_name_uses_the_last_component() {
        assert_eq!(default_name(Path::new("/a/b/c")).unwrap(), "c");
        assert_eq!(default_name(Path::new("relative")).unwrap(), "relative");
        assert!(default_name(Path::new("/")).is_err());
    }

    #[test]
    fn record_use_stamps_time_and_counts() {
        let mut project = Project::for_test("alpha", "/tmp/alpha");
        let now = Utc::now();

        project.record_use(now);
        assert_eq!(project.last_used_at, Some(now));
        assert_eq!(project.use_count, 1);

        project.record_use(now);
        assert_eq!(project.use_count, 2);

        project.use_count = u64::MAX;
        project.record_use(now);
        assert_eq!(project.use_count, u64::MAX, "the count must saturate");
    }

    #[test]
    fn insert_adds_a_project_and_rejects_a_duplicate() {
        let mut projects = Vec::new();
        let message = insert(
            &mut projects,
            PathBuf::from("/x/alpha"),
            None,
            Vec::new(),
            Vec::new(),
            false,
        )
        .expect("insert a fresh project");
        assert!(
            message.starts_with("added `alpha`"),
            "message was: {message}"
        );
        assert_eq!(projects.len(), 1);
        assert!(!projects[0].id.is_empty());

        let err = insert(
            &mut projects,
            PathBuf::from("/x/alpha"),
            None,
            Vec::new(),
            Vec::new(),
            false,
        )
        .expect_err("duplicate paths are refused without --force");
        assert!(matches!(err, Error::Duplicate { .. }));
    }

    #[test]
    fn insert_with_force_updates_the_existing_entry() {
        let mut projects = Vec::new();
        insert(
            &mut projects,
            PathBuf::from("/x/alpha"),
            None,
            Vec::new(),
            Vec::new(),
            false,
        )
        .expect("insert a fresh project");

        let message = insert(
            &mut projects,
            PathBuf::from("/x/alpha"),
            Some("beta".into()),
            vec!["b".into()],
            vec!["t".into()],
            true,
        )
        .expect("force updates the entry");
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "beta");
        assert_eq!(projects[0].aliases, vec!["b".to_owned()]);
        assert_eq!(projects[0].tags, vec!["t".to_owned()]);
        assert!(message.contains("updated `beta`"), "message was: {message}");
    }

    #[test]
    fn ids_are_short_unique_hex() {
        let mut projects = Vec::new();
        for _ in 0..1_000 {
            let id = new_id(&projects);
            assert_eq!(id.len(), 8, "id should be eight hex characters: {id}");
            assert!(id.chars().all(|ch| ch.is_ascii_hexdigit()));
            assert!(
                !projects.iter().any(|project: &Project| project.id == id),
                "ids must be unique"
            );
            projects.push(Project {
                id,
                ..Project::for_test("sample", "/tmp/sample")
            });
        }
    }

    #[test]
    fn control_char_guards_reject_escapes_and_lines() {
        assert!(reject_control_chars("alias", "fine").is_ok());
        assert!(reject_control_chars("alias", "two words").is_ok());
        assert!(reject_control_chars("alias", "café ☕").is_ok());
        for bad in [
            "bad\nvalue",
            "bad\rvalue",
            "bad\tvalue",
            "bad\u{0}value",
            "bad\u{1b}[31mred",
            "bad\u{7f}value",
            "bad\u{9b}value",
            "bad\u{202e}value",
            "bad\u{2066}value",
            "bad\u{200b}value",
            "bad\u{feff}value",
        ] {
            assert!(
                matches!(
                    reject_control_chars("alias", bad),
                    Err(Error::ControlInValue { .. })
                ),
                "should reject {bad:?}"
            );
            assert!(crate::sanitize::contains_dangerous(bad));
        }
        assert!(reject_control_chars_path(Path::new("/tmp/bad\npath")).is_err());
        assert!(reject_control_chars_path(Path::new("/tmp/bad\u{1b}path")).is_err());
        assert!(reject_control_chars_path(Path::new("/tmp/fine")).is_ok());

        // ZWJ/ZWNJ stay legal: they are load-bearing in emoji and in complex
        // scripts, and rejecting them would refuse legitimate names.
        assert!(reject_control_chars("name", "👨\u{200d}👩\u{200d}👧").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_refused() {
        use std::os::unix::ffi::OsStringExt;

        let raw = std::ffi::OsString::from_vec(b"/tmp/bad\xffname".to_vec());
        let err = reject_control_chars_path(Path::new(&raw)).expect_err("refused");
        assert!(matches!(err, Error::NonUtf8Path(_)), "{err:?}");
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn push_unique_trims_and_ignores_case() {
        let mut list = Vec::new();
        push_unique(&mut list, "  Rust ");
        push_unique(&mut list, "rust");
        push_unique(&mut list, "");
        assert_eq!(list, vec!["Rust".to_owned()]);
    }

    #[test]
    fn remove_unique_matches_case_insensitively() {
        let mut list = vec!["Rust".to_owned(), "cli".to_owned()];
        assert!(remove_unique(&mut list, " rust "));
        assert!(!remove_unique(&mut list, "rust"));
        assert_eq!(list, vec!["cli".to_owned()]);
        assert!(!remove_unique(&mut list, "  "));
    }
}
