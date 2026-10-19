use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Caps for the `.git` files a repository brings with it. Real values are
/// tiny (a ref line, a `gitdir:` indirection) or a few KB (a config); the
/// caps only stop a hostile repository from feeding an unbounded read into
/// `add`, `import`, `info`, or `doctor --refresh`.
const MAX_HEAD_BYTES: u64 = 64 * 1024;
const MAX_INDIRECTION_BYTES: u64 = 64 * 1024;
const MAX_CONFIG_BYTES: u64 = 16 * 1024 * 1024;

/// The most config files one read follows through `include.path` (the root
/// counts), and the most merged bytes the whole tree may total.
const MAX_INCLUDE_FILES: usize = 8;
const MAX_CONFIG_TOTAL: u64 = 16 * 1024 * 1024;

/// A ridiculous `gitdir:` pattern is not worth matching.
const MAX_PATTERN_CHARS: usize = 1024;

/// The total wildmatch work one config read may spend. A config may legally
/// carry thousands of `includeIf` lines with long `**` patterns inside the
/// 16 MiB cap; without a shared budget that is unbounded CPU even though the
/// bytes are bounded.
const MAX_WILDMATCH_STEPS: usize = 2_000_000;

/// The most sibling worktrees one read reports; a planted `worktrees/` tree
/// cannot grow `info` without bound.
const MAX_WORKTREES: usize = 64;

/// Reads a small text file, refusing anything that is not a regular file or
/// that exceeds `cap`. `None` means "treat as absent", which is how every
/// caller already handles a missing or unreadable `.git` file.
fn read_small(path: &Path, cap: u64) -> Option<String> {
    // Metadata first: opening a FIFO for reading blocks until a writer
    // appears, and a cloned repository can plant one (`.git/HEAD`, an
    // `include.path` target).
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > cap {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
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

/// A sibling worktree: the checkout path and the branch its own `HEAD` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<Branch>,
}

/// The sibling worktrees registered under a repository's common directory,
/// read from `.git/worktrees/*/gitdir` (still no subprocess and no `git2`).
///
/// Missing, unreadable, or malformed entries are skipped: a worktree that no
/// longer exists is not an entry, and a hostile admin directory cannot make
/// this read unbounded. The main checkout and the calling worktree are not
/// listed; `info` already shows the one being inspected.
pub fn worktrees_in(admin: &Path) -> Vec<Worktree> {
    let common = common_dir(admin);
    let Ok(entries) = std::fs::read_dir(common.join("worktrees")) else {
        return Vec::new();
    };
    let own = admin.canonicalize().unwrap_or_else(|_| admin.to_path_buf());

    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .take(MAX_WORKTREES)
        .collect();
    dirs.sort();

    let mut worktrees = Vec::new();
    for dir in dirs {
        if dir.canonicalize().unwrap_or_else(|_| dir.clone()) == own {
            continue;
        }
        let Some(gitdir) = read_small(&dir.join("gitdir"), MAX_INDIRECTION_BYTES) else {
            continue;
        };
        // Only the first line is the path, exactly as `worktrees/<id>/gitdir`
        // stores it.
        let target = gitdir.lines().next().unwrap_or("").trim();
        if target.is_empty() {
            continue;
        }
        let target = Path::new(target);
        let target = if target.is_absolute() {
            target.to_path_buf()
        } else {
            dir.join(target)
        };
        // The file points at `<checkout>/.git`; anything else is not a
        // checkout this parser understands.
        if target.file_name() != Some(std::ffi::OsStr::new(".git")) {
            continue;
        }
        let Some(path) = target.parent() else {
            continue;
        };
        if !path.is_dir() {
            continue;
        }
        // The admin `gitdir` only *claims* where the checkout is. Require the
        // checkout's own `.git` to point back at this admin directory, or a
        // planted file could turn any existing directory into a jump target.
        let recorded = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        let back_link = git_dir(path).map(|back| back.canonicalize().unwrap_or(back));
        if back_link.as_deref() != Some(recorded.as_path()) {
            continue;
        }
        worktrees.push(Worktree {
            path: path.to_path_buf(),
            branch: current_branch_in(&dir),
        });
    }
    worktrees
}

/// Resolves the directory holding `HEAD` and friends, or `None` when `path`
/// is not a Git working tree or bare repository.
///
/// Handles `.git` being a directory (normal checkout) or a file containing a
/// `gitdir:` line (worktrees, submodules), including relative targets, and a
/// bare repository, whose git directory *is* the path itself.
pub fn git_dir(path: &Path) -> Option<PathBuf> {
    let dot_git = path.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }

    if let Some(contents) = read_small(&dot_git, MAX_INDIRECTION_BYTES) {
        // Only the first line is the indirection, as git reads it.
        let target = contents.lines().next()?.strip_prefix("gitdir:")?.trim();
        if target.is_empty() {
            return None;
        }

        let target = Path::new(target);
        return Some(if target.is_absolute() {
            target.to_path_buf()
        } else {
            path.join(target)
        });
    }

    // A bare repository *is* its own git directory.
    if is_bare_git_dir(path) {
        return Some(path.to_path_buf());
    }
    None
}

/// A bare git directory: the structural layout (`HEAD` + `objects/` +
/// `refs/`), or git's own switch (`core.bare = true`).
fn is_bare_git_dir(path: &Path) -> bool {
    if path.join("HEAD").is_file() && path.join("objects").is_dir() && path.join("refs").is_dir() {
        return true;
    }
    read_small(&path.join("config"), MAX_CONFIG_BYTES).is_some_and(|config| core_bare(&config))
}

/// `[core] bare = true` in a config.
fn core_bare(config: &str) -> bool {
    let mut in_core = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            in_core = parse_section_header(line)
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case("core"));
            continue;
        }
        if !in_core {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("bare")
            && value.trim().eq_ignore_ascii_case("true")
        {
            return true;
        }
    }
    false
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

/// [`read_remote`] for a caller that already resolved the git directory with
/// [`git_dir`], so one query does not read `.git` twice.
pub fn read_remote_in(git_dir: &Path) -> Option<Remote> {
    read_remote_dir_with(git_dir, crate::project::home_dir().as_deref())
}

fn read_remote_dir_with(git_dir: &Path, home: Option<&Path>) -> Option<Remote> {
    let config_path = common_dir(git_dir).join("config");
    let mut includes = Includes {
        home,
        files_left: MAX_INCLUDE_FILES,
        bytes_left: MAX_CONFIG_TOTAL,
        steps_left: MAX_WILDMATCH_STEPS,
        seen: Vec::new(),
    };
    let contents = read_config(&config_path, git_dir, &mut includes)?;
    let parsed = parse_config(&contents);
    let mut remote = parsed.remote?;
    remote.url = rewrite_instead_of(&remote.url, &parsed.instead_of);
    // The URL is attacker-controlled even after a rewrite; the result must
    // still survive the terminal guard.
    if crate::sanitize::contains_dangerous(&remote.url) {
        return None;
    }
    Some(remote)
}

/// The state that keeps a config-include walk bounded and cycle-free.
struct Includes<'a> {
    home: Option<&'a Path>,
    files_left: usize,
    bytes_left: u64,
    steps_left: usize,
    seen: Vec<PathBuf>,
}

/// Reads `path`, splicing the files that `include.path` and
/// `includeIf "gitdir:…"` sections point at in place.
///
/// The subset of git's rules this build honors: `~/` and config-relative
/// include paths; `gitdir:` and `gitdir/i:` conditions with `*`, `**`, and
/// `?` matching, `.`/`~`-relative patterns, and the implicit `**/` prefix for
/// relative ones. Character classes and other conditions (`onbranch:`)
/// never match. Every failure — missing, not a regular file, over budget, a
/// cycle — is skipped like an absent file: these reads only feed display
/// metadata.
fn read_config(path: &Path, git_dir: &Path, includes: &mut Includes<'_>) -> Option<String> {
    if includes.files_left == 0 {
        return None;
    }
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if includes.seen.contains(&key) {
        return None;
    }
    let text = read_small(path, MAX_CONFIG_BYTES)?;
    let size = text.len() as u64;
    if size > includes.bytes_left {
        return None;
    }
    includes.files_left -= 1;
    includes.bytes_left -= size;
    includes.seen.push(key);

    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let mut merged = String::with_capacity(text.len());
    let mut section: Option<IncludeSection> = None;
    for line in text.lines() {
        if line.trim_start().starts_with('[') {
            section = parse_include_section(line);
        } else if let Some(section) = &section
            && let Some(included) = include_target(line, section, git_dir, base, includes)
        {
            merged.push_str(&included);
            if !included.ends_with('\n') {
                merged.push('\n');
            }
        }
        merged.push_str(line);
        merged.push('\n');
    }
    Some(merged)
}

/// The include section a config line sits in.
enum IncludeSection {
    Plain,
    Conditional(String),
}

/// Recognizes `[include]` and `[includeIf "condition"]` section headers.
fn parse_include_section(line: &str) -> Option<IncludeSection> {
    let (name, subsection) = parse_section_header(line.trim())?;
    if name.eq_ignore_ascii_case("include") {
        return Some(IncludeSection::Plain);
    }
    if name.eq_ignore_ascii_case("includeif") {
        return Some(IncludeSection::Conditional(subsection?));
    }
    None
}

/// The `path = …` line inside an include section, when it fires.
fn include_target(
    line: &str,
    section: &IncludeSection,
    git_dir: &Path,
    base: &Path,
    includes: &mut Includes<'_>,
) -> Option<String> {
    let (key, value) = line.split_once('=')?;
    if !key.trim().eq_ignore_ascii_case("path") {
        return None;
    }
    if let IncludeSection::Conditional(condition) = section
        && !condition_matches(
            condition,
            git_dir,
            base,
            includes.home,
            &mut includes.steps_left,
        )
    {
        return None;
    }
    let target = include_path(value.trim(), base, includes.home)?;
    read_config(&target, git_dir, includes)
}

/// Resolves one include target: `~/`, `~`, absolute, or relative to the
/// directory of the config file that named it.
fn include_path(value: &str, base: &Path, home: Option<&Path>) -> Option<PathBuf> {
    if value.is_empty() {
        return None;
    }
    if value == "~" {
        return home.map(Path::to_path_buf);
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return Some(home?.join(rest));
    }
    let path = Path::new(value);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    })
}

/// git's include conditions, within this build's subset: `gitdir:` and
/// `gitdir/i:`; anything else (`onbranch:`) never matches.
fn condition_matches(
    condition: &str,
    git_dir: &Path,
    base: &Path,
    home: Option<&Path>,
    steps: &mut usize,
) -> bool {
    if let Some(pattern) = strip_prefix_ci(condition, "gitdir/i:") {
        return gitdir_matches(pattern, git_dir, base, home, true, steps);
    }
    if let Some(pattern) = strip_prefix_ci(condition, "gitdir:") {
        return gitdir_matches(pattern, git_dir, base, home, false, steps);
    }
    false
}

fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then_some(&text[prefix.len()..])
}

/// Does the `gitdir:` pattern match the directory that holds `HEAD`?
///
/// git matches the resolved path first, then the literal one (a `~` pattern
/// rarely survives a symlinked home). `.`-relative patterns resolve against
/// the config file, `~/` against the home directory, and a relative pattern
/// gets git's implicit `**/` prefix.
fn gitdir_matches(
    pattern: &str,
    git_dir: &Path,
    base: &Path,
    home: Option<&Path>,
    icase: bool,
    steps: &mut usize,
) -> bool {
    let Some(pattern) = prepare_pattern(pattern, base, home) else {
        return false;
    };
    let resolved = git_dir
        .canonicalize()
        .unwrap_or_else(|_| git_dir.to_path_buf());
    wildmatch(&pattern, &resolved.to_string_lossy(), icase, steps)
        || wildmatch(&pattern, &git_dir.to_string_lossy(), icase, steps)
}

fn prepare_pattern(pattern: &str, base: &Path, home: Option<&Path>) -> Option<String> {
    let mut pattern = pattern.to_owned();
    if let Some(rest) = pattern.strip_prefix("~/") {
        pattern = home?.join(rest).to_string_lossy().into_owned();
    } else if let Some(rest) = pattern.strip_prefix("./") {
        pattern = base.join(rest).to_string_lossy().into_owned();
    } else if !pattern.starts_with('/') {
        pattern = format!("**/{pattern}");
    }
    // git: a trailing slash means "and everything below", base included.
    if pattern.ends_with('/') {
        pattern.push_str("**");
    }
    Some(pattern)
}

/// A subset of git's pathname wildmatch: `?` and `*` stop at `/`, `**`
/// crosses it, and the separators around `**` may match nothing (`**/foo`
/// matches `foo`, `foo/**` matches `foo`). Character classes (`[a-z]`) are
/// not supported. Failed states are memoized, so a hostile pattern cannot
/// cause exponential backtracking.
fn wildmatch(pattern: &str, text: &str, icase: bool, steps: &mut usize) -> bool {
    fn inner(
        pat: &[char],
        text: &[char],
        pi: usize,
        ti: usize,
        icase: bool,
        failed: &mut HashSet<(usize, usize)>,
        steps: &mut usize,
    ) -> bool {
        if *steps == 0 {
            return false;
        }
        *steps -= 1;
        if failed.contains(&(pi, ti)) {
            return false;
        }
        let matched = (|| {
            if pi == pat.len() {
                return ti == text.len();
            }
            // `**/` may match no components at all.
            if pat[pi..].starts_with(&['*', '*', '/'])
                && inner(pat, text, pi + 3, ti, icase, failed, steps)
            {
                return true;
            }
            // `/**` may match only the base directory itself.
            if pat[pi..] == ['/', '*', '*'] && ti == text.len() {
                return true;
            }
            match pat[pi] {
                '?' => {
                    ti < text.len()
                        && text[ti] != '/'
                        && inner(pat, text, pi + 1, ti + 1, icase, failed, steps)
                }
                '*' if pat.get(pi + 1) == Some(&'*') => (ti..=text.len())
                    .any(|split| inner(pat, text, pi + 2, split, icase, failed, steps)),
                '*' => {
                    let mut split = ti;
                    loop {
                        if inner(pat, text, pi + 1, split, icase, failed, steps) {
                            return true;
                        }
                        if split == text.len() || text[split] == '/' {
                            return false;
                        }
                        split += 1;
                    }
                }
                literal => {
                    if ti == text.len() {
                        return false;
                    }
                    let hit = if icase {
                        text[ti].eq_ignore_ascii_case(&literal)
                    } else {
                        text[ti] == literal
                    };
                    hit && inner(pat, text, pi + 1, ti + 1, icase, failed, steps)
                }
            }
        })();
        if !matched {
            failed.insert((pi, ti));
        }
        matched
    }

    if pattern.chars().count() > MAX_PATTERN_CHARS {
        return false;
    }
    let pat: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    inner(&pat, &text, 0, 0, icase, &mut HashSet::new(), steps)
}

/// The current branch, or a short hash when detached.
pub fn current_branch(path: &Path) -> Option<Branch> {
    current_branch_in(&git_dir(path)?)
}

/// [`current_branch`] for a caller that already resolved the git directory
/// with [`git_dir`].
pub fn current_branch_in(git_dir: &Path) -> Option<Branch> {
    let head = read_small(&git_dir.join("HEAD"), MAX_HEAD_BYTES)?;
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

/// What `read_remote` needs out of a merged config.
struct ParsedConfig {
    remote: Option<Remote>,
    /// `(prefix, base)` pairs from `url.<base>.insteadOf`.
    instead_of: Vec<(String, String)>,
}

/// INI-lite parser: the preferred remote (`origin`, else the first) and the
/// `insteadOf` rewrites.
///
/// Line continuations and inline comments are not part of the subset this
/// build reads; a value is the text after `=`, trimmed.
fn parse_config(config: &str) -> ParsedConfig {
    let mut section: Option<(String, Option<String>)> = None;
    let mut origin: Option<Remote> = None;
    let mut first: Option<Remote> = None;
    let mut instead_of = Vec::new();

    for line in config.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if line.starts_with('[') {
            section = parse_section_header(line);
            continue;
        }

        let Some((name, subsection)) = &section else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();

        if name.eq_ignore_ascii_case("remote") && key.eq_ignore_ascii_case("url") {
            let Some(remote_name) = subsection else {
                continue;
            };
            let remote = Remote {
                name: remote_name.clone(),
                url: value.to_owned(),
            };
            // `.git/config` comes with the repository and is attacker-controlled:
            // a remote whose name or URL carries terminal escapes or bidi
            // controls is treated as no remote at all.
            if crate::sanitize::contains_dangerous(&remote.name)
                || crate::sanitize::contains_dangerous(&remote.url)
            {
                continue;
            }
            if remote_name == "origin" {
                origin = Some(remote.clone());
            }
            if first.is_none() {
                first = Some(remote);
            }
        } else if name.eq_ignore_ascii_case("url") && key.eq_ignore_ascii_case("insteadof") {
            let Some(base) = subsection else {
                continue;
            };
            if value.is_empty()
                || crate::sanitize::contains_dangerous(base)
                || crate::sanitize::contains_dangerous(value)
            {
                continue;
            }
            instead_of.push((value.to_owned(), base.clone()));
        }
    }

    ParsedConfig {
        remote: origin.or(first),
        instead_of,
    }
}

/// The preferred remote alone, for tests and callers without rewrites.
#[cfg(test)]
fn parse_remote(config: &str) -> Option<Remote> {
    parse_config(config).remote
}

/// Rewrites `url` by the longest matching `insteadOf` prefix — git's rule for
/// `url.<base>.insteadOf`. A tie keeps the first rule seen.
fn rewrite_instead_of(url: &str, rules: &[(String, String)]) -> String {
    let mut best: Option<&(String, String)> = None;
    for rule in rules {
        if url.starts_with(&rule.0) && best.is_none_or(|current| rule.0.len() > current.0.len()) {
            best = Some(rule);
        }
    }
    match best {
        Some((prefix, base)) => format!("{base}{}", &url[prefix.len()..]),
        None => url.to_owned(),
    }
}

/// `[section "subsection"]` → `(name, Some(subsection))`; `[section]` →
/// `(name, None)`; a malformed header → `None`.
fn parse_section_header(line: &str) -> Option<(String, Option<String>)> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?.trim();
    let (name, rest) = match inner.split_once(char::is_whitespace) {
        Some((name, rest)) => (name, Some(rest.trim())),
        None => (inner, None),
    };
    if name.is_empty() {
        return None;
    }
    let subsection = match rest {
        Some(rest) => Some(rest.strip_prefix('"')?.strip_suffix('"')?.to_owned()),
        None => None,
    };
    if subsection.as_deref() == Some("") {
        return None;
    }
    Some((name.to_owned(), subsection))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Path-based spellings of the read paths, so the tests keep exercising
    /// the `.git` resolution too.
    fn read_remote(path: &Path) -> Option<Remote> {
        read_remote_in(&git_dir(path)?)
    }

    fn read_remote_with(path: &Path, home: Option<&Path>) -> Option<Remote> {
        read_remote_dir_with(&git_dir(path)?, home)
    }

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
        assert!(git_dir(dir.path()).is_none());

        std::fs::create_dir(dir.path().join(".git")).expect("create .git");
        assert!(git_dir(dir.path()).is_some());
    }

    #[test]
    fn reads_sibling_worktrees_and_skips_vanished_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let main = checkout(dir.path(), "ref: refs/heads/main\n", "");
        let admin = main.join(".git/worktrees/wt-a");
        let sibling = dir.path().join("sibling");
        write(
            &admin.join("gitdir"),
            &format!("{}\n", sibling.join(".git").display()),
        );
        write(&admin.join("HEAD"), "ref: refs/heads/feat\n");
        write(
            &sibling.join(".git"),
            &format!("gitdir: {}\n", admin.display()),
        );

        // A registration whose checkout no longer exists is not an entry.
        let gone = main.join(".git/worktrees/gone");
        write(
            &gone.join("gitdir"),
            &format!("{}\n", dir.path().join("gone/.git").display()),
        );
        write(&gone.join("HEAD"), "ref: refs/heads/gone\n");

        assert_eq!(
            worktrees_in(&main.join(".git")),
            vec![Worktree {
                path: sibling,
                branch: Some(Branch::Named("feat".to_owned())),
            }]
        );
    }

    #[test]
    fn worktrees_are_read_from_a_linked_checkouts_admin_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let main = checkout(dir.path(), "ref: refs/heads/main\n", "");
        let admin = main.join(".git/worktrees/wt-a");
        let sibling_a = dir.path().join("sibling-a");
        let sibling_b = dir.path().join("sibling-b");
        write(&admin.join("commondir"), "../..\n");
        write(
            &admin.join("gitdir"),
            &format!("{}\n", sibling_a.join(".git").display()),
        );
        write(&admin.join("HEAD"), "ref: refs/heads/feat-a\n");
        write(
            &sibling_a.join(".git"),
            &format!("gitdir: {}\n", admin.display()),
        );
        let admin_b = main.join(".git/worktrees/wt-b");
        write(
            &admin_b.join("gitdir"),
            &format!("{}\n", sibling_b.join(".git").display()),
        );
        write(&admin_b.join("HEAD"), "ref: refs/heads/feat-b\n");
        write(
            &sibling_b.join(".git"),
            &format!("gitdir: {}\n", admin_b.display()),
        );

        // Reading through a linked checkout's own admin dir finds its
        // siblings via `commondir`, and does not list itself.
        assert_eq!(
            worktrees_in(&admin),
            vec![Worktree {
                path: sibling_b,
                branch: Some(Branch::Named("feat-b".to_owned())),
            }]
        );
    }

    #[test]
    fn worktrees_must_point_back_at_their_admin_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let main = checkout(dir.path(), "ref: refs/heads/main\n", "");
        let admin = main.join(".git/worktrees/wt-a");
        let planted = dir.path().join("planted");
        std::fs::create_dir_all(&planted).expect("create planted dir");
        write(
            &admin.join("gitdir"),
            &format!("{}\n", planted.join(".git").display()),
        );
        write(&admin.join("HEAD"), "ref: refs/heads/evil\n");

        // The directory exists and was named by the admin file, but it is not
        // a checkout of this repository.
        assert!(worktrees_in(&main.join(".git")).is_empty());
    }

    #[test]
    fn wildmatch_spends_a_bounded_budget() {
        let mut steps = 100;
        let pattern = format!("/**{}x", "/**".repeat(300));
        assert!(!wildmatch(
            &pattern,
            "/tmp/a/b/c/d/e/f/g",
            false,
            &mut steps
        ));
        assert!(steps < 100, "the budget must be spent, not bypassed");

        let mut steps = MAX_WILDMATCH_STEPS;
        assert!(wildmatch(
            "**/Trash",
            "/home/x/.local/share/Trash",
            false,
            &mut steps
        ));
    }

    #[test]
    fn malformed_dot_git_file_is_not_a_repo() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".git"), "not a gitdir line").expect("write");
        assert!(git_dir(dir.path()).is_none());
    }

    #[test]
    fn reports_branch_and_remote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let checkout = checkout(
            dir.path(),
            "ref: refs/heads/main\n",
            "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = git@github.com:Dead-Abyss/overdosecd.git\n",
        );

        assert!(git_dir(&checkout).is_some());
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

        assert!(git_dir(&worktree).is_some());
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

    #[test]
    fn config_includes_are_honored() {
        let dir = tempfile::tempdir().expect("tempdir");
        let checkout = checkout(
            dir.path(),
            "ref: refs/heads/main\n",
            "[include]\n\tpath = extra.inc\n",
        );
        write(
            &checkout.join(".git/extra.inc"),
            "[remote \"origin\"]\n\turl = git@github.com:o/included.git\n",
        );
        assert_eq!(
            read_remote(&checkout).map(|remote| remote.url),
            Some("git@github.com:o/included.git".to_owned())
        );
    }

    #[test]
    fn include_paths_expand_tilde_and_missing_ones_are_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        write(
            &home.path().join("git/extra.inc"),
            "[remote \"origin\"]\n\turl = https://x.example/tilde.git\n",
        );
        let first = checkout(
            dir.path(),
            "ref: refs/heads/main\n",
            "[include]\n\tpath = ~/git/extra.inc\n",
        );
        assert_eq!(
            read_remote_with(&first, Some(home.path())).map(|remote| remote.url),
            Some("https://x.example/tilde.git".to_owned())
        );

        let tree = checkout(
            dir.path(),
            "ref: refs/heads/main\n",
            "[include]\n\tpath = missing.inc\n[remote \"origin\"]\n\turl = https://x.example/base.git\n",
        );
        assert_eq!(
            read_remote_with(&tree, None).map(|remote| remote.url),
            Some("https://x.example/base.git".to_owned()),
            "a missing include is skipped; the base config still counts"
        );
    }

    #[test]
    fn include_cycles_and_budgets_are_bounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = checkout(
            dir.path(),
            "ref: refs/heads/main\n",
            "[include]\n\tpath = a.inc\n",
        );
        write(
            &first.join(".git/a.inc"),
            "[include]\n\tpath = b.inc\n[remote \"origin\"]\n\turl = https://x.example/a.git\n",
        );
        write(&first.join(".git/b.inc"), "[include]\n\tpath = a.inc\n");
        assert_eq!(
            read_remote(&first).map(|remote| remote.url),
            Some("https://x.example/a.git".to_owned()),
            "a cycle is cut, not followed"
        );

        let dir = tempfile::tempdir().expect("tempdir");
        let tree = checkout(
            dir.path(),
            "ref: refs/heads/main\n",
            "[include]\n\tpath = link1.inc\n",
        );
        for index in 1..12 {
            let body = if index == 11 {
                "[remote \"origin\"]\n\turl = https://x.example/deep.git\n".to_owned()
            } else {
                format!("[include]\n\tpath = link{}.inc\n", index + 1)
            };
            write(&tree.join(format!(".git/link{index}.inc")), &body);
        }
        assert_eq!(
            read_remote(&tree),
            None,
            "beyond the budget nothing is read"
        );
    }

    #[test]
    fn include_if_gitdir_conditions_are_evaluated() {
        let dir = tempfile::tempdir().expect("tempdir");

        // An absolute pattern with a trailing slash matches the git dir.
        let checkout = dir.path().join("a/checkout");
        let git = checkout.join(".git");
        write(&git.join("HEAD"), "ref: refs/heads/main\n");
        write(
            &git.join("match.inc"),
            "[remote \"origin\"]\n\turl = https://x.example/match.git\n",
        );
        let pattern = git.display();
        write(
            &git.join("config"),
            &format!("[includeIf \"gitdir:{pattern}/\"]\n\tpath = match.inc\n"),
        );
        assert_eq!(
            read_remote(&checkout).map(|remote| remote.url),
            Some("https://x.example/match.git".to_owned())
        );

        // `gitdir/i:` folds case.
        let checkout = dir.path().join("b/checkout");
        let git = checkout.join(".git");
        write(&git.join("HEAD"), "ref: refs/heads/main\n");
        write(
            &git.join("upper.inc"),
            "[remote \"origin\"]\n\turl = https://x.example/upper.git\n",
        );
        let pattern = git.display().to_string().to_uppercase();
        write(
            &git.join("config"),
            &format!("[includeIf \"gitdir/i:{pattern}/\"]\n\tpath = upper.inc\n"),
        );
        assert_eq!(
            read_remote(&checkout).map(|remote| remote.url),
            Some("https://x.example/upper.git".to_owned())
        );

        // A relative pattern gets the implicit `**/`; a non-match and a
        // condition this build does not evaluate never include.
        let checkout = dir.path().join("c/checkout");
        let git = checkout.join(".git");
        write(&git.join("HEAD"), "ref: refs/heads/main\n");
        write(
            &git.join("rel.inc"),
            "[remote \"origin\"]\n\turl = https://x.example/rel.git\n",
        );
        write(
            &git.join("config"),
            "[includeIf \"gitdir:checkout/\"]\n\tpath = rel.inc\n\
             [includeIf \"gitdir:/definitely/not/here/**\"]\n\tpath = none.inc\n\
             [includeIf \"onbranch:main\"]\n\tpath = none.inc\n",
        );
        assert_eq!(
            read_remote(&checkout).map(|remote| remote.url),
            Some("https://x.example/rel.git".to_owned())
        );
    }

    #[test]
    fn instead_of_rules_rewrite_with_the_longest_prefix() {
        let rules = vec![
            (
                "https://github.com/".to_owned(),
                "git@github.com:".to_owned(),
            ),
            (
                "https://github.com/Dead-Abyss/".to_owned(),
                "ssh://git@github.com:2222/".to_owned(),
            ),
        ];
        assert_eq!(
            rewrite_instead_of("https://github.com/Dead-Abyss/overdosecd.git", &rules),
            "ssh://git@github.com:2222/overdosecd.git"
        );
        assert_eq!(
            rewrite_instead_of("https://github.com/other/repo.git", &rules),
            "git@github.com:other/repo.git"
        );
        assert_eq!(
            rewrite_instead_of("https://elsewhere.example/x.git", &rules),
            "https://elsewhere.example/x.git"
        );
    }

    #[test]
    fn instead_of_rules_apply_to_read_remote() {
        let dir = tempfile::tempdir().expect("tempdir");
        let checkout = checkout(
            dir.path(),
            "ref: refs/heads/main\n",
            "[url \"git@github.com:\"]\n\tinsteadOf = https://github.com/\n\
             [remote \"origin\"]\n\turl = https://github.com/Dead-Abyss/overdosecd.git\n",
        );
        assert_eq!(
            read_remote(&checkout).map(|remote| remote.url),
            Some("git@github.com:Dead-Abyss/overdosecd.git".to_owned())
        );

        // A rule whose base would smuggle escapes is dropped.
        let config = "[url \"https://x.example/\u{1b}]52;c;QQ\"]\n\tinsteadOf = https://evil.example/\n\
                      [remote \"origin\"]\n\turl = https://evil.example/repo.git\n";
        let parsed = parse_config(config);
        assert!(parsed.instead_of.is_empty());
        assert_eq!(
            parsed.remote.map(|remote| remote.url),
            Some("https://evil.example/repo.git".to_owned())
        );
    }

    #[test]
    fn bare_repositories_are_detected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bare = dir.path().join("bare.git");
        write(&bare.join("HEAD"), "ref: refs/heads/trunk\n");
        write(
            &bare.join("config"),
            "[remote \"origin\"]\n\turl = git@github.com:o/bare.git\n",
        );
        std::fs::create_dir_all(bare.join("objects")).expect("create objects");
        std::fs::create_dir_all(bare.join("refs")).expect("create refs");

        assert!(git_dir(&bare).is_some());
        assert_eq!(
            current_branch(&bare),
            Some(Branch::Named("trunk".to_owned()))
        );
        assert_eq!(
            read_remote(&bare).map(|remote| remote.url),
            Some("git@github.com:o/bare.git".to_owned())
        );

        // `core.bare = true` alone is git's own signal.
        let configured = dir.path().join("configured");
        write(&configured.join("config"), "[core]\n\tbare = true\n");
        assert!(git_dir(&configured).is_some());
    }

    #[test]
    fn non_regular_include_targets_are_skipped() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let checkout = checkout(
            dir.path(),
            "ref: refs/heads/main\n",
            "[include]\n\tpath = fifo.inc\n\tpath = dir.inc\n\
             [remote \"origin\"]\n\turl = https://x.example/base.git\n",
        );
        let fifo = checkout.join(".git/fifo.inc");
        let c_path = CString::new(fifo.as_os_str().as_bytes()).expect("path");
        // SAFETY: `mkfifo` only reads the path pointer.
        let created = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(created, 0, "mkfifo");
        std::fs::create_dir_all(checkout.join(".git/dir.inc")).expect("create dir");

        assert_eq!(
            read_remote(&checkout).map(|remote| remote.url),
            Some("https://x.example/base.git".to_owned()),
            "a FIFO and a directory are both skipped, and neither blocks"
        );
    }

    #[test]
    fn wildmatch_handles_the_documented_subset() {
        for (pattern, text, expected) in [
            ("/a/b", "/a/b", true),
            ("/a/*", "/a/b", true),
            ("/a/*", "/a/b/c", false),
            ("/a/**", "/a/b/c", true),
            ("/a/**", "/a", true),
            ("**/foo", "foo", true),
            ("**/foo", "/a/foo", true),
            ("/a/?", "/a/b", true),
            ("/a/?", "/a/bc", false),
            ("/a/b**c", "/a/b/x/c", true),
            ("/a/**/b", "/a/b", true),
            ("/a/**/b", "/a/x/y/b", true),
        ] {
            assert_eq!(
                matches(pattern, text, false),
                expected,
                "{pattern} vs {text}"
            );
        }
        assert!(matches("/A/B", "/a/b", true), "gitdir/i folds case");
        assert!(!matches("/A/B", "/a/b", false));
    }

    /// [`wildmatch`] with the production budget, for pattern tests.
    fn matches(pattern: &str, text: &str, icase: bool) -> bool {
        let mut steps = MAX_WILDMATCH_STEPS;
        wildmatch(pattern, text, icase, &mut steps)
    }
}
