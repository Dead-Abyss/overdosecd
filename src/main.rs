mod cli;
mod config;
mod dirs;
mod discovery;
mod doctor;
mod error;
mod git;
mod hook;
mod import;
mod matcher;
mod migrate;
mod output;
mod paths;
mod project;
mod sanitize;
mod store;
mod time;
mod ui;

// Linux only: turn an unsupported target into one clear error instead of a
// pile of cfg fallout.
#[cfg(not(target_os = "linux"))]
compile_error!("overdosecd only supports Linux (see README.md)");

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use clap::Parser;
use colored::Colorize;

use crate::cli::{
    AddArgs, AliasCommand, Cli, Command, DoctorArgs, ImportArgs, ListArgs, RemoveArgs, ScanArgs,
    SortBy, TagCommand,
};
use crate::config::{ColorMode, StorageBackend};
use crate::error::{Error, Result};
use crate::matcher::MatcherConfig;
use crate::project::{Project, TermKind};
use crate::store::json::{JsonStore, StoreFile};
use crate::store::sqlite::SqliteStore;
use crate::store::{AnyStore, Store};

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        // Piping into `head` and friends is not an error worth reporting.
        Err(Error::Io(err)) if err.kind() == io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(err) => {
            let code = err.exit_code();
            // The error's Display already escapes every value; the funnel is a
            // second net for a renderer that slipped.
            if output::print_stderr_line(&format!("error: {err}")).is_err() {
                let _ = output::print_stderr_line("error: refusing to print an unsafe message");
            }
            std::process::exit(code);
        }
    }
}

/// Runs the command and returns the process exit code on success.
///
/// Only `doctor` reports anything but `0`; everything else either succeeds or
/// fails with an [`Error`].
fn run(cli: Cli) -> Result<i32> {
    let data_dir = paths::resolve_data_dir(cli.data_dir.as_deref())?;

    let location = config::locate(config::env_path(), config::platform_path());
    let config = config::load(&location);

    // `doctor` and tab completion run before the strict config load, so a
    // broken config falls back to the default backend for them instead of
    // failing. Every other command stops at `config?` below anyway.
    let backend = config.as_ref().map_or_else(
        |_| StorageBackend::default(),
        |config| config.storage.backend,
    );
    let store = match backend {
        StorageBackend::Json => AnyStore::Json(JsonStore::new(paths::store_file(&data_dir))),
        StorageBackend::Sqlite => AnyStore::Sqlite(SqliteStore::new(paths::sqlite_file(&data_dir))),
    };

    // `doctor` reports an unreadable config instead of failing on it; every
    // other command stops at `config?` below. Either way the color mode follows
    // flag > $OVERDOSECD_COLOR > config file > auto.
    let configured = config
        .as_ref()
        .map_or(ColorMode::Auto, |config| config.general.color);
    let color_mode = config::resolve_color(cli.color, config::env_color().as_deref(), configured)?;
    config::apply_color(color_mode);

    // `init` and the cd hook only need the hook switches, so they read the
    // config leniently like `doctor`: a broken file falls back to defaults
    // (hook and hint off) instead of failing shell setup or a `cd`.
    let hooks = config
        .as_ref()
        .map_or(output::HookSettings::default(), |config| {
            output::HookSettings {
                record: config.general.hook,
                hint: config.general.hint,
            }
        });

    match cli.command {
        Command::Doctor(args) => cmd_doctor(
            &store,
            &data_dir,
            backend,
            args,
            config::status(&location, config),
        ),
        // Completion runs on every TAB press and uses no matcher knobs, so it
        // deliberately does not fail on a broken config.
        Command::Complete { prefix } => cmd_complete(&store, &prefix).map(|()| 0),
        // The hook runs on every `cd`; it is silent on success and always
        // exits 0 so it can never break the shell's cd.
        Command::Hook => {
            cmd_hook(&store, &data_dir, hooks);
            Ok(0)
        }
        Command::Init { shell } => {
            output::print(&output::init_script(shell, hooks))?;
            Ok(0)
        }
        command => {
            let config = config?;
            let matching = MatcherConfig::from(&config.matching);
            let debug = cli.debug;

            match command {
                Command::Add(args) => cmd_add(&store, args)?,
                Command::List(args) => cmd_list(&store, args)?,
                Command::Goto {
                    query,
                    confident,
                    no_track,
                } => cmd_goto(
                    &store,
                    &query.join(" "),
                    confident,
                    no_track,
                    &matching,
                    debug,
                )?,
                Command::Ui(args) => {
                    let options = ui::Options::resolve(&config.ui, args.query, args.no_track);
                    let context = ui::Context {
                        data_dir: &data_dir,
                        backend,
                        discovery: &config.discovery,
                    };
                    return ui::run(&store, &context, &matching, color_mode, &options);
                }
                Command::Info { query } => cmd_info(&store, &query.join(" "), &matching, debug)?,
                Command::Remove(args) => cmd_remove(&store, args, &matching, debug)?,
                Command::Pin { query } => {
                    cmd_pin(&store, &query.join(" "), true, &matching, debug)?
                }
                Command::Unpin { query } => {
                    cmd_pin(&store, &query.join(" "), false, &matching, debug)?
                }
                Command::Rename { old, new, force } => {
                    cmd_rename(&store, &old, &new, force, &matching, debug)?
                }
                Command::Alias { action } => cmd_alias(&store, action, &matching, debug)?,
                Command::Tag { action } => cmd_tag(&store, action, &matching, debug)?,
                Command::Migrate => cmd_migrate(&data_dir, backend, &location)?,
                Command::Import(args) => cmd_import(&store, args)?,
                Command::Scan(args) => cmd_scan(&data_dir, &config.discovery, args)?,
                Command::Completions { shell } => output::print(&output::completion_script(shell))?,
                // Handled in the outer match, before the config is required.
                Command::Doctor(_) => unreachable!("doctor is dispatched before the config load"),
                Command::Complete { .. } => {
                    unreachable!("complete is dispatched before the config load")
                }
                Command::Hook => unreachable!("hook is dispatched before the config load"),
                Command::Init { .. } => unreachable!("init is dispatched before the config load"),
            }

            Ok(0)
        }
    }
}

fn cmd_add(store: &impl Store, args: AddArgs) -> Result<()> {
    let input = match args.path {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    let input = expand_input(&input);

    if !input.exists() {
        return Err(Error::MissingDirectory(input));
    }
    if !input.is_dir() {
        return Err(Error::NotADirectory(input));
    }
    let canonical = project::normalize(&input)?;
    project::reject_control_chars_path(&canonical)?;

    let provided_name = match args.name {
        Some(name) => Some(project::sanitize_value("project name", &name)?),
        None => None,
    };

    let mut aliases = Vec::new();
    for alias in &args.alias {
        project::reject_control_chars("alias", alias)?;
        project::push_unique(&mut aliases, alias);
    }
    let mut tags = Vec::new();
    for tag in &args.tag {
        project::reject_control_chars("tag", tag)?;
        project::push_unique(&mut tags, tag);
    }
    let force = args.force;

    let message = store.update(move |projects| {
        project::insert(projects, canonical, provided_name, aliases, tags, force)
    })?;
    output::print_line(&message)
}

fn cmd_list(store: &impl Store, args: ListArgs) -> Result<()> {
    // Fail fast on a bad window, before touching the index.
    let since = args
        .since
        .as_deref()
        .map(|raw| time::parse_since(raw, Utc::now()))
        .transpose()?;

    let mut projects = store.load()?;
    if let Some(cutoff) = since {
        projects.retain(|project| project.last_used_at.is_some_and(|when| when >= cutoff));
    }
    sort_projects(&mut projects, args.sort);

    // One existence check per project per run: the flags decide both the
    // default hiding (missing directories are dropped) and the `!` markers.
    let missing: Vec<bool> = projects
        .iter()
        .map(|project| !project.path.exists())
        .collect();

    let mut visible: Vec<Project> = Vec::with_capacity(projects.len());
    let mut visible_missing: Vec<bool> = Vec::with_capacity(projects.len());
    for (project, missing) in projects.into_iter().zip(missing) {
        if args.all || !missing {
            visible.push(project);
            visible_missing.push(missing);
        }
    }

    if args.json {
        let file = StoreFile::new(visible);
        output::print_line(&serde_json::to_string_pretty(&file)?)
    } else if visible.is_empty() {
        if let Some(raw) = &args.since {
            output::print_line(&format!("no projects jumped to since {raw}"))
        } else {
            output::print_line("no projects yet. run `overdosecd add <path>` to get started.")
        }
    } else {
        output::print(&output::format_projects_with_missing(
            &visible,
            &visible_missing,
            project::home_dir().as_deref(),
            output::terminal_width(),
        ))
    }
}

/// Resolves a query to its best match. Under `--debug`, every ranked
/// candidate is printed to stderr first, so stdout stays untouched.
fn resolve(
    projects: &[Project],
    query: &str,
    matching: &MatcherConfig,
    debug: bool,
) -> Result<matcher::Match> {
    if debug {
        let ranked = matcher::rank(query, projects, matching);
        output::print_stderr(&output::format_debug(
            query,
            &ranked,
            project::home_dir().as_deref(),
        ))?;
    }
    matcher::best(query, projects, matching)
}

/// Prints completion candidates, one per line; used by the shell completion
/// functions so `ocd <TAB>` offers the same values a jump would accept.
fn cmd_complete(store: &impl Store, prefix: &str) -> Result<()> {
    let projects = store.load()?;
    for candidate in matcher::completion_candidates(&projects, prefix) {
        output::print_line(&candidate)?;
    }
    Ok(())
}

/// The optional `cd` hook. Silent on success, at most one stderr line, and it
/// returns nothing so the caller always exits 0: it must never change what
/// the user's `cd` does.
fn cmd_hook(store: &impl Store, data_dir: &Path, hooks: output::HookSettings) {
    // A wrapper can linger in a shell after the config was turned off again.
    if !hooks.any() {
        return;
    }
    let warn = |message: String| {
        let _ = output::print_stderr_line(&format!("overdosecd: {message}"));
    };

    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let Ok(current) = project::normalize(&cwd) else {
        return;
    };
    let projects = match store.load() {
        Ok(projects) => projects,
        Err(err) => {
            warn(format!("cd hook could not read the index: {err}"));
            return;
        }
    };

    if let Some(project) = projects.iter().find(|project| project.path == current) {
        if hooks.record
            && let Err(err) = store.record_use(&project.id, Utc::now())
        {
            warn(format!("cd hook could not record the jump: {err}"));
        }
        return;
    }

    if !hooks.hint {
        return;
    }
    let path = hook::file_path(data_dir);
    let mut visits = hook::Visits::load(&path).unwrap_or_default();
    let now = Utc::now();
    let count = visits.record_visit(&current, now);
    if visits.hint_due(&current, now) {
        visits.mark_hinted(&current, now);
        let name = project::default_name(&current)
            .map(|name| sanitize::text(&name))
            .unwrap_or_else(|_| sanitize::path(&current));
        let shown = output::shorten_home(&current, project::home_dir().as_deref());
        warn(format!(
            "`{name}` has {count} visits and is not indexed — add it with `ocd add \"{shown}\"`"
        ));
    }
    visits.prune(now);
    if let Err(err) = visits.save(&path) {
        warn(format!("cd hook could not save visit counters: {err}"));
    }
}

fn cmd_goto(
    store: &impl Store,
    query: &str,
    confident: bool,
    no_track: bool,
    matching: &MatcherConfig,
    debug: bool,
) -> Result<()> {
    let projects = store.load()?;
    let found = resolve(&projects, query, matching, debug)?;
    if confident && !found.all_confident() {
        // The caller decides what to do; `ocd` opens the picker prefilled.
        return Err(Error::WeakMatch {
            query: query.trim().to_owned(),
            name: found.name.clone(),
            reason: found.reason.label(),
        });
    }
    if !found.path.exists() {
        return Err(Error::StaleDirectory {
            name: found.name,
            path: found.path,
        });
    }

    // The jump itself must never fail because of bookkeeping: print the path
    // first, then record usage and degrade lock or save failures to a warning.
    // The printed line is captured by `cd "$(overdosecd goto …)"`, so a path a
    // terminal would mangle is refused rather than escaped: an escaped path
    // would be a wrong path.
    if sanitize::contains_dangerous(&found.path.to_string_lossy()) {
        return Err(Error::UnsafePath(sanitize::path(&found.path)));
    }
    let id = projects[found.index].id.clone();
    output::print_line(&found.path.display().to_string())?;

    if !no_track {
        let now = Utc::now();
        if let Err(err) = store.record_use(&id, now) {
            let _ = output::print_stderr_line(&format!("warning: could not record usage: {err}"));
        }
    }

    Ok(())
}

fn cmd_info(store: &impl Store, query: &str, matching: &MatcherConfig, debug: bool) -> Result<()> {
    let projects = store.load()?;
    let found = resolve(&projects, query, matching, debug)?;
    let project = &projects[found.index];
    let dash = "-".to_owned();

    let created = project.created_at.format("%Y-%m-%d %H:%M UTC");
    let last_used = project.last_used_at.map_or_else(
        || "never".to_owned(),
        |when| {
            format!(
                "{} ({})",
                output::relative_time(when, Utc::now()),
                when.format("%Y-%m-%d %H:%M UTC")
            )
        },
    );

    // One table: the rows keep their order and wording, and a new field is a
    // new entry instead of another `print_line`.
    let mut rows: Vec<(&str, String)> = vec![
        ("name:", sanitize::text(&project.name)),
        ("id:", sanitize::text(&project.id)),
        ("path:", sanitize::path(&project.path)),
        (
            "aliases:",
            if project.aliases.is_empty() {
                dash.clone()
            } else {
                sanitize::text(&project.aliases.join(", "))
            },
        ),
        (
            "tags:",
            if project.tags.is_empty() {
                dash.clone()
            } else {
                sanitize::text(&project.tags.join(", "))
            },
        ),
        ("created:", created.to_string()),
        ("last used:", last_used),
        ("uses:", project.use_count.to_string()),
    ];
    if let Some(history) = format_history(store, &project.id)? {
        rows.push(("history:", history));
    }
    rows.push((
        "pinned:",
        if project.pinned {
            "yes".yellow().to_string()
        } else {
            "no".to_owned()
        },
    ));
    if git::is_repo(&project.path) {
        rows.push(("git:", "yes".green().to_string()));
        let branch = git::current_branch(&project.path)
            .map(git::Branch::label)
            .unwrap_or_else(|| dash.clone());
        rows.push(("branch:", sanitize::text(&branch)));
        let remote = git::read_remote(&project.path)
            .map(|remote| format!("{} {}", remote.name, remote.url))
            .unwrap_or_else(|| dash.clone());
        rows.push(("remote:", sanitize::text(&remote)));
    } else {
        rows.push(("git:", "no".to_owned()));
    }
    rows.push((
        "match:",
        format!("{} (score {})", found.reason.label(), found.score),
    ));

    for (label, value) in &rows {
        output::print_line(&output::info_line(label, value))?;
    }
    Ok(())
}

/// The most recent jumps as one relative-time line; `None` when the backend
/// keeps no usage log (JSON) or the project has no history yet.
fn format_history(store: &impl Store, id: &str) -> Result<Option<String>> {
    let history = store.recent_uses(id, 5)?;
    if history.is_empty() {
        return Ok(None);
    }
    Ok(Some(output::format_times(&history, Utc::now())))
}

fn cmd_remove(
    store: &impl Store,
    args: RemoveArgs,
    matching: &MatcherConfig,
    debug: bool,
) -> Result<()> {
    let projects = store.load()?;
    let query = args.query.join(" ");
    let found = resolve(&projects, &query, matching, debug)?;
    let target = projects[found.index].clone();

    // Confirm before taking the lock; no index lock should be held while a
    // human decides.
    if !args.yes {
        if !io::stdin().is_terminal() {
            return Err(Error::ConfirmationRequired);
        }
        output::print_stderr(&format!(
            "remove `{}` ({})? [y/N] ",
            sanitize::text(&target.name),
            sanitize::path(&target.path)
        ))?;
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        let answer = answer.trim().to_lowercase();
        if answer != "y" && answer != "yes" {
            output::print_stderr_line("aborted")?;
            return Ok(());
        }
    }

    // Re-verify by id under the lock: the project could have been removed by
    // another process since the unlocked resolution above.
    let message = store.update(move |projects| {
        match projects.iter().position(|project| project.id == target.id) {
            Some(index) => {
                let removed = projects.remove(index);
                Ok(format!(
                    "removed `{}` -> {}",
                    sanitize::text(&removed.name),
                    sanitize::path(&removed.path)
                ))
            }
            None => Err(Error::NoLongerIndexed(target.name.clone())),
        }
    })?;
    output::print_line(&message)
}

/// Resolves a query to `(id, name)` without locking; callers must re-verify
/// the id inside [`Store::update`] before mutating.
fn resolve_id(
    store: &impl Store,
    query: &str,
    matching: &MatcherConfig,
    debug: bool,
) -> Result<(String, String)> {
    let projects = store.load()?;
    let found = resolve(&projects, query, matching, debug)?;
    Ok((
        projects[found.index].id.clone(),
        projects[found.index].name.clone(),
    ))
}

fn cmd_pin(
    store: &impl Store,
    query: &str,
    pinned: bool,
    matching: &MatcherConfig,
    debug: bool,
) -> Result<()> {
    let (id, name) = resolve_id(store, query, matching, debug)?;

    let message = store.update(move |projects| {
        let project = project::revalidate(projects, &id, name)?;

        let verb = if pinned { "pinned" } else { "unpinned" };
        if project.pinned == pinned {
            return Ok(format!(
                "already {verb} `{}`",
                sanitize::text(&project.name)
            ));
        }
        project.pinned = pinned;
        Ok(format!("{verb} `{}`", sanitize::text(&project.name)))
    })?;
    output::print_line(&message)
}

fn cmd_rename(
    store: &impl Store,
    old: &str,
    new: &str,
    force: bool,
    matching: &MatcherConfig,
    debug: bool,
) -> Result<()> {
    let new = project::sanitize_value("project name", new)?;

    let (id, current_name) = resolve_id(store, old, matching, debug)?;

    let message = store.update(move |projects| {
        let collision = projects
            .iter()
            .find(|project| project.id != id && project.name.eq_ignore_ascii_case(&new))
            .map(|project| project.path.clone());

        if let Some(path) = &collision
            && !force
        {
            return Err(Error::NameTaken {
                name: new.clone(),
                path: path.clone(),
            });
        }

        let project = project::revalidate(projects, &id, current_name)?;

        let old_name = project.name.clone();
        project.name = new.clone();
        let old_name = sanitize::text(&old_name);
        let new = sanitize::text(&new);
        if collision.is_some() {
            Ok(format!(
                "renamed `{old_name}` -> `{new}` (another project is already named `{new}`)"
            ))
        } else {
            Ok(format!("renamed `{old_name}` -> `{new}`"))
        }
    })?;
    output::print_line(&message)
}

fn cmd_alias(
    store: &impl Store,
    action: AliasCommand,
    matching: &MatcherConfig,
    debug: bool,
) -> Result<()> {
    match action {
        AliasCommand::Add { project, alias } => cmd_term(
            store,
            &project,
            &alias,
            TermKind::Alias,
            true,
            matching,
            debug,
        ),
        AliasCommand::Remove { project, alias } => cmd_term(
            store,
            &project,
            &alias,
            TermKind::Alias,
            false,
            matching,
            debug,
        ),
    }
}

fn cmd_tag(
    store: &impl Store,
    action: TagCommand,
    matching: &MatcherConfig,
    debug: bool,
) -> Result<()> {
    match action {
        TagCommand::Add { project, tag } => {
            cmd_term(store, &project, &tag, TermKind::Tag, true, matching, debug)
        }
        TagCommand::Remove { project, tag } => {
            cmd_term(store, &project, &tag, TermKind::Tag, false, matching, debug)
        }
    }
}

fn cmd_term(
    store: &impl Store,
    query: &str,
    value: &str,
    kind: TermKind,
    add: bool,
    matching: &MatcherConfig,
    debug: bool,
) -> Result<()> {
    let value = project::sanitize_value(kind.label(), value)?;

    let (id, name) = resolve_id(store, query, matching, debug)?;

    let message = store.update(move |projects| {
        let project = project::revalidate(projects, &id, name)?;
        project::set_term(project, kind, &value, add)
    })?;
    output::print_line(&message)
}

/// Imports `projects.json` into `projects.db`; the JSON original is kept as a
/// `projects.json.migrated-<timestamp>` sibling.
fn cmd_migrate(
    data_dir: &Path,
    backend: StorageBackend,
    location: &config::Location,
) -> Result<()> {
    match migrate::run(data_dir)? {
        migrate::Outcome::Migrated { projects, backup } => {
            let noun = output::plural(projects, "project", "projects");
            let backup_name = backup
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| backup.display().to_string());
            output::print_line(&format!(
                "migrated {projects} {noun} to {} (projects.json kept as {})",
                sanitize::path(&paths::sqlite_file(data_dir)),
                sanitize::text(&backup_name)
            ))?;

            if backend == StorageBackend::Json {
                let target = location
                    .path
                    .as_deref()
                    .map_or_else(|| "your config file".to_owned(), sanitize::path);
                output::print_line(&format!(
                    "note: set `[storage] backend = \"sqlite\"` in {target} to use the database"
                ))?;
            }
            Ok(())
        }
        migrate::Outcome::AlreadyMigrated { db } => output::print_line(&format!(
            "already migrated: {} is present",
            sanitize::path(&db)
        )),
    }
}

/// Imports another jumper's store: one planning pass against a snapshot, then
/// one locked update that re-verifies every path (vanished or racy entries are
/// reported, never resurrected).
fn cmd_import(store: &impl Store, args: ImportArgs) -> Result<()> {
    let home = project::home_dir();
    let path = import::resolve_path(args.source)?;
    let parsed = import::parse_at(args.source, &path)?;
    let projects = store.load()?;
    let mut plan = import::plan(
        &parsed,
        &projects,
        home.as_deref(),
        args.min_score,
        args.limit,
    );

    if args.dry_run {
        let mut rows = projects.clone();
        rows.extend(import::preview(&plan)?);
        // Same order and table as a `list` run would show afterwards.
        sort_projects(&mut rows, None);
        let missing = vec![false; rows.len()];
        let width = output::terminal_width();
        output::print(&output::format_projects_with_missing(
            &rows,
            &missing,
            home.as_deref(),
            width,
        ))?;
        return output::print_line(&import::summary(&plan, true));
    }

    let (imported, raced) = store.update(|projects| Ok(import::apply(projects, &plan.to_add)))?;
    if raced > 0 {
        plan.existing += raced;
        plan.to_add.truncate(imported);
    }
    output::print_line(&import::summary(&plan, false))
}

/// Previews explicit roots: the same bounded walk and options as the home
/// scan, printed best-first, writing neither the cache nor the index.
fn cmd_scan_roots(
    roots: &[PathBuf],
    settings: &config::Discovery,
    max_depth: Option<u16>,
) -> Result<()> {
    let home = project::home_dir();
    let mut options = settings.scan_options();
    if let Some(depth) = max_depth {
        options.max_depth = depth;
    }

    let mut out = String::new();
    let mut total = 0usize;
    let mut projects = 0usize;
    for root in roots {
        let root = expand_input(root);
        if !root.exists() {
            return Err(Error::MissingDirectory(root));
        }
        if !root.is_dir() {
            return Err(Error::NotADirectory(root));
        }
        let mut dirs = discovery::scan(&root, &options);
        discovery::best_first(&mut dirs);
        for dir in &dirs {
            let shown = output::shorten_home(&dir.path, home.as_deref());
            if dir.project_like {
                out.push_str(&format!("{shown} (project)\n"));
            } else {
                out.push_str(&format!("{shown}\n"));
            }
        }
        total += dirs.len();
        projects += dirs.iter().filter(|dir| dir.project_like).count();
    }
    let noun = output::plural(total, "directory", "directories");
    out.push_str(&format!("{total} {noun} ({projects} project-like)\n"));
    output::print(&out)
}

fn cmd_scan(data_dir: &Path, settings: &config::Discovery, args: ScanArgs) -> Result<()> {
    if !args.roots.is_empty() {
        return cmd_scan_roots(&args.roots, settings, args.max_depth);
    }
    let Some(home) = project::home_dir() else {
        return Err(Error::Storage(
            "could not determine the home directory".to_owned(),
        ));
    };
    let home_display = output::shorten_home(&home, project::home_dir().as_deref());
    let cache_path = discovery::cache_path(data_dir);

    if !args.refresh
        && !args.dry_run
        && let Some(cache) = discovery::load_cache(&cache_path)
        && !cache.is_stale(settings.ttl_hours, Utc::now())
    {
        let noun = output::plural(cache.dirs.len(), "directory", "directories");
        let age = output::relative_time(cache.scanned_at, Utc::now());
        return output::print_line(&format!(
            "home cache is fresh: {} {noun} scanned {age}; pass --refresh to rescan",
            cache.dirs.len()
        ));
    }

    let mut options = settings.scan_options();
    if let Some(depth) = args.max_depth {
        options.max_depth = depth;
    }

    let started = std::time::Instant::now();
    let dirs = discovery::scan(&home, &options);
    let elapsed = started.elapsed();
    let count = dirs.len();
    let projects = dirs.iter().filter(|dir| dir.project_like).count();
    let noun = output::plural(count, "directory", "directories");
    let project_noun = output::plural(projects, "project", "projects");

    if args.dry_run {
        return output::print_line(&format!(
            "would cache {count} {noun} under {home_display} ({projects} {project_noun}) in {elapsed:?}"
        ));
    }

    discovery::save_cache(&cache_path, &discovery::Cache::new(dirs, Utc::now()))?;
    output::print_line(&format!(
        "cached {count} {noun} under {home_display} ({projects} {project_noun}) in {elapsed:?}"
    ))
}

fn cmd_doctor(
    store: &impl Store,
    data_dir: &Path,
    backend: StorageBackend,
    args: DoctorArgs,
    config_status: config::Status,
) -> Result<i32> {
    let index_path = paths::index_file(data_dir, backend);

    if args.refresh {
        let (total, changed) = store.update(|projects| {
            let mut changed = 0;
            for project in projects.iter_mut() {
                if !project.path.exists() {
                    continue;
                }
                let detected = project::GitInfo::detect(&project.path);
                if detected != project.git {
                    project.git = detected;
                    changed += 1;
                }
            }
            Ok((projects.len(), changed))
        })?;
        if total == 0 {
            output::print_line("git metadata: no projects indexed")?;
        } else if changed == 0 {
            let noun = output::plural(total, "project", "projects");
            output::print_line(&format!("git metadata is up to date ({total} {noun})"))?;
        } else {
            output::print_line(&format!(
                "refreshed git metadata: {changed} of {total} projects updated"
            ))?;
        }
    }

    // A symlinked index is an error for every command; `doctor` is the
    // diagnostic, so it reports the link and keeps inspecting the rest.
    let mut link_issues = Vec::new();
    let mut projects = match store.load() {
        Ok(projects) => projects,
        Err(Error::SymlinkedPath { path, .. }) => {
            link_issues.push(doctor::Issue::SymlinkedFile(path));
            Vec::new()
        }
        Err(err) => return Err(err),
    };

    if args.fix && projects.iter().any(|project| !project.path.exists()) {
        if !io::stdin().is_terminal() {
            return Err(Error::FixRequiresTerminal);
        }

        // No lock while a human decides; the repairs are applied afterwards,
        // re-verifying by id under the lock.
        let repairs = {
            let stdin = io::stdin();
            let mut input = stdin.lock();
            let stderr = io::stderr();
            let mut prompt = stderr.lock();
            doctor::fix_session(
                &projects,
                project::home_dir().as_deref(),
                &mut input,
                &mut prompt,
            )?
        };
        if !repairs.is_empty() {
            let requested = repairs.len();
            let applied = store.update(|projects| Ok(doctor::apply_repairs(projects, &repairs)))?;
            output::print_line(&format!("repaired {applied} of {requested} stale projects"))?;
            projects = store.load()?;
        }
    }

    let schema = store.schema_version()?;
    let storage = match backend {
        StorageBackend::Json => "json".to_owned(),
        StorageBackend::Sqlite => match schema {
            Some(version) => format!("sqlite (schema v{version})"),
            None => "sqlite (no database yet)".to_owned(),
        },
    };

    let mut report = doctor::inspect(data_dir, backend, &projects);
    report.issues.extend(link_issues);
    if let config::Status::Invalid { path, message } = &config_status {
        report.issues.push(doctor::Issue::ConfigProblem {
            path: path.clone(),
            message: message.clone(),
        });
    }
    if let Some(cache) = discovery::load_cache(&discovery::cache_path(data_dir)) {
        output::print_line(&format!(
            "home cache: {} directories, scanned {}",
            cache.dirs.len(),
            output::relative_time(cache.scanned_at, Utc::now())
        ))?;
    }
    output::print(&output::format_doctor(
        &report,
        &index_path,
        projects.len(),
        project::home_dir().as_deref(),
        &config_status,
        &storage,
    ))?;

    Ok(if report.is_clean() { 0 } else { 1 })
}

fn sort_projects(projects: &mut [Project], sort: Option<SortBy>) {
    let sort = sort.unwrap_or(SortBy::Used);
    projects.sort_by(|a, b| project::sort_key(a, b, sort));
}

/// Expands a leading `~` without mangling non-UTF-8 paths that do not use it.
fn expand_input(input: &Path) -> PathBuf {
    if input == Path::new("~") || input.starts_with("~") {
        project::expand_tilde(&input.to_string_lossy())
    } else {
        input.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::Project;

    #[test]
    fn default_sort_pins_then_recent_then_name() {
        let mut pinned = Project::for_test("pinned", "/x/pinned");
        pinned.pinned = true;
        let mut recent = Project::for_test("recent", "/x/recent");
        recent.last_used_at = Some(Utc::now());
        let plain = Project::for_test("plain", "/x/plain");

        let mut projects = vec![plain, recent, pinned];
        sort_projects(&mut projects, None);
        let names: Vec<_> = projects
            .iter()
            .map(|project| project.name.as_str())
            .collect();
        assert_eq!(names, ["pinned", "recent", "plain"]);
    }

    #[test]
    fn explicit_sorts_ignore_pinning() {
        let mut b = Project::for_test("bravo", "/x/bravo");
        b.pinned = true;
        let a = Project::for_test("alpha", "/x/alpha");

        let mut projects = vec![b, a];
        sort_projects(&mut projects, Some(SortBy::Name));
        let names: Vec<_> = projects
            .iter()
            .map(|project| project.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "bravo"]);
    }

    #[test]
    fn expand_input_handles_tilde_forms() {
        assert_eq!(
            expand_input(Path::new("/abs/path")),
            PathBuf::from("/abs/path")
        );
        let expanded = expand_input(Path::new("~/code"));
        if let Some(home) = project::home_dir() {
            assert_eq!(expanded, home.join("code"));
        }
    }
}
