//! `overdosecd enter`: the wrapper's post-`cd` step.
//!
//! Applies the project's profile — exported variables and the run-on-enter
//! command — by printing shell code for the wrapper to `eval` (bash/zsh) or
//! `source` (fish). Only reached when `[general] hooks` is on, never by the
//! raw `goto`/`ui` commands, and every failure is a warning on stderr with an
//! empty stdout and exit 0: a jump must never break on this.

use std::path::Path;

use crate::cli::Shell;
use crate::error::Result;
use crate::paths;
use crate::project;
use crate::sanitize;
use crate::store::sqlite::{ProjectProfile, SqliteStore};

/// The one rule for an exported name: a shell identifier, nothing else. A
/// quoted *value* is safe; a quoted name is not a name.
pub(crate) fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Single-quotes `value` for bash and zsh (`'` closes and reopens).
fn bash_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Single-quotes `value` for fish (only `\` and `'` are special inside).
fn fish_quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The shell code for `profile`, or the reason it must not run.
///
/// A value a planted index smuggled in — a control character in a variable
/// name, a value, or the command — is refused here, never escaped and run.
fn render(shell: Shell, profile: &ProjectProfile) -> Result<String> {
    let mut out = String::new();
    for (name, value) in &profile.env {
        if !valid_env_name(name) {
            return Err(crate::error::Error::InvalidValue(format!(
                "`{}` is not a valid environment variable name",
                sanitize::text(name)
            )));
        }
        project::reject_control_chars("environment value", value)?;
        let quoted = match shell {
            Shell::Fish => fish_quote(value),
            Shell::Bash | Shell::Zsh => bash_quote(value),
        };
        match shell {
            Shell::Fish => out.push_str(&format!("set -gx {name} {quoted}\n")),
            Shell::Bash | Shell::Zsh => out.push_str(&format!("export {name}={quoted}\n")),
        }
    }

    if let Some(cmd) = &profile.enter_cmd {
        project::reject_control_chars("enter command", cmd)?;
        // The failure is warned about, not propagated: the jump already
        // happened, and the wrapper's `eval`/`source` must not fail with it.
        let guarded = match shell {
            Shell::Fish => format!(
                "{cmd}; or echo \"overdosecd: enter command failed (status $status)\" >&2\n"
            ),
            Shell::Bash | Shell::Zsh => format!(
                "{cmd} || printf 'overdosecd: enter command failed (status %s)\\n' \"$?\" >&2\n"
            ),
        };
        out.push_str(&guarded);
    }
    Ok(out)
}

/// Applies the profile for `path` to the wrapper's shell. Every failure is a
/// warning on stderr and nothing on stdout.
pub fn apply(shell: Shell, path: &Path, data_dir: &Path) {
    let warn = |message: String| {
        let _ = crate::output::print_stderr_line(&format!("overdosecd: {message}"));
    };

    // The wrapper passes the path `goto`/`ui` just printed, so it is the
    // stored canonical form; anything else simply has no profile.
    let Some(raw) = path.to_str() else {
        return;
    };
    if !path.is_absolute() {
        return;
    }

    let store = SqliteStore::new(paths::sqlite_file(data_dir));
    let profile = match store.profile_for_path(raw) {
        Ok(Some(profile)) => profile,
        Ok(None) => return,
        Err(err) => {
            warn(format!("could not read the index: {err}"));
            return;
        }
    };
    if profile.env.is_empty() && profile.enter_cmd.is_none() {
        return;
    }

    let code = match render(shell, &profile) {
        Ok(code) => code,
        Err(err) => {
            warn(format!("refusing the profile for {raw}: {err}"));
            return;
        }
    };

    // The command is shown before the wrapper runs it; the values above are
    // silent because `profile show` is where they are reviewed.
    if let Some(cmd) = &profile.enter_cmd {
        warn(format!("running: {}", sanitize::text(cmd)));
    }
    if let Err(err) = crate::output::print(&code) {
        warn(format!("could not emit the profile: {err}"));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn profile() -> ProjectProfile {
        ProjectProfile {
            env: BTreeMap::from([
                ("EDITOR".to_owned(), "vim".to_owned()),
                ("TRICKY".to_owned(), "it's a \"test\" $HOME `x`".to_owned()),
            ]),
            enter_cmd: Some("echo entered".to_owned()),
        }
    }

    #[test]
    fn names_must_be_shell_identifiers() {
        for good in ["FOO", "_x", "A1_b"] {
            assert!(valid_env_name(good), "{good}");
        }
        for bad in ["", "1FOO", "FO-O", "FO O", "fé", "FO;rm"] {
            assert!(!valid_env_name(bad), "{bad}");
        }
    }

    #[test]
    fn bash_renders_quoted_exports_and_a_guarded_command() {
        let code = render(Shell::Bash, &profile()).expect("renders");
        assert!(code.contains("export EDITOR='vim'\n"), "{code}");
        assert!(
            code.contains(r#"export TRICKY='it'\''s a "test" $HOME `x`'"#),
            "quotes and expansions stay literal: {code}"
        );
        assert!(
            code.contains("echo entered || printf 'overdosecd: enter command failed (status %s)"),
            "a failing command warns instead of failing the jump: {code}"
        );
    }

    #[test]
    fn fish_renders_its_own_spelling() {
        let code = render(Shell::Fish, &profile()).expect("renders");
        assert!(code.contains("set -gx EDITOR 'vim'\n"), "{code}");
        assert!(
            code.contains("set -gx TRICKY 'it\\'s a \"test\" $HOME `x`'"),
            "fish escaping: {code}"
        );
        assert!(
            code.contains("echo entered; or echo \"overdosecd: enter command failed"),
            "{code}"
        );
    }

    #[test]
    fn planted_values_are_refused_not_escaped() {
        let mut bad = profile();
        bad.env.insert("BAD NAME".to_owned(), "x".to_owned());
        assert!(render(Shell::Bash, &bad).is_err());

        let mut bad = profile();
        bad.env.insert("OK".to_owned(), "line\nbreak".to_owned());
        assert!(render(Shell::Bash, &bad).is_err());

        let mut bad = profile();
        bad.enter_cmd = Some("rm -rf /\u{1b}[31m".to_owned());
        assert!(render(Shell::Bash, &bad).is_err());
    }
}
