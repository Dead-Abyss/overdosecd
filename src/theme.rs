//! The theme: named roles mapped to colors the terminal understands.
//!
//! Colors are parsed into [`ColorSpec`] once at config load and rendered by
//! [`Theme`] only — a theme value can never put an arbitrary escape on the
//! terminal, and the `output` funnel still guards every stream. `base` is the
//! 0.3.0 look; `bleed` is the house theme and the default.

use std::str::FromStr;
use std::sync::OnceLock;

use colored::Colorize;
use ratatui::style::Color as TuiColor;
use serde::Deserialize;

use crate::error::{Error, Result};

/// The color roles every renderer draws through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Highlights, prompt markers, the cwd indicator.
    Accent,
    /// Secondary text and the picker frame in `base`.
    Dim,
    /// Success: clean doctor runs, a git repository.
    Good,
    /// Failures: stale entries, unsafe values.
    Bad,
    /// Warnings and pinned rows.
    Warn,
    /// Candidates that are not indexed yet (home rows).
    Info,
    /// The picker's border.
    Border,
    /// The picker's body text (`None` keeps the terminal's own foreground).
    Text,
}

/// A color from a theme file: `red`, `bright_black`, `color198`, or `#rrggbb`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpec {
    Ansi(NamedColor),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// The sixteen ANSI colors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
}

impl FromStr for NamedColor {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match value {
            "black" => Self::Black,
            "red" => Self::Red,
            "green" => Self::Green,
            "yellow" => Self::Yellow,
            "blue" => Self::Blue,
            "magenta" => Self::Magenta,
            "cyan" => Self::Cyan,
            "white" => Self::White,
            "bright_black" => Self::BrightBlack,
            "bright_red" => Self::BrightRed,
            "bright_green" => Self::BrightGreen,
            "bright_yellow" => Self::BrightYellow,
            "bright_blue" => Self::BrightBlue,
            "bright_magenta" => Self::BrightMagenta,
            "bright_cyan" => Self::BrightCyan,
            "bright_white" => Self::BrightWhite,
            _ => return Err(format!("unknown color name `{value}`")),
        })
    }
}

impl FromStr for ColorSpec {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if let Some(hex) = value.strip_prefix('#') {
            if hex.len() != 6 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
                return Err(format!("`{value}` is not #rrggbb"));
            }
            let channel = |index: usize| {
                u8::from_str_radix(&hex[index..index + 2], 16).expect("hex digits checked")
            };
            return Ok(Self::Rgb(channel(0), channel(2), channel(4)));
        }

        if let Some(index) = value.strip_prefix("color") {
            let index: u8 = index
                .parse()
                .map_err(|_| format!("`{value}` is not color0..color255"))?;
            return Ok(Self::Indexed(index));
        }

        value.parse().map(Self::Ansi)
    }
}

impl<'de> Deserialize<'de> for ColorSpec {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(serde::de::Error::custom)
    }
}

impl ColorSpec {
    fn colored(self) -> colored::Color {
        match self {
            Self::Ansi(name) => match name {
                NamedColor::Black => colored::Color::Black,
                NamedColor::Red => colored::Color::Red,
                NamedColor::Green => colored::Color::Green,
                NamedColor::Yellow => colored::Color::Yellow,
                NamedColor::Blue => colored::Color::Blue,
                NamedColor::Magenta => colored::Color::Magenta,
                NamedColor::Cyan => colored::Color::Cyan,
                NamedColor::White => colored::Color::White,
                NamedColor::BrightBlack => colored::Color::BrightBlack,
                NamedColor::BrightRed => colored::Color::BrightRed,
                NamedColor::BrightGreen => colored::Color::BrightGreen,
                NamedColor::BrightYellow => colored::Color::BrightYellow,
                NamedColor::BrightBlue => colored::Color::BrightBlue,
                NamedColor::BrightMagenta => colored::Color::BrightMagenta,
                NamedColor::BrightCyan => colored::Color::BrightCyan,
                NamedColor::BrightWhite => colored::Color::BrightWhite,
            },
            Self::Indexed(index) => colored::Color::AnsiColor(index),
            Self::Rgb(r, g, b) => colored::Color::TrueColor { r, g, b },
        }
    }

    /// ratatui spells the bright colors differently from the ANSI names.
    fn tui(self) -> TuiColor {
        match self {
            Self::Ansi(name) => match name {
                NamedColor::Black => TuiColor::Black,
                NamedColor::Red => TuiColor::Red,
                NamedColor::Green => TuiColor::Green,
                NamedColor::Yellow => TuiColor::Yellow,
                NamedColor::Blue => TuiColor::Blue,
                NamedColor::Magenta => TuiColor::Magenta,
                NamedColor::Cyan => TuiColor::Cyan,
                NamedColor::White => TuiColor::Gray,
                NamedColor::BrightBlack => TuiColor::DarkGray,
                NamedColor::BrightRed => TuiColor::LightRed,
                NamedColor::BrightGreen => TuiColor::LightGreen,
                NamedColor::BrightYellow => TuiColor::LightYellow,
                NamedColor::BrightBlue => TuiColor::LightBlue,
                NamedColor::BrightMagenta => TuiColor::LightMagenta,
                NamedColor::BrightCyan => TuiColor::LightCyan,
                NamedColor::BrightWhite => TuiColor::White,
            },
            Self::Indexed(index) => TuiColor::Indexed(index),
            Self::Rgb(r, g, b) => TuiColor::Rgb(r, g, b),
        }
    }
}

/// The bundled palettes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeName {
    /// The 0.3.0 colors.
    Base,
    /// blackrose's rose / bone / ash palette.
    Bleed,
}

impl FromStr for ThemeName {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "base" => Ok(Self::Base),
            "bleed" => Ok(Self::Bleed),
            _ => Err(()),
        }
    }
}

/// The `[theme]` config table: a bundled theme plus per-role overrides.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeConfig {
    pub name: ThemeName,
    pub accent: Option<ColorSpec>,
    pub dim: Option<ColorSpec>,
    pub good: Option<ColorSpec>,
    pub bad: Option<ColorSpec>,
    pub warn: Option<ColorSpec>,
    pub info: Option<ColorSpec>,
    pub border: Option<ColorSpec>,
    pub text: Option<ColorSpec>,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: ThemeName::Bleed,
            accent: None,
            dim: None,
            good: None,
            bad: None,
            warn: None,
            info: None,
            border: None,
            text: None,
        }
    }
}

/// A resolved palette. Every renderer draws through this, never through a raw
/// color string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    accent: ColorSpec,
    dim: ColorSpec,
    good: ColorSpec,
    bad: ColorSpec,
    warn: ColorSpec,
    info: ColorSpec,
    border: ColorSpec,
    text: Option<ColorSpec>,
}

impl Theme {
    /// The 0.3.0 look.
    pub fn base() -> Self {
        Self {
            accent: ColorSpec::Ansi(NamedColor::Cyan),
            dim: ColorSpec::Ansi(NamedColor::BrightBlack),
            good: ColorSpec::Ansi(NamedColor::Green),
            bad: ColorSpec::Ansi(NamedColor::Red),
            warn: ColorSpec::Ansi(NamedColor::Yellow),
            info: ColorSpec::Ansi(NamedColor::Blue),
            border: ColorSpec::Ansi(NamedColor::BrightBlack),
            text: None,
        }
    }

    /// blackrose's flagship palette: rose / bone / ash / ember / leaf.
    pub fn bleed() -> Self {
        Self {
            accent: ColorSpec::Rgb(0xe0, 0x45, 0x5e),
            dim: ColorSpec::Rgb(0x6b, 0x5a, 0x5f),
            good: ColorSpec::Rgb(0x7e, 0xe7, 0x87),
            bad: ColorSpec::Rgb(0xe0, 0x45, 0x5e),
            warn: ColorSpec::Rgb(0xff, 0x8a, 0x5c),
            info: ColorSpec::Rgb(0xff, 0x8a, 0x5c),
            border: ColorSpec::Rgb(0xe0, 0x45, 0x5e),
            text: Some(ColorSpec::Rgb(0xf5, 0xe9, 0xe2)),
        }
    }

    fn spec(&self, role: Role) -> Option<ColorSpec> {
        match role {
            Role::Accent => Some(self.accent),
            Role::Dim => Some(self.dim),
            Role::Good => Some(self.good),
            Role::Bad => Some(self.bad),
            Role::Warn => Some(self.warn),
            Role::Info => Some(self.info),
            Role::Border => Some(self.border),
            Role::Text => self.text,
        }
    }

    /// The ratatui color for a role; `None` means "leave the cell alone".
    pub fn tui(&self, role: Role) -> Option<TuiColor> {
        self.spec(role).map(ColorSpec::tui)
    }

    /// Paints CLI text. `colored`'s global override turns this into a no-op
    /// for `never` and `NO_COLOR`, exactly like every other colored string.
    pub fn paint(&self, role: Role, text: &str) -> String {
        match self.spec(role) {
            Some(color) => text.color(color.colored()).to_string(),
            None => text.to_owned(),
        }
    }

    fn overridden(mut self, config: &ThemeConfig) -> Self {
        self.accent = config.accent.unwrap_or(self.accent);
        self.dim = config.dim.unwrap_or(self.dim);
        self.good = config.good.unwrap_or(self.good);
        self.bad = config.bad.unwrap_or(self.bad);
        self.warn = config.warn.unwrap_or(self.warn);
        self.info = config.info.unwrap_or(self.info);
        self.border = config.border.unwrap_or(self.border);
        self.text = config.text.or(self.text);
        self
    }
}

/// Resolves `$OVERDOSECD_THEME` over `[theme] name`, then applies inline
/// overrides — the same flag > env > config shape as the color mode.
pub fn resolve(configured: &ThemeConfig, env: Option<&str>) -> Result<Theme> {
    let name = match env {
        None => configured.name,
        Some(value) => ThemeName::from_str(value).map_err(|()| Error::Config {
            origin: "$OVERDOSECD_THEME".to_owned(),
            message: format!("expected base or bleed (got `{value}`)"),
        })?,
    };
    let theme = match name {
        ThemeName::Base => Theme::base(),
        ThemeName::Bleed => Theme::bleed(),
    };
    Ok(theme.overridden(configured))
}

/// The process-wide theme, installed once in [`run`](crate::run) before any
/// output; the `colored` override plays the same role for on/off.
static ACTIVE: OnceLock<Theme> = OnceLock::new();

pub fn install(theme: Theme) {
    let _ = ACTIVE.set(theme);
}

pub fn active() -> &'static Theme {
    ACTIVE.get_or_init(Theme::bleed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_parse_in_all_three_forms() {
        assert_eq!(
            "bright_black".parse(),
            Ok(ColorSpec::Ansi(NamedColor::BrightBlack))
        );
        assert_eq!("color198".parse(), Ok(ColorSpec::Indexed(198)));
        assert_eq!("#E0455E".parse(), Ok(ColorSpec::Rgb(0xe0, 0x45, 0x5e)));
    }

    #[test]
    fn malformed_colors_are_refused() {
        for value in ["purple", "#e0455", "#gggggg", "color256", "colour1", "#"] {
            assert!(value.parse::<ColorSpec>().is_err(), "{value} parsed");
        }
    }

    #[test]
    fn bleed_is_the_default_and_base_is_explicit() {
        let default = ThemeConfig::default();
        assert_eq!(default.name, ThemeName::Bleed);
        assert_eq!(resolve(&default, None).expect("resolves"), Theme::bleed());

        let config = ThemeConfig {
            name: ThemeName::Base,
            ..ThemeConfig::default()
        };
        assert_eq!(resolve(&config, None).expect("resolves"), Theme::base());
    }

    #[test]
    fn the_environment_picks_the_name_and_overrides_still_apply() {
        let config = ThemeConfig {
            accent: Some("#010203".parse().unwrap()),
            ..ThemeConfig::default()
        };

        let env = resolve(&config, Some("base")).expect("resolves");
        assert_eq!(env.accent, ColorSpec::Rgb(1, 2, 3));
        assert_eq!(env.dim, Theme::base().dim);

        let err = resolve(&config, Some("neon")).expect_err("rejected");
        assert_eq!(err.exit_code(), 3);
        assert!(err.to_string().contains("OVERDOSECD_THEME"), "{err}");
    }

    #[test]
    fn theme_config_parses_and_rejects_unknown_roles() {
        let config: ThemeConfig =
            toml::from_str("name = \"base\"\naccent = \"magenta\"\n").expect("theme parses");
        assert_eq!(config.name, ThemeName::Base);
        assert_eq!(config.accent, Some(ColorSpec::Ansi(NamedColor::Magenta)));

        let err =
            toml::from_str::<ThemeConfig>("shadow = \"red\"\n").expect_err("unknown role rejected");
        assert!(err.to_string().contains("shadow"), "{err}");
    }
}
