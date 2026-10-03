use std::io::{IsTerminal, Write};
use std::path::Path;

use chrono::{DateTime, Utc};
use clap::CommandFactory;
use colored::Colorize;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::cli::{Cli, Shell};
use crate::config;
use crate::doctor::{Issue, Report};
use crate::error::{Error, Result};
use crate::matcher::Match;
use crate::project::Project;
use crate::sanitize;

/// The output funnel: nothing may carry a terminal control sequence that
/// overdosecd itself did not emit as a colour change.
///
/// Renderers escape values before they get here ([`sanitize::text`]); this is
/// the backstop that turns a missed one into a refusal instead of a terminal
/// escape. A failed check means a renderer bug, so the error says so.
fn guard(text: &str) -> Result<()> {
    match sanitize::foreign_escape(text) {
        Some(_) => Err(Error::UnsafeOutput),
        None => Ok(()),
    }
}

/// Writes a line to stdout, turning broken pipes into an error that `main`
/// can exit quietly on.
pub fn print_line(text: &str) -> Result<()> {
    guard(text)?;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    writeln!(handle, "{text}")?;
    Ok(())
}

/// Writes raw text (no trailing newline added) to stdout.
pub fn print(text: &str) -> Result<()> {
    guard(text)?;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(text.as_bytes())?;
    Ok(())
}

/// Writes raw text (no trailing newline added) to stderr through the same
/// guard as stdout.
pub fn print_stderr(text: &str) -> Result<()> {
    guard(text)?;
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    handle.write_all(text.as_bytes())?;
    Ok(())
}

/// Writes a line to stderr through the same guard as stdout.
pub fn print_stderr_line(text: &str) -> Result<()> {
    guard(text)?;
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    writeln!(handle, "{text}")?;
    Ok(())
}

/// The terminal's width in columns, or `None` when stdout is not a tty (or the
/// size cannot be determined). Piped output is never truncated, so it stays
/// complete for scripts.
pub fn terminal_width() -> Option<usize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    if let Ok((columns, _)) = ratatui::crossterm::terminal::size()
        && columns > 0
    {
        return Some(usize::from(columns));
    }
    // Some terminals report a zero row count, which hides the size from the
    // ioctl; fall back to the conventional `COLUMNS` variable when exported.
    parse_width(std::env::var("COLUMNS").ok().as_deref())
}

/// Parses a `COLUMNS`-style width: garbage and zero mean "unknown".
fn parse_width(value: Option<&str>) -> Option<usize> {
    value
        .and_then(|columns| columns.trim().parse().ok())
        .filter(|width| *width > 0)
}

/// Whether the optional `cd` wrapper is installed: `[general] hook` records
/// jumps into indexed projects, `[general] hint` counts visits to the ones
/// that are not indexed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HookSettings {
    pub record: bool,
    pub hint: bool,
}

impl HookSettings {
    pub fn any(self) -> bool {
        self.record || self.hint
    }
}

/// The shell function that makes `ocd` a jump command, plus its completion
/// registration.
pub fn init_script(shell: Shell, hooks: HookSettings) -> String {
    let commands = passthrough_commands();
    match shell {
        Shell::Bash => bash_zsh_script(&commands, &bash_completion(&commands), hooks),
        Shell::Zsh => bash_zsh_script(&commands, &zsh_completion(&commands), hooks),
        Shell::Fish => fish_script(&commands, hooks),
    }
}

/// The optional `cd` wrapper, appended to the shell integration when
/// `[general] hook` or `[general] hint` is on.
const HOOK_SCRIPT: &str = r#"
# Optional cd hook ([general] hook / hint in config.toml): a plain cd records
# a jump into an indexed project, and visits to directories that are not
# indexed are counted. cd's behavior and exit status are unchanged.
cd() {
  builtin cd "$@" && command overdosecd hook
}
"#;

/// The fish spelling of [`HOOK_SCRIPT`].
const FISH_HOOK_SCRIPT: &str = r#"
# Optional cd hook ([general] hook / hint in config.toml): a plain cd records
# a jump into an indexed project, and visits to directories that are not
# indexed are counted. cd's behavior and exit status are unchanged.
function cd --description 'cd, with a overdosecd hook'
    builtin cd $argv
    and command overdosecd hook
end
"#;

impl From<Shell> for clap_complete::Shell {
    fn from(shell: Shell) -> Self {
        match shell {
            Shell::Bash => clap_complete::Shell::Bash,
            Shell::Zsh => clap_complete::Shell::Zsh,
            Shell::Fish => clap_complete::Shell::Fish,
        }
    }
}

/// A static completion script for the `overdosecd` binary itself.
pub fn completion_script(shell: Shell) -> String {
    let mut command = Cli::command();
    let name = command.get_name().to_owned();
    let generator: clap_complete::Shell = shell.into();
    let mut script = Vec::new();
    clap_complete::generate(generator, &mut command, name, &mut script);
    String::from_utf8(script).expect("clap_complete emits UTF-8")
}

/// Subcommands and flags the wrapper forwards to the binary instead of
/// treating as a jump query. Generated from clap so it cannot drift; hidden
/// commands (the completion helper) are excluded so they stay valid project
/// names.
fn passthrough_commands() -> Vec<String> {
    let mut names: Vec<String> = Cli::command()
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
        .flat_map(|command| {
            std::iter::once(command.get_name().to_owned())
                .chain(command.get_all_aliases().map(str::to_owned))
        })
        .chain(["help", "--help", "-h", "--version", "-V"].map(str::to_owned))
        .collect();
    names.sort();
    names.dedup();
    names
}

fn bash_zsh_script(commands: &[String], completion: &str, hooks: HookSettings) -> String {
    let commands = commands.join("|");
    let hook = if hooks.any() { HOOK_SCRIPT } else { "" };
    format!(
        r#"# overdosecd shell integration (bash / zsh)
ocd() {{
  local target

  # Bare `ocd`, `ocd ui`, and `ocd --ui` open the picker and jump to whatever
  # Enter chose. The picker draws in the terminal, so stdout carries only the
  # selected path.
  if [ "$#" -eq 0 ]; then
    target="$(command overdosecd ui)" || return 1
    builtin cd "$target" || return 1
    return 0
  fi

  case "$1" in
    --cmd)
      # Terminal-command mode: run the CLI, never a picker, never a cd.
      shift
      if [ "$#" -eq 0 ]; then
        command overdosecd --help
        return $?
      fi
      case "$1" in
        {commands})
          command overdosecd "$@"
          ;;
        *)
          command overdosecd goto "$@"
          ;;
      esac
      return $?
      ;;
    ui|--ui)
      shift
      target="$(command overdosecd ui "$@")" || return 1
      builtin cd "$target" || return 1
      ;;
    {commands})
      command overdosecd "$@"
      ;;
    *)
      # A jump query: jump when the index is confident, otherwise open the
      # picker with the query typed in. Scripts (no stdin tty) keep plain
      # `goto` and its messages.
      if [ -t 0 ]; then
        if target="$(command overdosecd goto --confident "$@" 2>/dev/null)"; then
          builtin cd "$target" || return 1
          return 0
        fi
        target="$(command overdosecd ui --query "$*")" || return 1
        builtin cd "$target" || return 1
      else
        target="$(command overdosecd goto "$@")" || return 1
        builtin cd "$target" || return 1
      fi
      ;;
  esac
}}
{hook}{completion}"#
    )
}

/// Bash completion for `ocd`: subcommands and jump queries on the first word,
/// `--cmd` and `--no-track` for flags, subcommands after `--cmd`, and
/// `overdosecd complete` for everything else.
const BASH_COMPLETION: &str = r#"
# Jump-query completion: subcommands for the first word, then names, aliases,
# and tags from `overdosecd complete`.
_ocd() {
  local current candidate
  current="${COMP_WORDS[COMP_CWORD]}"
  COMPREPLY=()

  if [ "${COMP_WORDS[1]}" = "--cmd" ]; then
    if [ "$COMP_CWORD" -eq 2 ]; then
      COMPREPLY=($(compgen -W "%COMMANDS%" -- "$current"))
    fi
    return
  fi

  case "$current" in
    -*)
      COMPREPLY=($(compgen -W "--cmd --no-track" -- "$current"))
      return
      ;;
  esac

  if [ "$COMP_CWORD" -eq 1 ]; then
    COMPREPLY=($(compgen -W "%COMMANDS%" -- "$current"))
    while IFS= read -r candidate; do
      COMPREPLY+=("$candidate")
    done < <(command overdosecd complete "$current")
  fi
}
complete -F _ocd ocd
"#;

fn bash_completion(commands: &[String]) -> String {
    BASH_COMPLETION.replace("%COMMANDS%", &commands.join(" "))
}

/// Zsh completion for `ocd`, registered only after `compinit` has run so
/// `eval "$(overdosecd init zsh)"` stays clean and silent.
const ZSH_COMPLETION: &str = r#"
# Jump-query completion: subcommands for the first word, then names, aliases,
# and tags from `overdosecd complete`.
_ocd() {
  local -a candidates
  local current
  current="${(Q)words[CURRENT]}"

  if [[ "${words[2]}" == "--cmd" ]]; then
    if (( CURRENT == 3 )); then
      candidates+=(%COMMANDS%)
    fi
  elif [[ "$current" == -* ]]; then
    candidates=(--cmd --no-track)
  elif (( CURRENT == 2 )); then
    candidates+=(%COMMANDS%)
    candidates+=(${(f)"$(command overdosecd complete "$current")"})
  fi

  compadd -a candidates
}
if (( $+functions[compdef] )); then
  compdef _ocd ocd
fi
"#;

fn zsh_completion(commands: &[String]) -> String {
    ZSH_COMPLETION.replace("%COMMANDS%", &commands.join(" "))
}

fn fish_script(commands: &[String], hooks: HookSettings) -> String {
    let commands = commands.join(" ");
    let completion = fish_completion(&commands);
    let hook = if hooks.any() { FISH_HOOK_SCRIPT } else { "" };
    format!(
        r#"# overdosecd shell integration (fish)
function ocd
    # Bare `ocd`, `ocd ui`, and `ocd --ui` open the picker and jump to whatever
    # Enter chose. The picker draws in the terminal, so stdout carries only the
    # selected path.
    if test (count $argv) -eq 0
        set -l target (command overdosecd ui)
        or return 1
        builtin cd $target
        or return 1
        return 0
    end

    switch $argv[1]
        case --cmd
            # Terminal-command mode: run the CLI, never a picker, never a cd.
            set -l cmd $argv[2..-1]
            if test (count $cmd) -eq 0
                command overdosecd --help
                return $status
            end
            switch $cmd[1]
                case {commands}
                    command overdosecd $cmd
                case '*'
                    command overdosecd goto $cmd
            end
            return $status
        case ui --ui
            set -l target (command overdosecd ui $argv[2..-1])
            or return 1
            builtin cd $target
            or return 1
        case {commands}
            command overdosecd $argv
        case '*'
            # A jump query: jump when the index is confident, otherwise open
            # the picker with the query typed in; scripts (no tty on stdin)
            # keep plain `goto`.
            if isatty stdin
                set -l target (command overdosecd goto --confident $argv 2>/dev/null)
                if test $status -eq 0
                    builtin cd $target
                    or return 1
                    return 0
                end
                set target (command overdosecd ui --query "$argv")
                or return 1
                builtin cd $target
                or return 1
            else
                set -l target (command overdosecd goto $argv)
                or return 1
                builtin cd $target
                or return 1
            end
    end
end
{hook}{completion}"#
    )
}

/// Fish completion for `ocd` (shipped unverified: fish is not installed on the
/// development machine, so only the syntax is reviewed).
const FISH_COMPLETION: &str = r#"
# Jump-query completion: subcommands for the first word, then names, aliases,
# and tags from `overdosecd complete`.
complete -c ocd -f -n 'test (count (commandline -opc)) -eq 1' -a '%COMMANDS% --cmd'
complete -c ocd -f -n 'test (count (commandline -opc)) -eq 1' -a '(command overdosecd complete (commandline -ct))'
complete -c ocd -f -n 'set -l opc (commandline -opc); test (count $opc) -eq 1; and string match -q -- "-*" (commandline -ct)' -a '--cmd --no-track'
complete -c ocd -f -n 'set -l opc (commandline -opc); test (count $opc) -eq 2; and test $opc[2] = --cmd' -a '%COMMANDS%'
"#;

fn fish_completion(commands: &str) -> String {
    FISH_COMPLETION.replace("%COMMANDS%", commands)
}

/// Replaces a home prefix with `~` for display, escaping anything a terminal
/// would interpret (paths can arrive from a planted index).
pub fn shorten_home(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home
        && let Ok(stripped) = path.strip_prefix(home)
    {
        if stripped.as_os_str().is_empty() {
            return "~".to_owned();
        }
        return sanitize::text(&format!("~/{}", stripped.display()));
    }
    sanitize::text(&path.display().to_string())
}

/// Human-readable age such as `5m ago` or `3d ago`; older than a month it
/// falls back to an absolute date.
pub fn relative_time(when: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let delta = now.signed_duration_since(when);
    let minutes = delta.num_minutes();
    if minutes < 1 {
        "just now".to_owned()
    } else if minutes < 60 {
        format!("{minutes}m ago")
    } else if delta.num_hours() < 24 {
        format!("{}h ago", delta.num_hours())
    } else if delta.num_days() < 30 {
        format!("{}d ago", delta.num_days())
    } else {
        when.format("%Y-%m-%d").to_string()
    }
}

/// How a table cell is colored. Kept apart from the text so truncated cells
/// keep their tone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    Plain,
    Dim,
    Red,
    Yellow,
}

impl Tone {
    fn paint(self, text: &str) -> String {
        match self {
            Tone::Plain => text.to_owned(),
            Tone::Dim => text.dimmed().to_string(),
            Tone::Red => text.red().to_string(),
            Tone::Yellow => text.yellow().to_string(),
        }
    }
}

struct Cell {
    plain: String,
    tone: Tone,
}

/// Column floors for [`format_projects`]: how far a column may shrink before
/// the next one in the priority order starts giving up space.
const MIN_NAME_WIDTH: usize = 6;
const MIN_PATH_WIDTH: usize = 12;
const MIN_LIST_WIDTH: usize = 8;

/// Display width of `text` in terminal cells (CJK and emoji count as two).
fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Truncates `text` to at most `max` display columns, eliding the middle as
/// `head…tail` so both ends stay visible. Wide characters that would straddle
/// the boundary are dropped rather than split.
fn truncate_to(text: &str, max: usize) -> String {
    if display_width(text) <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    if max == 1 {
        return "…".to_owned();
    }

    // Bias the budget toward the tail: for paths the identifying part is the
    // final component.
    let tail_budget = (max - 1) * 2 / 3;
    let head_budget = max - 1 - tail_budget;
    format!(
        "{}…{}",
        take_prefix(text, head_budget),
        take_suffix(text, tail_budget)
    )
}

/// Path-flavored truncation: keeps the final component intact where possible
/// and cuts the head back to a component boundary (`~/code…/overdosecd`).
fn truncate_path(text: &str, max: usize) -> String {
    if display_width(text) <= max {
        return text.to_owned();
    }
    let last = text.rsplit('/').next().unwrap_or(text);
    let last_width = display_width(last);
    // `…` plus `/last` must fit; otherwise this strategy cannot help.
    if last.is_empty() || last_width + 2 > max {
        return truncate_to(text, max);
    }

    let tail = format!("/{last}");
    let head_budget = max - 1 - (last_width + 1);
    let mut head = take_prefix(text, head_budget);
    if let Some(position) = head.rfind('/') {
        head.truncate(position);
    }
    format!("{head}…{tail}")
}

/// The longest prefix of `text` that fits in `budget` display columns.
fn take_prefix(text: &str, budget: usize) -> String {
    let mut used = 0;
    let mut out = String::new();
    for character in text.chars() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + width > budget {
            break;
        }
        used += width;
        out.push(character);
    }
    out
}

/// The longest suffix of `text` that fits in `budget` display columns.
fn take_suffix(text: &str, budget: usize) -> String {
    let mut used = 0;
    let mut characters = Vec::new();
    for character in text.chars().rev() {
        let width = UnicodeWidthChar::width(character).unwrap_or(0);
        if used + width > budget {
            break;
        }
        used += width;
        characters.push(character);
    }
    characters.into_iter().rev().collect()
}

/// Shrinks `widths` until the table fits `budget`, giving space up in
/// `priority` order and never below `floors`. Columns not worth truncating
/// (the short `LAST USED` column) get `usize::MAX` as their floor.
fn shrink_to_budget(widths: &mut [usize], floors: &[usize], priority: &[usize], budget: usize) {
    let separators = 2 * widths.len().saturating_sub(1);
    let overflow =
        |widths: &[usize]| (widths.iter().sum::<usize>() + separators).saturating_sub(budget);

    for &index in priority {
        let excess = overflow(widths);
        if excess == 0 {
            return;
        }
        let give = excess.min(widths[index].saturating_sub(floors[index]));
        widths[index] -= give;
    }

    // Absurdly narrow terminal: floors give way too, down to one cell each.
    for &index in priority {
        let excess = overflow(widths);
        if excess == 0 {
            return;
        }
        let give = excess.min(widths[index].saturating_sub(1));
        widths[index] -= give;
    }
}

/// Renders projects as an aligned table; aliases/tags columns appear only
/// when at least one project uses them. When `width` is given (a tty), the
/// table is truncated to fit it; piped output passes `None` and stays whole.
///
/// Test convenience with a fresh existence check per project; `list` passes
/// its own flags to [`format_projects_with_missing`] so it stats once.
#[cfg(test)]
fn format_projects(projects: &[Project], home: Option<&Path>, width: Option<usize>) -> String {
    let missing: Vec<bool> = projects
        .iter()
        .map(|project| !project.path.exists())
        .collect();
    format_projects_with_missing(projects, &missing, home, width)
}

/// [`format_projects`] with the existence check already done by the caller,
/// so `list` can share one stat per project between its hiding filter and the
/// `!` markers instead of re-checking the filesystem on every pass.
///
/// `missing` must be aligned with `projects`.
pub fn format_projects_with_missing(
    projects: &[Project],
    missing: &[bool],
    home: Option<&Path>,
    width: Option<usize>,
) -> String {
    debug_assert_eq!(projects.len(), missing.len(), "aligned existence flags");
    if projects.is_empty() {
        return String::new();
    }

    let show_aliases = projects.iter().any(|project| !project.aliases.is_empty());
    let show_tags = projects.iter().any(|project| !project.tags.is_empty());
    let now = Utc::now();

    let mut header: Vec<String> = vec!["NAME".into(), "PATH".into()];
    if show_aliases {
        header.push("ALIASES".into());
    }
    if show_tags {
        header.push("TAGS".into());
    }
    header.push("LAST USED".into());

    let mut rows: Vec<Vec<Cell>> = Vec::with_capacity(projects.len());
    for (project, &missing) in projects.iter().zip(missing) {
        let mut marker = String::new();
        if project.pinned {
            marker.push('★');
        }
        if missing {
            marker.push('!');
        }
        let name = sanitize::text(&project.name);
        let name_plain = if marker.is_empty() {
            name
        } else {
            format!("{marker} {name}")
        };

        let name_tone = if missing {
            Tone::Red
        } else if project.pinned {
            Tone::Yellow
        } else {
            Tone::Plain
        };

        let path_plain = shorten_home(&project.path, home);
        let path_tone = if missing { Tone::Red } else { Tone::Dim };

        let last = project
            .last_used_at
            .map_or_else(|| "never".to_owned(), |when| relative_time(when, now));

        let mut cells = vec![
            Cell {
                plain: name_plain,
                tone: name_tone,
            },
            Cell {
                plain: path_plain,
                tone: path_tone,
            },
        ];
        if show_aliases {
            cells.push(Cell {
                plain: join_or_dash(&project.aliases),
                tone: Tone::Dim,
            });
        }
        if show_tags {
            cells.push(Cell {
                plain: join_or_dash(&project.tags),
                tone: Tone::Dim,
            });
        }
        cells.push(Cell {
            plain: last,
            tone: Tone::Plain,
        });
        rows.push(cells);
    }

    let mut widths: Vec<usize> = header.iter().map(|title| display_width(title)).collect();
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(display_width(&cell.plain));
        }
    }
    if let Some(budget) = width {
        let mut floors = vec![MIN_NAME_WIDTH, MIN_PATH_WIDTH];
        if show_aliases {
            floors.push(MIN_LIST_WIDTH);
        }
        if show_tags {
            floors.push(MIN_LIST_WIDTH);
        }
        floors.push(usize::MAX); // LAST USED stays whole.

        // The path gives up space first, then aliases/tags, then the name.
        let mut priority = vec![1];
        if show_aliases {
            priority.push(2);
        }
        if show_tags {
            priority.push(if show_aliases { 3 } else { 2 });
        }
        priority.push(0);
        priority.push(widths.len() - 1);

        shrink_to_budget(&mut widths, &floors, &priority, budget);
    }

    // The path column cuts differently: keep its final component.
    let truncate = |index: usize, text: &str, max: usize| -> String {
        if index == 1 {
            truncate_path(text, max)
        } else {
            truncate_to(text, max)
        }
    };

    let mut out = String::new();
    for (index, title) in header.iter().enumerate() {
        if index > 0 {
            out.push_str("  ");
        }
        let truncated = truncate(index, title, widths[index]);
        let padding = widths[index].saturating_sub(display_width(&truncated));
        out.push_str(&truncated.bold().to_string());
        out.push_str(&" ".repeat(padding));
    }
    out.push('\n');

    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            if index > 0 {
                out.push_str("  ");
            }
            let truncated = truncate(index, &cell.plain, widths[index]);
            let padding = widths[index].saturating_sub(display_width(&truncated));
            out.push_str(&cell.tone.paint(&truncated));
            out.push_str(&" ".repeat(padding));
        }
        let end = out.trim_end().len();
        out.truncate(end);
        out.push('\n');
    }

    if missing.iter().any(|&flag| flag) {
        out.push_str("\n! = directory missing\n");
    }

    out
}

/// The `--debug` view of a query: every ranked candidate with its match
/// reason and score. Printed to stderr so `goto`'s stdout stays a bare path.
pub fn format_debug(query: &str, ranked: &[Match], home: Option<&Path>) -> String {
    let query = sanitize::text(query);
    let mut out = String::new();
    if ranked.is_empty() {
        return format!("debug: no candidates for `{query}`\n");
    }

    let noun = plural(ranked.len(), "candidate", "candidates");
    out.push_str(&format!("debug: {} {noun} for `{query}`\n", ranked.len()));
    for (position, candidate) in ranked.iter().enumerate() {
        out.push_str(&format!(
            "  {}. {} -> {} ({}, score {})\n",
            position + 1,
            sanitize::text(&candidate.name),
            shorten_home(&candidate.path, home),
            candidate.reason.label(),
            candidate.score,
        ));
        // Multi-term queries show what each word contributed; single-word
        // output stays exactly as it always was.
        if candidate.terms.len() > 1 {
            for hit in &candidate.terms {
                out.push_str(&format!(
                    "     `{}` -> {} (signal {})\n",
                    sanitize::text(&hit.term),
                    hit.reason.label(),
                    hit.signal,
                ));
            }
        }
    }
    out
}

fn join_or_dash(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_owned()
    } else {
        sanitize::text(&values.join(", "))
    }
}

/// The column where `info` values start (`last used:` is the longest label).
const INFO_LABEL_WIDTH: usize = 10;

/// A dimmed, fixed-width label followed by a plain value, for `info` lines.
pub fn info_line(label: &str, value: &str) -> String {
    let padding = INFO_LABEL_WIDTH.saturating_sub(display_width(label)) + 1;
    format!("{}{}{value}", label.dimmed(), " ".repeat(padding))
}

/// Picks the singular or plural noun for `count`: the one place the CLI's
/// counted output lines choose their wording.
pub(crate) fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

/// Renders timestamps as one relative-time line, oldest first; empty when
/// there is nothing to show. Shared by `info`'s history row and the picker's
/// details pane.
pub(crate) fn format_times(times: &[DateTime<Utc>], now: DateTime<Utc>) -> String {
    times
        .iter()
        .map(|when| relative_time(*when, now))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One-line description of the effective config file for the `doctor` header.
fn describe_config(status: &config::Status, home: Option<&Path>) -> String {
    match status {
        config::Status::Loaded { path } => shorten_home(path, home),
        config::Status::Defaults { path: Some(path) } => {
            format!(
                "{} (not found; built-in defaults apply)",
                shorten_home(path, home)
            )
        }
        config::Status::Defaults { path: None } => "none (built-in defaults apply)".to_owned(),
        config::Status::Invalid { path, .. } => shorten_home(path, home),
    }
}

/// Renders the read-only `doctor` report.
pub fn format_doctor(
    report: &Report,
    index_path: &Path,
    projects: usize,
    home: Option<&Path>,
    config_status: &config::Status,
    storage: &str,
) -> String {
    let mut out = String::new();
    let noun = plural(projects, "project", "projects");
    out.push_str(&format!(
        "index: {} ({projects} {noun})\n",
        shorten_home(index_path, home)
    ));
    out.push_str(&format!("storage: {storage}\n"));
    out.push_str(&format!(
        "config: {}\n",
        describe_config(config_status, home)
    ));

    if report.is_clean() {
        out.push_str(&format!("\n{}\n", "no problems found".green()));
        if projects == 0 {
            out.push_str("\nhint: `overdosecd add <path>` starts the index\n");
        }
        return out;
    }

    out.push_str(&format!("\nissues ({}):\n", report.issues.len()));
    let mut stale = false;
    for issue in &report.issues {
        match issue {
            Issue::StalePath { name, path } => {
                stale = true;
                out.push_str(&format!(
                    "  - {} `{}` -> {} (directory no longer exists)\n",
                    "stale:".red(),
                    sanitize::text(name),
                    shorten_home(path, home)
                ));
            }
            Issue::DuplicateName { name, count } => out.push_str(&format!(
                "  - {} `{}` is used by {count} projects; queries for it are ambiguous (`overdosecd rename`)\n",
                "duplicate name:".yellow(),
                sanitize::text(name)
            )),
            Issue::QuarantineFile(path) => out.push_str(&format!(
                "  - {} {} (kept; review and delete manually)\n",
                "quarantine:".dimmed(),
                shorten_home(path, home)
            )),
            Issue::StrayFile(path) => out.push_str(&format!(
                "  - {} {} (no projects.db beside it; delete it if no overdosecd is running)\n",
                "stray:".red(),
                shorten_home(path, home)
            )),
            Issue::BackendDisagreement {
                json,
                db,
                differing,
            } => {
                let json_noun = plural(*json, "project", "projects");
                let db_noun = plural(*db, "project", "projects");
                out.push_str(&format!(
                    "  - {} projects.json ({json} {json_noun}) and projects.db ({db} {db_noun}) disagree ({differing} ids differ); `overdosecd migrate` or remove the inactive file\n",
                    "mismatch:".yellow()
                ));
            }
            Issue::InactiveIndexUnreadable { path, message } => out.push_str(&format!(
                "  - {} {} cannot be read: {message}\n",
                "unreadable:".red(),
                shorten_home(path, home)
            )),
            Issue::IndexNotWritable(path) => out.push_str(&format!(
                "  - {} {} is read-only\n",
                "permissions:".red(),
                shorten_home(path, home)
            )),
            Issue::IndexPermissions { path, mode } => out.push_str(&format!(
                "  - {} {} is mode {mode:04o} (expected 0600)\n",
                "permissions:".red(),
                shorten_home(path, home)
            )),
            Issue::CachePermissions { path, mode } => out.push_str(&format!(
                "  - {} {} is mode {mode:04o} (expected 0600)\n",
                "permissions:".red(),
                shorten_home(path, home)
            )),
            Issue::DataDirNotWritable(path) => out.push_str(&format!(
                "  - {} data directory {} is not writable\n",
                "permissions:".red(),
                shorten_home(path, home)
            )),
            Issue::DataDirPermissions { path, mode } => out.push_str(&format!(
                "  - {} {} is mode {mode:04o} (expected 0700)\n",
                "permissions:".red(),
                shorten_home(path, home)
            )),
            Issue::ControlInValue {
                project,
                field,
                value,
            } => out.push_str(&format!(
                "  - {} project {} has unsafe characters in its {field} (rename or repath it): {}\n",
                "unsafe:".red(),
                sanitize::quoted(project),
                sanitize::quoted(value)
            )),
            Issue::ConfigProblem { path, message } => out.push_str(&format!(
                "  - {} {}: {}\n",
                "config:".red(),
                shorten_home(path, home),
                sanitize::text(message)
            )),
            Issue::SymlinkedFile(path) => out.push_str(&format!(
                "  - {} {} is a symbolic link; overdosecd refuses it (move it aside and let overdosecd recreate it)\n",
                "symlink:".red(),
                shorten_home(path, home)
            )),
        }
    }

    let mut hints: Vec<String> = Vec::new();
    if stale {
        hints.push("`--fix` repairs stale projects interactively".to_owned());
    }
    if projects > 0 {
        hints.push("`--refresh` updates stored git remotes".to_owned());
    }
    if !hints.is_empty() {
        out.push_str(&format!("\n{} {}\n", "hint:".dimmed(), hints.join("; ")));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::Reason;
    use chrono::Duration;
    use std::path::PathBuf;

    fn no_color() {
        colored::control::set_override(false);
    }

    #[test]
    fn relative_time_buckets() {
        let now = Utc::now();
        assert_eq!(relative_time(now - Duration::seconds(30), now), "just now");
        assert_eq!(relative_time(now - Duration::minutes(5), now), "5m ago");
        assert_eq!(relative_time(now - Duration::hours(3), now), "3h ago");
        assert_eq!(relative_time(now - Duration::days(2), now), "2d ago");
        assert!(relative_time(now - Duration::days(45), now).starts_with("20"));
    }

    #[test]
    fn shorten_home_replaces_prefix() {
        let home = Path::new("/home/user");
        assert_eq!(
            shorten_home(Path::new("/home/user/code/x"), Some(home)),
            "~/code/x"
        );
        assert_eq!(shorten_home(Path::new("/home/user"), Some(home)), "~");
        assert_eq!(shorten_home(Path::new("/opt/x"), Some(home)), "/opt/x");
        assert_eq!(shorten_home(Path::new("/opt/x"), None), "/opt/x");
    }

    #[test]
    fn init_scripts_contain_wrapper_and_allowlist() {
        no_color();
        let zsh = init_script(Shell::Zsh, HookSettings::default());
        assert!(zsh.contains("ocd()"));
        assert!(zsh.contains("command overdosecd"));
        assert!(zsh.contains("|| return 1"));
        for command in [
            "add",
            "alias",
            "completions",
            "doctor",
            "goto",
            "list",
            "pin",
            "rename",
            "tag",
            "ui",
            "unpin",
        ] {
            assert!(
                zsh.contains(command),
                "allowlist should include `{command}`"
            );
        }

        let fish = init_script(Shell::Fish, HookSettings::default());
        assert!(fish.contains("function ocd"));
        assert!(fish.contains("add alias"));
    }

    #[test]
    fn wrappers_open_the_picker_for_bare_ocd_and_ui() {
        no_color();

        let zsh = init_script(Shell::Zsh, HookSettings::default());
        assert!(
            zsh.contains("command overdosecd ui"),
            "bare ocd must open the picker"
        );
        assert!(zsh.contains("ui|--ui)"), "--ui must alias ui");
        assert!(
            !zsh.contains("usage: ocd"),
            "bare ocd no longer prints usage"
        );

        let fish = init_script(Shell::Fish, HookSettings::default());
        assert!(fish.contains("case ui --ui"));
        assert!(fish.contains("command overdosecd ui $argv[2..-1]"));
    }

    #[test]
    fn wrappers_jump_confidently_and_fall_back_to_the_picker() {
        no_color();

        let zsh = init_script(Shell::Zsh, HookSettings::default());
        assert!(
            zsh.contains("command overdosecd goto --confident"),
            "the jump path asks for a confident match"
        );
        assert!(
            zsh.contains("command overdosecd ui --query"),
            "a miss falls back to the picker with the query"
        );
        assert!(
            zsh.contains("[ -t 0 ]"),
            "scripts keep plain goto and its messages"
        );
        assert!(zsh.contains("--cmd)"), "there is a terminal-command mode");
        assert!(zsh.contains("--cmd --no-track"), "completion offers --cmd");

        let fish = init_script(Shell::Fish, HookSettings::default());
        assert!(fish.contains("command overdosecd goto --confident"));
        assert!(fish.contains("isatty stdin"));
        assert!(fish.contains("case --cmd"));
        assert!(fish.contains("--cmd --no-track"));
    }

    #[test]
    fn init_scripts_install_the_hook_only_when_asked() {
        no_color();
        let jump = |shell| match shell {
            Shell::Fish => "builtin cd $target",
            _ => "builtin cd \"$target\"",
        };
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let plain = init_script(shell, HookSettings::default());
            assert!(
                plain.contains(jump(shell)),
                "{shell:?} jumps must use the builtin: {plain}"
            );
            assert!(
                !plain.contains("overdosecd hook"),
                "{shell:?} must not hook by default"
            );

            for hooks in [
                HookSettings {
                    record: true,
                    hint: false,
                },
                HookSettings {
                    record: false,
                    hint: true,
                },
            ] {
                let script = init_script(shell, hooks);
                assert!(script.contains("overdosecd hook"), "{shell:?}: {script}");
                let wrapper = match shell {
                    Shell::Fish => "builtin cd $argv",
                    _ => "builtin cd \"$@\"",
                };
                assert!(script.contains(wrapper), "{shell:?} wrapper: {script}");
            }
        }
        assert!(!HookSettings::default().any());
        assert!(
            HookSettings {
                record: false,
                hint: true
            }
            .any()
        );
    }

    #[test]
    fn completion_scripts_are_generated_for_every_shell() {
        no_color();
        for (shell, marker) in [
            (Shell::Bash, "_overdosecd"),
            (Shell::Zsh, "#compdef overdosecd"),
            (Shell::Fish, "complete -c overdosecd"),
        ] {
            let script = completion_script(shell);
            assert!(
                script.contains(marker),
                "{shell:?} script should contain {marker:?}:\n{script}"
            );
        }
    }

    #[test]
    fn allowlist_excludes_hidden_commands() {
        no_color();
        let zsh = init_script(Shell::Zsh, HookSettings::default());
        let case_line = zsh
            .lines()
            .find(|line| line.contains("--help|--version"))
            .expect("allowlist line");
        let tokens: Vec<&str> = case_line.split('|').collect();
        assert!(
            tokens.contains(&"completions"),
            "the public command is reserved: {case_line}"
        );
        assert!(
            !tokens.contains(&"complete"),
            "the hidden helper must stay usable as a project name: {case_line}"
        );
    }

    #[test]
    fn init_scripts_register_ocd_completion() {
        no_color();

        let bash = init_script(Shell::Bash, HookSettings::default());
        assert!(bash.contains("_ocd()"));
        assert!(bash.contains("complete -F _ocd ocd"));
        assert!(bash.contains("command overdosecd complete"));

        let zsh = init_script(Shell::Zsh, HookSettings::default());
        assert!(zsh.contains("compadd -a candidates"));
        assert!(zsh.contains("$+functions[compdef]"));
        assert!(zsh.contains("compdef _ocd ocd"));

        let fish = init_script(Shell::Fish, HookSettings::default());
        assert!(fish.contains("complete -c ocd"));
        assert!(fish.contains("command overdosecd complete"));
    }

    #[test]
    fn format_projects_marks_pinned_and_missing() {
        no_color();
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pinned = Project::for_test("alpha", dir.path().to_str().expect("utf-8 temp path"));
        pinned.pinned = true;
        let missing = Project::for_test("beta", "/tmp/overdosecd-does-not-exist-beta");

        let table = format_projects(&[pinned, missing], None, None);
        assert!(table.contains("★ alpha"), "table was:\n{table}");
        assert!(table.contains("! beta"), "table was:\n{table}");
        assert!(table.contains("NAME"));
        assert!(table.contains("LAST USED"));
        assert!(table.contains("! = directory missing"));
    }

    #[test]
    fn format_debug_lists_candidates_with_scores() {
        no_color();
        let matches = vec![
            Match::for_test("app-one", "/code/app-one", 2_000, Reason::NamePrefix),
            Match::for_test("app-two", "/code/app-two", 1_950, Reason::NamePrefix),
        ];

        let rendered = format_debug("app", &matches, None);
        assert!(
            rendered.contains("debug: 2 candidates for `app`"),
            "rendered:\n{rendered}"
        );
        assert!(rendered.contains("  1. app-one -> /code/app-one (name prefix, score 2000)"));
        assert!(rendered.contains("  2. app-two -> /code/app-two (name prefix, score 1950)"));
        assert_eq!(rendered.lines().count(), 3);

        assert_eq!(
            format_debug("qqq", &[], None),
            "debug: no candidates for `qqq`\n"
        );
        assert!(
            format_debug("app-one", &matches[..1], None)
                .contains("debug: 1 candidate for `app-one`")
        );
    }

    #[test]
    fn parse_width_accepts_positive_numbers_only() {
        assert_eq!(parse_width(Some("100")), Some(100));
        assert_eq!(parse_width(Some(" 80 ")), Some(80));
        assert_eq!(parse_width(Some("0")), None);
        assert_eq!(parse_width(Some("wide")), None);
        assert_eq!(parse_width(None), None);
    }

    #[test]
    fn info_line_aligns_values() {
        no_color();
        assert_eq!(info_line("name:", "alpha"), "name:      alpha");
        assert_eq!(info_line("last used:", "never"), "last used: never");
    }

    #[test]
    fn format_doctor_reports_issues_and_hints() {
        no_color();
        let report = Report {
            issues: vec![
                Issue::StalePath {
                    name: "gone".to_owned(),
                    path: PathBuf::from("/tmp/gone"),
                },
                Issue::QuarantineFile(PathBuf::from("/data/projects.json.corrupt-1")),
            ],
        };
        let rendered = format_doctor(
            &report,
            Path::new("/data/projects.json"),
            2,
            None,
            &config::Status::Defaults { path: None },
            "json",
        );
        assert!(rendered.contains("index: /data/projects.json (2 projects)"));
        assert!(rendered.contains("issues (2):"));
        assert!(rendered.contains("stale: `gone` -> /tmp/gone"));
        assert!(rendered.contains("quarantine: /data/projects.json.corrupt-1"));
        assert!(rendered.contains("`--fix` repairs stale projects"));
        assert!(rendered.contains("`--refresh` updates stored git remotes"));
    }

    #[test]
    fn format_doctor_reports_a_clean_index() {
        no_color();
        let rendered = format_doctor(
            &Report::default(),
            Path::new("/data/projects.json"),
            1,
            None,
            &config::Status::Defaults { path: None },
            "json",
        );
        assert!(rendered.contains("1 project)"));
        assert!(rendered.contains("storage: json"));
        assert!(rendered.contains("no problems found"));
        assert!(!rendered.contains("--fix"));
    }

    #[test]
    fn format_doctor_describes_the_config_file() {
        no_color();

        let missing = format_doctor(
            &Report::default(),
            Path::new("/data/projects.json"),
            0,
            None,
            &config::Status::Defaults {
                path: Some(PathBuf::from("/home/user/.config/overdosecd/config.toml")),
            },
            "json",
        );
        assert!(
            missing.contains("config: /home/user/.config/overdosecd/config.toml (not found; built-in defaults apply)"),
            "rendered:\n{missing}"
        );

        let loaded = format_doctor(
            &Report::default(),
            Path::new("/data/projects.json"),
            0,
            Some(Path::new("/home/user")),
            &config::Status::Loaded {
                path: PathBuf::from("/home/user/.config/overdosecd/config.toml"),
            },
            "json",
        );
        assert!(
            loaded.contains("config: ~/.config/overdosecd/config.toml\n"),
            "rendered:\n{loaded}"
        );

        let broken = format_doctor(
            &Report {
                issues: vec![Issue::ConfigProblem {
                    path: PathBuf::from("/home/user/.config/overdosecd/config.toml"),
                    message: "unknown field `frquency_max`".to_owned(),
                }],
            },
            Path::new("/data/projects.json"),
            0,
            Some(Path::new("/home/user")),
            &config::Status::Invalid {
                path: PathBuf::from("/home/user/.config/overdosecd/config.toml"),
                message: "unknown field `frquency_max`".to_owned(),
            },
            "json",
        );
        assert!(
            broken.contains(
                "  - config: ~/.config/overdosecd/config.toml: unknown field `frquency_max`"
            ),
            "rendered:\n{broken}"
        );
    }

    #[test]
    fn format_projects_only_shows_used_columns() {
        no_color();
        let plain = Project::for_test("alpha", "/tmp/overdosecd-does-not-exist-alpha");
        let table = format_projects(&[plain], None, None);
        assert!(!table.contains("ALIASES"));
        assert!(!table.contains("TAGS"));
    }

    /// Column of `needle` within `line`, measured in terminal cells.
    fn column_of(line: &str, needle: &str) -> usize {
        let byte = line.find(needle).expect("needle in line");
        UnicodeWidthStr::width(&line[..byte])
    }

    #[test]
    fn format_projects_aligns_wide_characters() {
        no_color();
        let dir = tempfile::tempdir().expect("tempdir");
        let cjk = dir.path().join("日本語");
        let ascii = dir.path().join("alpha");
        std::fs::create_dir(&cjk).expect("create cjk dir");
        std::fs::create_dir(&ascii).expect("create ascii dir");

        let cjk_project = Project::for_test("日本語", cjk.to_str().expect("utf-8 path"));
        let ascii_project = Project::for_test("alpha", ascii.to_str().expect("utf-8 path"));
        let table = format_projects(&[cjk_project, ascii_project], None, None);

        let lines: Vec<&str> = table.lines().collect();
        let path_column = column_of(lines[0], "PATH");
        assert_eq!(column_of(lines[1], cjk.to_str().unwrap()), path_column);
        assert_eq!(column_of(lines[2], ascii.to_str().unwrap()), path_column);
        assert_eq!(column_of(lines[0], "NAME"), 0);
    }

    #[test]
    fn format_projects_without_width_keeps_full_paths() {
        no_color();
        let long = "/tmp/overdosecd-does-not-exist-very-long-project-directory-name";
        let table = format_projects(&[Project::for_test("long", long)], None, None);
        assert!(table.contains(long), "table was:\n{table}");
        assert!(!table.contains('…'));
    }

    #[test]
    fn format_projects_truncates_to_the_given_width() {
        no_color();
        let long = "/tmp/overdosecd-does-not-exist-very-long-project-directory-name";
        let table = format_projects(&[Project::for_test("long", long)], None, Some(40));
        for line in table.lines() {
            assert!(
                display_width(line) <= 40,
                "line wider than 40 cells: {line:?}"
            );
        }
        assert!(table.contains('…'), "table was:\n{table}");
        assert!(
            table.contains("name"),
            "the final path component should survive: \n{table}"
        );
    }

    #[test]
    fn format_projects_fits_a_narrow_terminal_with_all_columns() {
        no_color();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().expect("utf-8 temp path");
        let mut alpha = Project::for_test("alpha", path);
        alpha.aliases.push("first".into());
        alpha.tags.push("rust".into());
        let mut bravo = Project::for_test("bravado", path);
        bravo.aliases.push("second".into());
        bravo.tags.push("cli".into());

        let table = format_projects(&[alpha, bravo], None, Some(40));
        for line in table.lines() {
            assert!(
                display_width(line) <= 40,
                "line wider than 40 cells: {line:?}"
            );
        }
    }

    #[test]
    fn truncate_to_elides_the_middle() {
        assert_eq!(truncate_to("short", 10), "short");
        assert_eq!(truncate_to("anything", 0), "");
        assert_eq!(truncate_to("anything", 1), "…");

        let cut = truncate_to("a-very-long-project-name", 12);
        assert!(display_width(&cut) <= 12);
        assert!(cut.contains('…'));
        assert!(cut.starts_with("a-v"));
        assert!(cut.ends_with("ame"), "cut was {cut:?}");
    }

    #[test]
    fn truncation_counts_wide_characters() {
        let text = "日本語日本語日本語"; // 24 cells
        let cut = truncate_to(text, 11);
        assert!(display_width(&cut) <= 11, "cut was {cut:?}");
        assert!(cut.contains('…'));

        let head = take_prefix(text, 5);
        assert_eq!(head, "日本");
        let tail = take_suffix(text, 7);
        assert_eq!(tail, "日本語");
    }

    #[test]
    fn truncate_path_keeps_the_final_component() {
        let full = "/home/user/code/overdosecd";
        assert_eq!(truncate_path(full, 30), full);

        let cut = truncate_path("/home/user/code/projects/overdosecd", 20);
        assert!(display_width(&cut) <= 20);
        assert!(cut.ends_with("/overdosecd"), "cut was {cut:?}");
        assert!(cut.starts_with("/home…"), "cut was {cut:?}");

        // A final component with no room for a head still survives.
        let cut = truncate_path("/a/b/a-component-that-is-long", 14);
        assert!(display_width(&cut) <= 14);
        assert!(cut.contains('…'), "cut was {cut:?}");
    }

    #[test]
    fn shrink_to_budget_respects_floors_before_giving_up() {
        let mut widths = vec![10, 40, 10];
        let floors = vec![6, 12, usize::MAX];
        shrink_to_budget(&mut widths, &floors, &[1, 0, 2], 40);
        assert_eq!(
            widths,
            vec![10, 16, 10],
            "the path gives up space first, and LAST USED never shrinks"
        );
    }

    #[test]
    fn the_output_funnel_refuses_foreign_sequences() {
        assert!(print_line("plain").is_ok());
        // Colour changes are the only escapes overdosecd itself emits.
        assert!(print_line("\u{1b}[31mred\u{1b}[0m").is_ok());
        for bad in [
            "title\u{1b}]0;pwned\u{7}",
            "hyperlink\u{1b}]8;;http://evil.example\u{1b}\\",
            "clear\u{1b}[2J",
            "bell\u{7}",
        ] {
            assert!(
                matches!(print_line(bad), Err(Error::UnsafeOutput)),
                "the funnel must refuse {bad:?}"
            );
        }
    }
}
