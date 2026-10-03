use std::io::Read;
use std::path::{Path, PathBuf};

/// Caps for the `.git` files a repository brings with it. Real values are
/// tiny (a ref line, a `gitdir:` indirection) or a few KB (a config); the
/// caps only stop a hostile repository from feeding an unbounded read into
/// `add`, `import`, `info`, or `doctor --refresh`.
const MAX_HEAD_BYTES: u64 = 64 * 1024;
const MAX_INDIRECTION_BYTES: u64 = 64 * 1024;
const MAX_CONFIG_BYTES: u64 = 16 * 1024 * 1024;

/// Reads a small text file, refusing anything that is not a regular file or
/// that exceeds `cap`. `None` means "treat as absent", which is how every
/// caller already handles a missing or unreadable `.git` file.
fn read_small(path: &Path, cap: u64) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > cap {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(cap + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > cap {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// A configured remote, as read from `.git/config`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Remote {
    pub name: String,
    pub url: String,
}

/// The current `HEAD` of a working tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Branch {
    Named(String),
    /// Short commit hash when `HEAD` is not on a branch.
    Detached(String),
}

impl Branch {
    /// How `info` and the picker's details pane render the branch.
    pub fn label(self) -> String {
        match self {
            Branch::Named(name) => name,
            Branch::Detached(sha) => format!("detached ({sha})"),
        }
    }
}

/// Returns `true` when `path` looks like a Git working tree.
pub fn is_repo(path: &Path) -> bool {
    git_dir(path).is_some()
}

/// Resolves the directory holding `HEAD` and friends.
///
/// Handles `.git` being a directory (normal checkout) or a file containing a
/// `gitdir:` line (worktrees, submodules), including relative targets.
pub fn git_dir(path: &Path) -> Option<PathBuf> {
    let dot_git = path.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }

    let contents = read_small(&dot_git, MAX_INDIRECTION_BYTES)?;
    // Only the first line is the indirection, as git reads it.
    let target = contents.lines().next()?.strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }

    let target = Path::new(target);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        path.join(target)
    })
}

/// The directory holding `config`.
///
/// Worktrees keep their own `HEAD` but share `config` with the main
/// repository, pointed at by a `commondir` file.
fn common_dir(git_dir: &Path) -> PathBuf {
    let Some(contents) = read_small(&git_dir.join("commondir"), MAX_INDIRECTION_BYTES) else {
        return git_dir.to_path_buf();
    };
    let target = contents.lines().next().unwrap_or("").trim();
    if target.is_empty() {
        return git_dir.to_path_buf();
    }

    let target = Path::new(target);
    let joined = if target.is_absolute() {
        target.to_path_buf()
    } else {
        git_dir.join(target)
    };
    joined.canonicalize().unwrap_or(joined)
}

/// Reads the preferred remote: `origin` when present, otherwise the first
/// remote in the config.
pub fn read_remote(path: &Path) -> Option<Remote> {
    let git_dir = git_dir(path)?;
    let contents = read_small(&common_dir(&git_dir).join("config"), MAX_CONFIG_BYTES)?;
    parse_remote(&contents)
}

/// The current branch, or a short hash when detached.
pub fn current_branch(path: &Path) -> Option<Branch> {
    let head = read_small(&git_dir(path)?.join("HEAD"), MAX_HEAD_BYTES)?;
    let head = head.trim();

    if let Some(reference) = head.strip_prefix("ref:") {
        let reference = reference.trim();
        let name = reference.strip_prefix("refs/heads/").unwrap_or(reference);
        if name.is_empty() {
            return None;
        }
        // A branch name is read live from a repository's own `HEAD`; refuse
        // one that a terminal would render as something other than itself.
        if crate::sanitize::contains_dangerous(name) {
            return None;
        }
        return Some(Branch::Named(name.to_owned()));
    }

    if head.len() >= 7 && head.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Some(Branch::Detached(head[..7].to_owned()));
    }

    None
}

/// Derives lowercased `(owner, repo)` from a remote URL.
///
/// Handles scp-like (`git@host:owner/repo.git`), `ssh://`, `https://`, and
/// bare `host/owner/repo` forms. The owner may be empty for local paths.
pub fn slug_from_url(url: &str) -> Option<(String, String)> {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        return None;
    }

    let path = if let Some((_, rest)) = url.split_once("://") {
        // scheme://[user@]host[:port]/owner/repo
        rest.split_once('/')?.1
    } else if let Some((_, rest)) = url.split_once('@') {
        // scp-like: user@host:owner/repo
        rest.split_once(':')?.1
    } else {
        url
    };

    let path = path.trim_start_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.rsplit('/');

    let repo = parts.next()?.trim();
    if repo.is_empty() {
        return None;
    }
    let owner = parts.next().unwrap_or("").trim();

    Some((owner.to_lowercase(), repo.to_lowercase()))
}

/// INI-lite parser: `[remote "name"]` sections and their `url = …` value.
fn parse_remote(config: &str) -> Option<Remote> {
    let mut current: Option<String> = None;
    let mut origin: Option<Remote> = None;
    let mut first: Option<Remote> = None;

    for line in config.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if line.starts_with('[') {
            current = parse_remote_section(line);
            continue;
        }

        let Some(name) = current.as_deref() else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("url") {
            continue;
        }

        let remote = Remote {
            name: name.to_owned(),
            url: value.trim().to_owned(),
        };
        // `.git/config` comes with the repository and is attacker-controlled:
        // a remote whose name or URL carries terminal escapes or bidi
        // controls is treated as no remote at all.
        if crate::sanitize::contains_dangerous(&remote.name)
            || crate::sanitize::contains_dangerous(&remote.url)
        {
            continue;
        }
        if name == "origin" {
            origin = Some(remote.clone());
        }
        if first.is_none() {
            first = Some(remote);
        }
    }

    origin.or(first)
}

fn parse_remote_section(line: &str) -> Option<String> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?.trim();
    let (section, rest) = inner.split_once(char::is_whitespace)?;
    if section != "remote" {
        return None;
    }
    let name = rest.trim().strip_prefix('"')?.strip_suffix('"')?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent");
        }
        std::fs::write(path, contents).expect("write file");
    }

    fn checkout(root: &Path, head: &str, config: &str) -> PathBuf {
        let dir = root.join("checkout");
        write(&dir.join(".git/HEAD"), head);
        write(&dir.join(".git/config"), config);
        dir
    }

    #[test]
    fn detects_git_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!is_repo(dir.path()));

        std::fs::create_dir(dir.path().join(".git")).expect("create .git");
        assert!(is_repo(dir.path()));
    }

    #[test]
    fn malformed_dot_git_file_is_not_a_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".git"), "not a gitdir line").expect("write");
        assert!(!is_repo(dir.path()));
    }

    #[test]
    fn reports_branch_and_remote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let checkout = checkout(
            dir.path(),
            "ref: refs/heads/main\n",
            "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = git@github.com:Dead-Abyss/overdosecd.git\n",
        );

        assert!(is_repo(&checkout));
        assert_eq!(
            current_branch(&checkout),
            Some(Branch::Named("main".to_owned()))
        );
        assert_eq!(
            read_remote(&checkout),
            Some(Remote {
                name: "origin".to_owned(),
                url: "git@github.com:Dead-Abyss/overdosecd.git".to_owned(),
            })
        );
    }

    #[test]
    fn prefers_origin_over_other_remotes() {
        let config = "[remote \"upstream\"]\n\turl = https://example.com/up/stream.git\n[remote \"origin\"]\n\turl = https://example.com/or/igin.git\n";
        let remote = parse_remote(config).expect("a remote");
        assert_eq!(remote.name, "origin");
        assert_eq!(remote.url, "https://example.com/or/igin.git");
    }

    #[test]
    fn refuses_hostile_git_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let checkout = checkout(
            dir.path(),
            "ref: refs/heads/\u{1b}[31mRED\n",
            "[remote \"origin\"]\n\turl = https://x.example/\u{1b}]52;c;QQ\n",
        );
        assert_eq!(current_branch(&checkout), None, "a dirty branch is refused");
        assert_eq!(read_remote(&checkout), None, "a dirty remote is refused");
    }

    #[test]
    fn refuses_a_bidi_branch_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let checkout = checkout(dir.path(), "ref: refs/heads/safe\u{202e}txt\n", "[core]\n");
        assert_eq!(current_branch(&checkout), None);
    }

    #[test]
    fn skips_a_dirty_remote_for_a_clean_one() {
        let config = "[remote \"origin\"]\n\turl = https://x.example/\u{1b}[31m\n[remote \"upstream\"]\n\turl = https://x.example/up/stream.git\n";
        let remote = parse_remote(config).expect("the clean remote");
        assert_eq!(remote.name, "upstream");
        assert_eq!(remote.url, "https://x.example/up/stream.git");
    }

    #[test]
    fn falls_back_to_the_first_remote() {
        let config = "[remote \"upstream\"]\n\turl = https://example.com/up/stream.git\n[core]\n\tbare = false\n";
        let remote = parse_remote(config).expect("a remote");
        assert_eq!(remote.name, "upstream");
    }

    #[test]
    fn no_remote_section_means_no_remote() {
        let config =
            "[core]\n\trepositoryformatversion = 0\n[branch \"main\"]\n\tremote = origin\n";
        assert_eq!(parse_remote(config), None);
    }

    #[test]
    fn detached_head_reports_a_short_hash() {
        let dir = tempfile::tempdir().expect("tempdir");
        let checkout = checkout(
            dir.path(),
            "0123456789abcdef0123456789abcdef01234567\n",
            "[core]\n",
        );
        assert_eq!(
            current_branch(&checkout),
            Some(Branch::Detached("0123456".to_owned()))
        );
    }

    #[test]
    fn worktrees_follow_gitdir_and_commondir() {
        let dir = tempfile::tempdir().expect("tempdir");

        let main = dir.path().join("main");
        write(&main.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(
            &main.join(".git/config"),
            "[remote \"origin\"]\n\turl = git@github.com:o/main-repo.git\n",
        );

        let worktree = dir.path().join("wt");
        write(
            &worktree.join(".git"),
            &format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()),
        );
        let worktree_gitdir = main.join(".git/worktrees/wt");
        write(&worktree_gitdir.join("HEAD"), "ref: refs/heads/feature\n");
        write(&worktree_gitdir.join("commondir"), "../..\n");

        assert!(is_repo(&worktree));
        assert_eq!(
            current_branch(&worktree),
            Some(Branch::Named("feature".to_owned()))
        );
        assert_eq!(
            read_remote(&worktree).map(|remote| remote.url),
            Some("git@github.com:o/main-repo.git".to_owned())
        );
    }

    #[test]
    fn oversized_git_files_are_treated_as_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let checkout = dir.path().join("checkout");
        std::fs::create_dir_all(checkout.join(".git")).expect("create .git");

        // Sparse files: large on paper, cheap on disk.
        for (name, size) in [
            ("HEAD", MAX_HEAD_BYTES + 1),
            ("config", MAX_CONFIG_BYTES + 1),
        ] {
            let file = std::fs::File::create(checkout.join(".git").join(name)).expect("create");
            file.set_len(size).expect("sparse size");
        }
        assert_eq!(current_branch(&checkout), None);
        assert_eq!(read_remote(&checkout), None);
    }

    #[test]
    fn gitdir_takes_only_the_first_line() {
        let dir = tempfile::tempdir().expect("tempdir");

        let main = dir.path().join("main");
        write(&main.join(".git/HEAD"), "ref: refs/heads/trunk\n");
        write(&main.join(".git/config"), "[core]\n");

        let worktree = dir.path().join("wt");
        write(
            &worktree.join(".git"),
            &format!(
                "gitdir: {}\ntrailing garbage that is not a path\n",
                main.join(".git").display()
            ),
        );
        assert_eq!(
            current_branch(&worktree),
            Some(Branch::Named("trunk".to_owned()))
        );
    }

    #[test]
    fn derives_slugs_from_common_url_shapes() {
        for (url, expected) in [
            (
                "git@github.com:Dead-Abyss/overdosecd.git",
                ("dead-abyss", "overdosecd"),
            ),
            (
                "https://github.com/Dead-Abyss/overdosecd.git",
                ("dead-abyss", "overdosecd"),
            ),
            (
                "https://github.com/Dead-Abyss/overdosecd",
                ("dead-abyss", "overdosecd"),
            ),
            (
                "ssh://git@github.com:22/Dead-Abyss/overdosecd.git",
                ("dead-abyss", "overdosecd"),
            ),
            ("git@github.com:o/r", ("o", "r")),
            ("https://github.com/o/r/", ("o", "r")),
        ] {
            assert_eq!(
                slug_from_url(url),
                Some((expected.0.to_owned(), expected.1.to_owned())),
                "URL: {url}"
            );
        }

        assert_eq!(slug_from_url("   "), None);
    }
}
