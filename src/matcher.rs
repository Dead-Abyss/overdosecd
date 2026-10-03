use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use crate::error::{Candidates, Error, Result};
use crate::project::Project;

// Base scores. Exact beats prefix beats tag beats fuzzy; bonuses below never
// promote a weak match above a stronger one on their own.
const EXACT_NAME: i64 = 10_000;
const EXACT_ALIAS: i64 = 5_000;
const NAME_PREFIX: i64 = 2_000;
const ALIAS_PREFIX: i64 = 1_500;
const TAG_EXACT: i64 = 1_200;
const FUZZY_NAME_BASE: i64 = 300;
const FUZZY_NAME_MAX: i64 = 400;
const FUZZY_ALIAS_BASE: i64 = 200;
const FUZZY_ALIAS_MAX: i64 = 200;
const FUZZY_PATH_BASE: i64 = 100;
const FUZZY_PATH_MAX: i64 = 100;
const EXACT_REPO: i64 = 4_000;
const REPO_PREFIX: i64 = 1_000;
const EXACT_SLUG: i64 = 3_000;
const FUZZY_REPO_BASE: i64 = 150;
const FUZZY_REPO_MAX: i64 = 150;

const GIT_BONUS: i64 = 100;
const RECENCY_WINDOW_HOURS: f64 = 720.0;
const MAX_CANDIDATES: usize = 5;

/// The knobs a config file may tune, with the values hardcoded before it
/// existed. Anything not listed here is a fixed part of the ranking contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatcherConfig {
    /// Allow fuzzy (subsequence) matching on names, aliases, paths, and repos.
    pub fuzzy: bool,

    /// Maximum recency bonus, awarded for a jump made just now.
    pub recency_max: i64,

    /// Ceiling of the log-scaled frequency bonus.
    pub frequency_max: i64,

    /// Bonus for a pinned project.
    pub pinned_bonus: i64,

    /// Score gap below which two candidates are reported as ambiguous.
    pub ambiguity_margin: i64,

    /// Penalty per path component, preferring shallower projects.
    pub depth_penalty: i64,
}

impl MatcherConfig {
    /// The built-in defaults, matching pre-config behaviour; the tests pin
    /// their expectations to it (production builds go through
    /// [`From<&crate::config::Matching>`]).
    #[cfg(test)]
    pub const DEFAULT: Self = Self {
        fuzzy: true,
        recency_max: 800,
        frequency_max: 400,
        pinned_bonus: 1_500,
        ambiguity_margin: 150,
        depth_penalty: 20,
    };

    /// Freshness bonus: full value for a jump made now, zero after 30 days.
    pub(crate) fn recency_bonus(
        &self,
        last_used_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> i64 {
        let Some(last_used_at) = last_used_at else {
            return 0;
        };
        let hours = now.signed_duration_since(last_used_at).num_minutes() as f64 / 60.0;
        let freshness = (1.0 - hours / RECENCY_WINDOW_HOURS).clamp(0.0, 1.0);
        (self.recency_max as f64 * freshness).round() as i64
    }

    /// Log-scaled frequency bonus, capped at `frequency_max`.
    pub(crate) fn frequency_bonus(&self, use_count: u64) -> i64 {
        (100.0 * (use_count as f64).ln_1p())
            .min(self.frequency_max as f64)
            .round() as i64
    }
}

impl From<&crate::config::Matching> for MatcherConfig {
    fn from(matching: &crate::config::Matching) -> Self {
        Self {
            fuzzy: matching.fuzzy,
            recency_max: matching.recency_max,
            frequency_max: matching.frequency_max,
            pinned_bonus: matching.pinned_bonus,
            ambiguity_margin: matching.ambiguity_margin,
            depth_penalty: matching.depth_penalty,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    ExactName,
    ExactAlias,
    NamePrefix,
    AliasPrefix,
    Tag,
    FuzzyName,
    FuzzyAlias,
    FuzzyPath,
    GitRepoExact,
    GitRepoPrefix,
    GitRemote,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Reason::ExactName => "exact name",
            Reason::ExactAlias => "exact alias",
            Reason::NamePrefix => "name prefix",
            Reason::AliasPrefix => "alias prefix",
            Reason::Tag => "tag",
            Reason::FuzzyName => "fuzzy name",
            Reason::FuzzyAlias => "fuzzy alias",
            Reason::FuzzyPath => "fuzzy path",
            Reason::GitRepoExact => "exact repo name",
            Reason::GitRepoPrefix => "repo prefix",
            Reason::GitRemote => "git remote",
        }
    }

    /// Whether a match in this class is strong enough to jump without asking.
    ///
    /// `goto --confident` (and the `ocd` wrapper behind it) refuses the fuzzy
    /// classes and opens the picker instead. `GitRemote` covers both the exact
    /// `owner/repo` slug and fuzzy remote matches, so it stays unconfident.
    pub(crate) fn is_confident(self) -> bool {
        match self {
            Reason::ExactName
            | Reason::ExactAlias
            | Reason::NamePrefix
            | Reason::AliasPrefix
            | Reason::Tag
            | Reason::GitRepoExact
            | Reason::GitRepoPrefix => true,
            Reason::FuzzyName | Reason::FuzzyAlias | Reason::FuzzyPath | Reason::GitRemote => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Match {
    pub index: usize,
    pub name: String,
    pub path: PathBuf,
    pub score: i64,
    pub reason: Reason,
    /// One entry per query term that matched; the whole query counts as one
    /// term when that path wins (a project name may contain spaces).
    pub terms: Vec<TermHit>,
}

/// One term's contribution to a [`Match`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TermHit {
    pub term: String,
    pub reason: Reason,
    /// The signal score before bonuses, for `--debug`.
    pub signal: i64,
}

impl Match {
    /// Whether every contributing term landed in a confident class; the
    /// `ocd` wrapper jumps on a confident match and asks otherwise.
    pub fn all_confident(&self) -> bool {
        !self.terms.is_empty() && self.terms.iter().all(|hit| hit.reason.is_confident())
    }

    /// A match for tests and fixtures.
    #[cfg(test)]
    pub fn for_test(name: &str, path: &str, score: i64, reason: Reason) -> Match {
        Match {
            index: 0,
            name: name.to_owned(),
            path: PathBuf::from(path),
            score,
            reason,
            terms: vec![TermHit {
                term: name.to_lowercase(),
                reason,
                signal: score,
            }],
        }
    }
}

/// Ranks every project that matches `query`, best first.
///
/// A single-term query takes the original path exactly. A multi-term query
/// requires every term to match (AND): the score is the sum of the per-term
/// signals with the bonuses applied once — unless the *whole* query scores
/// higher as one term, which is how project names containing spaces keep
/// matching exactly.
pub fn rank(query: &str, projects: &[Project], config: &MatcherConfig) -> Vec<Match> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let terms: Vec<&str> = query.split_whitespace().collect();

    let matcher = SkimMatcherV2::default().smart_case();
    let now = Utc::now();

    let mut matches: Vec<Match> = projects
        .iter()
        .enumerate()
        .filter_map(|(index, project)| {
            score_query(&query, &terms, project, config, &matcher, now).map(
                |(score, reason, terms)| Match {
                    index,
                    name: project.name.clone(),
                    path: project.path.clone(),
                    score,
                    reason,
                    terms,
                },
            )
        })
        .collect();

    matches.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    matches
}

/// The shell-completion candidates for `prefix`: project names, aliases, and
/// tags that start with it (case-insensitively), deduplicated and sorted
/// alphabetically. Missing directories are skipped, matching `list` default.
pub fn completion_candidates(projects: &[Project], prefix: &str) -> Vec<String> {
    let prefix = prefix.trim().to_lowercase();
    let mut candidates: Vec<String> = projects
        .iter()
        .filter(|project| project.path.exists())
        .flat_map(|project| {
            std::iter::once(&project.name)
                .chain(project.aliases.iter())
                .chain(project.tags.iter())
        })
        // Completion candidates are written straight into the shell's
        // COMPREPLY/COMPLIST on a TAB press; a value with control or
        // invisible characters must never make that trip.
        .filter(|value| !crate::sanitize::contains_dangerous(value))
        .filter(|value| value.to_lowercase().starts_with(&prefix))
        .cloned()
        .collect();

    candidates.sort_by_cached_key(|value| value.to_lowercase());
    candidates.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    candidates
}

/// The character positions in `text` that `query` matched, for highlighting
/// ranked results in the picker; every term contributes.
///
/// This is display-only: it is computed with a fresh matcher and never feeds
/// [`rank`] or [`best`], so highlighting can never change a score, an order,
/// or a tie-break (`--debug` output stays exactly as it was).
pub fn highlight_indices(query: &str, text: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() || text.is_empty() {
        return Vec::new();
    }
    let matcher = SkimMatcherV2::default().smart_case();
    let mut indices: Vec<usize> = Vec::new();
    for term in query.split_whitespace() {
        if let Some((_, mut term_indices)) = matcher.fuzzy_indices(text, term) {
            indices.append(&mut term_indices);
        }
    }
    indices.sort_unstable();
    indices.dedup();
    indices
}

/// Returns the single best match, or a user-facing error when the query is
/// missing, unknown, or too ambiguous to call.
pub fn best(query: &str, projects: &[Project], config: &MatcherConfig) -> Result<Match> {
    if projects.is_empty() {
        return Err(Error::EmptyIndex);
    }

    let mut ranked = rank(query, projects, config);
    if ranked.is_empty() {
        return Err(Error::NoMatch(query.trim().to_owned()));
    }

    let top = ranked.remove(0);
    let ambiguous = ranked
        .first()
        .is_some_and(|second| top.score - second.score <= config.ambiguity_margin);
    if ambiguous {
        ranked.insert(0, top.clone());
        return Err(Error::Ambiguous {
            query: query.trim().to_owned(),
            candidates: Candidates(ranked.into_iter().take(MAX_CANDIDATES).collect()),
        });
    }

    Ok(top)
}

fn improve(best: &mut Option<(i64, Reason)>, score: i64, reason: Reason) {
    if best.as_ref().is_none_or(|(current, _)| score > *current) {
        *best = Some((score, reason));
    }
}

/// The base score found so far, or the lowest possible value before any
/// signal has matched.
fn current(best: &Option<(i64, Reason)>) -> i64 {
    best.as_ref().map_or(i64::MIN, |(score, _)| *score)
}

/// The signals one term fires for a project, before the bonuses.
fn signal_score(
    query: &str,
    project: &Project,
    config: &MatcherConfig,
    matcher: &SkimMatcherV2,
) -> Option<(i64, Reason)> {
    let mut best: Option<(i64, Reason)> = None;

    let name = project.name.to_lowercase();
    if name == query {
        improve(&mut best, EXACT_NAME, Reason::ExactName);
    } else if name.starts_with(query) {
        improve(&mut best, NAME_PREFIX, Reason::NamePrefix);
    }
    // Each fuzzy signal is skipped once the running best exceeds its hard
    // cap: it could never improve the score, and `improve` only replaces on a
    // strict win, so the recorded reason stays identical too. This keeps the
    // per-component path scan — the expensive one — off the common exact,
    // prefix, alias, and tag paths.
    if config.fuzzy
        && current(&best) < FUZZY_NAME_BASE + FUZZY_NAME_MAX
        && let Some(raw) = matcher.fuzzy_match(&name, query)
    {
        improve(
            &mut best,
            FUZZY_NAME_BASE + raw.clamp(0, FUZZY_NAME_MAX),
            Reason::FuzzyName,
        );
    }

    for alias in &project.aliases {
        let alias = alias.to_lowercase();
        if alias == query {
            improve(&mut best, EXACT_ALIAS, Reason::ExactAlias);
        } else if alias.starts_with(query) {
            improve(&mut best, ALIAS_PREFIX, Reason::AliasPrefix);
        }
        if config.fuzzy
            && current(&best) < FUZZY_ALIAS_BASE + FUZZY_ALIAS_MAX
            && let Some(raw) = matcher.fuzzy_match(&alias, query)
        {
            improve(
                &mut best,
                FUZZY_ALIAS_BASE + raw.clamp(0, FUZZY_ALIAS_MAX),
                Reason::FuzzyAlias,
            );
        }
    }

    if project.tags.iter().any(|tag| tag.to_lowercase() == query) {
        improve(&mut best, TAG_EXACT, Reason::Tag);
    }

    if config.fuzzy
        && current(&best) < FUZZY_PATH_BASE + FUZZY_PATH_MAX
        && let Some(raw) = best_path_match(matcher, &project.path, query)
    {
        improve(
            &mut best,
            FUZZY_PATH_BASE + raw.clamp(0, FUZZY_PATH_MAX),
            Reason::FuzzyPath,
        );
    }

    if let Some(git) = &project.git
        && let Some(url) = git.remote_url.as_deref()
        && let Some((owner, repo)) = crate::git::slug_from_url(url)
    {
        if repo == query {
            improve(&mut best, EXACT_REPO, Reason::GitRepoExact);
        } else if repo.starts_with(query) {
            improve(&mut best, REPO_PREFIX, Reason::GitRepoPrefix);
        } else if config.fuzzy
            && current(&best) < FUZZY_REPO_BASE + FUZZY_REPO_MAX
            && let Some(raw) = matcher.fuzzy_match(&repo, query)
        {
            improve(
                &mut best,
                FUZZY_REPO_BASE + raw.clamp(0, FUZZY_REPO_MAX),
                Reason::GitRemote,
            );
        }

        // The owner only counts as part of the full `owner/repo` slug,
        // so an org name cannot match every repository on its own.
        if !owner.is_empty() && format!("{owner}/{repo}") == query {
            improve(&mut best, EXACT_SLUG, Reason::GitRemote);
        }
    }

    best
}

/// The bonuses a project earns regardless of which term matched; they are
/// applied once to the combined signal score.
fn apply_bonuses(base: i64, project: &Project, config: &MatcherConfig, now: DateTime<Utc>) -> i64 {
    let mut total = base;
    if project.pinned {
        total += config.pinned_bonus;
    }
    if project.git.is_some() {
        total += GIT_BONUS;
    }
    total -= config.depth_penalty * project.path.components().count() as i64;
    total += config.frequency_bonus(project.use_count);
    total += config.recency_bonus(project.last_used_at, now);
    total
}

/// Scores one project against the split query. A single term takes the
/// original path exactly; several terms must all match, and the whole query
/// still gets a chance as one term, so names containing spaces match exactly.
fn score_query(
    query: &str,
    terms: &[&str],
    project: &Project,
    config: &MatcherConfig,
    matcher: &SkimMatcherV2,
    now: DateTime<Utc>,
) -> Option<(i64, Reason, Vec<TermHit>)> {
    if let [term] = terms {
        let (signal, reason) = signal_score(term, project, config, matcher)?;
        return Some((
            apply_bonuses(signal, project, config, now),
            reason,
            vec![TermHit {
                term: (*term).to_owned(),
                reason,
                signal,
            }],
        ));
    }

    let whole = signal_score(query, project, config, matcher).map(|(signal, reason)| TermHit {
        term: query.to_owned(),
        reason,
        signal,
    });
    let and = and_terms(terms, project, config, matcher);

    let hits = match (whole, and) {
        (Some(whole), Some(and)) => {
            let whole_total = apply_bonuses(whole.signal, project, config, now);
            let and_total =
                apply_bonuses(and.iter().map(|hit| hit.signal).sum(), project, config, now);
            // A tie goes to the whole query: names with spaces win.
            if whole_total >= and_total {
                vec![whole]
            } else {
                and
            }
        }
        (Some(whole), None) => vec![whole],
        (None, Some(and)) => and,
        (None, None) => return None,
    };

    let base: i64 = hits.iter().map(|hit| hit.signal).sum();
    let reason = hits
        .iter()
        .max_by_key(|hit| hit.signal)
        .map(|hit| hit.reason)
        .expect("at least one hit");
    Some((apply_bonuses(base, project, config, now), reason, hits))
}

/// Every term must fire at least one signal (AND).
fn and_terms(
    terms: &[&str],
    project: &Project,
    config: &MatcherConfig,
    matcher: &SkimMatcherV2,
) -> Option<Vec<TermHit>> {
    let mut hits = Vec::with_capacity(terms.len());
    for term in terms {
        let (signal, reason) = signal_score(term, project, config, matcher)?;
        hits.push(TermHit {
            term: (*term).to_owned(),
            reason,
            signal,
        });
    }
    Some(hits)
}

fn best_path_match(matcher: &SkimMatcherV2, path: &Path, query: &str) -> Option<i64> {
    path.components()
        .filter_map(|component| {
            let text = component.as_os_str().to_string_lossy().to_lowercase();
            if text.is_empty() {
                return None;
            }
            matcher.fuzzy_match(&text, query)
        })
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    /// Most tests exercise behaviour independent of config, so they run against
    /// the built-in defaults; the config-specific tests call `super::rank` /
    /// `super::best` with a tweaked [`MatcherConfig`].
    fn best(query: &str, projects: &[Project]) -> Result<Match> {
        super::best(query, projects, &MatcherConfig::DEFAULT)
    }

    fn rank(query: &str, projects: &[Project]) -> Vec<Match> {
        super::rank(query, projects, &MatcherConfig::DEFAULT)
    }

    fn project(name: &str, path: &str) -> Project {
        Project::for_test(name, path)
    }

    fn repo_project(name: &str, path: &str, url: &str) -> Project {
        let mut project = Project::for_test(name, path);
        project.git = Some(crate::project::GitInfo {
            remote_name: Some("origin".to_owned()),
            remote_url: Some(url.to_owned()),
        });
        project
    }

    #[test]
    fn repo_name_matches_even_when_the_project_name_differs() {
        let projects = vec![repo_project(
            "notes",
            "/x/notes",
            "git@github.com:acme/widgets.git",
        )];
        let found = best("widgets", &projects).unwrap();
        assert_eq!(found.reason, Reason::GitRepoExact);
    }

    #[test]
    fn repo_prefix_and_fuzzy_matches_work() {
        let projects = vec![repo_project(
            "notes",
            "/x/notes",
            "git@github.com:acme/widgets.git",
        )];
        assert_eq!(
            best("wid", &projects).unwrap().reason,
            Reason::GitRepoPrefix
        );
        assert_eq!(best("wgts", &projects).unwrap().reason, Reason::GitRemote);
    }

    #[test]
    fn exact_slug_matches_but_owner_alone_does_not() {
        let projects = vec![repo_project(
            "notes",
            "/x/notes",
            "git@github.com:acme/widgets.git",
        )];
        assert_eq!(
            best("acme/widgets", &projects).unwrap().reason,
            Reason::GitRemote
        );
        assert!(matches!(best("acme", &projects), Err(Error::NoMatch(_))));
    }

    #[test]
    fn git_bonus_comes_from_stored_git_info() {
        let plain = project("alpha", "/a/alpha");
        let repo = repo_project("alpha", "/b/alpha", "git@github.com:o/alpha.git");
        let ranked = rank("alpha", &[plain, repo]);

        assert_eq!(ranked[0].path, std::path::PathBuf::from("/b/alpha"));
        assert_eq!(ranked[0].score - ranked[1].score, GIT_BONUS);
    }

    #[test]
    fn exact_name_beats_prefix() {
        let projects = vec![
            project("over", "/a/over"),
            project("overdosecd", "/a/overdosecd"),
        ];
        let found = best("over", &projects).unwrap();
        assert_eq!(found.name, "over");
        assert_eq!(found.reason, Reason::ExactName);
    }

    #[test]
    fn alias_beats_name_prefix() {
        let mut aliased = project("overdosecd", "/a/overdosecd");
        aliased.aliases.push("over".into());
        let projects = vec![project("over-tool", "/a/over-tool"), aliased];
        let found = best("over", &projects).unwrap();
        assert_eq!(found.name, "overdosecd");
        assert_eq!(found.reason, Reason::ExactAlias);
    }

    #[test]
    fn tags_match_when_names_do_not() {
        let mut tagged = project("alpha", "/a/alpha");
        tagged.tags.push("rust".into());
        let found = best("rust", &[tagged]).unwrap();
        assert_eq!(found.reason, Reason::Tag);
    }

    #[test]
    fn exact_name_wins_even_when_the_path_also_matches() {
        let projects = vec![project("alpha", "/x/alpha")];
        let found = best("alpha", &projects).unwrap();
        assert_eq!(found.reason, Reason::ExactName);
        assert_eq!(
            found.score,
            EXACT_NAME - MatcherConfig::DEFAULT.depth_penalty * 3,
        );
    }

    #[test]
    fn a_strong_signal_skips_the_path_scan() {
        // "al" matches both the name prefix (2,000) and a path component
        // (≤200); the reason and score must stay the strong signal's.
        let projects = vec![project("alpha", "/x/al")];
        let found = best("al", &projects).unwrap();
        assert_eq!(found.reason, Reason::NamePrefix);
        assert_eq!(
            found.score,
            NAME_PREFIX - MatcherConfig::DEFAULT.depth_penalty * 3,
        );
    }

    #[test]
    fn path_fuzzy_still_matches_when_no_other_signal_does() {
        // Only the path component matches, so the scan must still run.
        let projects = vec![project("beta", "/x/al")];
        let found = best("al", &projects).unwrap();
        assert_eq!(found.reason, Reason::FuzzyPath);
    }

    #[test]
    fn fuzzy_finds_subsequences() {
        let projects = vec![project("overdosecd", "/a/overdosecd")];
        let found = best("odscd", &projects).unwrap();
        assert_eq!(found.name, "overdosecd");
        assert_eq!(found.reason, Reason::FuzzyName);
    }

    #[test]
    fn no_match_is_an_error() {
        let projects = vec![project("alpha", "/a/alpha")];
        assert!(matches!(best("qqqqzz", &projects), Err(Error::NoMatch(_))));
    }

    #[test]
    fn empty_index_is_reported_distinctly() {
        assert!(matches!(best("anything", &[]), Err(Error::EmptyIndex)));
    }

    #[test]
    fn close_scores_are_ambiguous() {
        let projects = vec![
            project("app-one", "/a/app-one"),
            project("app-two", "/a/app-two"),
        ];
        match best("app", &projects) {
            Err(Error::Ambiguous { candidates, .. }) => {
                assert_eq!(candidates.0.len(), 2);
                assert_eq!(candidates.0[0].reason, Reason::NamePrefix);
            }
            other => panic!("expected an ambiguous error, got {other:?}"),
        }
    }

    #[test]
    fn pinned_breaks_ties() {
        let mut pinned = project("over", "/a/over");
        pinned.pinned = true;
        let projects = vec![project("over", "/b/over"), pinned];
        let found = best("over", &projects).unwrap();
        assert_eq!(found.path, PathBuf::from("/a/over"));
    }

    #[test]
    fn recency_and_frequency_rank_higher() {
        let now = Utc::now();
        let mut fresh = project("proj-one", "/x/proj-one");
        fresh.last_used_at = Some(now - Duration::hours(1));
        fresh.use_count = 5;
        let ranked = rank("proj", &[project("proj-two", "/x/proj-two"), fresh]);
        assert_eq!(ranked[0].name, "proj-one");

        let found = best(
            "proj",
            &[project("proj-two", "/x/proj-two"), {
                let mut used = project("proj-one", "/x/proj-one");
                used.last_used_at = Some(now - Duration::hours(1));
                used.use_count = 5;
                used
            }],
        )
        .unwrap();
        assert_eq!(found.name, "proj-one");
    }

    #[test]
    fn shallower_paths_rank_higher() {
        let shallow = project("proj-a", "/x/proj-a");
        let deep = project("proj-b", "/x/one/two/three/four/proj-b");
        let ranked = rank("proj", &[deep, shallow]);
        assert_eq!(ranked[0].name, "proj-a");
        assert!(ranked[0].score > ranked[1].score);
    }

    #[test]
    fn recency_bonus_decays_with_age() {
        let config = MatcherConfig::DEFAULT;
        let now = Utc::now();
        let fresh = config.recency_bonus(Some(now), now);
        let day_old = config.recency_bonus(Some(now - Duration::hours(24)), now);
        let month_old = config.recency_bonus(Some(now - Duration::hours(720)), now);
        assert!(fresh > day_old);
        assert!(day_old > month_old);
        assert_eq!(month_old, 0);
        assert_eq!(config.recency_bonus(None, now), 0);
    }

    #[test]
    fn frequency_bonus_saturates() {
        let config = MatcherConfig::DEFAULT;
        assert_eq!(config.frequency_bonus(0), 0);
        assert!(config.frequency_bonus(1) > 0);
        assert!(config.frequency_bonus(10) > config.frequency_bonus(1));
        assert_eq!(config.frequency_bonus(u64::MAX), 400);
    }

    #[test]
    fn highlight_indices_are_char_positions_for_display() {
        assert_eq!(highlight_indices("ods", "overdosecd"), vec![0, 4, 6]);
        assert_eq!(highlight_indices("ODS", "overdosecd"), vec![0, 4, 6]);
        assert_eq!(highlight_indices("ove", "overdosecd"), vec![0, 1, 2]);
        assert!(highlight_indices("zzz", "overdosecd").is_empty());
        assert!(highlight_indices("", "overdosecd").is_empty());
    }

    #[test]
    fn confidence_follows_the_match_classes() {
        for reason in [
            Reason::ExactName,
            Reason::ExactAlias,
            Reason::NamePrefix,
            Reason::AliasPrefix,
            Reason::Tag,
            Reason::GitRepoExact,
            Reason::GitRepoPrefix,
        ] {
            assert!(reason.is_confident(), "{reason:?}");
        }
        for reason in [
            Reason::FuzzyName,
            Reason::FuzzyAlias,
            Reason::FuzzyPath,
            Reason::GitRemote,
        ] {
            assert!(!reason.is_confident(), "{reason:?}");
        }
    }

    #[test]
    fn empty_query_matches_nothing() {
        let projects = vec![project("alpha", "/a/alpha")];
        assert!(rank("   ", &projects).is_empty());
    }

    #[test]
    fn completion_candidates_filter_sort_and_skip_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = dir.path().to_str().expect("utf-8 temp path");
        let mut overdosecd_project = Project::for_test("overdosecd", live);
        overdosecd_project.aliases.push("od".into());
        overdosecd_project.tags.push("rust".into());
        let mut notes = Project::for_test("notes", live);
        notes.aliases.push("od-notes".into());
        let stale = Project::for_test("od-old", "/tmp/overdosecd-does-not-exist-complete");

        let projects = vec![overdosecd_project, notes, stale];
        let all = completion_candidates(&projects, "");
        assert_eq!(
            all,
            vec!["notes", "od", "od-notes", "overdosecd", "rust"],
            "names, aliases, and tags, sorted and deduplicated"
        );

        let filtered = completion_candidates(&projects, "od");
        assert_eq!(filtered, vec!["od", "od-notes"]);
        assert_eq!(
            completion_candidates(&projects, " OD "),
            filtered,
            "matching is trimmed and case-insensitive"
        );
        assert_eq!(
            completion_candidates(&projects, "ov"),
            vec!["overdosecd"],
            "the name matches by prefix too"
        );

        assert!(
            !all.iter().any(|candidate| candidate == "od-old"),
            "missing directories are skipped"
        );
    }

    #[test]
    fn completion_candidates_deduplicate_case_insensitively() {
        let dir = tempfile::tempdir().expect("tempdir");
        let live = dir.path().to_str().expect("utf-8 temp path");
        let mut first = Project::for_test("alpha", live);
        first.tags.push("ALPHA".into());
        let mut second = Project::for_test("alpha", live);
        second.aliases.push("Alpha".into());

        let projects = vec![first, second];
        assert_eq!(completion_candidates(&projects, "alp"), vec!["alpha"]);
    }

    #[test]
    fn fuzzy_matching_can_be_disabled() {
        let projects = vec![project("overdosecd", "/a/overdosecd")];
        let strict = MatcherConfig {
            fuzzy: false,
            ..MatcherConfig::DEFAULT
        };

        assert!(matches!(
            super::best("odscd", &projects, &strict),
            Err(Error::NoMatch(_))
        ));
        // Path components no longer match either.
        assert!(matches!(
            super::best("a", &projects, &strict),
            Err(Error::NoMatch(_))
        ));
        // Exact and prefix matching are unaffected.
        assert_eq!(
            super::best("overdosecd", &projects, &strict)
                .unwrap()
                .reason,
            Reason::ExactName
        );
        assert_eq!(
            super::best("over", &projects, &strict).unwrap().reason,
            Reason::NamePrefix
        );
    }

    #[test]
    fn pinned_bonus_respects_config() {
        let mut pinned = project("over", "/a/over");
        pinned.pinned = true;
        let projects = vec![project("over", "/b/over"), pinned];

        let without = MatcherConfig {
            pinned_bonus: 0,
            ..MatcherConfig::DEFAULT
        };
        assert!(matches!(
            super::best("over", &projects, &without),
            Err(Error::Ambiguous { .. })
        ));
        assert!(super::best("over", &projects, &MatcherConfig::DEFAULT).is_ok());
    }

    #[test]
    fn ambiguity_margin_respects_config() {
        let shallow = project("app-one", "/x/app-one");
        let deep = project("app-two", "/x/deep/nested/app-two");
        let projects = vec![shallow.clone(), deep];

        assert!(matches!(
            super::best("app", &projects, &MatcherConfig::DEFAULT),
            Err(Error::Ambiguous { .. })
        ));

        let strict = MatcherConfig {
            ambiguity_margin: 0,
            ..MatcherConfig::DEFAULT
        };
        let found = super::best("app", &projects, &strict).unwrap();
        assert_eq!(found.path, shallow.path);
    }

    #[test]
    fn depth_penalty_respects_config() {
        let shallow = project("proj-a", "/x/proj-a");
        let deep = project("proj-b", "/x/one/two/three/four/proj-b");
        let projects = vec![deep, shallow];

        let flat = MatcherConfig {
            depth_penalty: 0,
            ..MatcherConfig::DEFAULT
        };
        let ranked = super::rank("proj", &projects, &flat);
        assert_eq!(
            ranked[0].score, ranked[1].score,
            "with no depth penalty the paths should tie"
        );
    }

    #[test]
    fn zero_recency_and_frequency_weights_disable_their_bonuses() {
        let now = Utc::now();
        let mut used = project("proj-one", "/x/proj-one");
        used.last_used_at = Some(now - Duration::hours(1));
        used.use_count = 5;
        let projects = vec![project("proj-two", "/x/proj-two"), used];

        let neutral = MatcherConfig {
            recency_max: 0,
            frequency_max: 0,
            ..MatcherConfig::DEFAULT
        };
        let ranked = super::rank("proj", &projects, &neutral);
        assert_eq!(
            ranked[0].score, ranked[1].score,
            "zeroed weights should remove the recency/frequency edge"
        );
    }

    #[test]
    fn every_term_must_match() {
        let projects = vec![
            project("rust-tools", "/x/rust-tools"),
            project("web-scratch", "/x/web-scratch"),
            project("rust-web-app", "/x/rust-web-app"),
        ];
        let ranked = rank("rust web", &projects);
        let names: Vec<&str> = ranked.iter().map(|found| found.name.as_str()).collect();
        assert_eq!(
            names,
            ["rust-web-app"],
            "projects matching only one term are excluded"
        );
        assert_eq!(ranked[0].terms.len(), 2);
    }

    #[test]
    fn multi_term_sums_signals_and_applies_bonuses_once() {
        let projects = vec![project("aa", "/x/aa")];
        let single = rank("aa", &projects).remove(0);
        let both = rank("aa aa", &projects).remove(0);
        assert_eq!(
            both.score,
            single.score + 10_000,
            "the second exact signal adds once; bonuses do not double"
        );
        assert_eq!(both.terms.len(), 2);
        assert!(both.all_confident());
    }

    #[test]
    fn whole_query_wins_for_names_with_spaces() {
        let projects = vec![
            project("my notes", "/x/my-notes"),
            project("notes-app", "/x/notes-app"),
        ];
        let ranked = rank("my notes", &projects);
        let top = &ranked[0];
        assert_eq!(top.name, "my notes");
        assert_eq!(
            top.reason,
            Reason::ExactName,
            "the whole query matches exactly"
        );
        assert_eq!(top.terms.len(), 1, "the whole-query path is one term");
        assert!(top.all_confident());
    }

    #[test]
    fn multi_term_confidence_needs_every_term() {
        let projects = vec![project("rust-web-tools", "/x/rust-web-tools")];
        let ranked = rank("rust web", &projects);
        assert_eq!(ranked.len(), 1, "both terms match fuzzily");
        assert!(
            !ranked[0].all_confident(),
            "a fuzzy term keeps the query unconfident"
        );

        let mut aliased = project("rust", "/x/rust");
        aliased.aliases.push("web".to_owned());
        let ranked = rank("rust web", &[aliased]);
        assert_eq!(ranked[0].terms.len(), 2);
        assert!(
            ranked[0].all_confident(),
            "exact name plus exact alias is confident"
        );
    }

    #[test]
    fn multi_term_ambiguity_respects_the_margin() {
        let projects = vec![
            project("app-one", "/x/app-one"),
            project("app-two", "/x/app-two"),
        ];
        let strict = MatcherConfig {
            ambiguity_margin: 10_000,
            ..MatcherConfig::DEFAULT
        };
        let err = super::best("app o", &projects, &strict).expect_err("ambiguous");
        assert!(matches!(err, Error::Ambiguous { .. }), "{err:?}");
    }

    #[test]
    fn highlight_unions_every_term() {
        let indices = highlight_indices("ab cd", "abcd");
        assert_eq!(indices, [0, 1, 2, 3], "both terms highlight");
        assert_eq!(
            highlight_indices("cd", "abcd"),
            [2, 3],
            "single terms keep the old behavior"
        );
        assert!(highlight_indices("zz", "abcd").is_empty());
    }

    use proptest::prelude::*;

    proptest! {
        /// Arbitrary queries and project names must never panic, and ranking
        /// the same inputs twice must produce the same scores.
        #[test]
        fn rank_never_panics_and_is_deterministic(
            query in any::<String>(),
            names in prop::collection::vec(any::<String>(), 0..10),
        ) {
            let projects: Vec<Project> = names
                .iter()
                .enumerate()
                .map(|(index, name)| project(name, &format!("/x/{index}")))
                .collect();
            let ranked = super::rank(&query, &projects, &MatcherConfig::DEFAULT);
            let again = super::rank(&query, &projects, &MatcherConfig::DEFAULT);
            let scores: Vec<i64> = ranked.iter().map(|found| found.score).collect();
            let scores_again: Vec<i64> = again.iter().map(|found| found.score).collect();
            prop_assert_eq!(scores, scores_again, "ranking is deterministic");

            for found in &ranked {
                // A single contributing term keeps the exact old confidence
                // rule; multi-term matches require every term to be confident.
                if let [hit] = found.terms.as_slice() {
                    prop_assert_eq!(found.all_confident(), hit.reason.is_confident());
                }
                prop_assert!(!found.terms.is_empty());
            }

            // Highlights are display-only positions inside the text.
            for name in &names {
                let indices = highlight_indices(&query, name);
                prop_assert!(indices.iter().all(|&index| index < name.len().max(1)));
            }
        }
    }
}
