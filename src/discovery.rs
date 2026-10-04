//! Home-directory discovery: the fuzzy fallback for queries the index does
//! not answer.
//!
//! Nothing here ever adds to the index. A bounded walk under `$HOME` fills a
//! rebuildable cache (`home-dirs.json`), and [`rank`] scores the cached
//! directories for the picker. The walk does not follow symlinks, skips a
//! built-in list of cache/build directories, and caps its depth and size, so
//! "search my home" stays a bounded, predictable operation.

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::sanitize::contains_dangerous;

/// Directories the walk never enters: caches, build outputs, and vendored
/// trees that would drown the cache without ever being the target.
const DEFAULT_SKIP: [&str; 18] = [
    ".git",
    "node_modules",
    ".cache",
    ".npm",
    ".cargo",
    ".rustup",
    ".venv",
    "venv",
    "__pycache__",
    ".tox",
    ".pnpm-store",
    ".yarn",
    ".gradle",
    ".m2",
    "target",
    "dist",
    "build",
    ".local/share/Trash",
];

/// Files that mark a directory as a project.
const MARKERS: [&str; 4] = ["Cargo.toml", "package.json", "pyproject.toml", "go.mod"];

/// How the walk behaves; the picker builds this from `[discovery]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub max_depth: u16,
    pub max_entries: usize,
    /// Extra directory names to skip, on top of [`DEFAULT_SKIP`].
    pub skip: Vec<String>,
}

/// One directory the walk found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dir {
    pub path: PathBuf,
    pub name: String,
    pub depth: u16,
    /// Has `.git` or a project marker file.
    pub project_like: bool,
}

/// A ranked home-directory match for a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeMatch {
    pub path: PathBuf,
    pub name: String,
    pub score: i64,
}

/// How many more candidates than `max_entries` the walk may collect before
/// it stops; enough for `best_first` to choose well, bounded regardless.
const WALK_OVERSHOOT: usize = 8;

/// Whether `path` is skipped: a pattern without `/` matches the directory
/// name, one with `/` matches the trailing path components, so
/// `.local/share/Trash` actually skips (it never could as a bare name).
fn is_skipped(skip: &[&str], name: &str, path: &Path) -> bool {
    skip.iter().any(|pattern| {
        if pattern.contains('/') {
            let mut wanted = pattern.rsplit('/');
            let mut components = path.components().rev();
            wanted.all(|part| {
                components
                    .next()
                    .is_some_and(|component| component.as_os_str() == part)
            })
        } else {
            *pattern == name
        }
    })
}

/// Walks `root` and returns every directory it may offer, shallowest and
/// project-like first when the cap bites. `root` itself is not included.
pub fn scan(root: &Path, options: &Options) -> Vec<Dir> {
    let skip: Vec<&str> = DEFAULT_SKIP
        .iter()
        .copied()
        .chain(options.skip.iter().map(String::as_str))
        .collect();

    let mut dirs: Vec<Dir> = Vec::new();
    let mut queue: VecDeque<(PathBuf, u16)> = VecDeque::new();
    queue.push_back((root.to_path_buf(), 0));

    // The cap is applied while walking, not only at the end: a planted tree
    // of millions of empty directories must not grow the queue unbounded
    // before `truncate` ever runs. The overshoot keeps enough candidates for
    // `best_first` to pick the same winners it always did on a real home.
    let budget = options
        .max_entries
        .saturating_mul(WALK_OVERSHOOT)
        .max(options.max_entries);

    while let Some((path, depth)) = queue.pop_front() {
        if depth >= options.max_depth {
            continue;
        }
        if dirs.len().saturating_add(queue.len()) >= budget {
            break;
        }
        let Ok(entries) = fs::read_dir(&path) else {
            continue;
        };

        let mut has_marker = false;
        for entry in entries.flatten() {
            // `file_type` does not follow symlinks: a linked directory is
            // skipped instead of risking a loop.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if file_type.is_dir() {
                if is_skipped(&skip, &name, &entry.path()) {
                    continue;
                }
                queue.push_back((entry.path(), depth + 1));
            } else if MARKERS.contains(&name.as_str()) {
                has_marker = true;
            }
        }

        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let project_like = has_marker || path.join(".git").exists();
        // A directory name a terminal would interpret is never offered: the
        // walk (and therefore the cache) only carries displayable paths.
        if contains_dangerous(&name) || contains_dangerous(&path.to_string_lossy()) {
            continue;
        }
        dirs.push(Dir {
            path,
            name,
            depth,
            project_like,
        });
    }

    // Deterministic output, best candidates first when the cap truncates.
    best_first(&mut dirs);
    dirs.truncate(options.max_entries);
    dirs.sort_by(|a, b| a.path.cmp(&b.path));
    dirs
}

/// Best candidates first: project-like, then shallow, then by path. Used to
/// decide what the entry cap truncates, and by `scan <root>` previews, which
/// show this order directly.
pub fn best_first(dirs: &mut [Dir]) {
    dirs.sort_by(|a, b| {
        b.project_like
            .cmp(&a.project_like)
            .then_with(|| a.depth.cmp(&b.depth))
            .then_with(|| a.path.cmp(&b.path))
    });
}

/// Ranks cached directories for `query`: fuzzy on the name, project-like
/// first, shallower first. Returns at most `limit` matches; nothing is
/// returned for an empty query.
pub fn rank(query: &str, dirs: &[Dir], limit: usize) -> Vec<HomeMatch> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let terms: Vec<&str> = query.split_whitespace().collect();

    let matcher = SkimMatcherV2::default().smart_case();
    let mut matches: Vec<HomeMatch> = dirs
        .iter()
        .filter_map(|dir| {
            // Every term must match (AND); the bonuses apply once.
            let mut raw = 0;
            for term in &terms {
                raw += matcher.fuzzy_match(&dir.name, term)?;
            }
            let score = raw + i64::from(dir.project_like) * 50 - i64::from(dir.depth) * 5;
            Some(HomeMatch {
                path: dir.path.clone(),
                name: dir.name.clone(),
                score,
            })
        })
        .collect();

    matches.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    matches.truncate(limit);
    matches
}

/// The cache file the picker loads and `scan --home` writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cache {
    pub version: u32,
    pub scanned_at: DateTime<Utc>,
    pub dirs: Vec<Dir>,
}

/// Current on-disk cache version.
const CACHE_VERSION: u32 = 1;

impl Cache {
    pub fn new(dirs: Vec<Dir>, now: DateTime<Utc>) -> Self {
        Self {
            version: CACHE_VERSION,
            scanned_at: now,
            dirs,
        }
    }

    /// Whether the cache is old enough to refresh in the background.
    pub fn is_stale(&self, ttl_hours: u32, now: DateTime<Utc>) -> bool {
        let age = now.signed_duration_since(self.scanned_at);
        age.num_hours() >= i64::from(ttl_hours.max(1))
    }
}

/// The cache file next to the index.
pub fn cache_path(data_dir: &Path) -> PathBuf {
    data_dir.join("home-dirs.json")
}

/// Reads the cache, or `None` when it is missing or unreadable: a cache is
/// always rebuildable, so a broken one is never an error and never quarantined.
pub fn load_cache(path: &Path) -> Option<Cache> {
    let mut cache: Cache = crate::paths::read_rebuildable_json(path, MAX_CACHE_BYTES)?;
    if cache.version != CACHE_VERSION {
        return None;
    }
    // A cache written before the control-character guard (or crafted by
    // hand) must not put escape sequences in front of the terminal.
    cache.dirs.retain(|dir| {
        !contains_dangerous(&dir.name) && !contains_dangerous(&dir.path.to_string_lossy())
    });
    Some(cache)
}

/// A cache larger than this is not a cache: it is ignored and rebuilt.
const MAX_CACHE_BYTES: u64 = 64 << 20;

/// Writes the cache atomically and privately, like the index itself.
pub fn save_cache(path: &Path, cache: &Cache) -> Result<()> {
    let mut body = serde_json::to_vec_pretty(cache)?;
    body.push(b'\n');
    crate::store::json::write_private(path, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::home_dir;
    use std::collections::BTreeSet;

    fn touch(path: &Path) {
        fs::create_dir_all(path).expect("create directory");
    }

    fn names(dirs: &[Dir]) -> BTreeSet<String> {
        dirs.iter().map(|dir| dir.name.clone()).collect()
    }

    /// The walk options production builds from `[discovery]` defaults.
    fn options(max_depth: u16) -> Options {
        Options {
            max_depth,
            max_entries: 50_000,
            skip: Vec::new(),
        }
    }

    fn fixture() -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        touch(&root.join("code/alpha"));
        touch(&root.join("code/beta/target")); // skipped
        touch(&root.join("code/gamma/.git"));
        touch(&root.join("deep/one/two/three"));
        fs::write(root.join("code/alpha/Cargo.toml"), "[package]").expect("marker");
        fs::write(root.join("code/beta/package.json"), "{}").expect("marker");
        temp
    }

    #[test]
    fn scan_is_bounded_skips_and_marks_projects() {
        let temp = fixture();
        let root = temp.path();
        let options = options(4);
        let dirs = scan(root, &options);
        let found = names(&dirs);

        assert!(found.contains("code"), "{found:?}");
        assert!(found.contains("alpha"), "{found:?}");
        assert!(!found.contains("target"), "skipped trees are not walked");
        assert!(
            !found.contains("three"),
            "max_depth stops the walk: {found:?}"
        );

        let alpha = dirs.iter().find(|dir| dir.name == "alpha").expect("alpha");
        assert!(alpha.project_like, "Cargo.toml marks a project");
        let gamma = dirs.iter().find(|dir| dir.name == "gamma").expect("gamma");
        assert!(gamma.project_like, ".git marks a project");
        let code = dirs.iter().find(|dir| dir.name == "code").expect("code");
        assert!(!code.project_like);
        assert!(code.depth < alpha.depth);
    }

    #[test]
    fn multi_component_skip_patterns_match_path_suffixes() {
        let skip = [".local/share/Trash", "cache"];
        assert!(is_skipped(
            &skip,
            "Trash",
            Path::new("/home/user/.local/share/Trash")
        ));
        assert!(
            !is_skipped(
                &skip,
                "Trash",
                Path::new("/home/user/.local/share/Trash/sub")
            ),
            "only the directory itself is skipped"
        );
        assert!(is_skipped(&skip, "cache", Path::new("/home/user/cache")));
        assert!(!is_skipped(
            &skip,
            "Trash",
            Path::new("/home/user/other/Trash")
        ));
        assert!(!is_skipped(&skip, "share", Path::new("/home/user/share")));
    }

    #[test]
    fn an_oversized_cache_is_ignored() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("home-dirs.json");
        let file = std::fs::File::create(&path).expect("create");
        file.set_len(MAX_CACHE_BYTES + 1).expect("sparse size");
        assert_eq!(load_cache(&path), None, "never read, never parsed");
    }

    #[test]
    fn scan_does_not_follow_symlinks() {
        let temp = fixture();
        let root = temp.path();
        touch(&root.join("loop"));
        #[cfg(unix)]
        std::os::unix::fs::symlink(root, root.join("loop/self")).expect("symlink");

        let dirs = scan(root, &options(6));
        assert!(
            !dirs.iter().any(|dir| dir.path.ends_with("loop/self")),
            "a symlinked directory must not be entered"
        );
    }

    #[test]
    fn scan_honours_extra_skips_and_the_entry_cap() {
        let temp = fixture();
        let root = temp.path();
        let options = Options {
            max_depth: 4,
            max_entries: 2,
            skip: vec!["code".to_owned()],
        };
        let dirs = scan(root, &options);
        assert_eq!(dirs.len(), 2, "the cap truncates: {dirs:?}");
        assert!(!names(&dirs).contains("code"), "custom skips apply");
    }

    #[test]
    fn rank_prefers_projects_and_shallow_paths() {
        let temp = fixture();
        let dirs = scan(temp.path(), &options(4));

        let matches = rank("alph", &dirs, 10);
        assert_eq!(matches[0].name, "alpha");
        // `alpha` carries a Cargo.toml, so the project-like bonus is baked
        // into its score; a demoted copy must rank lower.
        let mut demoted = dirs.clone();
        for dir in demoted.iter_mut().filter(|dir| dir.name == "alpha") {
            dir.project_like = false;
        }
        assert!(matches[0].score > rank("alph", &demoted, 10)[0].score);
        assert!(rank("", &dirs, 10).is_empty(), "no query, no results");
        assert!(rank("zzzz", &dirs, 10).is_empty());

        let capped = rank("a", &dirs, 1);
        assert_eq!(capped.len(), 1, "the limit caps the list");
    }

    #[test]
    fn cache_round_trips_and_refuses_other_versions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = cache_path(temp.path());
        assert_eq!(path.file_name().unwrap(), "home-dirs.json");

        let dirs = scan(temp.path(), &options(2));
        let cache = Cache::new(dirs.clone(), Utc::now());
        save_cache(&path, &cache).expect("save");
        let loaded = load_cache(&path).expect("load");
        assert_eq!(loaded.dirs, dirs);

        fs::write(
            &path,
            r#"{"version":99,"scanned_at":"2026-01-01T00:00:00Z","dirs":[]}"#,
        )
        .expect("write");
        assert!(load_cache(&path).is_none(), "other versions are ignored");

        fs::write(&path, "not json").expect("write");
        assert!(load_cache(&path).is_none(), "a broken cache is rebuildable");
    }

    #[test]
    fn cache_drops_control_character_dirs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = cache_path(temp.path());
        let dir = |name: &str| Dir {
            path: PathBuf::from("/home/user").join(name),
            name: name.to_owned(),
            depth: 1,
            project_like: false,
        };
        let cache = Cache::new(
            vec![
                dir("good"),
                dir("bad\u{1b}]0;title"),
                dir("bad\u{7f}"),
                dir("bad\u{9b}"),
            ],
            Utc::now(),
        );
        save_cache(&path, &cache).expect("save");
        let loaded = load_cache(&path).expect("load");
        let names: Vec<&str> = loaded.dirs.iter().map(|dir| dir.name.as_str()).collect();
        assert_eq!(names, ["good"], "old caches are filtered on load");
    }

    #[cfg(unix)]
    #[test]
    fn scan_skips_control_character_names() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        fs::create_dir(root.join("ok")).expect("create ok");
        fs::create_dir(root.join("bad\u{1b}name")).expect("create bad");
        let dirs = scan(root, &options(8));
        let names: Vec<&str> = dirs.iter().map(|dir| dir.name.as_str()).collect();
        assert!(names.contains(&"ok"), "{names:?}");
        assert!(
            !names.iter().any(|name| name.contains('\u{1b}')),
            "escape names are never offered: {names:?}"
        );
    }

    #[test]
    fn home_rank_requires_every_term() {
        let dir = |name: &str| Dir {
            path: PathBuf::from("/root").join(name),
            name: name.to_owned(),
            depth: 1,
            project_like: false,
        };
        let dirs = vec![dir("alpha-beta"), dir("alpha"), dir("beta")];
        let ranked = rank("alpha beta", &dirs, 10);
        let names: Vec<&str> = ranked.iter().map(|found| found.name.as_str()).collect();
        assert_eq!(names, ["alpha-beta"], "single-term dirs are excluded");
    }

    #[test]
    fn best_first_prefers_projects_and_shallow_paths() {
        let dir = |name: &str, depth: u16, project_like: bool| Dir {
            path: PathBuf::from("/root").join(name),
            name: name.to_owned(),
            depth,
            project_like,
        };
        let mut dirs = vec![
            dir("deep-plain", 3, false),
            dir("deep-project", 2, true),
            dir("shallow-plain", 1, false),
            dir("shallow-project", 1, true),
        ];
        best_first(&mut dirs);
        let names: Vec<&str> = dirs.iter().map(|dir| dir.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "shallow-project",
                "deep-project",
                "shallow-plain",
                "deep-plain"
            ]
        );
    }

    #[test]
    fn stale_after_the_ttl() {
        let now = Utc::now();
        let mut cache = Cache::new(Vec::new(), now);
        assert!(!cache.is_stale(24, now));
        cache.scanned_at = now - chrono::Duration::hours(25);
        assert!(cache.is_stale(24, now));
        assert!(
            Cache::new(Vec::new(), now - chrono::Duration::minutes(90)).is_stale(0, now),
            "ttl 0 still means at least an hour"
        );
    }

    #[test]
    #[cfg(unix)]
    fn the_cache_file_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let path = cache_path(temp.path());
        save_cache(&path, &Cache::new(Vec::new(), Utc::now())).expect("save");
        let mode = fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the directory list is private");
    }

    #[test]
    fn scanning_a_missing_root_is_empty() {
        let dirs = scan(Path::new("/definitely/not/there"), &options(8));
        assert!(dirs.is_empty());
    }

    /// Manual probe: `cargo test --release -- --ignored discovery_scan --nocapture`
    #[test]
    #[ignore = "perf probe; run manually with --release"]
    fn discovery_scan_of_the_real_home() {
        let Some(home) = home_dir() else {
            return;
        };
        let options = options(8);
        let start = std::time::Instant::now();
        let dirs = scan(&home, &options);
        let elapsed = start.elapsed();
        let projects = dirs.iter().filter(|dir| dir.project_like).count();
        println!(
            "{}: {} directories ({projects} project-like) in {elapsed:?}",
            home.display(),
            dirs.len()
        );
    }
}
