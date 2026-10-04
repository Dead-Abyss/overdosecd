use std::fs;
use std::io;
use std::path::PathBuf;

use clap::ValueEnum;
use serde::Deserialize;

use crate::error::{Error, Result};

/// Environment variable pointing at an explicit config file.
const ENV_CONFIG: &str = "OVERDOSECD_CONFIG";

/// Environment variable overriding the color mode.
const ENV_COLOR: &str = "OVERDOSECD_COLOR";

/// When to colorize output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    /// Colorize when stdout is a terminal and `NO_COLOR` is unset.
    #[default]
    Auto,
    /// Always colorize, even when piped.
    Always,
    /// Never colorize.
    Never,
}

/// The parsed config file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub matching: Matching,
    pub storage: Storage,
    pub ui: Ui,
    pub discovery: Discovery,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct General {
    pub color: ColorMode,
    /// Install a `cd` wrapper (`overdosecd init`) that records a jump whenever
    /// a plain `cd` lands inside an indexed project.
    pub hook: bool,
    /// Count visits to directories that are not indexed and suggest `ocd add`
    /// after enough of them. Implies the wrapper too.
    pub hint: bool,
}

/// Which storage backend holds the index.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageBackend {
    /// `projects.json`, the pre-SQLite format.
    #[default]
    Json,
    /// `projects.db` (SQLite); `overdosecd migrate` moves an index over.
    Sqlite,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Storage {
    pub backend: StorageBackend,
}

/// How the interactive picker renders and behaves.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Ui {
    /// Height of the inline box in rows (at least 3; clamped to the terminal).
    pub height: u16,

    /// Draw a border around the box.
    pub border: bool,

    /// When the picker captures the mouse.
    pub mouse: MouseMode,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            height: 10,
            border: false,
            mouse: MouseMode::Auto,
        }
    }
}

/// When the picker captures the mouse.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MouseMode {
    /// Capture in a plain terminal, but not inside a multiplexer, where the
    /// capture takes over the multiplexer's own pane UI (tmux, zellij,
    /// screen).
    #[default]
    Auto,
    Always,
    Never,
}

/// Home-directory discovery: when the index has nothing for a query, the
/// picker may offer directories found under `$HOME`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Discovery {
    /// Search `$HOME` at all.
    pub home: bool,

    /// How deep the walk goes, relative to `$HOME`.
    pub max_depth: u16,

    /// Cap on cached directories; shallow and project-like ones win.
    pub max_entries: usize,

    /// Age after which the picker refreshes the cache in the background.
    pub ttl_hours: u32,

    /// Directory names to skip, added to the built-in list.
    pub skip: Vec<String>,
}

impl Default for Discovery {
    fn default() -> Self {
        Self {
            home: true,
            max_depth: 8,
            max_entries: 50_000,
            ttl_hours: 24,
            skip: Vec::new(),
        }
    }
}

impl Discovery {
    /// The scan options this config describes.
    pub fn scan_options(&self) -> crate::discovery::Options {
        crate::discovery::Options {
            max_depth: self.max_depth,
            max_entries: self.max_entries,
            skip: self.skip.clone(),
        }
    }
}

/// The matcher knobs, defaulting to the values hardcoded before this file
/// existed. Nothing here can promote a weak match above a stronger one; the
/// knobs tune bonuses and the ambiguity guard, not the match classes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Matching {
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

impl Default for Matching {
    fn default() -> Self {
        Self {
            fuzzy: true,
            recency_max: 800,
            frequency_max: 400,
            pinned_bonus: 1_500,
            ambiguity_margin: 150,
            depth_penalty: 20,
        }
    }
}

/// Where the config file would be read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// The file to read, when one could be determined.
    pub path: Option<PathBuf>,

    /// True when the path came from `$OVERDOSECD_CONFIG`; a missing file is then
    /// an error instead of a silent fallback.
    pub explicit: bool,
}

/// Locates the config file: `$OVERDOSECD_CONFIG` wins over the platform path.
pub fn locate(env: Option<PathBuf>, platform: Option<PathBuf>) -> Location {
    match env {
        Some(path) => Location {
            path: Some(path),
            explicit: true,
        },
        None => Location {
            path: platform,
            explicit: false,
        },
    }
}

/// The `$OVERDOSECD_CONFIG` value, if it is set and non-empty.
pub fn env_path() -> Option<PathBuf> {
    crate::dirs::os_env(ENV_CONFIG).map(PathBuf::from)
}

/// The `$OVERDOSECD_COLOR` value, if it is set and non-empty.
pub fn env_color() -> Option<String> {
    crate::dirs::os_env(ENV_COLOR).map(|value| value.to_string_lossy().into_owned())
}

/// The platform config path (`~/.config/overdosecd/config.toml` on Linux).
pub fn platform_path() -> Option<PathBuf> {
    crate::dirs::config_dir().map(|dir| dir.join("config.toml"))
}

/// Reads the config file.
///
/// A missing file at the platform path is not an error; built-in defaults
/// apply. A missing file that `$OVERDOSECD_CONFIG` pointed at is, because the
/// user asked for that file explicitly.
pub fn load(location: &Location) -> Result<Config> {
    let Some(path) = &location.path else {
        return Ok(Config::default());
    };

    // A relative path resolves against the current directory, so a planted
    // directory could choose the settings (backend, skip lists, hook). The
    // environment value is the one that can be relative in practice; a
    // relative platform path means a broken home directory, and is refused
    // for the same reason.
    let kind = if location.explicit {
        "the config file from $OVERDOSECD_CONFIG"
    } else {
        "the platform config path"
    };
    crate::paths::absolute(kind, path)?;

    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            if location.explicit {
                return Err(Error::ConfigNotFound(path.clone()));
            }
            return Ok(Config::default());
        }
        Err(err) => {
            return Err(Error::Config {
                origin: path.display().to_string(),
                message: err.to_string(),
            });
        }
    };

    parse(&text).map_err(|message| Error::Config {
        origin: path.display().to_string(),
        message,
    })
}

/// What `doctor` reports about the config file, without failing on problems.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// No file to read; built-in defaults apply.
    Defaults { path: Option<PathBuf> },

    /// The file was read and parsed.
    Loaded { path: PathBuf },

    /// The file (or `$OVERDOSECD_CONFIG`) is unusable.
    Invalid { path: PathBuf, message: String },
}

/// Classifies an already-loaded config, for the `doctor` report.
pub fn status(location: &Location, loaded: Result<Config>) -> Status {
    let Some(path) = &location.path else {
        return Status::Defaults { path: None };
    };

    match loaded {
        Ok(_) if path.exists() => Status::Loaded { path: path.clone() },
        Ok(_) => Status::Defaults {
            path: Some(path.clone()),
        },
        // The report already prints the path, so keep only the reason.
        Err(Error::Config { message, .. }) => Status::Invalid {
            path: path.clone(),
            message,
        },
        Err(Error::ConfigNotFound(_)) => Status::Invalid {
            path: path.clone(),
            message: format!("not found (from {ENV_CONFIG})"),
        },
        Err(err) => Status::Invalid {
            path: path.clone(),
            message: err.to_string(),
        },
    }
}

/// Resolves the effective color mode: flag, then `$OVERDOSECD_COLOR`, then the
/// config file, then `auto`.
pub fn resolve_color(
    flag: Option<ColorMode>,
    env: Option<&str>,
    configured: ColorMode,
) -> Result<ColorMode> {
    if let Some(mode) = flag {
        return Ok(mode);
    }
    if let Some(value) = env {
        return ColorMode::from_str(value, true).map_err(|_| Error::Config {
            origin: format!("${ENV_COLOR}"),
            message: format!("expected auto, always, or never (got `{value}`)"),
        });
    }
    Ok(configured)
}

/// Applies a resolved color mode to the global `colored` state.
pub fn apply_color(mode: ColorMode) {
    match mode {
        // `colored` already reads the tty and `NO_COLOR` on its own.
        ColorMode::Auto => {}
        ColorMode::Always => colored::control::set_override(true),
        ColorMode::Never => colored::control::set_override(false),
    }
}

fn parse(text: &str) -> std::result::Result<Config, String> {
    let config: Config = toml::from_str(text).map_err(|err| err.message().to_owned())?;
    config.validate()?;
    Ok(config)
}

impl Config {
    fn validate(&self) -> std::result::Result<(), String> {
        let matching = &self.matching;
        for (name, value) in [
            ("matching.recency_max", matching.recency_max),
            ("matching.frequency_max", matching.frequency_max),
            ("matching.pinned_bonus", matching.pinned_bonus),
            ("matching.ambiguity_margin", matching.ambiguity_margin),
            ("matching.depth_penalty", matching.depth_penalty),
        ] {
            if value < 0 {
                return Err(format!("{name} must be >= 0 (got {value})"));
            }
        }
        if self.ui.height < 3 {
            return Err(format!(
                "ui.height must be at least 3 (got {})",
                self.ui.height
            ));
        }
        if self.discovery.max_depth < 1 {
            return Err("discovery.max_depth must be at least 1".to_owned());
        }
        if self.discovery.max_entries < 100 {
            return Err(format!(
                "discovery.max_entries must be at least 100 (got {})",
                self.discovery.max_entries
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        (dir, path)
    }

    #[test]
    fn empty_config_is_all_defaults() {
        let config = parse("").expect("empty config parses");
        assert_eq!(config, Config::default());
        assert_eq!(config.general.color, ColorMode::Auto);
        assert!(!config.general.hook, "the cd hook is opt-in");
        assert!(!config.general.hint, "visit hints are opt-in");
        assert!(config.matching.fuzzy);
        assert_eq!(config.matching.recency_max, 800);
        assert_eq!(config.matching.frequency_max, 400);
        assert_eq!(config.matching.pinned_bonus, 1_500);
        assert_eq!(config.matching.ambiguity_margin, 150);
        assert_eq!(config.matching.depth_penalty, 20);
        assert_eq!(config.ui.height, 10);
        assert!(!config.ui.border);
        assert_eq!(config.ui.mouse, MouseMode::Auto);
    }

    #[test]
    fn ui_settings_parse() {
        let config = parse("[ui]\nheight = 4\nborder = true\nmouse = \"always\"\n")
            .expect("ui config parses");
        assert_eq!(config.ui.height, 4);
        assert!(config.ui.border);
        assert_eq!(config.ui.mouse, MouseMode::Always);

        let err = parse("[ui]\nmouse = \"sometimes\"\n").expect_err("bad mouse mode");
        assert!(err.contains("sometimes"), "error was: {err}");
    }

    #[test]
    fn tiny_ui_heights_are_rejected() {
        let err = parse("[ui]\nheight = 2\n").expect_err("height must be constructible");
        assert!(err.contains("ui.height"), "error was: {err}");
    }

    #[test]
    fn partial_files_keep_the_other_defaults() {
        let config = parse("[matching]\nfuzzy = false\n").expect("partial config parses");
        assert!(!config.matching.fuzzy);
        assert_eq!(config.matching.depth_penalty, 20);
    }

    #[test]
    fn hook_and_hint_are_independent_switches() {
        let config = parse("[general]\nhook = true\n").expect("hook parses");
        assert!(config.general.hook);
        assert!(!config.general.hint);
        let config = parse("[general]\nhint = true\n").expect("hint parses");
        assert!(!config.general.hook);
        assert!(config.general.hint);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = parse("[matching]\nfrquency_max = 10\n").expect_err("typo should fail");
        assert!(err.contains("frquency_max"), "error was: {err}");
    }

    #[test]
    fn negative_weights_are_rejected() {
        let err = parse("[matching]\ndepth_penalty = -1\n").expect_err("negative should fail");
        assert!(err.contains("matching.depth_penalty"), "error was: {err}");
    }

    #[test]
    fn bad_color_values_are_rejected() {
        let err = parse("[general]\ncolor = \"sometimes\"\n").expect_err("bad color should fail");
        assert!(err.contains("sometimes"), "error was: {err}");
    }

    #[test]
    fn storage_backend_defaults_to_json_and_parses_sqlite() {
        let config = parse("").expect("empty config parses");
        assert_eq!(config.storage.backend, StorageBackend::Json);

        let config = parse("[storage]\nbackend = \"sqlite\"\n").expect("storage parses");
        assert_eq!(config.storage.backend, StorageBackend::Sqlite);
    }

    #[test]
    fn unknown_storage_backends_are_rejected() {
        let err = parse("[storage]\nbackend = \"csv\"\n").expect_err("unknown backend should fail");
        assert!(err.contains("csv"), "error was: {err}");
    }

    #[test]
    fn load_reads_an_existing_file() {
        let (_dir, path) = temp_path("config.toml");
        fs::write(&path, "[matching]\nambiguity_margin = 7\n").expect("write config");
        let location = Location {
            path: Some(path),
            explicit: true,
        };
        assert_eq!(
            load(&location)
                .expect("config loads")
                .matching
                .ambiguity_margin,
            7
        );
    }

    #[test]
    fn missing_platform_file_falls_back_to_defaults() {
        let (_dir, path) = temp_path("missing.toml");
        let location = Location {
            path: Some(path),
            explicit: false,
        };
        assert_eq!(load(&location).expect("missing is fine"), Config::default());
    }

    #[test]
    fn missing_explicit_file_is_an_error() {
        let (_dir, path) = temp_path("missing.toml");
        let location = Location {
            path: Some(path.clone()),
            explicit: true,
        };
        match load(&location) {
            Err(Error::ConfigNotFound(error_path)) => assert_eq!(error_path, path),
            other => panic!("expected ConfigNotFound, got {other:?}"),
        }
    }

    #[test]
    fn malformed_file_reports_the_path() {
        let (_dir, path) = temp_path("config.toml");
        fs::write(&path, "this is not toml =").expect("write config");
        let location = Location {
            path: Some(path.clone()),
            explicit: true,
        };
        match load(&location) {
            Err(Error::Config { origin, .. }) => assert_eq!(origin, path.display().to_string()),
            other => panic!("expected a config error, got {other:?}"),
        }
    }

    #[test]
    fn relative_env_config_is_refused() {
        let location = Location {
            path: Some(PathBuf::from("config.toml")),
            explicit: true,
        };
        let err = load(&location).expect_err("refused");
        assert!(matches!(err, Error::RelativePath { .. }), "{err:?}");

        // No path at all still means built-in defaults.
        assert!(
            load(&Location {
                path: None,
                explicit: false
            })
            .is_ok()
        );
    }

    #[test]
    fn locate_prefers_the_environment_path() {
        let env = PathBuf::from("/tmp/env-config.toml");
        let platform = PathBuf::from("/home/x/.config/overdosecd/config.toml");

        assert_eq!(
            locate(Some(env.clone()), Some(platform.clone())),
            Location {
                path: Some(env),
                explicit: true
            }
        );
        assert_eq!(
            locate(None, Some(platform.clone())),
            Location {
                path: Some(platform),
                explicit: false
            }
        );
        assert_eq!(
            locate(None, None),
            Location {
                path: None,
                explicit: false
            }
        );
    }

    #[test]
    fn color_precedence_is_flag_then_env_then_config() {
        assert_eq!(
            resolve_color(Some(ColorMode::Never), Some("always"), ColorMode::Always).unwrap(),
            ColorMode::Never
        );
        assert_eq!(
            resolve_color(None, Some("always"), ColorMode::Never).unwrap(),
            ColorMode::Always
        );
        assert_eq!(
            resolve_color(None, None, ColorMode::Never).unwrap(),
            ColorMode::Never
        );
        assert_eq!(
            resolve_color(None, None, ColorMode::Auto).unwrap(),
            ColorMode::Auto
        );
    }

    #[test]
    fn invalid_environment_color_is_a_config_error() {
        let err = resolve_color(None, Some("rainbow"), ColorMode::Auto).expect_err("bad env color");
        assert_eq!(err.exit_code(), 3);
        assert!(err.to_string().contains(ENV_COLOR), "error was: {err}");
    }

    #[test]
    fn status_distinguishes_defaults_loaded_and_invalid() {
        let (_dir, missing) = temp_path("missing.toml");
        let location = Location {
            path: Some(missing.clone()),
            explicit: false,
        };
        let defaults = status(&location, load(&location));
        assert!(matches!(defaults, Status::Defaults { .. }));

        let location = Location {
            path: Some(missing),
            explicit: true,
        };
        let explicit_missing = status(&location, load(&location));
        match explicit_missing {
            Status::Invalid { message, .. } => {
                assert!(message.contains("not found"), "message was: {message}");
                assert!(message.contains(ENV_CONFIG), "message was: {message}");
            }
            other => panic!("expected Invalid, got {other:?}"),
        }

        let (_dir, path) = temp_path("config.toml");
        fs::write(&path, "[general]\ncolor = \"never\"\n").expect("write config");
        let location = Location {
            path: Some(path.clone()),
            explicit: true,
        };
        let loaded = status(&location, load(&location));
        assert_eq!(loaded, Status::Loaded { path });

        let (_dir, broken) = temp_path("config.toml");
        fs::write(&broken, "= broken").expect("write config");
        let location = Location {
            path: Some(broken.clone()),
            explicit: true,
        };
        let invalid = status(&location, load(&location));
        match invalid {
            Status::Invalid { path, message } => {
                assert_eq!(path, broken);
                assert!(!message.is_empty());
            }
            other => panic!("expected Invalid, got {other:?}"),
        }
    }
}
