//! The picker state machine: query, ranking, selection, modes, and the
//! actions it can run.
//!
//! Deliberately terminal-free: [`Picker::handle_key`] takes a crossterm key
//! event and returns an [`Outcome`], so the whole interaction is unit-tested
//! without a tty. The event loop in `super` executes the run actions against
//! the store.

use std::collections::BTreeSet;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

use crate::cli::SortBy;
use crate::discovery::{self, Dir, HomeMatch};
use crate::matcher::{self, MatcherConfig};
use crate::project::{self, Project, TermKind};

use super::actions::{self, Action, PromptKind};

/// Rows the wheel moves per notch.
const MOUSE_SCROLL_STEP: isize = 3;

/// How many home-directory rows the picker offers per query.
const HOME_RESULT_LIMIT: usize = 20;

/// What the picker asks the event loop to execute against the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunAction {
    /// Jump to `projects[index]`, after checking the directory still exists.
    Jump(usize),

    /// Jump to a home-directory match (not in the index: no usage is
    /// recorded).
    JumpHome { path: PathBuf },

    /// Pin or unpin every id in one update.
    Pin { ids: Vec<String>, pinned: bool },

    /// Add a path (the add prompt).
    Add { path: String },

    /// Remove every id, after the confirmation.
    Remove { ids: Vec<String> },

    /// Rename one project; `force` is set after the collision confirmation.
    Rename {
        id: String,
        name: String,
        force: bool,
    },

    /// Add or remove an alias or tag.
    Term {
        kind: TermKind,
        id: String,
        value: String,
        add: bool,
    },

    /// Move a stale project to a new path.
    Repath { id: String, path: String },

    /// Re-run the health checks and show the issue list.
    Health,

    /// Run `overdosecd migrate`.
    Migrate,
}

/// What the picker wants the event loop to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Keep running.
    Continue,
    /// Leave without a selection: exit code 1, empty stdout.
    Cancel,
    /// Execute `action` against the store, then keep running.
    Run(RunAction),
}

/// How the picker reads keys: `Search` filters, `Nav` is the vim list mode,
/// `Prompt` edits a one-line input, `Palette` picks an action, `Help` shows
/// the key list, and `Health` lists the index's problems.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Search,
    Nav,
    Prompt,
    Palette,
    Help,
    Health,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub kind: PromptKind,
    pub input: String,
    /// The project a target-needing prompt applies to.
    pub target: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Palette {
    pub query: String,
    pub selected: usize,
}

pub struct Status {
    pub text: String,
    pub error: bool,
}

/// A y/N question asked before something destructive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub message: String,
    pub action: ConfirmAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfirmAction {
    Remove { ids: Vec<String> },
    Rename { id: String, name: String },
}

/// One row in the health view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRow {
    pub label: String,
    /// Set when the row is a stale project that can be relocated or removed.
    pub stale: Option<StaleRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleRef {
    pub id: String,
    pub path: PathBuf,
}

/// What the detail pane needs; fetched by the event loop because it touches
/// the store and the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetailsInfo {
    pub id: String,
    pub branch: Option<String>,
    pub history: Vec<DateTime<Utc>>,
}

pub struct Picker {
    projects: Vec<Project>,
    /// Indices into `projects`, in display order.
    entries: Vec<usize>,
    selected: usize,
    query: String,
    mode: Mode,
    /// Where `Esc` returns from a prompt or the palette.
    return_mode: Mode,
    prompt: Prompt,
    palette: Palette,
    confirm: Option<Confirm>,
    status: Option<Status>,
    details: bool,
    details_info: Option<DetailsInfo>,
    sort: SortBy,
    show_missing: bool,
    /// Project ids marked for a batched action.
    marks: BTreeSet<String>,
    /// Directories found under `$HOME`, loaded by the event loop.
    home_dirs: Vec<Dir>,
    /// The ranked home matches for the current query.
    home: Vec<HomeMatch>,
    /// Whether the home cache has been loaded at least once.
    home_loaded: bool,
    /// `[discovery] home`: master switch for the fallback.
    home_enabled: bool,
    /// Show home matches even when the index has matches.
    force_home: bool,
    issues: Vec<IssueRow>,
    health_selected: usize,
    /// Cached `!path.exists()` per project, filled on demand.
    missing: Vec<bool>,
    /// Rows the list area last had; half of it is a page step.
    viewport: usize,
    /// Where the list was last drawn, for mouse hit-testing.
    list_area: Option<Rect>,
    /// The entry position the list's first drawn row showed.
    list_start: usize,
    matching: MatcherConfig,
}

impl Picker {
    pub fn new(projects: Vec<Project>, matching: MatcherConfig) -> Self {
        let mut picker = Self {
            projects,
            entries: Vec::new(),
            selected: 0,
            query: String::new(),
            mode: Mode::Search,
            return_mode: Mode::Search,
            prompt: Prompt {
                kind: PromptKind::Add,
                input: String::new(),
                target: None,
            },
            palette: Palette::default(),
            confirm: None,
            status: None,
            details: false,
            details_info: None,
            sort: SortBy::Used,
            show_missing: true,
            marks: BTreeSet::new(),
            home_dirs: Vec::new(),
            home: Vec::new(),
            home_loaded: false,
            home_enabled: true,
            force_home: false,
            issues: Vec::new(),
            health_selected: 0,
            missing: Vec::new(),
            viewport: 10,
            list_area: None,
            list_start: 0,
            matching,
        };
        picker.refilter(None);
        picker
    }

    pub fn projects(&self) -> &[Project] {
        &self.projects
    }

    pub fn entries(&self) -> &[usize] {
        &self.entries
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn prompt(&self) -> &Prompt {
        &self.prompt
    }

    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    pub fn palette_matches(&self) -> Vec<Action> {
        actions::palette_matches(&self.palette.query)
    }

    pub fn confirm(&self) -> Option<&Confirm> {
        self.confirm.as_ref()
    }

    pub fn status(&self) -> Option<&Status> {
        self.status.as_ref()
    }

    pub fn details_info(&self) -> Option<&DetailsInfo> {
        self.details_info.as_ref()
    }

    pub fn sort(&self) -> SortBy {
        self.sort
    }

    pub fn show_missing(&self) -> bool {
        self.show_missing
    }

    pub fn marks(&self) -> &BTreeSet<String> {
        &self.marks
    }

    pub fn issues(&self) -> &[IssueRow] {
        &self.issues
    }

    pub fn health_selected(&self) -> usize {
        self.health_selected
    }

    /// The highlighted project, when a project row is selected.
    pub fn selected_project(&self) -> Option<&Project> {
        if self.selected >= self.entries.len() {
            return None;
        }
        self.entries
            .get(self.selected)
            .map(|&index| &self.projects[index])
    }

    /// The highlighted home result, when a home row is selected.
    pub(crate) fn selected_home(&self) -> Option<&HomeMatch> {
        self.home
            .get(self.selected.checked_sub(self.entries.len())?)
    }

    /// Rows on offer: the index matches, then the home matches.
    pub fn total_len(&self) -> usize {
        self.entries.len() + self.home.len()
    }

    pub fn home_matches(&self) -> &[HomeMatch] {
        &self.home
    }

    /// Whether the home fallback is enabled at all.
    pub fn home_enabled(&self) -> bool {
        self.home_enabled
    }

    /// Whether the event loop should load the home cache: a query is typed,
    /// the index came up empty (or home results were forced), and the cache
    /// has not been loaded yet.
    pub fn needs_home(&self) -> bool {
        self.home_enabled
            && !self.home_loaded
            && !self.query.trim().is_empty()
            && (self.entries.is_empty() || self.force_home)
    }

    /// Installs the cached directories (loaded by the event loop) and
    /// recomputes the matches.
    pub fn set_home_dirs(&mut self, dirs: Vec<Dir>) {
        self.home_dirs = dirs;
        self.home_loaded = true;
        let preserve = self.selected_project().map(|project| project.id.clone());
        self.refilter(preserve.as_deref());
    }

    /// Enables or disables the fallback (`[discovery] home`).
    pub fn set_home_enabled(&mut self, enabled: bool) {
        self.home_enabled = enabled;
        if !enabled {
            self.home.clear();
        }
    }

    /// Whether the project at display position `position` is missing.
    pub fn is_missing(&self, position: usize) -> bool {
        self.entries
            .get(position)
            .is_some_and(|&index| !self.projects[index].path.exists())
    }

    pub fn set_status(&mut self, text: impl Into<String>, error: bool) {
        self.status = Some(Status {
            text: text.into(),
            error,
        });
    }

    /// Switches modes and clears any transient message.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.status = None;
    }

    /// Records the list height so `u`/`d` and PgUp/PgDn move what is on
    /// screen.
    pub fn set_viewport(&mut self, rows: usize) {
        if rows > 0 {
            self.viewport = rows;
        }
    }

    /// Records where the list was drawn and which entry its first row showed,
    /// so mouse clicks can be mapped back to entries.
    pub fn set_list_geometry(&mut self, area: Rect, start: usize) {
        self.list_area = Some(area);
        self.list_start = start;
    }

    /// Replaces the health rows and switches to the health view.
    pub fn set_issues(&mut self, issues: Vec<IssueRow>) {
        self.issues = issues;
        self.health_selected = 0;
        self.set_mode(Mode::Health);
    }

    /// Whether the detail pane can be drawn in the current mode.
    pub fn details_visible(&self) -> bool {
        self.details && matches!(self.mode, Mode::Search | Mode::Nav)
    }

    /// The project whose detail info must be fetched next, when the pane is
    /// open and the highlighted project changed.
    pub fn details_request(&self) -> Option<String> {
        if !self.details_visible() {
            return None;
        }
        let project = self.selected_project()?;
        match &self.details_info {
            Some(info) if info.id == project.id => None,
            _ => Some(project.id.clone()),
        }
    }

    pub fn set_details_info(
        &mut self,
        id: String,
        branch: Option<String>,
        history: Vec<DateTime<Utc>>,
    ) {
        self.details_info = Some(DetailsInfo {
            id,
            branch,
            history,
        });
    }

    /// Recomputes the visible entries: the default (or chosen) order for an
    /// empty query, the ranked matches otherwise. With `preserve`, keeps the
    /// row with that project id selected when it is still visible; otherwise
    /// the best row wins.
    pub fn refilter(&mut self, preserve: Option<&str>) {
        let query = self.query.trim().to_owned();
        if query.is_empty() {
            let mut entries: Vec<usize> = (0..self.projects.len()).collect();
            if !self.show_missing {
                let missing: Vec<bool> = self.missing_flags().to_vec();
                entries.retain(|&index| !missing[index]);
            }
            let sort = self.sort;
            entries.sort_by(|&a, &b| project::sort_key(&self.projects[a], &self.projects[b], sort));
            self.entries = entries;
        } else {
            self.entries = matcher::rank(&query, &self.projects, &self.matching)
                .into_iter()
                .map(|found| found.index)
                .collect();
        }

        // Home matches only when the index has nothing (or when the user
        // asked for them), and never for a directory that is already indexed.
        let home_matches = if self.home_enabled
            && !query.is_empty()
            && (self.entries.is_empty() || self.force_home)
        {
            discovery::rank(&query, &self.home_dirs, HOME_RESULT_LIMIT)
                .into_iter()
                .filter(|found| {
                    !self
                        .projects
                        .iter()
                        .any(|project| project.path == found.path)
                })
                .collect()
        } else {
            Vec::new()
        };
        self.home = home_matches;

        let last = self.total_len().saturating_sub(1);
        self.selected = match preserve {
            Some(id) => self
                .entries
                .iter()
                .position(|&index| self.projects[index].id == id)
                .unwrap_or(0),
            None => 0,
        }
        .min(last);
    }

    /// Replaces the in-memory index, e.g. after a mutation elsewhere.
    pub fn set_projects(&mut self, projects: Vec<Project>, preserve: Option<&str>) {
        self.projects = projects;
        self.missing.clear();
        self.marks
            .retain(|id| self.projects.iter().any(|project| &project.id == id));
        self.details_info = None;
        self.refilter(preserve);
    }

    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.refilter(None);
    }

    /// Applies a confirmed pin flip to the in-memory copies.
    pub fn set_pinned_ids(&mut self, ids: &[String], pinned: bool) {
        for project in self.projects.iter_mut() {
            if ids.contains(&project.id) {
                project.pinned = pinned;
            }
        }
    }

    /// Opens the add prompt: a highlighted home result's path, the trimmed
    /// query, or the current directory.
    pub(crate) fn enter_add(&mut self) {
        if let Some(home) = self.selected_home() {
            let path = home.path.display().to_string();
            self.open_prompt_with(PromptKind::Add, None, path);
            return;
        }
        self.enter_add_with(std::env::current_dir().ok());
    }

    /// Testable variant of [`Picker::enter_add`] with an explicit directory.
    pub fn enter_add_with(&mut self, cwd: Option<PathBuf>) {
        let prefill = if self.query.trim().is_empty() {
            cwd.map(|path| path.display().to_string())
                .unwrap_or_default()
        } else {
            self.query.trim().to_owned()
        };
        self.open_prompt_with(PromptKind::Add, None, prefill);
    }

    /// Inserts pasted text into the active input, dropping dangerous
    /// characters: line breaks would break the layout, and escape bytes
    /// (multiplexers wrap paste in bracketed-paste envelopes) or bidi controls
    /// must never end up in a query or a prompt value.
    pub fn insert_text(&mut self, text: &str) {
        let text: String = text
            .chars()
            .filter(|ch| !crate::sanitize::is_dangerous(*ch))
            .collect();
        if text.is_empty() {
            return;
        }
        match self.mode {
            Mode::Search | Mode::Nav => {
                self.query.push_str(&text);
                self.set_mode(Mode::Search);
                self.refilter(None);
            }
            Mode::Prompt => self.prompt.input.push_str(&text),
            Mode::Palette => {
                self.palette.query.push_str(&text);
                self.palette.selected = 0;
            }
            Mode::Help | Mode::Health => {}
        }
    }

    /// Handles one key press and reports what should happen next.
    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return Outcome::Cancel;
        }
        if self.confirm.is_some() {
            return self.handle_confirm(key);
        }
        match self.mode {
            Mode::Search => self.handle_search(key, ctrl),
            Mode::Nav => self.handle_nav(key, ctrl),
            Mode::Prompt => self.handle_prompt(key, ctrl),
            Mode::Palette => self.handle_palette(key, ctrl),
            Mode::Help => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?')) {
                    self.set_mode(self.return_mode);
                }
                Outcome::Continue
            }
            Mode::Health => self.handle_health(key),
        }
    }

    /// Handles a mouse event: the wheel moves the current view's highlight, a
    /// left click selects the row under the pointer.
    pub fn handle_mouse(&mut self, event: MouseEvent) -> Outcome {
        match event.kind {
            MouseEventKind::ScrollDown => match self.mode {
                Mode::Health => self.move_health(MOUSE_SCROLL_STEP),
                Mode::Palette => self.move_palette(MOUSE_SCROLL_STEP),
                _ => self.move_selection(MOUSE_SCROLL_STEP),
            },
            MouseEventKind::ScrollUp => match self.mode {
                Mode::Health => self.move_health(-MOUSE_SCROLL_STEP),
                Mode::Palette => self.move_palette(-MOUSE_SCROLL_STEP),
                _ => self.move_selection(-MOUSE_SCROLL_STEP),
            },
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(position) = self.position_at(event.row) {
                    match self.mode {
                        Mode::Health => self.health_selected = position,
                        Mode::Palette => self.palette.selected = position,
                        _ => self.selected = position,
                    }
                }
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    fn handle_confirm(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let Some(confirm) = self.confirm.take() else {
                    return Outcome::Continue;
                };
                match confirm.action {
                    ConfirmAction::Remove { ids } => Outcome::Run(RunAction::Remove { ids }),
                    ConfirmAction::Rename { id, name } => Outcome::Run(RunAction::Rename {
                        id,
                        name,
                        force: true,
                    }),
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.confirm = None;
                self.set_status("cancelled", false);
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    fn handle_search(&mut self, key: KeyEvent, ctrl: bool) -> Outcome {
        match key.code {
            KeyCode::Esc => Outcome::Cancel,
            KeyCode::Tab => {
                self.set_mode(Mode::Nav);
                Outcome::Continue
            }
            KeyCode::Enter => self.jump(),
            KeyCode::Char('p') if ctrl => self.ask_pin(),
            KeyCode::Char('a') if ctrl => {
                self.enter_add();
                Outcome::Continue
            }
            KeyCode::Char('x') if ctrl => self.ask_remove(),
            KeyCode::Char(' ') | KeyCode::Null if ctrl || key.code == KeyCode::Null => {
                self.open_palette();
                Outcome::Continue
            }
            KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.query.push(ch);
                self.refilter(None);
                Outcome::Continue
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.refilter(None);
                Outcome::Continue
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-self.page()),
            KeyCode::PageDown => self.move_selection(self.page()),
            _ => Outcome::Continue,
        }
    }

    fn handle_nav(&mut self, key: KeyEvent, ctrl: bool) -> Outcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('g') | KeyCode::Home => self.select(0),
            KeyCode::Char('G') | KeyCode::End => self.select(usize::MAX),
            KeyCode::Char('u') | KeyCode::PageUp => self.move_selection(-self.page()),
            KeyCode::Char('d') | KeyCode::PageDown => self.move_selection(self.page()),
            KeyCode::Char('p') if ctrl => self.ask_pin(),
            KeyCode::Char('a') if ctrl => {
                self.enter_add();
                Outcome::Continue
            }
            KeyCode::Char('x') if ctrl => self.ask_remove(),
            KeyCode::Char('m') => {
                if let Some(project) = self.selected_project() {
                    let id = project.id.clone();
                    if !self.marks.remove(&id) {
                        self.marks.insert(id);
                    }
                }
                Outcome::Continue
            }
            KeyCode::Char(':') | KeyCode::Null => {
                self.open_palette();
                Outcome::Continue
            }
            KeyCode::Char(' ') if ctrl => {
                self.open_palette();
                Outcome::Continue
            }
            KeyCode::Char('?') => {
                self.return_mode = Mode::Nav;
                self.set_mode(Mode::Help);
                Outcome::Continue
            }
            KeyCode::Tab | KeyCode::Char('i') | KeyCode::Char('/') => {
                self.set_mode(Mode::Search);
                Outcome::Continue
            }
            KeyCode::Enter => self.jump(),
            KeyCode::Esc => Outcome::Cancel,
            _ => Outcome::Continue,
        }
    }

    fn handle_prompt(&mut self, key: KeyEvent, ctrl: bool) -> Outcome {
        match key.code {
            KeyCode::Esc => {
                self.set_mode(self.return_mode);
                Outcome::Continue
            }
            KeyCode::Enter => self.submit_prompt(),
            KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.prompt.input.push(ch);
                Outcome::Continue
            }
            KeyCode::Backspace => {
                self.prompt.input.pop();
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    fn handle_palette(&mut self, key: KeyEvent, ctrl: bool) -> Outcome {
        match key.code {
            KeyCode::Esc | KeyCode::Tab => {
                self.close_palette();
                Outcome::Continue
            }
            KeyCode::Char(' ') | KeyCode::Null if ctrl || key.code == KeyCode::Null => {
                self.close_palette();
                Outcome::Continue
            }
            KeyCode::Enter => match self.palette_matches().get(self.palette.selected).copied() {
                Some(action) => {
                    // Close first: prompts capture where `esc` returns to, and
                    // view actions (sort, details, ...) must show their effect.
                    self.close_palette();
                    self.run_action(action)
                }
                None => Outcome::Continue,
            },
            KeyCode::Char('j') | KeyCode::Down => self.move_palette(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_palette(-1),
            KeyCode::Char(ch) if !ctrl => {
                self.palette.query.push(ch);
                self.palette.selected = 0;
                Outcome::Continue
            }
            KeyCode::Backspace => {
                self.palette.query.pop();
                self.palette.selected = 0;
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    fn handle_health(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.move_health(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_health(-1),
            KeyCode::Char('r') => Outcome::Run(RunAction::Health),
            KeyCode::Char('x') => self.ask_remove_stale(),
            KeyCode::Enter => self.open_prompt(PromptKind::Repath),
            KeyCode::Esc => {
                self.set_mode(Mode::Search);
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    /// Runs a palette action.
    fn run_action(&mut self, action: Action) -> Outcome {
        // The palette's add entry and `^a` must open the same prefilled
        // prompt; `enter_add` owns the prefill rule.
        if action == Action::Add {
            self.enter_add();
            return Outcome::Continue;
        }
        if let Some(prompt) = action.prompt() {
            return self.open_prompt(prompt);
        }
        match action {
            Action::Jump => self.jump(),
            Action::Pin => self.ask_pin(),
            Action::Remove => self.ask_remove(),
            // Add and the prompts were handled above; the match still has to
            // name them.
            Action::Add
            | Action::Rename
            | Action::AliasAdd
            | Action::AliasRemove
            | Action::TagAdd
            | Action::TagRemove => Outcome::Continue,
            Action::Details => {
                self.details = !self.details;
                Outcome::Continue
            }
            Action::SortUsed | Action::SortName | Action::SortCreated => {
                if let Some(sort) = action.sort() {
                    self.sort = sort;
                    let preserve = self.selected_project().map(|project| project.id.clone());
                    self.refilter(preserve.as_deref());
                    self.set_status(format!("sorted by {}", sort_label(sort)), false);
                }
                Outcome::Continue
            }
            Action::ToggleMissing => {
                self.show_missing = !self.show_missing;
                self.refilter(None);
                self.set_status(
                    if self.show_missing {
                        "showing missing projects"
                    } else {
                        "hiding missing projects"
                    },
                    false,
                );
                Outcome::Continue
            }
            Action::MarkAll => {
                self.marks = self
                    .entries
                    .iter()
                    .map(|&index| self.projects[index].id.clone())
                    .collect();
                Outcome::Continue
            }
            Action::SearchHome => {
                self.force_home = !self.force_home;
                self.refilter(None);
                self.set_status(
                    if self.force_home {
                        "searching home directories too"
                    } else {
                        "home search limited to index misses"
                    },
                    false,
                );
                Outcome::Continue
            }
            Action::UnmarkAll => {
                self.marks.clear();
                Outcome::Continue
            }
            Action::Health => Outcome::Run(RunAction::Health),
            Action::Migrate => Outcome::Run(RunAction::Migrate),
            Action::Help => {
                self.return_mode = self.mode;
                self.set_mode(Mode::Help);
                Outcome::Continue
            }
            Action::Quit => Outcome::Cancel,
        }
    }

    fn open_palette(&mut self) {
        self.return_mode = match self.mode {
            Mode::Nav | Mode::Search => self.mode,
            _ => Mode::Search,
        };
        self.palette = Palette::default();
        self.set_mode(Mode::Palette);
    }

    fn close_palette(&mut self) {
        self.set_mode(self.return_mode);
    }

    /// Opens a prompt for the highlighted project.
    fn open_prompt(&mut self, kind: PromptKind) -> Outcome {
        if kind == PromptKind::Repath {
            return self.open_repath_prompt();
        }

        let Some(project) = self.selected_project() else {
            self.no_target();
            return Outcome::Continue;
        };
        let (id, name) = (project.id.clone(), project.name.clone());
        let prefill = match kind {
            PromptKind::Rename => name,
            PromptKind::Add
            | PromptKind::AliasAdd
            | PromptKind::AliasRemove
            | PromptKind::TagAdd
            | PromptKind::TagRemove
            | PromptKind::Repath => String::new(),
        };
        self.open_prompt_with(kind, Some(id), prefill);
        Outcome::Continue
    }

    fn open_repath_prompt(&mut self) -> Outcome {
        let Some(row) = self.issues.get(self.health_selected) else {
            self.no_target();
            return Outcome::Continue;
        };
        let Some(stale) = &row.stale else {
            self.set_status("this issue cannot be repaired from here", true);
            return Outcome::Continue;
        };
        let (id, path) = (stale.id.clone(), stale.path.display().to_string());
        self.open_prompt_with(PromptKind::Repath, Some(id), path);
        Outcome::Continue
    }

    fn open_prompt_with(&mut self, kind: PromptKind, target: Option<String>, prefill: String) {
        self.return_mode = match self.mode {
            Mode::Palette | Mode::Health => Mode::Nav,
            mode => mode,
        };
        self.prompt = Prompt {
            kind,
            input: prefill,
            target,
        };
        self.set_mode(Mode::Prompt);
    }

    fn submit_prompt(&mut self) -> Outcome {
        let input = self.prompt.input.trim().to_owned();
        let kind = self.prompt.kind;
        let target = self.prompt.target.clone();

        if input.is_empty() {
            self.set_status("enter a value first", true);
            return Outcome::Continue;
        }

        match kind {
            PromptKind::Add => Outcome::Run(RunAction::Add { path: input }),
            PromptKind::Repath => match target {
                Some(id) => Outcome::Run(RunAction::Repath { id, path: input }),
                None => {
                    self.no_target();
                    Outcome::Continue
                }
            },
            PromptKind::Rename => match target {
                Some(id) => Outcome::Run(RunAction::Rename {
                    id,
                    name: input,
                    force: false,
                }),
                None => {
                    self.no_target();
                    Outcome::Continue
                }
            },
            PromptKind::AliasAdd | PromptKind::AliasRemove => {
                self.term_action(TermKind::Alias, kind, target, input)
            }
            PromptKind::TagAdd | PromptKind::TagRemove => {
                self.term_action(TermKind::Tag, kind, target, input)
            }
        }
    }

    fn term_action(
        &mut self,
        term: TermKind,
        kind: PromptKind,
        target: Option<String>,
        value: String,
    ) -> Outcome {
        let Some(id) = target else {
            self.no_target();
            return Outcome::Continue;
        };
        let add = matches!(kind, PromptKind::AliasAdd | PromptKind::TagAdd);
        Outcome::Run(RunAction::Term {
            kind: term,
            id,
            value,
            add,
        })
    }

    /// Asks for the y/N confirmation before removing.
    pub(crate) fn ask_remove(&mut self) -> Outcome {
        let ids = self.targets();
        if ids.is_empty() {
            self.no_target();
            return Outcome::Continue;
        }
        let message = if ids.len() == 1 {
            let name = self.name_of(&ids[0]);
            format!("remove `{name}`? y/N")
        } else {
            format!("remove {} projects? y/N", ids.len())
        };
        self.confirm = Some(Confirm {
            message,
            action: ConfirmAction::Remove { ids },
        });
        Outcome::Continue
    }

    /// Asks for the y/N confirmation before a name collision rename.
    pub fn ask_rename_force(&mut self, id: String, name: String) {
        self.confirm = Some(Confirm {
            message: format!("another project is already named `{name}` — rename anyway? y/N"),
            action: ConfirmAction::Rename { id, name },
        });
    }

    /// The ids a mutation applies to: the marked rows when any are marked,
    /// otherwise the highlighted project. A highlighted *home* row has no id.
    pub fn targets(&self) -> Vec<String> {
        if self.selected_home().is_some() {
            return Vec::new();
        }
        if self.marks.is_empty() {
            return self
                .selected_project()
                .map(|project| project.id.clone())
                .into_iter()
                .collect();
        }
        self.entries
            .iter()
            .map(|&index| &self.projects[index])
            .filter(|project| self.marks.contains(&project.id))
            .map(|project| project.id.clone())
            .collect()
    }

    /// The status for an action that needs an indexed project.
    fn no_target(&mut self) {
        let message = if self.selected_home().is_some() {
            "home match is not indexed yet — ^a adds it"
        } else {
            "nothing selected"
        };
        self.set_status(message, true);
    }

    fn ask_pin(&mut self) -> Outcome {
        let ids = self.targets();
        if ids.is_empty() {
            self.no_target();
            return Outcome::Continue;
        }
        let pinned = !ids.iter().all(|id| {
            self.projects
                .iter()
                .any(|project| &project.id == id && project.pinned)
        });
        Outcome::Run(RunAction::Pin { ids, pinned })
    }

    fn ask_remove_stale(&mut self) -> Outcome {
        let Some(row) = self.issues.get(self.health_selected) else {
            self.no_target();
            return Outcome::Continue;
        };
        let Some(stale) = &row.stale else {
            self.set_status("only stale projects can be removed here", true);
            return Outcome::Continue;
        };
        let id = stale.id.clone();
        let name = self.name_of(&id);
        self.confirm = Some(Confirm {
            message: format!("remove `{name}`? y/N"),
            action: ConfirmAction::Remove { ids: vec![id] },
        });
        Outcome::Continue
    }

    pub(crate) fn name_of(&self, id: &str) -> String {
        self.projects
            .iter()
            .find(|project| project.id == id)
            .map(|project| project.name.clone())
            .unwrap_or_else(|| id.to_owned())
    }

    fn jump(&mut self) -> Outcome {
        if let Some(home) = self.selected_home() {
            return Outcome::Run(RunAction::JumpHome {
                path: home.path.clone(),
            });
        }
        match self.entries.get(self.selected) {
            Some(&index) => Outcome::Run(RunAction::Jump(index)),
            None => {
                self.set_status("no matches", false);
                Outcome::Continue
            }
        }
    }

    fn select(&mut self, position: usize) -> Outcome {
        self.selected = position.min(self.total_len().saturating_sub(1));
        Outcome::Continue
    }

    fn move_selection(&mut self, delta: isize) -> Outcome {
        if self.total_len() > 0 {
            let last = self.total_len() - 1;
            self.selected = self.selected.saturating_add_signed(delta).min(last);
        }
        Outcome::Continue
    }

    fn move_palette(&mut self, delta: isize) -> Outcome {
        let count = self.palette_matches().len();
        if count > 0 {
            self.palette.selected = self
                .palette
                .selected
                .saturating_add_signed(delta)
                .min(count - 1);
        }
        Outcome::Continue
    }

    fn move_health(&mut self, delta: isize) -> Outcome {
        if !self.issues.is_empty() {
            let last = self.issues.len() - 1;
            self.health_selected = self.health_selected.saturating_add_signed(delta).min(last);
        }
        Outcome::Continue
    }

    /// The entry position under screen row `row`, when the click landed on
    /// the list.
    fn position_at(&self, row: u16) -> Option<usize> {
        let area = self.list_area?;
        if row < area.y || row >= area.bottom() {
            return None;
        }
        let position = self.list_start + usize::from(row - area.y);
        let count = match self.mode {
            Mode::Palette => self.palette_matches().len(),
            Mode::Health => self.issues.len(),
            _ => self.total_len(),
        };
        (position < count).then_some(position)
    }

    fn missing_flags(&mut self) -> &[bool] {
        if self.missing.len() != self.projects.len() {
            self.missing = self
                .projects
                .iter()
                .map(|project| !project.path.exists())
                .collect();
        }
        &self.missing
    }

    /// Half a screenful, at least one row.
    fn page(&self) -> isize {
        (self.viewport / 2).max(1) as isize
    }
}

/// The human label for a sort order; also the picker status's wording.
pub(super) fn sort_label(sort: SortBy) -> &'static str {
    match sort {
        SortBy::Used => "last used",
        SortBy::Name => "name",
        SortBy::Created => "created",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared with `home_tests`.
    pub(super) fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn project(name: &str) -> Project {
        Project::for_test(name, &format!("/x/{name}"))
    }

    pub(super) fn fixture(names: &[&str]) -> Picker {
        Picker::new(
            names.iter().map(|name| project(name)).collect(),
            MatcherConfig::DEFAULT,
        )
    }

    fn visible(picker: &Picker) -> Vec<&str> {
        picker
            .entries()
            .iter()
            .map(|&index| picker.projects()[index].name.as_str())
            .collect()
    }

    /// Types every character of `text` as key presses.
    fn type_text(picker: &mut Picker, text: &str) {
        for ch in text.chars() {
            picker.handle_key(key(KeyCode::Char(ch)));
        }
    }

    /// Opens the palette and runs the first action matching `query`.
    fn run_palette(picker: &mut Picker, query: &str) -> Outcome {
        if picker.mode() != Mode::Nav {
            picker.handle_key(key(KeyCode::Tab));
        }
        picker.handle_key(key(KeyCode::Char(':')));
        type_text(picker, query);
        picker.handle_key(key(KeyCode::Enter))
    }

    #[test]
    fn an_empty_query_shows_the_default_order() {
        let mut recent = project("recent");
        recent.last_used_at = Some(chrono::Utc::now());
        let mut pinned = project("pinned");
        pinned.pinned = true;
        let picker = Picker::new(
            vec![project("plain"), recent, pinned],
            MatcherConfig::DEFAULT,
        );
        assert_eq!(visible(&picker), ["pinned", "recent", "plain"]);
    }

    #[test]
    fn typing_filters_and_selects_the_best_match() {
        let mut picker = fixture(&["overdosecd", "notes"]);
        type_text(&mut picker, "ods");
        assert_eq!(picker.query(), "ods");
        assert_eq!(visible(&picker), ["overdosecd"]);
        assert_eq!(picker.selected(), 0);

        picker.handle_key(key(KeyCode::Backspace));
        assert_eq!(picker.query(), "od");
        picker.handle_key(key(KeyCode::Backspace));
        picker.handle_key(key(KeyCode::Backspace));
        assert_eq!(visible(&picker).len(), 2, "empty query lists everything");
    }

    #[test]
    fn search_mode_types_j_and_k_instead_of_moving() {
        let mut picker = fixture(&["jack", "kite"]);
        type_text(&mut picker, "jk");
        assert_eq!(picker.query(), "jk");
        assert_eq!(visible(&picker), ["jack"]);
    }

    #[test]
    fn tab_switches_to_nav_mode_where_jk_moves() {
        let mut picker = fixture(&["alpha", "beta", "gamma"]);
        picker.handle_key(key(KeyCode::Tab));
        assert_eq!(picker.mode(), Mode::Nav);

        picker.handle_key(key(KeyCode::Char('j')));
        picker.handle_key(key(KeyCode::Char('j')));
        assert_eq!(picker.selected(), 2);
        picker.handle_key(key(KeyCode::Char('j')));
        assert_eq!(picker.selected(), 2, "clamps at the end");
        picker.handle_key(key(KeyCode::Char('g')));
        assert_eq!(picker.selected(), 0, "g jumps to the top");
        picker.handle_key(key(KeyCode::Char('G')));
        assert_eq!(picker.selected(), 2, "G jumps to the bottom");

        picker.handle_key(key(KeyCode::Tab));
        assert_eq!(picker.mode(), Mode::Search);
    }

    #[test]
    fn changing_the_query_resets_the_selection() {
        let mut picker = fixture(&["alpha", "beta"]);
        picker.handle_key(key(KeyCode::Down));
        assert_eq!(picker.selected(), 1);
        picker.handle_key(key(KeyCode::Char('a')));
        assert_eq!(picker.selected(), 0);
    }

    #[test]
    fn enter_jumps_to_the_highlighted_project_index() {
        let mut picker = fixture(&["alpha", "beta"]);
        picker.handle_key(key(KeyCode::Down));
        assert_eq!(
            picker.handle_key(key(KeyCode::Enter)),
            Outcome::Run(RunAction::Jump(1))
        );

        picker.set_query("nomatchzzz".into());
        assert_eq!(picker.handle_key(key(KeyCode::Enter)), Outcome::Continue);
        assert_eq!(picker.status().expect("status").text, "no matches");
    }

    #[test]
    fn esc_cancels_from_search_and_nav_but_backs_out_of_prompts() {
        let mut picker = fixture(&["alpha"]);
        assert_eq!(picker.handle_key(key(KeyCode::Esc)), Outcome::Cancel);

        picker.handle_key(key(KeyCode::Tab));
        assert_eq!(picker.handle_key(key(KeyCode::Esc)), Outcome::Cancel);

        picker.enter_add_with(Some("/x".into()));
        assert_eq!(picker.handle_key(key(KeyCode::Esc)), Outcome::Continue);
        assert_eq!(
            picker.mode(),
            Mode::Nav,
            "esc returns to the mode the prompt opened from (Nav)"
        );

        assert_eq!(picker.handle_key(ctrl(KeyCode::Char('c'))), Outcome::Cancel);
    }

    #[test]
    fn ctrl_chords_and_the_palette_open_what_they_should() {
        let mut picker = fixture(&["alpha", "beta"]);
        assert_eq!(
            picker.handle_key(ctrl(KeyCode::Char('p'))),
            Outcome::Run(RunAction::Pin {
                ids: vec!["alpha".to_owned()],
                pinned: true,
            })
        );

        picker.set_query("beta".into());
        picker.handle_key(ctrl(KeyCode::Char('a')));
        assert_eq!(picker.mode(), Mode::Prompt);
        assert_eq!(picker.prompt().kind, PromptKind::Add);
        assert_eq!(picker.prompt().input, "beta", "prefilled with the query");

        picker.handle_key(key(KeyCode::Esc));
        picker.handle_key(ctrl(KeyCode::Char(' ')));
        assert_eq!(picker.mode(), Mode::Palette);
        assert_eq!(picker.handle_key(key(KeyCode::Esc)), Outcome::Continue);
        assert_eq!(picker.mode(), Mode::Search, "esc returns where it opened");
    }

    #[test]
    fn the_add_prompt_prefills_the_query_or_the_current_directory() {
        let mut picker = fixture(&["alpha"]);
        picker.enter_add_with(Some("/home/user/code".into()));
        assert_eq!(picker.prompt().input, "/home/user/code");

        picker.set_query("over".into());
        picker.enter_add_with(Some("/home/user/code".into()));
        assert_eq!(picker.prompt().input, "over");

        picker.set_query(String::new());
        picker.enter_add();
        assert!(
            !picker.prompt().input.is_empty(),
            "an empty query prefills the current directory"
        );
    }

    #[test]
    fn palette_add_prefills_like_the_ctrl_chord() {
        let mut picker = fixture(&["alpha"]);
        picker.set_query("over".into());
        assert_eq!(run_palette(&mut picker, "add"), Outcome::Continue);
        assert_eq!(picker.mode(), Mode::Prompt);
        assert_eq!(picker.prompt().kind, PromptKind::Add);
        assert_eq!(picker.prompt().input, "over", "prefilled with the query");
    }

    #[test]
    fn prompts_submit_run_actions() {
        // Rename: prefilled with the current name, edited before submitting.
        let mut picker = fixture(&["alpha"]);
        run_palette(&mut picker, "rename");
        assert_eq!(picker.mode(), Mode::Prompt);
        assert_eq!(picker.prompt().kind, PromptKind::Rename);
        assert_eq!(picker.prompt().input, "alpha");
        for _ in 0..5 {
            picker.handle_key(key(KeyCode::Backspace));
        }
        type_text(&mut picker, "alpha-two");
        assert_eq!(
            picker.handle_key(key(KeyCode::Enter)),
            Outcome::Run(RunAction::Rename {
                id: "alpha".to_owned(),
                name: "alpha-two".to_owned(),
                force: false,
            })
        );

        // Alias add: an empty prompt, typed by hand.
        let mut picker = fixture(&["alpha"]);
        run_palette(&mut picker, "alias");
        assert_eq!(picker.prompt().kind, PromptKind::AliasAdd);
        type_text(&mut picker, "a");
        assert_eq!(
            picker.handle_key(key(KeyCode::Enter)),
            Outcome::Run(RunAction::Term {
                kind: TermKind::Alias,
                id: "alpha".to_owned(),
                value: "a".to_owned(),
                add: true,
            })
        );

        // Tag remove.
        let mut picker = fixture(&["alpha"]);
        run_palette(&mut picker, "remove a tag");
        assert_eq!(picker.prompt().kind, PromptKind::TagRemove);
        type_text(&mut picker, "rust");
        assert_eq!(
            picker.handle_key(key(KeyCode::Enter)),
            Outcome::Run(RunAction::Term {
                kind: TermKind::Tag,
                id: "alpha".to_owned(),
                value: "rust".to_owned(),
                add: false,
            })
        );

        // An empty submit keeps the prompt open with a status.
        let mut picker = fixture(&["alpha"]);
        run_palette(&mut picker, "rename");
        for _ in 0..5 {
            picker.handle_key(key(KeyCode::Backspace));
        }
        assert_eq!(picker.handle_key(key(KeyCode::Enter)), Outcome::Continue);
        assert_eq!(picker.mode(), Mode::Prompt);
        assert!(picker.status().expect("status").error);
    }

    #[test]
    fn removal_asks_for_confirmation() {
        let mut picker = fixture(&["alpha", "beta"]);
        assert_eq!(
            picker.handle_key(ctrl(KeyCode::Char('x'))),
            Outcome::Continue
        );
        let confirm = picker.confirm().expect("confirmation");
        assert_eq!(confirm.message, "remove `alpha`? y/N");

        assert_eq!(
            picker.handle_key(key(KeyCode::Char('y'))),
            Outcome::Run(RunAction::Remove {
                ids: vec!["alpha".to_owned()],
            })
        );
        assert!(picker.confirm().is_none());

        picker.ask_remove();
        assert_eq!(
            picker.handle_key(key(KeyCode::Char('n'))),
            Outcome::Continue
        );
        assert!(picker.confirm().is_none());
        assert_eq!(picker.status().expect("status").text, "cancelled");
    }

    #[test]
    fn marks_target_batches() {
        let mut picker = fixture(&["alpha", "beta", "gamma"]);
        picker.handle_key(key(KeyCode::Tab));
        picker.handle_key(key(KeyCode::Char('m')));
        picker.handle_key(key(KeyCode::Char('j')));
        picker.handle_key(key(KeyCode::Char('m')));
        assert_eq!(
            picker.targets(),
            vec!["alpha".to_owned(), "beta".to_owned()],
            "marked rows in display order"
        );

        assert_eq!(
            picker.handle_key(ctrl(KeyCode::Char('p'))),
            Outcome::Run(RunAction::Pin {
                ids: vec!["alpha".to_owned(), "beta".to_owned()],
                pinned: true,
            })
        );
        picker.set_pinned_ids(&["alpha".to_owned(), "beta".to_owned()], true);
        assert_eq!(
            picker.handle_key(ctrl(KeyCode::Char('p'))),
            Outcome::Run(RunAction::Pin {
                ids: vec!["alpha".to_owned(), "beta".to_owned()],
                pinned: false,
            }),
            "a fully pinned batch unpins"
        );

        picker.handle_key(key(KeyCode::Char(':')));
        type_text(&mut picker, "unmark");
        picker.handle_key(key(KeyCode::Enter));
        assert!(picker.marks().is_empty());
        assert_eq!(
            picker.targets(),
            vec!["beta".to_owned()],
            "without marks the highlighted row is the target"
        );

        picker.handle_key(key(KeyCode::Char(':')));
        type_text(&mut picker, "mark every");
        picker.handle_key(key(KeyCode::Enter));
        assert_eq!(picker.marks().len(), 3, "mark all marks every visible row");
    }

    #[test]
    fn batch_removal_confirms_with_a_count() {
        let mut picker = fixture(&["alpha", "beta", "gamma"]);
        picker.handle_key(key(KeyCode::Tab));
        picker.handle_key(key(KeyCode::Char('m')));
        picker.handle_key(key(KeyCode::Char('j')));
        picker.handle_key(key(KeyCode::Char('m')));
        picker.handle_key(ctrl(KeyCode::Char('x')));
        let confirm = picker.confirm().expect("confirmation");
        assert_eq!(confirm.message, "remove 2 projects? y/N");
        assert_eq!(
            picker.handle_key(key(KeyCode::Char('y'))),
            Outcome::Run(RunAction::Remove {
                ids: vec!["alpha".to_owned(), "beta".to_owned()],
            })
        );
    }

    #[test]
    fn palette_filters_and_runs_sort_and_view_actions() {
        let mut recent = project("bravo");
        recent.last_used_at = Some(chrono::Utc::now());
        let mut picker = Picker::new(vec![project("alpha"), recent], MatcherConfig::DEFAULT);
        assert_eq!(visible(&picker), ["bravo", "alpha"], "recent first");

        assert_eq!(run_palette(&mut picker, "sort by name"), Outcome::Continue);
        assert_eq!(picker.sort(), SortBy::Name);
        assert_eq!(visible(&picker), ["alpha", "bravo"]);
        assert_eq!(picker.mode(), Mode::Nav, "the palette closed");
        assert!(picker.status().expect("status").text.contains("name"));

        assert_eq!(run_palette(&mut picker, "details"), Outcome::Continue);
        assert!(picker.details_visible());
        assert_eq!(run_palette(&mut picker, "details"), Outcome::Continue);
        assert!(!picker.details_visible());

        assert_eq!(run_palette(&mut picker, "quit"), Outcome::Cancel);

        assert_eq!(
            run_palette(&mut picker, "health"),
            Outcome::Run(RunAction::Health)
        );
        assert_eq!(
            run_palette(&mut picker, "migrate"),
            Outcome::Run(RunAction::Migrate)
        );
    }

    #[test]
    fn palette_queries_that_match_nothing_show_a_message() {
        let mut picker = fixture(&["alpha"]);
        picker.handle_key(key(KeyCode::Tab));
        picker.handle_key(key(KeyCode::Char(':')));
        type_text(&mut picker, "zzzz");
        assert!(picker.palette_matches().is_empty());
        assert_eq!(picker.handle_key(key(KeyCode::Enter)), Outcome::Continue);
        assert_eq!(picker.mode(), Mode::Palette, "stays open");
    }

    #[test]
    fn help_overlay_opens_and_closes() {
        let mut picker = fixture(&["alpha"]);
        picker.handle_key(key(KeyCode::Tab));
        picker.handle_key(key(KeyCode::Char('?')));
        assert_eq!(picker.mode(), Mode::Help);
        assert_eq!(picker.handle_key(key(KeyCode::Esc)), Outcome::Continue);
        assert_eq!(picker.mode(), Mode::Nav);
    }

    #[test]
    fn health_view_relocates_and_removes_stale_projects() {
        let mut picker = fixture(&["alpha"]);
        let stale = || IssueRow {
            label: "stale `alpha` -> /x/alpha".to_owned(),
            stale: Some(StaleRef {
                id: "alpha".to_owned(),
                path: PathBuf::from("/x/alpha"),
            }),
        };

        picker.set_issues(vec![stale()]);
        assert_eq!(picker.mode(), Mode::Health);

        assert_eq!(picker.handle_key(key(KeyCode::Enter)), Outcome::Continue);
        assert_eq!(picker.mode(), Mode::Prompt);
        assert_eq!(picker.prompt().kind, PromptKind::Repath);
        assert_eq!(
            picker.prompt().input,
            "/x/alpha",
            "prefilled with the old path"
        );
        assert_eq!(picker.handle_key(key(KeyCode::Esc)), Outcome::Continue);
        assert_eq!(picker.mode(), Mode::Nav);

        picker.set_issues(vec![stale()]);
        assert_eq!(
            picker.handle_key(key(KeyCode::Char('x'))),
            Outcome::Continue
        );
        let confirm = picker.confirm().expect("confirmation");
        assert_eq!(confirm.message, "remove `alpha`? y/N");
        assert_eq!(
            picker.handle_key(key(KeyCode::Char('y'))),
            Outcome::Run(RunAction::Remove {
                ids: vec!["alpha".to_owned()],
            })
        );

        picker.set_issues(vec![stale()]);
        assert_eq!(
            picker.handle_key(key(KeyCode::Char('r'))),
            Outcome::Run(RunAction::Health)
        );
        assert_eq!(picker.handle_key(key(KeyCode::Esc)), Outcome::Continue);
        assert_eq!(picker.mode(), Mode::Search);
    }

    #[test]
    fn details_request_tracks_the_selection() {
        let mut picker = fixture(&["alpha", "beta"]);
        assert_eq!(picker.details_request(), None, "closed by default");

        run_palette(&mut picker, "details");
        assert_eq!(picker.details_request(), Some("alpha".to_owned()));
        picker.set_details_info("alpha".to_owned(), Some("main".to_owned()), Vec::new());
        assert_eq!(picker.details_request(), None, "already fetched");

        picker.handle_key(key(KeyCode::Char('j')));
        assert_eq!(picker.details_request(), Some("beta".to_owned()));
    }

    #[test]
    fn missing_projects_can_be_hidden() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut live = project("alpha");
        live.path = dir.path().to_path_buf();
        let picker_projects = vec![live, project("beta")];
        let mut picker = Picker::new(picker_projects, MatcherConfig::DEFAULT);
        assert_eq!(picker.entries().len(), 2, "missing ones show by default");
        assert!(!picker.is_missing(0));
        assert!(picker.is_missing(1));

        run_palette(&mut picker, "missing");
        assert!(!picker.show_missing());
        assert_eq!(visible(&picker), ["alpha"]);
        assert!(picker.status().expect("status").text.contains("hiding"));

        run_palette(&mut picker, "missing");
        assert!(picker.show_missing());
        assert_eq!(picker.entries().len(), 2);
    }

    #[test]
    fn refilter_preserves_a_project_by_id() {
        let mut picker = fixture(&["alpha", "beta"]);
        picker.handle_key(key(KeyCode::Down));
        assert_eq!(picker.selected_project().expect("selection").name, "beta");

        picker.refilter(Some("beta"));
        assert_eq!(picker.selected_project().expect("selection").name, "beta");

        picker.refilter(Some("missing-id"));
        assert_eq!(picker.selected_project().expect("selection").name, "alpha");
    }

    #[test]
    fn pasted_text_is_inserted_without_control_characters() {
        let mut picker = fixture(&["alpha", "beta"]);
        picker.insert_text("be\nta\r");
        assert_eq!(picker.query(), "beta");
        assert_eq!(visible(&picker), ["beta"]);

        // Bracketed-paste envelopes can carry escape bytes; none of them may
        // survive into the input.
        picker.set_query(String::new());
        picker.insert_text("be\x1b[<65;10;5Mta");
        assert_eq!(picker.query(), "be[<65;10;5Mta");
        assert!(!picker.query().contains('\x1b'));

        picker.enter_add_with(Some("/x".into()));
        picker.prompt.input.clear();
        picker.insert_text("/x\n/y");
        assert_eq!(picker.prompt().input, "/x/y");

        picker.handle_key(key(KeyCode::Esc));
        picker.handle_key(key(KeyCode::Tab));
        picker.handle_key(key(KeyCode::Char(':')));
        picker.insert_text("so\x01rt");
        assert_eq!(picker.palette().query, "sort");
    }

    #[test]
    fn paging_uses_the_viewport() {
        let names: Vec<String> = (0..40).map(|index| format!("project-{index:02}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut picker = fixture(&names);
        picker.set_viewport(10);
        picker.handle_key(key(KeyCode::Tab));

        picker.handle_key(key(KeyCode::PageDown));
        assert_eq!(picker.selected(), 5, "half of a 10-row viewport");
        picker.handle_key(key(KeyCode::Char('d')));
        assert_eq!(picker.selected(), 10);
        picker.handle_key(key(KeyCode::Char('u')));
        assert_eq!(picker.selected(), 5);
    }

    #[test]
    fn mouse_scroll_and_click_move_the_selection() {
        let mut picker = fixture(&["alpha", "beta", "delta", "gamma"]);
        picker.set_list_geometry(Rect::new(0, 5, 40, 4), 0);

        picker.handle_mouse(mouse(MouseEventKind::ScrollDown));
        assert_eq!(picker.selected(), 3, "one notch moves three rows");
        picker.handle_mouse(mouse(MouseEventKind::ScrollUp));
        assert_eq!(picker.selected(), 0);

        picker.handle_mouse(click(7));
        assert_eq!(
            picker.selected_project().expect("selection").name,
            "delta",
            "clicks map to the row under the pointer"
        );

        let before = picker.selected();
        picker.handle_mouse(click(20));
        assert_eq!(
            picker.selected(),
            before,
            "clicks outside the list do nothing"
        );
    }

    #[test]
    fn mouse_clicks_respect_a_scrolled_list() {
        let mut picker = fixture(&["alpha", "beta", "delta", "gamma"]);
        picker.set_list_geometry(Rect::new(0, 5, 40, 2), 2);

        picker.handle_mouse(click(5));
        assert_eq!(picker.selected_project().expect("selection").name, "delta");
        picker.handle_mouse(click(6));
        assert_eq!(picker.selected_project().expect("selection").name, "gamma");
        picker.handle_mouse(click(7));
        assert_eq!(
            picker.selected_project().expect("selection").name,
            "gamma",
            "rows past the entries are ignored"
        );
    }

    #[test]
    fn mouse_clicks_move_the_palette_and_health_selection() {
        let mut picker = fixture(&["alpha"]);
        picker.handle_key(key(KeyCode::Tab));
        picker.handle_key(key(KeyCode::Char(':')));
        picker.set_list_geometry(Rect::new(0, 5, 40, 20), 0);
        picker.handle_mouse(click(7));
        assert_eq!(picker.palette().selected, 2);

        picker.handle_key(key(KeyCode::Esc));
        picker.set_issues(vec![
            IssueRow {
                label: "one".to_owned(),
                stale: None,
            },
            IssueRow {
                label: "two".to_owned(),
                stale: None,
            },
        ]);
        picker.handle_mouse(click(6));
        assert_eq!(picker.health_selected(), 1);
    }

    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 10,
            row: 6,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click(row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Manual probe for the per-keystroke cost on a big index:
    /// `cargo test --release -- --ignored rank_latency --nocapture`
    #[test]
    #[ignore = "perf probe; run manually with --release"]
    fn rank_latency_at_100k() {
        let projects: Vec<Project> = (0..100_000)
            .map(|index| project(&format!("project-{index:06}")))
            .collect();
        let mut picker = Picker::new(projects, MatcherConfig::DEFAULT);

        let start = std::time::Instant::now();
        for _ in 0..10 {
            picker.set_query("proj-42".into());
        }
        let query_time = start.elapsed() / 10;

        let start = std::time::Instant::now();
        for _ in 0..10 {
            picker.set_query("proj 42".into());
        }
        let multi_time = start.elapsed() / 10;

        let start = std::time::Instant::now();
        picker.set_query(String::new());
        let default_time = start.elapsed();

        println!(
            "100k projects: query keystroke {query_time:?}, two-term keystroke {multi_time:?}, \
             empty-query reset {default_time:?}"
        );
    }
}

#[cfg(test)]
mod home_tests {
    use super::tests::{fixture, key};
    use super::*;

    fn dir(name: &str, project_like: bool) -> Dir {
        Dir {
            path: PathBuf::from(format!("/home/user/{name}")),
            name: name.to_owned(),
            depth: 1,
            project_like,
        }
    }

    #[test]
    fn home_results_fill_in_when_the_index_has_nothing() {
        let mut picker = fixture(&["overdosecd"]);
        assert!(!picker.needs_home(), "nothing typed yet");

        picker.set_query("notes".into());
        assert!(picker.needs_home(), "query with no index match");
        assert!(picker.home_matches().is_empty(), "cache not loaded yet");

        picker.set_home_dirs(vec![
            dir("notes", false),
            dir("notes-old", false),
            dir("other", false),
        ]);
        assert_eq!(picker.entries().len(), 0);
        assert_eq!(picker.home_matches().len(), 2, "notes and notes-old match");
        assert_eq!(picker.total_len(), 2);
        assert_eq!(picker.selected_home().expect("home row").name, "notes");
    }

    #[test]
    fn home_results_stay_hidden_when_the_index_matches() {
        let mut picker = fixture(&["overdosecd"]);
        picker.set_home_dirs(vec![dir("overdosecd", true)]);
        picker.set_query("over".into());
        assert!(!picker.entries().is_empty(), "the index matches");
        assert!(picker.home_matches().is_empty(), "no fallback needed");
    }

    #[test]
    fn indexed_paths_are_not_offered_twice() {
        let mut picker = fixture(&["alpha"]);
        let mut indexed = dir("alpha", true);
        indexed.path = PathBuf::from("/x/alpha");
        picker.set_home_dirs(vec![indexed, dir("alphabeta", true)]);
        picker.set_query("alphab".into());
        assert!(picker.entries().is_empty(), "no index match for alphab");
        assert_eq!(
            picker.home_matches().len(),
            1,
            "the indexed path is filtered out"
        );
        assert_eq!(picker.home_matches()[0].name, "alphabeta");
    }

    #[test]
    fn enter_on_a_home_row_jumps_to_its_path() {
        let mut picker = fixture(&["overdosecd"]);
        picker.set_home_dirs(vec![dir("notes", false)]);
        picker.set_query("notes".into());
        assert_eq!(picker.total_len(), 1);
        assert_eq!(
            picker.handle_key(key(KeyCode::Enter)),
            Outcome::Run(RunAction::JumpHome {
                path: PathBuf::from("/home/user/notes"),
            })
        );
    }

    #[test]
    fn navigation_spans_index_and_home_rows() {
        let mut picker = fixture(&["alpha"]);
        picker.set_home_dirs(vec![dir("alpha-notes", false)]);
        picker.set_query("alpha".into());
        assert_eq!(picker.total_len(), 1, "the index match stands alone");

        // Force the fallback so both groups are on offer.
        picker.handle_key(key(KeyCode::Tab));
        picker.handle_key(key(KeyCode::Char(':')));
        for ch in "also search".chars() {
            picker.handle_key(key(KeyCode::Char(ch)));
        }
        picker.handle_key(key(KeyCode::Enter));
        assert_eq!(picker.total_len(), 2, "index match plus home match");

        picker.handle_key(key(KeyCode::Down));
        assert_eq!(picker.selected(), 1);
        assert!(
            picker.selected_project().is_none(),
            "home row, not a project"
        );
        picker.handle_key(key(KeyCode::Down));
        assert_eq!(picker.selected(), 1, "clamps at the end");
        picker.handle_key(key(KeyCode::Up));
        assert_eq!(picker.selected(), 0);
        assert!(picker.selected_project().is_some());
    }

    #[test]
    fn home_rows_cannot_be_pinned_or_removed_but_can_be_added() {
        let mut picker = fixture(&["overdosecd"]);
        picker.set_home_dirs(vec![dir("notes", false)]);
        picker.set_query("notes".into());

        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            Outcome::Continue
        );
        assert!(
            picker
                .status()
                .expect("status")
                .text
                .contains("not indexed yet")
        );

        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Outcome::Continue
        );
        assert_eq!(picker.mode(), Mode::Prompt);
        assert_eq!(picker.prompt().input, "/home/user/notes");
    }

    #[test]
    fn the_palette_can_force_home_results() {
        let mut picker = fixture(&["alpha"]);
        picker.set_home_dirs(vec![dir("alpha-notes", false)]);
        picker.set_query("alpha".into());
        assert!(picker.home_matches().is_empty(), "index match wins");

        picker.handle_key(key(KeyCode::Tab));
        picker.handle_key(key(KeyCode::Char(':')));
        for ch in "also search".chars() {
            picker.handle_key(key(KeyCode::Char(ch)));
        }
        assert_eq!(picker.handle_key(key(KeyCode::Enter)), Outcome::Continue);
        assert_eq!(picker.total_len(), 2, "forced home results appear");

        picker.handle_key(key(KeyCode::Char(':')));
        for ch in "also search".chars() {
            picker.handle_key(key(KeyCode::Char(ch)));
        }
        picker.handle_key(key(KeyCode::Enter));
        assert_eq!(picker.total_len(), 1, "toggle back off");
    }

    #[test]
    fn disabling_home_clears_the_results() {
        let mut picker = fixture(&["overdosecd"]);
        picker.set_home_dirs(vec![dir("notes", false)]);
        picker.set_query("notes".into());
        assert_eq!(picker.total_len(), 1);
        picker.set_home_enabled(false);
        assert_eq!(picker.total_len(), 0);
        assert!(!picker.needs_home());
    }
}
