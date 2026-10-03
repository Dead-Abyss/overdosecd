//! The interactive picker.
//!
//! It draws a compact box inline, anchored where the cursor was (ratatui's
//! inline viewport) instead of taking over the screen: the scrollback and
//! whatever the user was reading stay visible, and the box is cleared on the
//! way out. Only the chosen path is written to stdout, so
//! `cd "$(overdosecd ui)"` keeps working.
//!
//! The interaction rules live in `picker.rs` (terminal-free, unit-tested);
//! this module owns the terminal, executes the picker's run actions against
//! the store, and fetches the data the detail pane shows.

mod actions;
mod backend;
mod picker;
mod render;

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use chrono::Utc;
use ratatui::backend::{Backend, ClearType};
use ratatui::crossterm::cursor;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::config::{ColorMode, MouseMode, StorageBackend, Ui};
use crate::discovery::{self, Dir};
use crate::doctor::{self, Issue, Repair};
use crate::error::{Error, Result};
use crate::git;
use crate::matcher::MatcherConfig;
use crate::migrate;
use crate::output;
use crate::project::{self, TermKind};
use crate::sanitize;
use crate::store::Store;

use self::picker::{IssueRow, Outcome, Picker, RunAction, StaleRef};

/// Environment variables that mark a terminal multiplexer which handles the
/// mouse for its own pane UI.
const MULTIPLEXER_VARS: [&str; 3] = ["TMUX", "ZELLIJ", "STY"];

/// The resolved picker options.
pub struct Options {
    /// Height of the inline box in rows.
    pub height: u16,
    /// Draw a border around the box.
    pub border: bool,
    /// Capture the mouse (wheel scrolls, click selects).
    pub mouse: bool,
    /// Prefill the search box (from `ui --query`).
    pub query: Option<String>,
    /// Do not record the jump in usage statistics (`ui --no-track`).
    pub no_track: bool,
}

impl Options {
    /// Resolves the config and the command-line flags: mouse `auto` only
    /// turns on in a plain terminal, because capturing it suspends the
    /// multiplexer's own pane UI.
    pub fn resolve(ui: &Ui, query: Option<String>, no_track: bool) -> Self {
        Self {
            height: ui.height,
            border: ui.border,
            mouse: match ui.mouse {
                MouseMode::Always => true,
                MouseMode::Never => false,
                MouseMode::Auto => !in_multiplexer(),
            },
            query,
            no_track,
        }
    }
}

/// What the picker cannot know on its own: where the index lives, which
/// backend is active (for the health view and `migrate`), and how the home
/// fallback should behave.
pub struct Context<'a> {
    pub data_dir: &'a Path,
    pub backend: StorageBackend,
    pub discovery: &'a crate::config::Discovery,
}

/// How many home-directory rows the picker offers per query.
const HOME_RESULT_LIMIT: usize = 20;

/// What the picker exited with.
enum Selected {
    Project(usize),
    Home(PathBuf),
}

/// Runs the picker and returns the process exit code: `0` after a jump (the
/// path is on stdout by then), `1` when the user cancels.
pub fn run(
    store: &impl Store,
    context: &Context,
    matching: &MatcherConfig,
    color: ColorMode,
    options: &Options,
) -> Result<i32> {
    let projects = store.load()?;
    if projects.is_empty() && !context.discovery.home {
        return Err(Error::EmptyIndex);
    }

    let home = project::home_dir();
    let mut picker = Picker::new(projects, *matching);
    if let Some(query) = &options.query {
        // The wrapper passes the user's words through; the search box is a
        // renderer, so it gets the escaped form.
        picker.set_query(sanitize::text(query));
    }

    let picked = pick(
        store,
        context,
        &mut picker,
        home.as_deref(),
        colors_enabled(color),
        options,
    )?;

    let (path, record) = match picked {
        Some(Selected::Project(index)) => {
            let project = &picker.projects()[index];
            (project.path.clone(), Some(project.id.clone()))
        }
        Some(Selected::Home(path)) => (path, None),
        None => return Ok(1),
    };

    // Like `goto`: the jump itself must never fail because of bookkeeping, so
    // print the path first and degrade recording failures to a warning. And
    // like `goto`, the line is captured by `cd "$(…)`": a dangerous path is
    // refused, never escaped.
    if sanitize::contains_dangerous(&path.to_string_lossy()) {
        return Err(Error::UnsafePath(sanitize::path(&path)));
    }
    output::print_line(&path.display().to_string())?;
    if let Some(id) = record
        && !options.no_track
        && let Err(err) = store.record_use(&id, Utc::now())
    {
        let _ = output::print_stderr_line(&format!("warning: could not record usage: {err}"));
    }
    Ok(0)
}

/// Renders the picker until Enter selects a project or the user cancels.
fn pick(
    store: &impl Store,
    context: &Context,
    picker: &mut Picker,
    home: Option<&Path>,
    color: bool,
    options: &Options,
) -> Result<Option<Selected>> {
    let mut session = Session::enter(options)?;
    picker.set_home_enabled(context.discovery.home);
    picker.set_home_limit(HOME_RESULT_LIMIT);
    let mut scanner: Option<mpsc::Receiver<Vec<Dir>>> = None;

    // Mouse tracking stays on until it is explicitly released, and a default
    // signal disposition would skip every `Drop`, leaving the shell with a
    // mouse-reporting terminal. Route TERM/INT/HUP through a flag and exit via
    // the normal cancel path so `Session::drop` always runs.
    let interrupted = Arc::new(AtomicBool::new(false));
    let terminate = [
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGHUP,
    ];
    for signal in terminate {
        signal_hook::flag::register(signal, Arc::clone(&interrupted))?;
    }

    loop {
        if interrupted.load(Ordering::Relaxed) {
            return Ok(None);
        }
        session
            .terminal
            .draw(|frame| render::draw(frame, picker, home, color, options.border))?;
        refresh_details(store, picker);
        load_home(context, picker, &mut scanner);

        // Wake up to pick up a finished scan or a signal; otherwise block
        // until there is input.
        let timeout = if scanner.is_some() {
            Duration::from_millis(150)
        } else {
            Duration::from_millis(200)
        };
        if !event::poll(timeout)? {
            continue;
        }

        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => match picker.handle_key(key) {
                Outcome::Continue => {}
                Outcome::Cancel => return Ok(None),
                Outcome::Run(action) => {
                    if let Some(selected) = apply(store, context, picker, home, action)? {
                        return Ok(Some(selected));
                    }
                }
            },
            Event::Mouse(mouse) => {
                picker.handle_mouse(mouse);
            }
            Event::Paste(text) => picker.insert_text(&text),
            _ => {}
        }
    }
}

/// Loads the home-directory cache when the picker needs it, and refreshes it
/// in the background when it is older than the configured TTL.
fn load_home(
    context: &Context,
    picker: &mut Picker,
    scanner: &mut Option<mpsc::Receiver<Vec<Dir>>>,
) {
    // A finished background scan wins over anything else.
    if let Some(receiver) = scanner {
        match receiver.try_recv() {
            Ok(dirs) => {
                *scanner = None;
                let dirs = drop_data_dir(dirs, context.data_dir);
                picker.set_home_dirs(dirs.clone());
                let cache = discovery::Cache::new(dirs, Utc::now());
                if let Err(err) =
                    discovery::save_cache(&discovery::cache_path(context.data_dir), &cache)
                {
                    picker.set_status(format!("home cache not saved: {err}"), true);
                } else {
                    picker.set_status("home search updated", false);
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                *scanner = None;
                picker.set_status("home scan failed", true);
            }
        }
        return;
    }

    if !picker.needs_home() {
        return;
    }

    let cache_path = discovery::cache_path(context.data_dir);
    match discovery::load_cache(&cache_path) {
        Some(cache) => {
            let now = Utc::now();
            let stale = cache.is_stale(context.discovery.ttl_hours, now);
            picker.set_home_dirs(drop_data_dir(cache.dirs, context.data_dir));
            if stale {
                start_scan(context, picker, scanner);
            }
        }
        None => {
            // No cache yet: scan in the background, and mark the cache as
            // loaded with an empty list so the scan spawns only once.
            picker.set_home_dirs(Vec::new());
            start_scan(context, picker, scanner);
        }
    }
}

/// Starts a background scan; the result is picked up by [`load_home`].
fn start_scan(
    context: &Context,
    picker: &mut Picker,
    scanner: &mut Option<mpsc::Receiver<Vec<Dir>>>,
) {
    let Some(root) = project::home_dir() else {
        return;
    };
    let options = context.discovery.scan_options();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let dirs = discovery::scan(&root, &options);
        let _ = sender.send(dirs);
    });
    *scanner = Some(receiver);
    picker.set_status("scanning home…", false);
}

/// Never offer overdosecd's own data directory as a place to jump to.
fn drop_data_dir(dirs: Vec<Dir>, data_dir: &Path) -> Vec<Dir> {
    dirs.into_iter()
        .filter(|dir| !dir.path.starts_with(data_dir))
        .collect()
}

/// Fetches the live data the detail pane shows (branch, recent jumps) when
/// the highlighted project changed.
fn refresh_details(store: &impl Store, picker: &mut Picker) {
    let Some(id) = picker.details_request() else {
        return;
    };
    let branch = picker
        .projects()
        .iter()
        .find(|project| project.id == id)
        .and_then(|project| git::current_branch(&project.path).map(git::Branch::label));
    let history = store.recent_uses(&id, 5).unwrap_or_default();
    picker.set_details_info(id, branch, history);
}

/// Executes one run action; `Ok(Some(selected))` means the picker should exit
/// with that selection.
fn apply(
    store: &impl Store,
    context: &Context,
    picker: &mut Picker,
    home: Option<&Path>,
    action: RunAction,
) -> Result<Option<Selected>> {
    match action {
        RunAction::Jump(index) => {
            let path = &picker.projects()[index].path;
            if !path.exists() {
                picker.set_status(
                    format!(
                        "directory no longer exists: {}",
                        output::shorten_home(path, home)
                    ),
                    true,
                );
                return Ok(None);
            }
            Ok(Some(Selected::Project(index)))
        }
        RunAction::JumpHome { path } => {
            if !path.exists() {
                picker.set_status(
                    format!(
                        "directory no longer exists: {}",
                        output::shorten_home(&path, home)
                    ),
                    true,
                );
                return Ok(None);
            }
            Ok(Some(Selected::Home(path)))
        }
        RunAction::Pin { ids, pinned } => {
            pin(store, picker, &ids, pinned);
            Ok(None)
        }
        RunAction::Add { path } => {
            add_project(store, picker, &path);
            Ok(None)
        }
        RunAction::Remove { ids } => {
            remove(store, picker, &ids);
            Ok(None)
        }
        RunAction::Rename { id, name, force } => {
            rename(store, picker, id, name, force);
            Ok(None)
        }
        RunAction::Term {
            kind,
            id,
            value,
            add,
        } => {
            set_term(store, picker, kind, &id, &value, add);
            Ok(None)
        }
        RunAction::Repath { id, path } => {
            repath(store, picker, &id, &path, home);
            Ok(None)
        }
        RunAction::Health => {
            health(context, picker);
            Ok(None)
        }
        RunAction::Migrate => {
            migrate_index(context, picker);
            Ok(None)
        }
    }
}

/// Pins or unpins every id in one update, reporting partial applications.
fn pin(store: &impl Store, picker: &mut Picker, ids: &[String], pinned: bool) {
    let result = store.update(|projects| {
        let mut applied = 0;
        for id in ids {
            if let Some(project) = projects.iter_mut().find(|project| &project.id == id) {
                project.pinned = pinned;
                applied += 1;
            }
        }
        Ok(applied)
    });

    match result {
        Ok(applied) => {
            let verb = if pinned { "pinned" } else { "unpinned" };
            let message = if applied == 1 && ids.len() == 1 {
                format!("{verb} `{}`", name_of(picker, &ids[0]))
            } else if applied == ids.len() {
                format!("{verb} {applied} projects")
            } else {
                format!("{verb} {applied} of {} projects", ids.len())
            };
            picker.set_pinned_ids(ids, pinned);
            let preserve = picker.selected_project().map(|project| project.id.clone());
            picker.refilter(preserve.as_deref());
            picker.set_status(message, false);
        }
        Err(err) => picker.set_status(error_status(&err), true),
    }
}

/// Removes every id in one update.
fn remove(store: &impl Store, picker: &mut Picker, ids: &[String]) {
    let result = store.update(|projects| {
        let mut applied = 0;
        for id in ids {
            if let Some(position) = projects.iter().position(|project| &project.id == id) {
                projects.remove(position);
                applied += 1;
            }
        }
        Ok(applied)
    });

    match result {
        Ok(applied) => {
            let message = if applied == 1 && ids.len() == 1 {
                format!("removed `{}`", name_of(picker, &ids[0]))
            } else if applied == ids.len() {
                format!("removed {applied} projects")
            } else {
                format!("removed {applied} of {} projects", ids.len())
            };
            reload(store, picker, None);
            picker.set_status(message, false);
        }
        Err(err) => picker.set_status(error_status(&err), true),
    }
}

/// Renames one project; a name collision asks for the force confirmation
/// instead of failing.
fn rename(store: &impl Store, picker: &mut Picker, id: String, name: String, force: bool) {
    let name = match project::sanitize_value("project name", &name) {
        Ok(name) => name,
        Err(err) => {
            picker.set_status(error_status(&err), true);
            return;
        }
    };

    let confirmed_id = id.clone();
    let target_name = name.clone();
    let result = store.update(move |projects| {
        let collision = projects
            .iter()
            .find(|project| project.id != id && project.name.eq_ignore_ascii_case(&target_name));
        if let Some(collision) = collision
            && !force
        {
            return Err(Error::NameTaken {
                name: target_name.clone(),
                path: collision.path.clone(),
            });
        }

        let project = project::revalidate(projects, &id, target_name.clone())?;
        let old_name = project.name.clone();
        project.name = target_name.clone();
        Ok(old_name)
    });

    match result {
        Ok(old_name) => {
            reload(store, picker, Some(&confirmed_id));
            picker.set_status(format!("renamed `{old_name}` -> `{name}`"), false);
        }
        Err(Error::NameTaken { name, .. }) => picker.ask_rename_force(confirmed_id, name),
        Err(err) => picker.set_status(error_status(&err), true),
    }
}

/// Adds or removes an alias / tag on one project.
fn set_term(
    store: &impl Store,
    picker: &mut Picker,
    kind: TermKind,
    id: &str,
    value: &str,
    add: bool,
) {
    let value = match project::sanitize_value(kind.label(), value) {
        Ok(value) => value,
        Err(err) => {
            picker.set_status(error_status(&err), true);
            return;
        }
    };

    let label = id.to_owned();
    let id = label.clone();
    let result = store.update(move |projects| {
        let project = project::revalidate(projects, &id, label)?;
        project::set_term(project, kind, &value, add)
    });

    match result {
        Ok(message) => {
            picker.set_status(message, false);
            reload(store, picker, None);
        }
        Err(err) => picker.set_status(error_status(&err), true),
    }
}

/// Moves a stale project to a validated new path.
fn repath(store: &impl Store, picker: &mut Picker, id: &str, raw: &str, home: Option<&Path>) {
    let projects = picker.projects().to_vec();
    let path = match doctor::validate_target(&projects, id, raw, home) {
        Ok(path) => path,
        Err(err) => {
            picker.set_status(error_status(&err), true);
            return;
        }
    };

    let target = Repair::Relocate {
        id: id.to_owned(),
        path: path.clone(),
    };
    let result = store.update(move |projects| Ok(doctor::apply_repairs(projects, &[target])));

    match result {
        Ok(1) => {
            picker.set_status(
                format!("relocated to {}", output::shorten_home(&path, home)),
                false,
            );
            reload(store, picker, Some(id));
        }
        Ok(_) => picker.set_status("that project is no longer indexed", true),
        Err(err) => picker.set_status(error_status(&err), true),
    }
}

/// Runs the read-only doctor checks and shows them as a list.
fn health(context: &Context, picker: &mut Picker) {
    let home = project::home_dir();
    let issues = doctor::inspect(context.data_dir, context.backend, picker.projects());
    let rows: Vec<IssueRow> = issues
        .issues
        .iter()
        .map(|issue| issue_row(issue, picker, home.as_deref()))
        .collect();

    if rows.is_empty() {
        picker.set_status("no problems found", false);
        return;
    }
    picker.set_issues(rows);
}

/// One compact line per issue; stale projects keep their id so the health
/// view can relocate or remove them.
fn issue_row(issue: &Issue, picker: &Picker, home: Option<&Path>) -> IssueRow {
    let informational = |label: String| IssueRow { label, stale: None };

    match issue {
        Issue::StalePath { name, path } => {
            let id = picker
                .projects()
                .iter()
                .find(|project| &project.path == path)
                .map(|project| project.id.clone());
            IssueRow {
                label: format!(
                    "stale `{}` -> {}",
                    sanitize::text(name),
                    output::shorten_home(path, home)
                ),
                stale: id.map(|id| StaleRef {
                    id,
                    path: path.clone(),
                }),
            }
        }
        Issue::DuplicateName { name, count } => informational(format!(
            "{count} projects share the name `{}`",
            sanitize::text(name)
        )),
        Issue::QuarantineFile(path) => informational(format!(
            "quarantined index file: {}",
            output::shorten_home(path, home)
        )),
        Issue::StrayFile(path) => informational(format!(
            "leftover SQLite file: {}",
            output::shorten_home(path, home)
        )),
        Issue::BackendDisagreement {
            json,
            db,
            differing,
        } => informational(format!(
            "projects.json and projects.db disagree: {differing} differing ids ({json} vs {db})"
        )),
        Issue::InactiveIndexUnreadable { path, message } => informational(format!(
            "cannot read {}: {}",
            output::shorten_home(path, home),
            sanitize::text(message)
        )),
        Issue::IndexNotWritable(path) => informational(format!(
            "the index is not writable: {}",
            output::shorten_home(path, home)
        )),
        Issue::IndexPermissions { path, mode } => informational(format!(
            "index mode is {mode:o}, expected 600: {}",
            output::shorten_home(path, home)
        )),
        Issue::CachePermissions { path, mode } => informational(format!(
            "home cache mode is {mode:o}, expected 600: {}",
            output::shorten_home(path, home)
        )),
        Issue::DataDirNotWritable(path) => informational(format!(
            "the data directory is not writable: {}",
            output::shorten_home(path, home)
        )),
        Issue::DataDirPermissions { path, mode } => informational(format!(
            "data directory mode is {mode:o}, expected 700: {}",
            output::shorten_home(path, home)
        )),
        Issue::ControlInValue {
            project,
            field,
            value,
        } => informational(format!(
            "unsafe characters in {project:?} ({field}): {value:?} — rename or repath it"
        )),
        Issue::ConfigProblem { path, message } => informational(format!(
            "config problem in {}: {}",
            output::shorten_home(path, home),
            sanitize::text(message)
        )),
        Issue::SymlinkedFile(path) => informational(format!(
            "symbolic link where a store file belongs: {} (move it aside)",
            output::shorten_home(path, home)
        )),
    }
}

/// Runs `overdosecd migrate` from the palette.
fn migrate_index(context: &Context, picker: &mut Picker) {
    match migrate::run(context.data_dir) {
        Ok(migrate::Outcome::Migrated { projects, backup }) => {
            let backup_name = backup
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| backup.display().to_string());
            picker.set_status(
                format!(
                    "migrated {projects} projects to projects.db (kept as {backup_name}); \
                     set backend = \"sqlite\" and restart"
                ),
                false,
            );
        }
        Ok(migrate::Outcome::AlreadyMigrated { db }) => picker.set_status(
            format!("already migrated: {} is present", db.display()),
            false,
        ),
        Err(err) => picker.set_status(error_status(&err), true),
    }
}

/// Replaces the in-memory index from the store.
fn reload(store: &impl Store, picker: &mut Picker, preserve: Option<&str>) {
    match store.load() {
        Ok(projects) => picker.set_projects(projects, preserve),
        Err(err) => picker.set_status(error_status(&err), true),
    }
}

fn name_of(picker: &Picker, id: &str) -> String {
    picker
        .projects()
        .iter()
        .find(|project| project.id == id)
        .map(|project| project.name.clone())
        .unwrap_or_else(|| id.to_owned())
}

/// Opens the controlling terminal for rendering. Stdout is deliberately not
/// used: the shell captures it with `$(...)`, so only the chosen path may be
/// written there.
fn open_terminal() -> Result<File> {
    OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .map_err(|_| Error::NoTerminal)
}

/// Opens the controlling terminal for the cursor query, which has to read the
/// terminal's answer back.
fn open_terminal_read_write() -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|_| Error::NoTerminal)
}

/// Whether the picker draws with color. `auto` means "a terminal is
/// available" (it is; the picker has one) unless `NO_COLOR` is set — unlike
/// `list`, stdout being piped must not turn the UI monochrome.
fn colors_enabled(mode: ColorMode) -> bool {
    match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        // The empty string counts as unset, per the NO_COLOR convention.
        ColorMode::Auto => std::env::var_os("NO_COLOR").is_none_or(|value| value.is_empty()),
    }
}

/// `true` when the process runs inside a multiplexer that keeps the mouse for
/// its own pane UI (tmux, zellij, screen).
fn in_multiplexer() -> bool {
    in_multiplexer_with(|name| std::env::var(name).ok())
}

/// Testable variant of [`in_multiplexer`] with an injected environment.
fn in_multiplexer_with(lookup: impl Fn(&str) -> Option<String>) -> bool {
    MULTIPLEXER_VARS
        .iter()
        .any(|name| lookup(name).is_some_and(|value| !value.is_empty()))
}

/// The inline box height: the configured value, but never so tall that the
/// box hides the whole terminal (at least one row of context stays), and
/// never zero.
fn viewport_height(configured: u16, rows: u16) -> u16 {
    if rows <= 3 {
        return rows.max(1);
    }
    configured.clamp(3, rows - 1)
}

/// Adds `input` through the store and shows the result. On failure the add
/// prompt stays open so the path can be fixed.
fn add_project(store: &impl Store, picker: &mut Picker, input: &str) {
    let (message, name) = match add_path(store, input) {
        Ok(added) => added,
        Err(err) => {
            picker.set_status(error_status(&err), true);
            return;
        }
    };

    match store.load() {
        Ok(projects) => {
            picker.set_projects(projects, None);
            picker.set_query(sanitize::text(&name));
            picker.set_mode(picker::Mode::Search);
            picker.set_status(message, false);
        }
        Err(err) => picker.set_status(error_status(&err), true),
    }
}

/// Expands `input` (leading `~` included), checks that it is an existing
/// directory, and inserts it; returns the message and the project name.
fn add_path(store: &impl Store, input: &str) -> Result<(String, String)> {
    let input = input.trim();
    let expanded = if input == "~" || input.starts_with('~') {
        project::expand_tilde(input)
    } else {
        PathBuf::from(input)
    };

    if !expanded.exists() {
        return Err(Error::MissingDirectory(expanded));
    }
    if !expanded.is_dir() {
        return Err(Error::NotADirectory(expanded));
    }

    let canonical = project::normalize(&expanded)?;
    project::reject_control_chars_path(&canonical)?;
    let name = project::default_name(&canonical)?;

    let message = store.update(move |projects| {
        project::insert(projects, canonical, None, Vec::new(), Vec::new(), false)
    })?;
    Ok((message, name))
}

/// A single-line, picker-friendly rendering of an error (the status line has
/// no room for multi-line messages).
fn error_status(err: &Error) -> String {
    match err {
        Error::Duplicate { name, .. } => format!("already indexed as `{name}`"),
        other => other
            .to_string()
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned(),
    }
}

/// Owns the terminal for the picker: raw mode, bracketed paste, optional
/// mouse capture, and the inline viewport.
///
/// `Drop` restores everything on every exit path, including a panic
/// mid-render: the box is erased from its top row down (nothing above it is
/// touched, so the shell's next prompt lands where the box was), raw mode and
/// the capture modes are left, and the cursor is shown.
struct Session {
    terminal: Terminal<backend::TtyBackend>,
    output: File,
}

impl Session {
    fn enter(options: &Options) -> Result<Self> {
        // Until the session exists, this guard undoes any partial setup.
        let mut pending = PendingRestore {
            output: Some(open_terminal()?),
        };

        enable_raw_mode()?;
        execute!(pending.output(), EnableBracketedPaste)?;
        if options.mouse {
            execute!(pending.output(), EnableMouseCapture)?;
        }

        let rows = ratatui::crossterm::terminal::size().map_or(24, |(_, rows)| rows);
        let backend = backend::TtyBackend::new(open_terminal()?, open_terminal_read_write()?);
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(viewport_height(options.height, rows)),
            },
        )?;

        Ok(Self {
            terminal,
            output: pending.output.take().expect("still armed"),
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Deliberately not `Terminal::clear()`: ratatui 0.30 snapshots the
        // cursor position, clears the viewport, and then moves the cursor back
        // to where the last frame left it - inside the erased box - so the
        // shell's next prompt lands a box-height below the `ocd` line. Erase
        // from the viewport origin downwards and leave the cursor there, which
        // is both what the old `clear()` contract promised and what the shell
        // needs. It also skips `Terminal::clear`'s blocking cursor query, so
        // the box is erased even on terminals that never answer `ESC [ 6 n`.
        let area = self.terminal.get_frame().area();
        let backend = self.terminal.backend_mut();
        let _ = backend.set_cursor_position(area.as_position());
        let _ = backend.clear_region(ClearType::AfterCursor);
        let _ = disable_raw_mode();
        let _ = execute!(
            &mut self.output,
            DisableBracketedPaste,
            DisableMouseCapture,
            cursor::Show
        );
    }
}

/// Undoes terminal setup when [`Session::enter`] fails before the session
/// owns the cleanup.
struct PendingRestore {
    output: Option<File>,
}

impl PendingRestore {
    fn output(&mut self) -> &mut File {
        self.output.as_mut().expect("pending restore is armed")
    }
}

impl Drop for PendingRestore {
    fn drop(&mut self) {
        if let Some(output) = &mut self.output {
            let _ = disable_raw_mode();
            let _ = execute!(
                output,
                DisableBracketedPaste,
                DisableMouseCapture,
                cursor::Show
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use chrono::DateTime;

    use super::*;
    use crate::project::Project;
    use crate::ui::picker::{Mode, Picker};

    /// In-memory [`Store`] for exercising the picker's mutations without a
    /// data directory.
    struct FakeStore {
        projects: RefCell<Vec<Project>>,
    }

    impl FakeStore {
        fn new(projects: Vec<Project>) -> Self {
            Self {
                projects: RefCell::new(projects),
            }
        }
    }

    impl Store for FakeStore {
        fn load(&self) -> Result<Vec<Project>> {
            Ok(self.projects.borrow().clone())
        }

        fn update<R>(&self, f: impl FnOnce(&mut Vec<Project>) -> Result<R>) -> Result<R> {
            f(&mut self.projects.borrow_mut())
        }

        fn record_use(&self, _id: &str, _now: DateTime<Utc>) -> Result<()> {
            Ok(())
        }

        fn recent_uses(&self, _id: &str, _limit: usize) -> Result<Vec<DateTime<Utc>>> {
            Ok(Vec::new())
        }

        fn schema_version(&self) -> Result<Option<u32>> {
            Ok(None)
        }
    }

    fn project(name: &str, path: &str) -> Project {
        let mut project = Project::for_test(name, path);
        project.id = name.to_owned();
        project
    }

    fn picker_with(projects: Vec<Project>) -> Picker {
        Picker::new(projects, MatcherConfig::DEFAULT)
    }

    #[test]
    fn colors_follow_the_flag() {
        assert!(colors_enabled(ColorMode::Always));
        assert!(!colors_enabled(ColorMode::Never));
    }

    #[test]
    fn multiplexers_disable_mouse_auto() {
        assert!(!in_multiplexer_with(|_| None));
        assert!(in_multiplexer_with(|name| {
            (name == "TMUX").then(|| "/tmp/tmux-1000/default,1,0".to_owned())
        }));
        assert!(
            !in_multiplexer_with(|name| (name == "ZELLIJ").then(String::new)),
            "an empty value counts as unset"
        );
    }

    #[test]
    fn mouse_option_resolves_explicit_modes() {
        let always = Ui {
            mouse: MouseMode::Always,
            ..Ui::default()
        };
        assert!(Options::resolve(&always, None, false).mouse);

        let never = Ui {
            mouse: MouseMode::Never,
            ..Ui::default()
        };
        assert!(!Options::resolve(&never, None, false).mouse);
    }

    #[test]
    fn viewport_height_leaves_a_row_of_context() {
        assert_eq!(viewport_height(10, 24), 10);
        assert_eq!(viewport_height(100, 24), 23);
        assert_eq!(viewport_height(10, 4), 3);
        assert_eq!(viewport_height(10, 2), 2);
        assert_eq!(viewport_height(0, 0), 1);
    }

    #[test]
    fn duplicate_errors_render_on_one_line() {
        let err = Error::Duplicate {
            name: "alpha".into(),
            path: PathBuf::from("/x/alpha"),
        };
        assert_eq!(error_status(&err), "already indexed as `alpha`");
        assert!(!error_status(&Error::NoTerminal).contains('\n'));
    }

    #[test]
    fn add_path_inserts_a_real_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("alpha");
        std::fs::create_dir_all(&target).expect("create the target directory");
        let store = FakeStore::new(Vec::new());

        let (message, name) =
            add_path(&store, target.to_str().expect("utf-8 temp path")).expect("add");
        assert_eq!(name, "alpha");
        assert!(
            message.starts_with("added `alpha`"),
            "message was: {message}"
        );
        assert_eq!(store.load().expect("load").len(), 1);
    }

    #[test]
    fn add_path_rejects_missing_and_duplicate_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FakeStore::new(Vec::new());

        let missing = dir.path().join("missing");
        let err = add_path(&store, missing.to_str().unwrap()).expect_err("missing path");
        assert!(matches!(err, Error::MissingDirectory(_)));

        let file = dir.path().join("a-file");
        std::fs::write(&file, "not a directory").expect("write file");
        let err = add_path(&store, file.to_str().unwrap()).expect_err("a file is not a directory");
        assert!(matches!(err, Error::NotADirectory(_)));

        let target = dir.path().join("alpha");
        std::fs::create_dir_all(&target).expect("create the target directory");
        add_path(&store, target.to_str().unwrap()).expect("first add");
        let err = add_path(&store, target.to_str().unwrap()).expect_err("duplicate add");
        assert!(matches!(err, Error::Duplicate { .. }));
        assert_eq!(error_status(&err), "already indexed as `alpha`");
    }

    #[test]
    fn pin_updates_the_store_and_follows_the_row() {
        let store = FakeStore::new(vec![
            project("alpha", "/x/alpha"),
            project("beta", "/x/beta"),
        ]);
        let mut picker = picker_with(store.load().unwrap());
        assert_eq!(picker.selected_project().unwrap().name, "alpha");

        pin(&store, &mut picker, &["alpha".to_owned()], true);
        assert!(store.load().expect("load")[0].pinned);
        assert_eq!(
            picker.selected_project().expect("selection").name,
            "alpha",
            "the pinned row stays highlighted"
        );
        let status = picker.status().expect("status");
        assert_eq!(status.text, "pinned `alpha`");
        assert!(!status.error);

        pin(&store, &mut picker, &["alpha".to_owned()], false);
        assert!(!store.load().expect("load")[0].pinned);
        assert_eq!(picker.status().expect("status").text, "unpinned `alpha`");
    }

    #[test]
    fn pin_reports_partial_batches() {
        let store = FakeStore::new(vec![
            project("alpha", "/x/alpha"),
            project("beta", "/x/beta"),
        ]);
        let mut picker = picker_with(store.load().unwrap());

        pin(
            &store,
            &mut picker,
            &["alpha".to_owned(), "gone".to_owned()],
            true,
        );
        assert_eq!(
            picker.status().expect("status").text,
            "pinned 1 of 2 projects"
        );
    }

    #[test]
    fn remove_deletes_the_ids_and_reloads() {
        let store = FakeStore::new(vec![
            project("alpha", "/x/alpha"),
            project("beta", "/x/beta"),
            project("gamma", "/x/gamma"),
        ]);
        let mut picker = picker_with(store.load().unwrap());

        remove(
            &store,
            &mut picker,
            &["alpha".to_owned(), "gamma".to_owned()],
        );
        assert_eq!(store.load().expect("load").len(), 1);
        assert_eq!(picker.projects().len(), 1);
        assert_eq!(picker.status().expect("status").text, "removed 2 projects");
    }

    #[test]
    fn rename_reports_a_collision_and_force_renames() {
        let store = FakeStore::new(vec![
            project("alpha", "/x/alpha"),
            project("beta", "/x/beta"),
        ]);
        let mut picker = picker_with(store.load().unwrap());

        rename(
            &store,
            &mut picker,
            "alpha".to_owned(),
            "beta".to_owned(),
            false,
        );
        let confirm = picker.confirm().expect("collision asks for confirmation");
        assert!(confirm.message.contains("another project"));
        assert_eq!(store.load().expect("load")[0].name, "alpha");

        rename(
            &store,
            &mut picker,
            "alpha".to_owned(),
            "beta".to_owned(),
            true,
        );
        assert_eq!(store.load().expect("load")[0].name, "beta");
        assert_eq!(
            picker.status().expect("status").text,
            "renamed `alpha` -> `beta`"
        );
    }

    #[test]
    fn terms_are_added_and_removed() {
        let store = FakeStore::new(vec![project("alpha", "/x/alpha")]);
        let mut picker = picker_with(store.load().unwrap());

        set_term(&store, &mut picker, TermKind::Alias, "alpha", "a", true);
        assert_eq!(
            picker.status().expect("status").text,
            "added alias `a` to `alpha`"
        );
        assert_eq!(store.load().expect("load")[0].aliases, vec!["a".to_owned()]);

        set_term(&store, &mut picker, TermKind::Alias, "alpha", "a", true);
        assert!(
            picker
                .status()
                .expect("status")
                .text
                .contains("is already set")
        );

        set_term(&store, &mut picker, TermKind::Tag, "alpha", "rust", true);
        set_term(&store, &mut picker, TermKind::Tag, "alpha", "rust", false);
        assert!(store.load().expect("load")[0].tags.is_empty());
        assert_eq!(
            picker.status().expect("status").text,
            "removed tag `rust` from `alpha`"
        );

        set_term(&store, &mut picker, TermKind::Alias, "alpha", "gone", false);
        let status = picker.status().expect("status");
        assert!(status.error);
        assert!(status.text.contains("is not set"), "{}", status.text);
    }

    #[test]
    fn repath_relocates_a_stale_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("moved");
        std::fs::create_dir_all(&target).expect("create the target directory");
        let store = FakeStore::new(vec![project("alpha", "/x/gone")]);
        let mut picker = picker_with(store.load().unwrap());

        repath(&store, &mut picker, "alpha", target.to_str().unwrap(), None);
        let saved = store.load().expect("load");
        assert_eq!(
            saved[0].path,
            project::normalize(&target).expect("canonical")
        );
        assert!(
            picker
                .status()
                .expect("status")
                .text
                .starts_with("relocated"),
            "{:?}",
            picker.status().map(|status| status.text.clone())
        );
    }

    #[test]
    fn repath_refuses_a_taken_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let taken = dir.path().join("taken");
        std::fs::create_dir_all(&taken).expect("create the target directory");
        let canonical = project::normalize(&taken).expect("canonical");
        let store = FakeStore::new(vec![
            project("alpha", "/x/gone"),
            project("beta", canonical.to_str().unwrap()),
        ]);
        let mut picker = picker_with(store.load().unwrap());

        repath(&store, &mut picker, "alpha", taken.to_str().unwrap(), None);
        let status = picker.status().expect("status");
        assert!(status.error);
        assert!(
            status.text.contains("already uses this path"),
            "{}",
            status.text
        );
    }

    #[test]
    fn health_lists_issues_and_spawns_repath_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
                .expect("chmod 0700");
        }
        let store = FakeStore::new(vec![
            project("alpha", "/x/gone"),
            project("beta", dir.path().to_str().unwrap()),
        ]);
        let mut picker = picker_with(store.load().unwrap());
        let discovery = crate::config::Discovery::default();
        let context = Context {
            data_dir: dir.path(),
            backend: StorageBackend::Json,
            discovery: &discovery,
        };

        health(&context, &mut picker);
        assert_eq!(picker.mode(), Mode::Health);
        let rows = picker.issues();
        assert_eq!(rows.len(), 1, "one stale project: {rows:?}");
        let stale = rows[0].stale.as_ref().expect("stale row");
        assert_eq!(stale.id, "alpha");
    }

    #[test]
    fn add_project_clears_the_prompt_and_shows_the_new_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("alpha");
        std::fs::create_dir_all(&target).expect("create the target directory");
        let store = FakeStore::new(Vec::new());
        let mut picker = picker_with(Vec::new());
        picker.enter_add_with(Some(PathBuf::from("/somewhere")));

        add_project(&store, &mut picker, target.to_str().unwrap());
        assert_eq!(picker.mode(), Mode::Search);
        assert_eq!(picker.query(), "alpha");
        assert_eq!(picker.entries().len(), 1);
        let status = picker.status().expect("status");
        assert!(status.text.starts_with("added `alpha`"), "{}", status.text);
        assert!(!status.error);
    }

    #[test]
    fn a_failed_add_keeps_the_prompt_open() {
        let store = FakeStore::new(Vec::new());
        let mut picker = picker_with(Vec::new());
        picker.enter_add_with(Some(PathBuf::from("/somewhere")));

        add_project(&store, &mut picker, "/definitely/not/a/real/path");
        assert_eq!(picker.mode(), Mode::Prompt);
        let status = picker.status().expect("status");
        assert!(status.error);
        assert!(status.text.contains("does not exist"), "{}", status.text);
    }
}
