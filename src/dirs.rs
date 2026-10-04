//! Platform directory resolution, hand-rolled so the unmaintained
//! `directories` crate is not needed.
//!
//! Linux only: `$XDG_DATA_HOME` or `~/.local/share`, and `$XDG_CONFIG_HOME`
//! or `~/.config`. A relative `XDG_*` value is ignored, as the XDG spec
//! requires.
//!
//! Recorded trade-off: an unset `HOME` errors instead of resolving through
//! `getpwuid`, so the caller gets `None` and reports a clear error rather
//! than a silently resolved home.

use std::ffi::OsString;
use std::path::PathBuf;

/// Environment values the layout is built from, resolved once.
#[derive(Debug, Clone)]
pub struct Layout {
    home: PathBuf,
    xdg_data: Option<PathBuf>,
    xdg_config: Option<PathBuf>,
}

impl Layout {
    /// Reads the process environment; `None` when no home directory can be
    /// determined.
    pub(crate) fn discover() -> Option<Self> {
        let home = home_dir()?;
        Some(Self {
            home,
            xdg_data: absolute_env("XDG_DATA_HOME"),
            xdg_config: absolute_env("XDG_CONFIG_HOME"),
        })
    }

    /// Test constructor with explicit values.
    #[cfg(test)]
    fn for_test(home: PathBuf) -> Self {
        Self {
            home,
            xdg_data: None,
            xdg_config: None,
        }
    }

    /// The per-application data directory (`projects.json` lives here).
    pub fn data_dir(&self, app: &str) -> PathBuf {
        self.base_data().join(app)
    }

    /// The per-application config directory (`config.toml` lives here).
    pub fn config_dir(&self, app: &str) -> PathBuf {
        self.xdg_config
            .clone()
            .unwrap_or_else(|| self.home.join(".config"))
            .join(app)
    }

    /// The base local data directory, without an application segment; this is
    /// where zoxide keeps `db.zo`.
    pub fn data_local_dir(&self) -> PathBuf {
        self.base_data()
    }

    /// `$XDG_DATA_HOME` or `~/.local/share`.
    fn base_data(&self) -> PathBuf {
        self.xdg_data
            .clone()
            .unwrap_or_else(|| self.home.join(".local").join("share"))
    }
}

/// The user's home directory: `$HOME`.
pub fn home_dir() -> Option<PathBuf> {
    os_env("HOME").map(PathBuf::from)
}

/// `~/.local/share/overdosecd`.
pub fn data_dir() -> Option<PathBuf> {
    Layout::discover().map(|layout| layout.data_dir("overdosecd"))
}

/// The overdosecd config directory (`~/.config/overdosecd`).
pub fn config_dir() -> Option<PathBuf> {
    Layout::discover().map(|layout| layout.config_dir("overdosecd"))
}

/// The local data directory used by third-party tools such as zoxide.
pub fn data_local_dir() -> Option<PathBuf> {
    Layout::discover().map(|layout| layout.data_local_dir())
}

/// A non-empty environment value.
pub(crate) fn os_env(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

/// A non-empty environment value that is an absolute path; relative XDG
/// values are ignored per the specification.
fn absolute_env(name: &str) -> Option<PathBuf> {
    os_env(name)
        .map(PathBuf::from)
        .filter(|value| value.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_dir_reads_the_home_variable() {
        // The test process always has a `HOME`; the value is whatever the
        // sandbox set, so assert the shape rather than a literal.
        let home = home_dir().expect("a home directory");
        assert!(home.is_absolute(), "home must be absolute: {home:?}");
    }

    #[test]
    fn relative_xdg_values_are_ignored() {
        assert_eq!(absolute_env("OVERDOSECD_TEST_UNSET_VARIABLE"), None);
    }

    #[test]
    fn layout_matches_the_documented_paths() {
        let layout = Layout::for_test(PathBuf::from("/home/user"));
        assert_eq!(
            layout.data_dir("overdosecd"),
            PathBuf::from("/home/user/.local/share/overdosecd")
        );
        assert_eq!(
            layout.config_dir("overdosecd"),
            PathBuf::from("/home/user/.config/overdosecd")
        );
        assert_eq!(
            layout.data_local_dir(),
            PathBuf::from("/home/user/.local/share")
        );

        let with_xdg = Layout {
            xdg_data: Some(PathBuf::from("/xdg/data")),
            xdg_config: Some(PathBuf::from("/xdg/config")),
            ..Layout::for_test(PathBuf::from("/home/user"))
        };
        assert_eq!(
            with_xdg.data_dir("overdosecd"),
            PathBuf::from("/xdg/data/overdosecd")
        );
        assert_eq!(
            with_xdg.config_dir("overdosecd"),
            PathBuf::from("/xdg/config/overdosecd")
        );
    }
}
