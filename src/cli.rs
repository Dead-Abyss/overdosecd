use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::config::ColorMode;
use crate::project::Kind;

#[derive(Debug, Parser)]
#[command(
    name = "overdosecd",
    version,
    about = "A smart directory and project jumper for the terminal"
)]
pub struct Cli {
    /// Override the data directory (default: $OVERDOSECD_DATA_DIR or the platform data dir)
    #[arg(long, global = true, value_name = "PATH")]
    pub data_dir: Option<PathBuf>,

    /// When to colorize output (default: $OVERDOSECD_COLOR or the config file)
    #[arg(long, global = true, value_enum, value_name = "WHEN")]
    pub color: Option<ColorMode>,

    /// Print every ranked candidate for a query to stderr, with scores
    #[arg(long, global = true)]
    pub debug: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Add a directory to the project index
    Add(AddArgs),

    /// List indexed projects
    List(ListArgs),

    /// Print the path of the best match (used by the ocd shell function)
    Goto {
        /// Project name, alias, tag, or multi-word query
        #[arg(required = true, value_name = "QUERY")]
        query: Vec<String>,

        /// Refuse fuzzy-only matches: exit 1 instead of jumping, so the
        /// caller can ask (the `ocd` wrapper opens the picker)
        #[arg(long)]
        confident: bool,

        /// Do not record this jump in usage statistics
        #[arg(long)]
        no_track: bool,
    },

    /// Open the interactive picker and print the selected path (used by `ocd`)
    Ui(UiArgs),

    /// Show details for a project
    Info {
        /// Project name, alias, tag, or multi-word query
        #[arg(required = true, value_name = "QUERY")]
        query: Vec<String>,
    },

    /// Remove a project from the index
    Remove(RemoveArgs),

    /// Print a shell function that turns `ocd` into a jump command
    Init {
        /// Shell to generate the wrapper for
        #[arg(value_enum)]
        shell: Shell,
    },

    /// Pin a project so it ranks first
    Pin {
        /// Project name, alias, tag, or multi-word query
        #[arg(required = true, value_name = "QUERY")]
        query: Vec<String>,
    },

    /// Unpin a project
    Unpin {
        /// Project name, alias, tag, or multi-word query
        #[arg(required = true, value_name = "QUERY")]
        query: Vec<String>,
    },

    /// Rename a project
    Rename {
        /// Current project name, alias, tag, or fuzzy query
        old: String,

        /// New project name
        new: String,

        /// Rename even when another project already has the new name
        #[arg(long)]
        force: bool,
    },

    /// Manage project aliases
    Alias {
        #[command(subcommand)]
        action: TermCommand,
    },

    /// Manage project tags
    Tag {
        #[command(subcommand)]
        action: TermCommand,
    },

    /// Check the index for problems and repair stale projects
    Doctor(DoctorArgs),

    /// Scan $HOME for directories the picker can offer when the index has no
    /// match
    Scan(ScanArgs),

    /// Import projects.json into a new SQLite database (projects.db)
    Migrate,

    /// Import an existing directory jumper's database (zoxide, autojump, zsh-z)
    Import(ImportArgs),

    /// Print a shell completion script for the `overdosecd` command itself
    Completions {
        /// Shell to generate the completion script for
        #[arg(value_enum)]
        shell: Shell,
    },

    /// Print jump-query completion candidates (used by shell completion)
    #[command(hide = true)]
    Complete {
        /// Prefix to complete; empty lists every candidate
        #[arg(default_value = "")]
        prefix: String,
    },

    /// Record a jump for the current directory (used by the optional cd hook)
    #[command(hide = true)]
    Hook,
}

/// The sub-actions `alias` and `tag` share; the parent command picks the
/// [`TermKind`](crate::project::TermKind) the value belongs to.
#[derive(Debug, Subcommand)]
pub enum TermCommand {
    /// Add an alias or tag to a project
    Add {
        /// Project name, alias, tag, or fuzzy query
        project: String,
        /// Alias or tag to add
        term: String,
    },

    /// Remove an alias or tag from a project
    Remove {
        /// Project name, alias, tag, or fuzzy query
        project: String,
        /// Alias or tag to remove
        term: String,
    },
}

#[derive(Debug, Args)]
pub(crate) struct UiArgs {
    /// Prefill the search box with this query
    #[arg(long, value_name = "TEXT")]
    pub query: Option<String>,

    /// Do not record the jump in usage statistics
    #[arg(long)]
    pub no_track: bool,
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// Directory to add (defaults to the current directory)
    pub path: Option<PathBuf>,

    /// Project name (defaults to the directory name)
    #[arg(long)]
    pub name: Option<String>,

    /// Extra name to match against; repeatable
    #[arg(long, value_name = "ALIAS")]
    pub alias: Vec<String>,

    /// Tag; repeatable
    #[arg(long, value_name = "TAG")]
    pub tag: Vec<String>,

    /// Update the existing entry when the path is already indexed
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Sort order: `used` (pinned, then recently used), `name`, or `created`
    #[arg(long, value_enum)]
    pub sort: Option<SortBy>,

    /// Only list projects of this type: `rust`, `node`, `python`, `go`, or
    /// `unknown` (detected from marker files at `add`)
    #[arg(long = "type", value_enum, value_name = "TYPE")]
    pub kind: Option<Kind>,

    /// Only list projects jumped to since WHEN: a duration (7d, 48h, 2w) or a
    /// date (2026-09-01)
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,

    /// Include projects whose directory is missing
    #[arg(long)]
    pub all: bool,

    /// Print the index as JSON
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SortBy {
    Name,
    Used,
    Created,
}

#[derive(Debug, Args)]
pub struct RemoveArgs {
    /// Project name, alias, tag, or multi-word query
    #[arg(required = true, value_name = "QUERY")]
    pub query: Vec<String>,

    /// Skip the confirmation prompt
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// Which jumper's store to import
    #[arg(value_enum)]
    pub source: ImportSource,

    /// Show what would be imported, and write nothing
    #[arg(long)]
    pub dry_run: bool,

    /// Drop entries whose mapped use count is below this value
    #[arg(long, value_name = "N")]
    pub min_score: Option<f64>,

    /// Import at most this many entries (highest scores first)
    #[arg(long, value_name = "N")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ImportSource {
    /// zoxide's `db.zo` (bincode format v3)
    Zoxide,
    /// autojump's `autojump.txt` (weight and path, tab separated)
    Autojump,
    /// zsh-z's data file, the rupa/z `path|rank|epoch` format
    ZshZ,
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Interactively relocate or remove projects whose directory is missing
    #[arg(long)]
    pub fix: bool,

    /// Re-read the stored metadata (git remote, project kind) for every
    /// indexed project
    #[arg(long)]
    pub refresh: bool,
}

#[derive(Debug, Args)]
#[command(group = clap::ArgGroup::new("target").required(true).args(["roots", "home"]))]
pub struct ScanArgs {
    /// Directories to walk and preview (writes neither the cache nor the index)
    #[arg(value_name = "ROOT")]
    pub roots: Vec<PathBuf>,

    /// Scan $HOME into the discovery cache
    #[arg(long)]
    pub home: bool,

    /// Rescan even when the cached directory list is still fresh
    #[arg(long, conflicts_with = "roots")]
    pub refresh: bool,

    /// Count what a scan would find without writing the cache
    #[arg(long, conflicts_with = "roots")]
    pub dry_run: bool,

    /// Override `[discovery] max_depth` for this run
    #[arg(long, value_name = "N")]
    pub max_depth: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
