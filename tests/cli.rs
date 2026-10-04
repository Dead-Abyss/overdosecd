use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command as Cmd;
use predicates::prelude::*;
use tempfile::TempDir;

/// The storage backends every behavior test runs against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Json,
    Sqlite,
}

/// Runs a test body once per backend, binding the sandbox to `$sandbox`.
///
/// JSON is the default and needs no config file; SQLite is selected by
/// writing `[storage] backend = "sqlite"` into the sandbox config.
macro_rules! dual {
    ($sandbox:ident, $body:block) => {
        for backend in [Backend::Json, Backend::Sqlite] {
            let $sandbox = Sandbox::with_backend(backend);
            $body
        }
    };
}

struct Sandbox {
    /// Keeps the temporary directory alive; build paths from `root` instead.
    _tempdir: TempDir,
    /// Canonical form of the temp root. The CLI canonicalizes stored paths,
    /// so `HOME` and every expectation have to match that form.
    root: PathBuf,
    projects: PathBuf,
    data: PathBuf,
    backend: Backend,
    config_path: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        Self::with_backend(Backend::Json)
    }

    fn with_backend(backend: Backend) -> Self {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = canonical_path(tempdir.path());
        let projects = root.join("projects");
        fs::create_dir_all(&projects).expect("create projects dir");
        let data = root.join("data");
        let config_path = root.join("overdosecd-config.toml");
        let sandbox = Self {
            _tempdir: tempdir,
            root,
            projects,
            data,
            backend,
            config_path,
        };
        if backend == Backend::Sqlite {
            sandbox.write_config("");
        }
        sandbox
    }

    fn project(&self, name: &str) -> PathBuf {
        let path = self.projects.join(name);
        fs::create_dir_all(&path).expect("create project dir");
        path
    }

    /// The index file for this backend.
    fn index_file(&self) -> PathBuf {
        self.data.join(self.index_name())
    }

    fn index_name(&self) -> &'static str {
        match self.backend {
            Backend::Json => "projects.json",
            Backend::Sqlite => "projects.db",
        }
    }

    /// The config file the CLI should read, when the sandbox needs one:
    /// JSON is the built-in default, so only SQLite writes a config file.
    fn storage_config(&self) -> Option<&Path> {
        (self.backend == Backend::Sqlite).then_some(self.config_path.as_path())
    }

    fn cmd(&self) -> Cmd {
        Cmd::from_std(self.std_cmd())
    }

    /// The command in a fresh session: no controlling terminal, so `/dev/tty`
    /// is unavailable even when the suite runs in a terminal.
    #[cfg(unix)]
    fn ttyless_cmd(&self) -> Cmd {
        use std::os::unix::process::CommandExt;

        let mut raw = self.std_cmd();
        unsafe {
            raw.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        Cmd::from_std(raw)
    }

    /// The raw command with the sandbox environment applied, for tests that
    /// spawn the binary without assert_cmd helpers.
    fn std_cmd(&self) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_overdosecd"));
        command
            .env("OVERDOSECD_DATA_DIR", &self.data)
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("NO_COLOR", "1");
        if let Some(config) = self.storage_config() {
            command.env("OVERDOSECD_CONFIG", config);
        }
        command
    }

    /// Writes a config file and returns its path, meant to be passed via
    /// `OVERDOSECD_CONFIG` so the test does not depend on platform config paths.
    /// The sandbox's storage backend is kept, so a test body only supplies
    /// its own settings.
    fn write_config(&self, body: &str) -> PathBuf {
        let mut content = String::new();
        if self.backend == Backend::Sqlite {
            content.push_str("[storage]\nbackend = \"sqlite\"\n");
        }
        content.push_str(body);
        fs::write(&self.config_path, content).expect("write config file");
        self.config_path.clone()
    }

    fn add(&self, path: &Path) -> &Self {
        self.cmd().arg("add").arg(path).assert().success();
        self
    }

    fn canonical(&self, path: &Path) -> String {
        canonical_path(path).display().to_string()
    }

    /// Backdates a project's `last_used_at` on either backend, so `--since`
    /// windows can be tested without waiting.
    fn age_last_used(&self, name: &str, ago: chrono::Duration) {
        let when = chrono::Utc::now() - ago;
        let when = when.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);
        match self.backend {
            Backend::Json => {
                let path = self.index_file();
                let raw = fs::read_to_string(&path).expect("read json index");
                let mut value: serde_json::Value =
                    serde_json::from_str(&raw).expect("parse json index");
                for project in value["projects"].as_array_mut().expect("projects array") {
                    if project["name"] == name {
                        project["last_used_at"] = serde_json::Value::String(when.clone());
                    }
                }
                let body = serde_json::to_string_pretty(&value).expect("encode json index");
                fs::write(&path, body).expect("write json index");
            }
            Backend::Sqlite => {
                let conn = rusqlite::Connection::open(self.index_file()).expect("open database");
                conn.execute(
                    "UPDATE projects SET last_used_at = ?1 WHERE name = ?2",
                    rusqlite::params![when, name],
                )
                .expect("age the project");
            }
        }
    }
}

/// Resolves a path the way the CLI's `project::normalize` does: symlinks
/// resolved.
fn canonical_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).expect("canonicalize")
}

/// The `~`-shortened form the CLI prints for a path under the sandbox home:
/// `~/` plus the relative path with platform separators (the CLI's
/// `shorten_home` always prefixes `~/`, then displays the rest natively).
fn shortened(relative: &str) -> String {
    format!("~/{}", relative.replace('/', std::path::MAIN_SEPARATOR_STR))
}

#[test]
fn add_list_goto_info_remove_round_trip() {
    dual!(sandbox, {
        let project = sandbox.project("overdosecd");
        let canonical = sandbox.canonical(&project);

        sandbox
            .cmd()
            .arg("add")
            .arg(&project)
            .assert()
            .success()
            .stdout(predicate::str::contains("added `overdosecd`"));

        sandbox.cmd().arg("list").assert().success().stdout(
            predicate::str::contains("overdosecd")
                .and(predicate::str::contains(shortened("projects/overdosecd"))),
        );

        sandbox
            .cmd()
            .args(["goto", "overdosecd"])
            .assert()
            .success()
            .stdout(format!("{canonical}\n"));

        sandbox
            .cmd()
            .args(["info", "over"])
            .assert()
            .success()
            .stdout(predicate::str::contains("name:      overdosecd"));

        sandbox
            .cmd()
            .args(["remove", "overdosecd", "--yes"])
            .assert()
            .success()
            .stdout(predicate::str::contains("removed `overdosecd`"));

        sandbox
            .cmd()
            .arg("list")
            .assert()
            .success()
            .stdout(predicate::str::contains("no projects yet"));
    });
}

#[test]
fn add_without_path_uses_current_directory() {
    dual!(sandbox, {
        let project = sandbox.project("cwd-project");

        sandbox
            .cmd()
            .current_dir(&project)
            .arg("add")
            .assert()
            .success();

        sandbox
            .cmd()
            .args(["goto", "cwd-project"])
            .assert()
            .success()
            .stdout(format!("{}\n", sandbox.canonical(&project)));
    });
}

#[test]
fn add_expands_tilde_and_defaults_name() {
    dual!(sandbox, {
        let project = sandbox.project("tilde-project");

        sandbox
            .cmd()
            .args(["add", "~/projects/tilde-project"])
            .assert()
            .success()
            .stdout(predicate::str::contains("added `tilde-project`"));

        sandbox
            .cmd()
            .args(["goto", "tilde-project"])
            .assert()
            .success()
            .stdout(format!("{}\n", sandbox.canonical(&project)));
    });
}

#[test]
fn add_accepts_name_alias_and_tag() {
    dual!(sandbox, {
        let project = sandbox.project("real-directory");

        sandbox
            .cmd()
            .arg("add")
            .arg(&project)
            .args(["--name", "over", "--alias", "w", "--tag", "rust"])
            .assert()
            .success();

        sandbox
            .cmd()
            .args(["goto", "w"])
            .assert()
            .success()
            .stdout(format!("{}\n", sandbox.canonical(&project)));

        sandbox
            .cmd()
            .args(["goto", "rust"])
            .assert()
            .success()
            .stdout(format!("{}\n", sandbox.canonical(&project)));

        sandbox
            .cmd()
            .args(["info", "over"])
            .assert()
            .success()
            .stdout(
                predicate::str::contains("aliases:   w")
                    .and(predicate::str::contains("tags:      rust")),
            );
    });
}

#[test]
fn adding_a_missing_directory_fails() {
    dual!(sandbox, {
        let missing = sandbox.root.join("not-here");

        sandbox
            .cmd()
            .arg("add")
            .arg(&missing)
            .assert()
            .failure()
            .code(1)
            .stderr(predicate::str::contains("directory does not exist"));
    });
}

#[test]
fn duplicate_path_is_rejected_unless_forced() {
    dual!(sandbox, {
        let project = sandbox.project("overdosecd");
        sandbox.add(&project);

        sandbox
            .cmd()
            .arg("add")
            .arg(&project)
            .assert()
            .failure()
            .code(1)
            .stderr(predicate::str::contains("already indexed as `overdosecd`"));

        sandbox
            .cmd()
            .arg("add")
            .arg(&project)
            .args(["--force", "--name", "over-renamed"])
            .assert()
            .success()
            .stdout(predicate::str::contains("updated `over-renamed`"));

        sandbox
            .cmd()
            .args(["goto", "over-renamed"])
            .assert()
            .success();
    });
}

#[test]
fn ambiguous_query_lists_candidates_with_exit_1() {
    dual!(sandbox, {
        sandbox.add(&sandbox.project("app-one"));
        sandbox.add(&sandbox.project("app-two"));

        sandbox
            .cmd()
            .args(["goto", "app"])
            .assert()
            .failure()
            .code(1)
            .stderr(
                predicate::str::contains("multiple projects matched query: app")
                    .and(predicate::str::contains("app-one"))
                    .and(predicate::str::contains("app-two"))
                    .and(predicate::str::contains("name prefix"))
                    .and(predicate::str::contains(shortened("projects/app-one"))),
            );
    });
}

#[test]
fn unknown_query_fails_with_exit_1() {
    dual!(sandbox, {
        sandbox.add(&sandbox.project("overdosecd"));

        sandbox
            .cmd()
            .args(["goto", "qqqqzz"])
            .assert()
            .failure()
            .code(1)
            .stderr(predicate::str::contains("no project matched query: qqqqzz"));
    });
}

#[test]
fn exact_name_wins_over_prefix() {
    dual!(sandbox, {
        let exact = sandbox.project("over");
        sandbox.add(&exact);
        sandbox.add(&sandbox.project("overdosecd"));

        sandbox
            .cmd()
            .args(["goto", "over"])
            .assert()
            .success()
            .stdout(format!("{}\n", sandbox.canonical(&exact)));
    });
}

#[test]
fn goto_reports_missing_directory() {
    dual!(sandbox, {
        let project = sandbox.project("vanishing");
        sandbox.add(&project);
        fs::remove_dir_all(&project).expect("remove project dir");

        sandbox
            .cmd()
            .args(["goto", "vanishing"])
            .assert()
            .failure()
            .code(1)
            .stderr(predicate::str::contains("directory no longer exists"));
    });
}

#[test]
fn list_hides_missing_directories_unless_all() {
    dual!(sandbox, {
        let project = sandbox.project("vanishing");
        sandbox.add(&project);
        fs::remove_dir_all(&project).expect("remove project dir");

        sandbox
            .cmd()
            .arg("list")
            .assert()
            .success()
            .stdout(predicate::str::contains("no projects yet"));

        sandbox
            .cmd()
            .args(["list", "--all"])
            .assert()
            .success()
            .stdout(
                predicate::str::contains("vanishing")
                    .and(predicate::str::contains("directory missing")),
            );
    });
}

#[test]
fn list_json_contains_version_and_projects() {
    dual!(sandbox, {
        sandbox.add(&sandbox.project("overdosecd"));

        let output = sandbox.cmd().args(["list", "--json"]).output().unwrap();
        assert!(output.status.success());

        let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");
        assert_eq!(value["version"], 1);
        let projects = value["projects"].as_array().expect("projects array");
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0]["name"], "overdosecd");
        assert!(projects[0]["path"].is_string());
        assert!(projects[0]["aliases"].is_array());
    });
}

#[test]
fn piped_list_is_never_truncated() {
    let sandbox = Sandbox::new();
    let name = "a-directory-with-a-quite-long-name-so-the-table-would-not-fit-a-narrow-terminal";
    sandbox.add(&sandbox.project(name));

    // Not a tty: the full path survives, so scripts keep their data.
    sandbox
        .cmd()
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains(name).and(predicate::str::contains("…").not()));
}

#[test]
fn list_since_filters_by_last_used() {
    dual!(sandbox, {
        sandbox.add(&sandbox.project("ancient"));
        sandbox.add(&sandbox.project("fresh"));
        sandbox.cmd().args(["goto", "ancient"]).assert().success();
        sandbox.cmd().args(["goto", "fresh"]).assert().success();

        sandbox.age_last_used("ancient", chrono::Duration::days(10));

        sandbox
            .cmd()
            .args(["list", "--since", "7d"])
            .assert()
            .success()
            .stdout(
                predicate::str::contains("fresh").and(predicate::str::contains("ancient").not()),
            );

        sandbox
            .cmd()
            .args(["list", "--since", "30d"])
            .assert()
            .success()
            .stdout(predicate::str::contains("fresh").and(predicate::str::contains("ancient")));

        sandbox
            .cmd()
            .args(["list", "--since", "2020-01-01"])
            .assert()
            .success()
            .stdout(predicate::str::contains("ancient"));
    });
}

#[test]
fn list_since_without_matches_prints_its_own_message() {
    dual!(sandbox, {
        // Indexed but never jumped to: excluded by any window.
        sandbox.add(&sandbox.project("overdosecd"));

        sandbox
            .cmd()
            .args(["list", "--since", "7d"])
            .assert()
            .success()
            .stdout(
                predicate::str::contains("no projects jumped to since 7d")
                    .and(predicate::str::contains("overdosecd").not()),
            );
    });
}

#[test]
fn list_since_rejects_garbage() {
    let sandbox = Sandbox::new();

    sandbox
        .cmd()
        .args(["list", "--since", "7x"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("invalid --since value"));
}

#[cfg(unix)]
#[test]
fn add_rejects_a_path_containing_a_newline() {
    let sandbox = Sandbox::new();
    let project = sandbox.projects.join("bad\nname");
    fs::create_dir_all(&project).expect("create project dir");

    sandbox
        .cmd()
        .arg("add")
        .arg(&project)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "must not contain control or invisible characters",
        ));

    // An escape sequence is a control character too: the terminal must
    // never be fed one.
    let project = sandbox.projects.join("bad\u{1b}]0;name");
    fs::create_dir_all(&project).expect("create project dir");

    sandbox
        .cmd()
        .arg("add")
        .arg(&project)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "must not contain control or invisible characters",
        ));

    sandbox
        .cmd()
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("no projects yet"));
}

#[test]
fn add_rejects_newline_names_aliases_and_tags() {
    let sandbox = Sandbox::new();
    let project = sandbox.project("guarded");

    sandbox
        .cmd()
        .arg("add")
        .arg(&project)
        .args(["--name", "bad\nname"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "must not contain control or invisible characters",
        ));

    sandbox
        .cmd()
        .arg("add")
        .arg(&project)
        .args(["--alias", "bad\u{1b}alias"])
        .assert()
        .failure()
        .code(1);

    sandbox
        .cmd()
        .arg("add")
        .arg(&project)
        .args(["--tag", "bad\ntag"])
        .assert()
        .failure()
        .code(1);

    // The failed adds indexed nothing.
    sandbox
        .cmd()
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("no projects yet"));
}

#[test]
fn mutations_reject_control_characters() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("guarded"));

    sandbox
        .cmd()
        .args(["rename", "guarded", "bad\nname"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains(
            "must not contain control or invisible characters",
        ));

    sandbox
        .cmd()
        .args(["rename", "guarded", "bad\u{1b}name"])
        .assert()
        .failure()
        .code(1);

    sandbox
        .cmd()
        .args(["alias", "add", "guarded", "bad\nalias"])
        .assert()
        .failure()
        .code(1);

    sandbox
        .cmd()
        .args(["tag", "add", "guarded", "bad\ntag"])
        .assert()
        .failure()
        .code(1);

    // Nothing changed.
    sandbox
        .cmd()
        .args(["info", "guarded"])
        .assert()
        .success()
        .stdout(predicate::str::contains("name:      guarded"));
}

#[test]
fn debug_flag_lists_candidates_on_stderr() {
    let sandbox = Sandbox::new();
    let project = sandbox.project("overdosecd");
    sandbox.add(&project);
    let canonical = sandbox.canonical(&project);

    sandbox
        .cmd()
        .args(["--debug", "goto", "over"])
        .assert()
        .success()
        .stdout(format!("{canonical}\n"))
        .stderr(
            predicate::str::contains("debug: 1 candidate for `over`")
                .and(predicate::str::contains("name prefix, score")),
        );
}

#[test]
fn debug_flag_explains_no_matches_and_ambiguity() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("app-one"));
    sandbox.add(&sandbox.project("app-two"));

    sandbox
        .cmd()
        .args(["--debug", "info", "app"])
        .assert()
        .code(1)
        .stderr(
            predicate::str::contains("debug: 2 candidates for `app`")
                .and(predicate::str::contains("multiple projects matched")),
        );

    sandbox
        .cmd()
        .args(["--debug", "goto", "qqqq"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("debug: no candidates for `qqqq`"));
}

#[test]
fn completions_prints_a_script_for_every_shell() {
    dual!(sandbox, {
        sandbox
            .cmd()
            .args(["completions", "bash"])
            .assert()
            .success()
            .stdout(predicate::str::contains("_overdosecd"));

        sandbox
            .cmd()
            .args(["completions", "zsh"])
            .assert()
            .success()
            .stdout(predicate::str::contains("#compdef overdosecd"));

        sandbox
            .cmd()
            .args(["completions", "fish"])
            .assert()
            .success()
            .stdout(predicate::str::contains("complete -c overdosecd"));

        sandbox
            .cmd()
            .args(["completions", "powershell"])
            .assert()
            .failure()
            .code(2);
    });
}

#[test]
fn complete_lists_jump_candidates() {
    dual!(sandbox, {
        let overdosecd = sandbox.project("overdosecd");
        let notes = sandbox.project("notes");
        sandbox
            .cmd()
            .arg("add")
            .arg(&overdosecd)
            .args(["--alias", "over-alias"])
            .assert()
            .success();
        sandbox
            .cmd()
            .arg("add")
            .arg(&notes)
            .args(["--tag", "writing"])
            .assert()
            .success();

        // An empty prefix lists every name, alias, and tag.
        sandbox.cmd().arg("complete").assert().success().stdout(
            predicate::str::contains("notes")
                .and(predicate::str::contains("over-alias"))
                .and(predicate::str::contains("overdosecd"))
                .and(predicate::str::contains("writing")),
        );

        // Prefix filtering is case-insensitive and sorted.
        sandbox
            .cmd()
            .args(["complete", "OVER"])
            .assert()
            .success()
            .stdout("over-alias\noverdosecd\n");

        // The helper stays hidden from the documented command list.
        sandbox.cmd().arg("--help").assert().success().stdout(
            predicate::str::contains("completions")
                .and(predicate::str::contains("\n  complete ").not()),
        );
    });
}

#[test]
fn remove_without_yes_requires_a_terminal() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("overdosecd"));

    sandbox
        .cmd()
        .args(["remove", "overdosecd"])
        .write_stdin("")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("refusing to remove"));

    // The project is still there.
    sandbox
        .cmd()
        .args(["goto", "overdosecd"])
        .assert()
        .success();
}

#[test]
fn corrupt_index_is_quarantined_with_exit_3() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(&sandbox.data).expect("create data dir");
    fs::write(sandbox.data.join("projects.json"), "{ not json").expect("write corrupt file");

    sandbox
        .cmd()
        .arg("list")
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains("moved to"));

    let backups: Vec<_> = fs::read_dir(&sandbox.data)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("corrupt"))
        .collect();
    assert_eq!(
        backups.len(),
        1,
        "expected a quarantine backup: {backups:?}"
    );
}

#[test]
fn paths_with_spaces_and_unicode_work() {
    dual!(sandbox, {
        let project = sandbox.project("spacey café");

        sandbox.add(&project);

        sandbox
            .cmd()
            .args(["goto", "spacey café"])
            .assert()
            .success()
            .stdout(format!("{}\n", sandbox.canonical(&project)));
    });
}

#[cfg(unix)]
#[test]
fn symlinked_directory_is_stored_canonically() {
    dual!(sandbox, {
        let real = sandbox.project("real-project");
        let link = sandbox.root.join("link-project");
        std::os::unix::fs::symlink(&real, &link).expect("create symlink");

        sandbox.add(&link);

        sandbox
            .cmd()
            .args(["goto", "real-project"])
            .assert()
            .success()
            .stdout(format!("{}\n", sandbox.canonical(&real)));
    });
}

#[test]
fn init_prints_a_wrapper_for_every_shell() {
    let sandbox = Sandbox::new();
    for (shell, needle) in [
        ("bash", "ocd()"),
        ("zsh", "ocd()"),
        ("fish", "function ocd"),
    ] {
        sandbox
            .cmd()
            .args(["init", shell])
            .assert()
            .success()
            .stdout(
                predicate::str::contains(needle)
                    .and(predicate::str::contains("command overdosecd"))
                    .and(predicate::str::contains("add")),
            );
    }
}

#[test]
fn data_dir_flag_overrides_the_environment() {
    dual!(sandbox, {
        let project = sandbox.project("overdosecd");
        let other_data = sandbox.root.join("flag-data");

        sandbox
            .cmd()
            .arg("--data-dir")
            .arg(&other_data)
            .arg("add")
            .arg(&project)
            .assert()
            .success();

        assert!(
            other_data.join(sandbox.index_name()).exists(),
            "the flagged data directory should hold the index"
        );
        assert!(
            !sandbox.data.join(sandbox.index_name()).exists(),
            "the environment path should not be written"
        );
    });
}

#[test]
fn goto_records_usage() {
    dual!(sandbox, {
        sandbox.add(&sandbox.project("overdosecd"));

        sandbox
            .cmd()
            .args(["goto", "overdosecd"])
            .assert()
            .success();
        sandbox
            .cmd()
            .args(["goto", "overdosecd"])
            .assert()
            .success();

        let output = sandbox.cmd().args(["list", "--json"]).output().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let project = &value["projects"][0];
        assert_eq!(project["use_count"], 2);
        assert!(!project["last_used_at"].is_null());

        sandbox
            .cmd()
            .args(["info", "overdosecd"])
            .assert()
            .success()
            .stdout(
                predicate::str::contains("last used: just now")
                    .and(predicate::str::contains("uses:      2")),
            );
    });
}

#[test]
fn goto_no_track_leaves_stats_alone() {
    dual!(sandbox, {
        sandbox.add(&sandbox.project("overdosecd"));

        sandbox
            .cmd()
            .args(["goto", "overdosecd", "--no-track"])
            .assert()
            .success();

        let output = sandbox.cmd().args(["list", "--json"]).output().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["projects"][0]["use_count"], 0);
        assert!(value["projects"][0]["last_used_at"].is_null());
    });
}

#[test]
fn jumps_reorder_the_default_list_view() {
    dual!(sandbox, {
        sandbox.add(&sandbox.project("alpha"));
        sandbox.add(&sandbox.project("bravo"));

        sandbox.cmd().args(["goto", "bravo"]).assert().success();

        let output = sandbox.cmd().arg("list").output().unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        let alpha = text.find("alpha").expect("alpha row");
        let bravo = text.find("bravo").expect("bravo row");
        assert!(
            bravo < alpha,
            "bravo should sort first after a jump:\n{text}"
        );
    });
}

#[test]
fn empty_index_points_at_add() {
    dual!(sandbox, {
        sandbox
            .cmd()
            .args(["goto", "anything"])
            .assert()
            .failure()
            .code(1)
            .stderr(
                predicate::str::contains("no projects indexed yet")
                    .and(predicate::str::contains("overdosecd add")),
            );
    });
}

/// A held write lock allows reads (a jump still prints its path) but blocks
/// writers for the same ~500 ms window before the timeout, on both backends:
/// the JSON sidecar lock and a SQLite write transaction.
#[test]
fn a_held_lock_allows_a_jump_but_blocks_writes() {
    {
        let sandbox = Sandbox::new();
        let project = sandbox.project("overdosecd");
        sandbox.add(&project);

        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(sandbox.data.join("projects.lock"))
            .expect("open lock file");
        let mut held = fd_lock::RwLock::new(file);
        let _guard = held.try_write().expect("test should hold the lock");

        assert_reads_pass_writes_time_out(&sandbox, &project);
    }

    {
        let sandbox = Sandbox::with_backend(Backend::Sqlite);
        let project = sandbox.project("overdosecd");
        sandbox.add(&project);

        let held = rusqlite::Connection::open(sandbox.index_file()).expect("open database");
        held.execute_batch("BEGIN IMMEDIATE")
            .expect("hold the write lock");

        assert_reads_pass_writes_time_out(&sandbox, &project);

        held.execute_batch("ROLLBACK").expect("release the lock");
    }
}

/// The shared expectation of the lock tests: the jump passes and warns about
/// the unwritten usage; the write fails with the lock timeout.
fn assert_reads_pass_writes_time_out(sandbox: &Sandbox, project: &Path) {
    sandbox
        .cmd()
        .args(["goto", "overdosecd"])
        .assert()
        .success()
        .stdout(format!("{}\n", sandbox.canonical(project)))
        .stderr(predicate::str::contains("could not record usage"));

    sandbox
        .cmd()
        .arg("add")
        .arg(&sandbox.root)
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains(
            "timed out waiting for the index lock",
        ));
}

/// A database file that is not SQLite fails loudly instead of reading as an
/// empty index.
#[test]
fn sqlite_corrupt_database_fails_with_exit_3() {
    let sandbox = Sandbox::with_backend(Backend::Sqlite);
    fs::create_dir_all(&sandbox.data).expect("create data dir");
    fs::write(sandbox.index_file(), "definitely not a database").expect("write corrupt file");

    sandbox
        .cmd()
        .arg("list")
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains("projects.db"));

    sandbox
        .cmd()
        .args(["goto", "anything"])
        .assert()
        .failure()
        .code(3);
}

#[test]
fn sqlite_info_shows_jump_history() {
    let sandbox = Sandbox::with_backend(Backend::Sqlite);
    sandbox.add(&sandbox.project("overdosecd"));
    sandbox
        .cmd()
        .args(["goto", "overdosecd"])
        .assert()
        .success();
    sandbox
        .cmd()
        .args(["goto", "overdosecd"])
        .assert()
        .success();

    sandbox
        .cmd()
        .args(["info", "overdosecd"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("history:   just now, just now")
                .and(predicate::str::contains("uses:      2")),
        );
}

#[test]
fn sqlite_goto_no_track_records_no_history() {
    let sandbox = Sandbox::with_backend(Backend::Sqlite);
    sandbox.add(&sandbox.project("overdosecd"));

    sandbox
        .cmd()
        .args(["goto", "overdosecd", "--no-track"])
        .assert()
        .success();

    sandbox
        .cmd()
        .args(["info", "overdosecd"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("history:")
                .not()
                .and(predicate::str::contains("uses:      0")),
        );
}

/// The JSON backend keeps counters only, so `info` must not grow a history
/// line there.
#[test]
fn json_info_never_shows_history() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("overdosecd"));
    sandbox
        .cmd()
        .args(["goto", "overdosecd"])
        .assert()
        .success();

    sandbox
        .cmd()
        .args(["info", "overdosecd"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("history:")
                .not()
                .and(predicate::str::contains("uses:      1")),
        );
}

#[test]
fn doctor_reports_the_sqlite_schema() {
    let sandbox = Sandbox::with_backend(Backend::Sqlite);

    sandbox
        .cmd()
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "storage: sqlite (no database yet)",
        ));

    sandbox.add(&sandbox.project("overdosecd"));

    sandbox
        .cmd()
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("storage: sqlite (schema v1)"));
}

#[test]
fn doctor_reports_stray_sqlite_files() {
    let sandbox = Sandbox::with_backend(Backend::Sqlite);
    fs::create_dir_all(&sandbox.data).expect("create data dir");
    fs::write(sandbox.data.join("projects.db-wal"), "leftover").expect("write stray wal");

    sandbox.cmd().arg("doctor").assert().code(1).stdout(
        predicate::str::contains("stray:").and(predicate::str::contains("projects.db-wal")),
    );
}

#[test]
fn doctor_reports_a_disagreeing_json_and_sqlite_pair() {
    let sandbox = Sandbox::with_backend(Backend::Sqlite);
    sandbox.add(&sandbox.project("alpha"));

    // A stale JSON copy with a different id appears next to the database.
    let beta = sandbox.project("beta");
    let beta_json = serde_json::to_string(&sandbox.canonical(&beta)).expect("encode path");
    fs::write(
        sandbox.data.join("projects.json"),
        format!(
            r#"{{"version":1,"projects":[{{"id":"deadbeef","name":"beta","path":{beta_json},"aliases":[],"tags":[],"created_at":"2026-09-28T00:00:00Z","last_used_at":null,"use_count":0,"pinned":false}}]}}"#
        ),
    )
    .expect("write stale json");

    sandbox
        .cmd()
        .arg("doctor")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("mismatch:").and(predicate::str::contains("disagree")));

    assert!(
        sandbox.data.join("projects.json").exists(),
        "doctor must not quarantine or rewrite the inactive index"
    );
}

#[test]
fn doctor_accepts_an_agreeing_pair() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("alpha"));
    let config = sandbox.write_config("[storage]\nbackend = \"sqlite\"\n");
    sandbox
        .cmd()
        .env("OVERDOSECD_CONFIG", &config)
        .arg("migrate")
        .assert()
        .success();

    // Restore the backup as the live JSON file: same ids as the database.
    let backup = fs::read_dir(&sandbox.data)
        .expect("read data dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("projects.json.migrated-"))
        })
        .expect("backup exists");
    fs::rename(&backup, sandbox.data.join("projects.json")).expect("restore backup");

    sandbox
        .cmd()
        .env("OVERDOSECD_CONFIG", &config)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("no problems found"));
}

#[test]
fn doctor_reports_json_quarantine_files_with_sqlite_active() {
    let sandbox = Sandbox::with_backend(Backend::Sqlite);
    sandbox.add(&sandbox.project("overdosecd"));
    let quarantined = sandbox.data.join("projects.json.corrupt-20260101000000");
    fs::write(&quarantined, "{ not json").expect("write quarantine file");

    sandbox.cmd().arg("doctor").assert().code(1).stdout(
        predicate::str::contains("quarantine:")
            .and(predicate::str::contains("projects.json.corrupt-")),
    );

    assert!(
        quarantined.exists(),
        "doctor must never delete quarantined files"
    );
}

#[cfg(unix)]
#[test]
fn doctor_reports_loose_sqlite_permissions() {
    let sandbox = Sandbox::with_backend(Backend::Sqlite);
    sandbox.add(&sandbox.project("overdosecd"));
    let index = sandbox.data.join("projects.db");

    set_mode(&index, 0o644);
    sandbox
        .cmd()
        .arg("doctor")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("is mode 0644 (expected 0600)"));

    set_mode(&index, 0o600);
    sandbox.cmd().arg("doctor").assert().success();
}

/// Names of the `projects.json.migrated-*` backups in the sandbox data dir.
fn migrate_backups(sandbox: &Sandbox) -> Vec<String> {
    fs::read_dir(&sandbox.data)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("projects.json.migrated-"))
                .collect()
        })
        .unwrap_or_default()
}

/// Seeds the default JSON backend, migrates with a SQLite config, and checks
/// the same data comes back through the new backend.
/// The exact bytes zoxide writes to `db.zo`: bincode 1.x's legacy (fixint,
/// little-endian) encoding of a `u32` format version followed by the entries.
fn write_zoxide_fixture(dir: &Path, entries: &[(&Path, f64, u64)]) {
    #[derive(serde::Serialize)]
    struct ZDir {
        path: String,
        rank: f64,
        last_accessed: u64,
    }
    let dirs: Vec<ZDir> = entries
        .iter()
        .map(|(path, rank, last_accessed)| ZDir {
            path: path.to_string_lossy().into_owned(),
            rank: *rank,
            last_accessed: *last_accessed,
        })
        .collect();
    let mut bytes = Vec::new();
    bincode::serialize_into(&mut bytes, &3u32).expect("version");
    bincode::serialize_into(&mut bytes, &dirs).expect("entries");
    fs::create_dir_all(dir).expect("create store dir");
    fs::write(dir.join("db.zo"), bytes).expect("write db.zo");
}

#[test]
fn import_zoxide_lands_counters_and_timestamps() {
    dual!(sandbox, {
        let store = sandbox.root.join("zoxide-store");
        let one = PathBuf::from(sandbox.canonical(&sandbox.project("one")));
        let two = PathBuf::from(sandbox.canonical(&sandbox.project("two")));
        write_zoxide_fixture(&store, &[(&one, 12.0, 1_700_000_000), (&two, 3.0, 0)]);

        sandbox
            .cmd()
            .env("_ZO_DATA_DIR", &store)
            .args(["import", "zoxide"])
            .assert()
            .success()
            .stdout(predicate::str::contains("imported 2 projects"));

        sandbox
            .cmd()
            .args(["list", "--json"])
            .assert()
            .success()
            .stdout(
                predicate::str::contains("\"use_count\": 12")
                    .and(predicate::str::contains("2023-11-14")),
            );
    });
}

#[test]
fn import_is_idempotent() {
    dual!(sandbox, {
        let store = sandbox.root.join("zoxide-store");
        let one = PathBuf::from(sandbox.canonical(&sandbox.project("one")));
        write_zoxide_fixture(&store, &[(&one, 5.0, 0)]);

        sandbox
            .cmd()
            .env("_ZO_DATA_DIR", &store)
            .args(["import", "zoxide"])
            .assert()
            .success();
        sandbox
            .cmd()
            .env("_ZO_DATA_DIR", &store)
            .args(["import", "zoxide"])
            .assert()
            .success()
            .stdout(
                predicate::str::contains("nothing to import")
                    .and(predicate::str::contains("1 already indexed")),
            );

        // The counters from the first run are untouched.
        sandbox
            .cmd()
            .args(["list", "--json"])
            .assert()
            .success()
            .stdout(predicate::str::contains("\"use_count\": 5"));
    });
}

#[test]
fn import_dry_run_matches_the_list_it_would_produce() {
    dual!(sandbox, {
        let store = sandbox.root.join("zoxide-store");
        let one = PathBuf::from(sandbox.canonical(&sandbox.project("one")));
        let two = PathBuf::from(sandbox.canonical(&sandbox.project("two")));
        write_zoxide_fixture(&store, &[(&one, 7.0, 1_700_000_000), (&two, 2.0, 0)]);

        let dry = sandbox
            .cmd()
            .env("_ZO_DATA_DIR", &store)
            .args(["import", "zoxide", "--dry-run"])
            .assert()
            .success();
        let dry_stdout = String::from_utf8(dry.get_output().stdout.clone()).expect("utf-8");
        assert!(
            dry_stdout.contains("dry run: would import 2 projects"),
            "{dry_stdout}"
        );

        // Nothing was written by the dry run.
        sandbox
            .cmd()
            .args(["list"])
            .assert()
            .success()
            .stdout(predicate::str::contains("no projects yet"));

        sandbox
            .cmd()
            .env("_ZO_DATA_DIR", &store)
            .args(["import", "zoxide"])
            .assert()
            .success();
        let list = sandbox.cmd().args(["list"]).assert().success();
        let list_stdout = String::from_utf8(list.get_output().stdout.clone()).expect("utf-8");
        assert!(
            dry_stdout.starts_with(&list_stdout),
            "dry run:\n{dry_stdout}\nlist:\n{list_stdout}"
        );
    });
}

#[test]
fn import_autojump_reports_blacklist_missing_and_malformed() {
    dual!(sandbox, {
        let store = sandbox.root.join("autojump-store");
        fs::create_dir_all(&store).expect("create store dir");
        let one = PathBuf::from(sandbox.canonical(&sandbox.project("one")));
        let text = format!(
            "9\t{}\n-1\t/tmp/blacklisted\n2\t{}\nnot-a-line\n",
            one.display(),
            sandbox.root.join("never-exists").display()
        );
        fs::write(store.join("autojump.txt"), text).expect("write autojump.txt");

        sandbox
            .cmd()
            .env("AUTOJUMP_DATA_DIR", &store)
            .args(["import", "autojump"])
            .assert()
            .success()
            .stdout(
                predicate::str::contains("imported 1 project")
                    .and(predicate::str::contains("1 missing"))
                    .and(predicate::str::contains("1 excluded"))
                    .and(predicate::str::contains("1 malformed")),
            );
    });
}

#[cfg(unix)]
#[test]
fn import_zshz_keeps_pipes_in_paths() {
    dual!(sandbox, {
        let dir = sandbox.projects.join("we|ird");
        fs::create_dir_all(&dir).expect("create project dir");
        let canonical = PathBuf::from(sandbox.canonical(&dir));
        let data = sandbox.root.join("z-data");
        fs::write(&data, format!("{}|4.7|1700000000\n", canonical.display()))
            .expect("write z data");

        sandbox
            .cmd()
            .env("ZSHZ_DATA", &data)
            .args(["import", "zsh-z"])
            .assert()
            .success()
            .stdout(predicate::str::contains("imported 1 project"));
        sandbox
            .cmd()
            .args(["list", "--json"])
            .assert()
            .success()
            .stdout(predicate::str::contains("we|ird"));
    });
}

#[test]
fn import_missing_source_names_the_path() {
    // JSON-only on purpose: the source lookup fails before any store is read.
    let sandbox = Sandbox::new();
    sandbox
        .cmd()
        .env("_ZO_DATA_DIR", sandbox.root.join("nope"))
        .args(["import", "zoxide"])
        .assert()
        .code(1)
        .stderr(
            predicate::str::contains("cannot import from")
                .and(predicate::str::contains("db.zo"))
                .and(predicate::str::contains("not found")),
        );
}

#[test]
fn hook_records_a_cd_into_an_indexed_project() {
    dual!(sandbox, {
        let config = sandbox.write_config("[general]\nhook = true\n");
        let project = sandbox.project("alpha");
        sandbox.add(&project);

        sandbox
            .cmd()
            .env("OVERDOSECD_CONFIG", &config)
            .current_dir(&project)
            .arg("hook")
            .assert()
            .success()
            .stdout("")
            .stderr("");

        sandbox
            .cmd()
            .args(["list", "--json"])
            .assert()
            .success()
            .stdout(predicate::str::contains("\"use_count\": 1"));
        // No hint state is written when the directory is indexed.
        assert!(!sandbox.data.join("hook-visits.json").exists());
    });
}

#[test]
fn hook_is_silent_when_disabled() {
    dual!(sandbox, {
        let project = sandbox.project("alpha");
        sandbox.add(&project);

        sandbox
            .cmd()
            .current_dir(&project)
            .arg("hook")
            .assert()
            .success()
            .stdout("")
            .stderr("");
        assert!(!sandbox.data.join("hook-visits.json").exists());
    });
}

#[test]
fn hook_hints_after_three_visits_and_then_cools_down() {
    dual!(sandbox, {
        let config = sandbox.write_config("[general]\nhook = true\nhint = true\n");
        let dir = sandbox.root.join("frequented");
        fs::create_dir_all(&dir).expect("create dir");

        for _ in 0..2 {
            sandbox
                .cmd()
                .env("OVERDOSECD_CONFIG", &config)
                .current_dir(&dir)
                .arg("hook")
                .assert()
                .success()
                .stderr("");
        }
        sandbox
            .cmd()
            .env("OVERDOSECD_CONFIG", &config)
            .current_dir(&dir)
            .arg("hook")
            .assert()
            .success()
            .stderr(
                predicate::str::contains("3 visits")
                    .and(predicate::str::contains("is not indexed"))
                    .and(predicate::str::contains("ocd add")),
            );
        // The cooldown keeps the next visits quiet.
        sandbox
            .cmd()
            .env("OVERDOSECD_CONFIG", &config)
            .current_dir(&dir)
            .arg("hook")
            .assert()
            .success()
            .stderr("");
        assert!(sandbox.data.join("hook-visits.json").exists());
    });
}

#[test]
fn hook_never_fails_a_cd() {
    // JSON-only on purpose: the index is hand-broken to prove the hook stays
    // successful; the store quarantines it as on any other read.
    let sandbox = Sandbox::new();
    let config = sandbox.write_config("[general]\nhook = true\n");
    fs::create_dir_all(&sandbox.data).expect("create data dir");
    fs::write(sandbox.data.join("projects.json"), "{ not json").expect("write");
    let dir = sandbox.project("alpha");

    sandbox
        .cmd()
        .env("OVERDOSECD_CONFIG", &config)
        .current_dir(&dir)
        .arg("hook")
        .assert()
        .success()
        .stdout("")
        .stderr(predicate::str::contains("cd hook could not read the index"));
}

#[test]
fn init_installs_the_hook_only_when_enabled() {
    let sandbox = Sandbox::new();
    sandbox
        .cmd()
        .args(["init", "bash"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("builtin cd \"$target\"")
                .and(predicate::str::contains("overdosecd hook").not()),
        );

    let config = sandbox.write_config("[general]\nhook = true\n");
    sandbox
        .cmd()
        .env("OVERDOSECD_CONFIG", &config)
        .args(["init", "bash"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("builtin cd \"$@\"")
                .and(predicate::str::contains("command overdosecd hook")),
        );
}

#[test]
fn goto_multi_term_requires_every_word() {
    let sandbox = Sandbox::new();
    let rust = sandbox.project("rust-tools");
    let web = sandbox.project("web-scratch");
    sandbox.add(&rust);
    sandbox.add(&web);

    sandbox
        .cmd()
        .args(["goto", "rust", "web"])
        .assert()
        .failure()
        .code(1);

    let both = sandbox.project("rust-web-app");
    sandbox.add(&both);
    sandbox
        .cmd()
        .args(["goto", "rust", "web"])
        .assert()
        .success()
        .stdout(format!("{}\n", sandbox.canonical(&both)));
}

#[test]
fn debug_multi_term_lists_each_word() {
    let sandbox = Sandbox::new();
    let project = sandbox.project("rust-web-app");
    sandbox.add(&project);

    sandbox
        .cmd()
        .args(["--debug", "goto", "rust", "web"])
        .assert()
        .success()
        .stderr(predicate::str::contains("`rust` ->").and(predicate::str::contains("`web` ->")));
}

#[test]
fn mutations_accept_multiple_words() {
    dual!(sandbox, {
        let project = sandbox.project("rust-web-app");
        sandbox.add(&project);

        sandbox
            .cmd()
            .args(["pin", "rust", "web"])
            .assert()
            .success()
            .stdout(predicate::str::contains("pinned `rust-web-app`"));
        sandbox
            .cmd()
            .args(["info", "rust", "web"])
            .assert()
            .success()
            .stdout(predicate::str::contains("name:      rust-web-app"));
    });
}

#[test]
fn scan_previews_explicit_roots_and_writes_nothing() {
    // JSON-only on purpose: the preview touches neither the index nor the cache.
    let sandbox = Sandbox::new();
    let root = sandbox.root.join("tree");
    let project = root.join("proj");
    fs::create_dir_all(&project).expect("create project dir");
    fs::write(project.join("Cargo.toml"), "[package]").expect("write marker");
    fs::create_dir_all(root.join("plain")).expect("create plain dir");
    fs::create_dir_all(root.join("node_modules").join("dep")).expect("create skipped dir");

    sandbox
        .cmd()
        .arg("scan")
        .arg(&root)
        .arg("--max-depth")
        .arg("2")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("proj (project)")
                .and(predicate::str::contains("plain"))
                .and(predicate::str::contains("(1 project-like)"))
                .and(predicate::str::contains("node_modules").not())
                .and(predicate::str::contains("dep").not()),
        );

    // Nothing was written anywhere: no index, no cache.
    assert!(!sandbox.data.join("projects.json").exists());
    assert!(!sandbox.data.join("home-dirs.json").exists());
}

#[test]
fn scan_requires_a_target_and_rejects_mixed_modes() {
    let sandbox = Sandbox::new();
    let root = sandbox.project("tree");

    // No target: neither roots nor --home.
    sandbox.cmd().arg("scan").assert().code(2);
    // Roots and the cache mode are exclusive, as are the cache-only flags.
    sandbox
        .cmd()
        .arg("scan")
        .arg(&root)
        .arg("--home")
        .assert()
        .code(2);
    sandbox
        .cmd()
        .arg("scan")
        .arg(&root)
        .arg("--dry-run")
        .assert()
        .code(2);
    // The cache mode keeps working.
    sandbox
        .cmd()
        .args(["scan", "--home", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("would cache"));
}

#[test]
fn scan_reports_a_missing_root() {
    let sandbox = Sandbox::new();
    sandbox
        .cmd()
        .arg("scan")
        .arg(sandbox.root.join("nope"))
        .assert()
        .code(1)
        .stderr(predicate::str::contains("directory does not exist"));
}

#[test]
fn migrate_moves_projects_and_keeps_the_original() {
    let sandbox = Sandbox::new();
    let project = sandbox.project("overdosecd");
    sandbox
        .cmd()
        .arg("add")
        .arg(&project)
        .args(["--alias", "w", "--tag", "rust"])
        .assert()
        .success();
    sandbox
        .cmd()
        .args(["goto", "overdosecd"])
        .assert()
        .success();

    let config = sandbox.write_config("[storage]\nbackend = \"sqlite\"\n");
    sandbox
        .cmd()
        .env("OVERDOSECD_CONFIG", &config)
        .arg("migrate")
        .assert()
        .success()
        .stdout(
            predicate::str::contains("migrated 1 project")
                .and(predicate::str::contains("projects.json.migrated-")),
        );

    assert!(!sandbox.data.join("projects.json").exists());
    assert_eq!(migrate_backups(&sandbox).len(), 1);

    sandbox
        .cmd()
        .env("OVERDOSECD_CONFIG", &config)
        .args(["goto", "w"])
        .assert()
        .success()
        .stdout(format!("{}\n", sandbox.canonical(&project)));
    sandbox
        .cmd()
        .env("OVERDOSECD_CONFIG", &config)
        .args(["info", "overdosecd"])
        .assert()
        .success()
        .stdout(
            // One jump before the migration, one after: the usage carried over.
            predicate::str::contains("tags:      rust")
                .and(predicate::str::contains("uses:      2")),
        );
}

#[test]
fn migrate_is_idempotent() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("overdosecd"));
    let config = sandbox.write_config("[storage]\nbackend = \"sqlite\"\n");

    sandbox
        .cmd()
        .env("OVERDOSECD_CONFIG", &config)
        .arg("migrate")
        .assert()
        .success()
        .stdout(predicate::str::contains("migrated 1 project"));

    sandbox
        .cmd()
        .env("OVERDOSECD_CONFIG", &config)
        .arg("migrate")
        .assert()
        .success()
        .stdout(predicate::str::contains("already migrated"));
}

#[test]
fn migrate_refuses_to_clobber_an_existing_database() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("alpha"));
    let config = sandbox.write_config("[storage]\nbackend = \"sqlite\"\n");
    sandbox
        .cmd()
        .env("OVERDOSECD_CONFIG", &config)
        .arg("migrate")
        .assert()
        .success();

    // A fresh JSON index appears while the database already holds data.
    sandbox.add(&sandbox.project("beta"));

    sandbox
        .cmd()
        .env("OVERDOSECD_CONFIG", &config)
        .arg("migrate")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("refusing to overwrite"));

    assert!(
        sandbox.data.join("projects.json").exists(),
        "a refused migration must leave the json in place"
    );
    assert_eq!(migrate_backups(&sandbox).len(), 1);
}

#[test]
fn migrate_without_an_index_fails() {
    let sandbox = Sandbox::new();

    sandbox
        .cmd()
        .arg("migrate")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("nothing to migrate"));
}

#[test]
fn migrate_hints_when_the_backend_still_points_at_json() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("overdosecd"));

    sandbox.cmd().arg("migrate").assert().success().stdout(
        predicate::str::contains("migrated 1 project")
            .and(predicate::str::contains("backend = \"sqlite\"")),
    );
}

#[test]
fn migrate_of_a_corrupt_index_fails_with_exit_3() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(&sandbox.data).expect("create data dir");
    fs::write(sandbox.data.join("projects.json"), "{ not json").expect("write corrupt file");

    sandbox
        .cmd()
        .arg("migrate")
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains("moved to"));
}

/// Sets Unix permission bits; callers restore them before the tempdir drops.
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

/// True when the restrictive mode just set had no effect: running as root
/// ignores permission bits, and a chmod-based test that continued would
/// assert nothing.
#[cfg(unix)]
fn permissions_are_ignored(path: &Path) -> bool {
    if path.is_dir() {
        fs::read_dir(path).is_ok()
    } else {
        fs::read(path).is_ok()
    }
}

#[cfg(unix)]
#[test]
fn read_only_data_dir_still_jumps() {
    let sandbox = Sandbox::new();
    let project = sandbox.project("overdosecd");
    sandbox.add(&project);

    set_mode(&sandbox.data, 0o500);

    sandbox
        .cmd()
        .args(["goto", "overdosecd"])
        .assert()
        .success()
        .stdout(format!("{}\n", sandbox.canonical(&project)))
        .stderr(predicate::str::contains("could not record usage"));

    // The jump degrades to a warning, but `add` needs the write lock and the
    // atomic save, so it must fail as a storage error instead.
    let other = sandbox.project("other");
    sandbox
        .cmd()
        .arg("add")
        .arg(&other)
        .assert()
        .failure()
        .code(3);

    // Restore write access so the tempdir can be cleaned up.
    set_mode(&sandbox.data, 0o700);
}

#[cfg(unix)]
#[test]
fn unreadable_parent_directory_marks_the_project_missing() {
    dual!(sandbox, {
        let parent = sandbox.root.join("unreadable");
        let project = parent.join("hidden-project");
        fs::create_dir_all(&project).expect("create nested project dir");
        sandbox.add(&project);

        set_mode(&parent, 0o000);
        if permissions_are_ignored(&parent) {
            set_mode(&parent, 0o700);
            return;
        }

        // An unreadable parent makes `exists()` false, so the project is treated
        // exactly like a deleted directory everywhere.
        sandbox
            .cmd()
            .arg("list")
            .assert()
            .success()
            .stdout(predicate::str::contains("no projects yet"));

        sandbox
            .cmd()
            .args(["list", "--all"])
            .assert()
            .success()
            .stdout(
                predicate::str::contains("hidden-project")
                    .and(predicate::str::contains("directory missing")),
            );

        sandbox
            .cmd()
            .args(["goto", "hidden-project"])
            .assert()
            .failure()
            .code(1)
            .stderr(predicate::str::contains("directory no longer exists"));

        sandbox
            .cmd()
            .arg("doctor")
            .assert()
            .failure()
            .code(1)
            .stdout(predicate::str::contains("directory no longer exists"));

        set_mode(&parent, 0o700);
    });
}

#[cfg(unix)]
#[test]
fn unreadable_index_fails_with_exit_3() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("overdosecd"));
    let index = sandbox.data.join("projects.json");

    set_mode(&index, 0o000);
    if permissions_are_ignored(&index) {
        set_mode(&index, 0o600);
        return;
    }

    // Nothing can open the index, and quarantine itself needs the file too,
    // so every command reports a storage failure instead of an empty index.
    sandbox.cmd().arg("list").assert().code(3);
    sandbox.cmd().args(["goto", "overdosecd"]).assert().code(3);
    sandbox.cmd().arg("doctor").assert().code(3);

    set_mode(&index, 0o600);
}

#[cfg(unix)]
#[test]
fn unreadable_data_dir_fails_reads_with_exit_3() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("overdosecd"));

    set_mode(&sandbox.data, 0o000);
    if permissions_are_ignored(&sandbox.data) {
        set_mode(&sandbox.data, 0o700);
        return;
    }

    // An index path inside an unreadable directory is a storage failure, not
    // a missing file that reads as an empty index.
    sandbox.cmd().arg("list").assert().code(3);
    sandbox.cmd().args(["goto", "overdosecd"]).assert().code(3);

    set_mode(&sandbox.data, 0o700);
}

#[test]
fn concurrent_adds_do_not_lose_entries() {
    dual!(sandbox, {
        let mut children = Vec::new();
        for index in 0..5 {
            let path = sandbox.project(&format!("concurrent-{index}"));
            let child = sandbox
                .std_cmd()
                .arg("add")
                .arg(&path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn add");
            children.push((path, child));
        }
        for (path, mut child) in children {
            // Five racing adds can legally hit the ~500 ms lock timeout
            // (SQLite's busy wait especially, on a loaded machine); the
            // invariant under test is that no add is *lost*, so a timed-out
            // one is retried rather than treated as a data-loss signal.
            let mut added = child.wait().expect("wait for add").success();
            for _ in 0..3 {
                if added {
                    break;
                }
                added = sandbox
                    .std_cmd()
                    .arg("add")
                    .arg(&path)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .expect("retry add")
                    .success();
            }
            assert!(added, "a concurrent add failed even after retries");
        }

        let output = sandbox.cmd().args(["list", "--json"]).output().unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            value["projects"].as_array().unwrap().len(),
            5,
            "concurrent adds lost entries"
        );
    });
}

#[test]
fn pin_floats_a_project_to_the_top() {
    dual!(sandbox, {
        sandbox.add(&sandbox.project("alpha"));
        sandbox.add(&sandbox.project("bravo"));

        sandbox
            .cmd()
            .args(["pin", "bravo"])
            .assert()
            .success()
            .stdout(predicate::str::contains("pinned `bravo`"));

        let output = sandbox.cmd().arg("list").output().unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        let alpha = text.find("alpha").expect("alpha row");
        let bravo = text.find("bravo").expect("bravo row");
        assert!(
            bravo < alpha,
            "a pinned project should float first:\n{text}"
        );
        assert!(text.contains("★ bravo"), "missing pin marker:\n{text}");

        sandbox
            .cmd()
            .args(["info", "bravo"])
            .assert()
            .success()
            .stdout(predicate::str::contains("pinned:    yes"));
    });
}

#[test]
fn pin_and_unpin_are_idempotent() {
    dual!(sandbox, {
        sandbox.add(&sandbox.project("alpha"));

        sandbox
            .cmd()
            .args(["unpin", "alpha"])
            .assert()
            .success()
            .stdout(predicate::str::contains("already unpinned"));

        sandbox.cmd().args(["pin", "alpha"]).assert().success();

        sandbox
            .cmd()
            .args(["pin", "alpha"])
            .assert()
            .success()
            .stdout(predicate::str::contains("already pinned"));

        sandbox
            .cmd()
            .args(["unpin", "alpha"])
            .assert()
            .success()
            .stdout(predicate::str::contains("unpinned `alpha`"));
    });
}

#[test]
fn rename_changes_the_name_and_goto_follows() {
    dual!(sandbox, {
        let project = sandbox.project("real-directory");
        sandbox.add(&project);

        sandbox
            .cmd()
            .args(["rename", "real-directory", "over"])
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "renamed `real-directory` -> `over`",
            ));

        sandbox
            .cmd()
            .args(["goto", "over"])
            .assert()
            .success()
            .stdout(format!("{}\n", sandbox.canonical(&project)));

        sandbox
            .cmd()
            .args(["info", "over"])
            .assert()
            .success()
            .stdout(predicate::str::contains("name:      over"));
    });
}

#[test]
fn rename_collision_requires_force() {
    dual!(sandbox, {
        sandbox.add(&sandbox.project("alpha"));
        sandbox.add(&sandbox.project("beta"));

        sandbox
            .cmd()
            .args(["rename", "alpha", "beta"])
            .assert()
            .failure()
            .code(1)
            .stderr(predicate::str::contains("already named `beta`"));

        sandbox
            .cmd()
            .args(["rename", "alpha", "beta", "--force"])
            .assert()
            .success()
            .stdout(
                predicate::str::contains("renamed `alpha` -> `beta`")
                    .and(predicate::str::contains("another project is already named")),
            );
    });
}

#[test]
fn alias_add_and_remove_round_trip() {
    dual!(sandbox, {
        let project = sandbox.project("overdosecd");
        sandbox.add(&project);

        sandbox
            .cmd()
            .args(["alias", "add", "overdosecd", "w"])
            .assert()
            .success()
            .stdout(predicate::str::contains("added alias `w` to `overdosecd`"));

        sandbox
            .cmd()
            .args(["goto", "w"])
            .assert()
            .success()
            .stdout(format!("{}\n", sandbox.canonical(&project)));

        sandbox
            .cmd()
            .args(["alias", "add", "overdosecd", "W"])
            .assert()
            .success()
            .stdout(predicate::str::contains("is already set on"));

        sandbox
            .cmd()
            .args(["alias", "remove", "overdosecd", "W"])
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "removed alias `W` from `overdosecd`",
            ));

        sandbox
            .cmd()
            .args(["alias", "remove", "overdosecd", "w"])
            .assert()
            .failure()
            .code(1)
            .stderr(predicate::str::contains(
                "alias `w` is not set on `overdosecd`",
            ));
    });
}

#[test]
fn tag_add_and_remove_round_trip() {
    dual!(sandbox, {
        sandbox.add(&sandbox.project("overdosecd"));

        sandbox
            .cmd()
            .args(["tag", "add", "overdosecd", "rust"])
            .assert()
            .success()
            .stdout(predicate::str::contains("added tag `rust` to `overdosecd`"));

        sandbox
            .cmd()
            .args(["info", "overdosecd"])
            .assert()
            .success()
            .stdout(predicate::str::contains("tags:      rust"));

        sandbox
            .cmd()
            .args(["tag", "add", "overdosecd", "Rust"])
            .assert()
            .success()
            .stdout(predicate::str::contains("is already set on"));

        sandbox
            .cmd()
            .args(["tag", "remove", "overdosecd", "RUST"])
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "removed tag `RUST` from `overdosecd`",
            ));

        sandbox
            .cmd()
            .args(["tag", "remove", "overdosecd", "rust"])
            .assert()
            .failure()
            .code(1)
            .stderr(predicate::str::contains(
                "tag `rust` is not set on `overdosecd`",
            ));
    });
}

#[test]
fn mutations_report_ambiguous_targets() {
    dual!(sandbox, {
        sandbox.add(&sandbox.project("app-one"));
        sandbox.add(&sandbox.project("app-two"));

        for args in [
            vec!["pin", "app"],
            vec!["unpin", "app"],
            vec!["rename", "app", "renamed"],
            vec!["alias", "add", "app", "a"],
            vec!["tag", "remove", "app", "t"],
        ] {
            sandbox
                .cmd()
                .args(&args)
                .assert()
                .failure()
                .code(1)
                .stderr(predicate::str::contains(
                    "multiple projects matched query: app",
                ));
        }
    });
}

#[test]
fn mutations_report_unknown_targets() {
    dual!(sandbox, {
        sandbox.add(&sandbox.project("overdosecd"));

        sandbox
            .cmd()
            .args(["pin", "qqqqzz"])
            .assert()
            .failure()
            .code(1)
            .stderr(predicate::str::contains("no project matched query: qqqqzz"));
    });
}

#[test]
fn alias_rejects_an_empty_value() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("overdosecd"));

    sandbox
        .cmd()
        .args(["alias", "add", "overdosecd", "  "])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("alias must not be empty"));
}

#[test]
fn init_includes_the_mutation_subcommands() {
    let sandbox = Sandbox::new();
    sandbox
        .cmd()
        .args(["init", "zsh"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("pin")
                .and(predicate::str::contains("unpin"))
                .and(predicate::str::contains("rename"))
                .and(predicate::str::contains("alias"))
                .and(predicate::str::contains("tag")),
        );
}

/// Writes a minimal `.git` directory so tests never need a git binary.
fn make_repo(dir: &Path, head: &str, remote: Option<(&str, &str)>) {
    let git = dir.join(".git");
    fs::create_dir_all(&git).expect("create .git");
    fs::write(git.join("HEAD"), head).expect("write HEAD");

    let mut config = String::from("[core]\n\trepositoryformatversion = 0\n");
    if let Some((name, url)) = remote {
        config.push_str(&format!(
            "[remote \"{name}\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/remotes/{name}/*\n"
        ));
    }
    fs::write(git.join("config"), config).expect("write config");
}

#[test]
fn info_shows_branch_and_remote_for_repositories() {
    dual!(sandbox, {
        let project = sandbox.project("overdosecd");
        make_repo(
            &project,
            "ref: refs/heads/main\n",
            Some(("origin", "git@github.com:Dead-Abyss/overdosecd.git")),
        );
        sandbox.add(&project);

        sandbox
            .cmd()
            .args(["info", "overdosecd"])
            .assert()
            .success()
            .stdout(
                predicate::str::contains("git:       yes")
                    .and(predicate::str::contains("branch:    main"))
                    .and(predicate::str::contains(
                        "remote:    origin git@github.com:Dead-Abyss/overdosecd.git",
                    )),
            );
    });
}

#[test]
fn included_git_config_is_honored_and_sanitized() {
    dual!(sandbox, {
        let project = sandbox.project("included");
        make_repo(&project, "ref: refs/heads/main\n", None);
        let git = project.join(".git");
        fs::write(
            git.join("extra.inc"),
            "[url \"git@github.com:\"]\n\tinsteadOf = https://github.com/\n\
             [remote \"origin\"]\n\turl = https://github.com/Dead-Abyss/overdosecd.git\n",
        )
        .unwrap();
        fs::write(
            git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n[include]\n\tpath = extra.inc\n",
        )
        .unwrap();
        sandbox.add(&project);

        sandbox
            .cmd()
            .args(["info", "included"])
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "remote:    origin git@github.com:Dead-Abyss/overdosecd.git",
            ));

        // A hostile include cannot reach the terminal.
        let hostile = sandbox.project("hostile");
        make_repo(&hostile, "ref: refs/heads/main\n", None);
        let git = hostile.join(".git");
        fs::write(
            git.join("evil.inc"),
            "[remote \"origin\"]\n\turl = https://x.example/\u{1b}]52;c;QQ\n",
        )
        .unwrap();
        fs::write(
            git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n[include]\n\tpath = evil.inc\n",
        )
        .unwrap();
        sandbox.add(&hostile);

        sandbox
            .cmd()
            .args(["info", "hostile"])
            .assert()
            .success()
            .stdout(
                predicate::str::contains("remote:    -")
                    .and(predicate::str::contains("\u{1b}").not()),
            );
    });
}

#[test]
fn goto_finds_a_project_by_repository_name() {
    dual!(sandbox, {
        let project = sandbox.project("local-name");
        make_repo(
            &project,
            "ref: refs/heads/main\n",
            Some(("origin", "git@github.com:acme/widgets.git")),
        );
        sandbox.add(&project);

        let expected = format!("{}\n", sandbox.canonical(&project));
        sandbox
            .cmd()
            .args(["goto", "widgets"])
            .assert()
            .success()
            .stdout(expected.clone());

        sandbox
            .cmd()
            .args(["goto", "acme/widgets"])
            .assert()
            .success()
            .stdout(expected);
    });
}

#[test]
fn detached_head_is_reported() {
    dual!(sandbox, {
        let project = sandbox.project("overdosecd");
        make_repo(&project, "0123456789abcdef0123456789abcdef01234567\n", None);
        sandbox.add(&project);

        sandbox
            .cmd()
            .args(["info", "overdosecd"])
            .assert()
            .success()
            .stdout(
                predicate::str::contains("branch:    detached (0123456)")
                    .and(predicate::str::contains("remote:    -")),
            );
    });
}

#[test]
fn index_files_without_git_metadata_still_load() {
    let sandbox = Sandbox::new();
    let project = sandbox.project("legacy");
    let canonical = sandbox.canonical(&project);
    // Encode the path as JSON so it is embedded safely.
    let path_json = serde_json::to_string(&canonical).expect("encode path");
    fs::create_dir_all(&sandbox.data).unwrap();
    fs::write(
        sandbox.data.join("projects.json"),
        format!(
            r#"{{"version":1,"projects":[{{"id":"abc12345","name":"legacy","path":{path_json},"aliases":[],"tags":[],"created_at":"2026-09-28T00:00:00Z","last_used_at":null,"use_count":0,"pinned":false}}]}}"#
        ),
    )
    .unwrap();

    sandbox
        .cmd()
        .args(["goto", "legacy"])
        .assert()
        .success()
        .stdout(format!("{canonical}\n"));

    sandbox
        .cmd()
        .args(["info", "legacy"])
        .assert()
        .success()
        .stdout(predicate::str::contains("git:       no"));
}

#[test]
fn add_force_refreshes_git_metadata() {
    dual!(sandbox, {
        let project = sandbox.project("notes");
        sandbox.add(&project);

        // The directory becomes a repository after it was indexed.
        make_repo(
            &project,
            "ref: refs/heads/main\n",
            Some(("origin", "git@github.com:o/widgets.git")),
        );

        sandbox
            .cmd()
            .arg("add")
            .arg(&project)
            .arg("--force")
            .assert()
            .success();

        sandbox
            .cmd()
            .args(["goto", "widgets"])
            .assert()
            .success()
            .stdout(format!("{}\n", sandbox.canonical(&project)));
    });
}

#[test]
fn doctor_reports_a_clean_index() {
    dual!(sandbox, {
        sandbox.add(&sandbox.project("overdosecd"));

        sandbox.cmd().arg("doctor").assert().success().stdout(
            predicate::str::contains("1 project)")
                .and(predicate::str::contains("storage: "))
                .and(predicate::str::contains("no problems found")),
        );
    });
}

#[test]
fn doctor_reports_stale_paths_with_exit_1() {
    dual!(sandbox, {
        let project = sandbox.project("gone");
        sandbox.add(&project);
        fs::remove_dir_all(&project).expect("remove project dir");

        sandbox.cmd().arg("doctor").assert().code(1).stdout(
            predicate::str::contains("stale: `gone`")
                .and(predicate::str::contains("directory no longer exists")),
        );
    });
}

#[test]
fn doctor_reports_duplicate_names() {
    dual!(sandbox, {
        let one = sandbox.project("one");
        let two = sandbox.project("two");
        sandbox
            .cmd()
            .arg("add")
            .arg(&one)
            .args(["--name", "App"])
            .assert()
            .success();
        sandbox
            .cmd()
            .arg("add")
            .arg(&two)
            .args(["--name", "app"])
            .assert()
            .success();

        sandbox
            .cmd()
            .arg("doctor")
            .assert()
            .code(1)
            .stdout(predicate::str::contains(
                "duplicate name: `App` is used by 2 projects",
            ));
    });
}

#[test]
fn doctor_reports_control_characters_in_indexed_values() {
    // JSON-only on purpose: the entry must be hand-written, because every
    // entry point refuses control characters.
    let sandbox = Sandbox::new();
    let project = sandbox.project("guarded");
    let canonical = sandbox.canonical(&project);
    let path_json = serde_json::to_string(&canonical).expect("encode path");
    fs::create_dir_all(&sandbox.data).unwrap();
    fs::write(
        sandbox.data.join("projects.json"),
        format!(
            r#"{{"version":1,"projects":[{{"id":"abc12345","name":"bad\u001bname","path":{path_json},"aliases":[],"tags":[],"created_at":"2026-09-28T00:00:00Z","last_used_at":null,"use_count":0,"pinned":false}}]}}"#
        ),
    )
    .unwrap();

    sandbox.cmd().arg("doctor").assert().code(1).stdout(
        predicate::str::contains("unsafe characters")
            .and(predicate::str::contains("unsafe:"))
            .and(predicate::str::contains(r"\u{1b}")),
    );
}

#[test]
fn doctor_reports_quarantine_files_without_deleting_them() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("overdosecd"));
    let quarantined = sandbox.data.join("projects.json.corrupt-20260101000000");
    fs::write(&quarantined, "{ not json").expect("write quarantine file");

    sandbox
        .cmd()
        .arg("doctor")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("quarantine:"));

    assert!(
        quarantined.exists(),
        "doctor must never delete quarantined files"
    );
}

#[test]
fn doctor_fix_refuses_without_a_terminal() {
    let sandbox = Sandbox::new();
    let project = sandbox.project("gone");
    sandbox.add(&project);
    fs::remove_dir_all(&project).expect("remove project dir");

    sandbox
        .cmd()
        .args(["doctor", "--fix"])
        .write_stdin("")
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "refusing to repair in a non-interactive shell",
        ));
}

#[test]
fn doctor_refresh_updates_stored_git_remotes() {
    dual!(sandbox, {
        let project = sandbox.project("checkout");
        make_repo(
            &project,
            "ref: refs/heads/main\n",
            Some(("origin", "git@github.com:o/old-name.git")),
        );
        sandbox.add(&project);

        // The remote changes outside overdosecd's knowledge.
        make_repo(
            &project,
            "ref: refs/heads/main\n",
            Some(("origin", "git@github.com:o/new-name.git")),
        );
        sandbox.cmd().args(["goto", "new-name"]).assert().code(1);

        sandbox
            .cmd()
            .args(["doctor", "--refresh"])
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "refreshed git metadata: 1 of 1 projects updated",
            ));

        sandbox
            .cmd()
            .args(["goto", "new-name"])
            .assert()
            .success()
            .stdout(format!("{}\n", sandbox.canonical(&project)));

        sandbox
            .cmd()
            .args(["doctor", "--refresh"])
            .assert()
            .success()
            .stdout(predicate::str::contains("git metadata is up to date"));
    });
}

#[cfg(unix)]
#[test]
fn doctor_reports_loose_index_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("overdosecd"));
    let index = sandbox.data.join("projects.json");

    fs::set_permissions(&index, fs::Permissions::from_mode(0o644)).expect("chmod 0644");
    sandbox
        .cmd()
        .arg("doctor")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("mode 0644 (expected 0600)"));

    fs::set_permissions(&index, fs::Permissions::from_mode(0o600)).expect("chmod 0600");
    sandbox.cmd().arg("doctor").assert().success();
}

#[cfg(unix)]
#[test]
fn doctor_reports_a_loose_data_directory() {
    use std::os::unix::fs::PermissionsExt;

    dual!(sandbox, {
        sandbox.add(&sandbox.project("overdosecd"));
        fs::set_permissions(&sandbox.data, fs::Permissions::from_mode(0o755)).expect("chmod 0755");

        sandbox
            .cmd()
            .arg("doctor")
            .assert()
            .code(1)
            .stdout(predicate::str::contains("expected 0700"));

        fs::set_permissions(&sandbox.data, fs::Permissions::from_mode(0o700))
            .expect("restore 0700");
        sandbox.cmd().arg("doctor").assert().success();
    });
}

#[cfg(unix)]
#[test]
fn doctor_reports_an_unwritable_data_dir() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("overdosecd"));

    fs::set_permissions(&sandbox.data, fs::Permissions::from_mode(0o500)).expect("chmod 0500");
    sandbox.cmd().arg("doctor").assert().code(1).stdout(
        predicate::str::contains("data directory").and(predicate::str::contains("is not writable")),
    );

    // Restore write access so the tempdir can be cleaned up.
    fs::set_permissions(&sandbox.data, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
}

#[test]
fn config_file_tightens_the_ambiguity_margin() {
    dual!(sandbox, {
        let shallow = sandbox.project("app-one");
        let deep = sandbox.project("deep/nested/app-two");
        sandbox.add(&shallow);
        sandbox.add(&deep);

        sandbox
            .cmd()
            .args(["goto", "app"])
            .assert()
            .failure()
            .code(1)
            .stderr(predicate::str::contains(
                "multiple projects matched query: app",
            ));

        let config = sandbox.write_config("[matching]\nambiguity_margin = 0\n");
        sandbox
            .cmd()
            .env("OVERDOSECD_CONFIG", &config)
            .args(["goto", "app"])
            .assert()
            .success()
            .stdout(format!("{}\n", sandbox.canonical(&shallow)));
    });
}

#[test]
fn broken_config_fails_with_exit_3() {
    dual!(sandbox, {
        sandbox.add(&sandbox.project("overdosecd"));
        let config = sandbox.write_config("[matching]\ndepth_penalty = -3\n");

        sandbox
            .cmd()
            .env("OVERDOSECD_CONFIG", &config)
            .arg("list")
            .assert()
            .failure()
            .code(3)
            .stderr(
                predicate::str::contains("invalid config")
                    .and(predicate::str::contains("overdosecd-config.toml"))
                    .and(predicate::str::contains("matching.depth_penalty")),
            );
    });
}

#[test]
fn missing_config_override_fails_with_exit_3() {
    dual!(sandbox, {
        sandbox
            .cmd()
            .env(
                "OVERDOSECD_CONFIG",
                sandbox.root.join("no-such-config.toml"),
            )
            .arg("list")
            .assert()
            .failure()
            .code(3)
            .stderr(predicate::str::contains("config file not found"));
    });
}

#[test]
fn color_flag_overrides_no_color() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("overdosecd"));

    sandbox
        .cmd()
        .args(["--color", "always", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}["));

    sandbox
        .cmd()
        .args(["list", "--color", "never"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}[").not());
}

#[test]
fn color_reaches_info_doctor_and_ambiguity_output() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("app-one"));
    sandbox.add(&sandbox.project("app-two"));
    let stale = sandbox.project("vanishing");
    sandbox.add(&stale);

    // `info` dims its labels.
    sandbox
        .cmd()
        .args(["--color", "always", "info", "app-one"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}[2mname:"));

    fs::remove_dir_all(&stale).expect("remove project dir");

    // `doctor` renders issue kinds in red.
    sandbox
        .cmd()
        .args(["--color", "always", "doctor"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("\u{1b}[31mstale:"));

    // Ambiguous candidates bold the names on stderr.
    sandbox
        .cmd()
        .args(["--color", "always", "goto", "app"])
        .assert()
        .code(1)
        .stderr(
            predicate::str::contains("\u{1b}[1mapp-one").and(predicate::str::contains("\u{1b}[2m")),
        );
}

#[test]
fn config_and_environment_can_force_color() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("overdosecd"));

    let config = sandbox.write_config("[general]\ncolor = \"always\"\n");
    sandbox
        .cmd()
        .env("OVERDOSECD_CONFIG", &config)
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}["));

    sandbox
        .cmd()
        .env("OVERDOSECD_COLOR", "always")
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}["));

    sandbox
        .cmd()
        .env("OVERDOSECD_COLOR", "rainbow")
        .arg("list")
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains("OVERDOSECD_COLOR"));
}

#[cfg(target_os = "linux")]
#[test]
fn platform_config_file_is_picked_up() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("overdosecd"));

    let path = sandbox
        .root
        .join("config")
        .join("overdosecd")
        .join("config.toml");
    fs::create_dir_all(path.parent().expect("config parent")).expect("create config dir");
    fs::write(&path, "[general]\ncolor = \"always\"\n").expect("write config");

    sandbox
        .cmd()
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}["));
}

#[test]
fn doctor_reports_the_config_file() {
    dual!(sandbox, {
        sandbox.add(&sandbox.project("overdosecd"));

        let config = sandbox.write_config("[general]\ncolor = \"never\"\n");
        sandbox
            .cmd()
            .env("OVERDOSECD_CONFIG", &config)
            .arg("doctor")
            .assert()
            .success()
            .stdout(
                predicate::str::contains("config:")
                    .and(predicate::str::contains("overdosecd-config.toml"))
                    .and(predicate::str::contains("no problems found")),
            );

        // Drop the sandbox's config file (which selects SQLite) to exercise
        // the "no config, built-in defaults apply" path.
        sandbox
            .cmd()
            .env_remove("OVERDOSECD_CONFIG")
            .arg("doctor")
            .assert()
            .success()
            .stdout(predicate::str::contains(
                "(not found; built-in defaults apply)",
            ));
    });
}

#[test]
fn doctor_reports_config_problems_as_issues() {
    dual!(sandbox, {
        sandbox.add(&sandbox.project("overdosecd"));
        let config = sandbox.write_config("this is not toml =");

        sandbox
            .cmd()
            .env("OVERDOSECD_CONFIG", &config)
            .arg("doctor")
            .assert()
            .code(1)
            .stdout(
                predicate::str::contains("config:")
                    .and(predicate::str::contains("overdosecd-config.toml"))
                    .and(predicate::str::contains("issues (1)")),
            );
    });
}

#[test]
fn confident_goto_refuses_fuzzy_matches() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("overdosecd"));

    // A prefix match is confident.
    sandbox
        .cmd()
        .args(["goto", "--confident", "over"])
        .assert()
        .success()
        .stdout(predicate::str::contains("overdosecd"));

    // A fuzzy-only match is not: exit 1, nothing on stdout, and a hint on
    // stderr (the wrapper turns this into the picker).
    sandbox
        .cmd()
        .args(["goto", "--confident", "odscd"])
        .assert()
        .code(1)
        .stdout("")
        .stderr(
            predicate::str::contains("no confident match")
                .and(predicate::str::contains("overdosecd"))
                .and(predicate::str::contains("fuzzy name")),
        );

    // Without the flag the same fuzzy query still jumps.
    sandbox
        .cmd()
        .args(["goto", "odscd"])
        .assert()
        .success()
        .stdout(predicate::str::contains("overdosecd"));

    // Every term of a multi-word query obeys the same rule.
    sandbox.add(&sandbox.project("rust-web-app"));
    sandbox
        .cmd()
        .args(["goto", "--confident", "rust", "web"])
        .assert()
        .failure()
        .code(1);
}

#[test]
fn ui_on_an_empty_index_reports_it() {
    let sandbox = Sandbox::new();

    // Home discovery is on by default, so an empty index only reports the
    // old error when the fallback is switched off.
    let config = sandbox.write_config("[discovery]\nhome = false\n");
    sandbox
        .cmd()
        .env("OVERDOSECD_CONFIG", &config)
        .arg("ui")
        .assert()
        .code(1)
        .stdout("")
        .stderr(predicate::str::contains("no projects indexed"));
}

#[cfg(unix)]
#[test]
fn ui_on_an_empty_index_opens_with_discovery() {
    let sandbox = Sandbox::new();

    // With discovery on, the picker starts and the empty index is no longer
    // an error; without a terminal it refuses on that instead.
    sandbox
        .ttyless_cmd()
        .timeout(std::time::Duration::from_secs(30))
        .arg("ui")
        .assert()
        .code(1)
        .stdout("")
        .stderr(predicate::str::contains("needs a terminal"));
}

#[cfg(unix)]
#[test]
fn ui_refuses_without_a_terminal() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("alpha"));

    sandbox
        .ttyless_cmd()
        .timeout(std::time::Duration::from_secs(30))
        .arg("ui")
        .assert()
        .code(1)
        .stdout("")
        .stderr(predicate::str::contains("needs a terminal"));
}

// Display invariant: no command's stdout or stderr carries a terminal
// control byte for any stored or live value. The sandbox sets `NO_COLOR=1`,
// so even the colour path stays silent and any 0x1b byte is a defect.

/// Writes a hand-built JSON index. The entry guards refuse these values, so a
/// planted file is the only way to exercise the load path.
fn write_json_index(sandbox: &Sandbox, projects: serde_json::Value) {
    fs::create_dir_all(&sandbox.data).expect("create data dir");
    let body = serde_json::json!({ "version": 1, "projects": projects });
    fs::write(
        sandbox.index_file(),
        serde_json::to_string_pretty(&body).expect("encode index"),
    )
    .expect("write index");
}

/// A planted project whose name, alias, tag, and stored remote all carry
/// characters no entry point would accept. Returns the hostile name.
fn plant_hostile_values(sandbox: &Sandbox, canonical_path: &str) -> String {
    let name = "hostile\u{1b}]52;c;QQ";
    let alias = "alias\u{202e}bidi";
    let tag = "tag\u{2066}iso";
    let url = "git@github.com:x/\u{1b}[31mevil.git";
    match sandbox.backend {
        Backend::Json => write_json_index(
            sandbox,
            serde_json::json!([{
                "id": "abc12345",
                "name": name,
                "path": canonical_path,
                "aliases": [alias],
                "tags": [tag],
                "created_at": "2026-09-28T00:00:00Z",
                "last_used_at": null,
                "use_count": 0,
                "pinned": false,
                "git": { "remote_name": "origin", "remote_url": url },
            }]),
        ),
        Backend::Sqlite => {
            sandbox.add(Path::new(canonical_path));
            let connection =
                rusqlite::Connection::open(sandbox.index_file()).expect("open database");
            connection
                .execute(
                    "UPDATE projects SET name = ?1, aliases = ?2, tags = ?3, \
                     git = 1, git_remote_name = 'origin', git_remote_url = ?4",
                    rusqlite::params![
                        name,
                        serde_json::json!([alias]).to_string(),
                        serde_json::json!([tag]).to_string(),
                        url
                    ],
                )
                .expect("plant hostile values");
        }
    }
    name.to_owned()
}

/// Fails when either stream carries a terminal escape or bell byte.
fn assert_no_control_bytes(output: &std::process::Output, what: &str) {
    for (stream, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        assert!(
            !bytes.contains(&0x1b) && !bytes.contains(&0x07),
            "{what}: {stream} carried a terminal control byte:\n{}",
            String::from_utf8_lossy(bytes)
        );
    }
}

#[test]
fn planted_index_values_never_reach_the_terminal_raw() {
    dual!(sandbox, {
        let project = sandbox.project("hostile");
        let canonical = sandbox.canonical(&project);
        let _name = plant_hostile_values(&sandbox, &canonical);

        // The row is escaped, not hidden: `list` still shows the project.
        let listed = sandbox.cmd().arg("list").output().expect("list");
        let text = String::from_utf8_lossy(&listed.stdout);
        assert!(
            text.contains("hostile\\u{1b}]52;c;QQ"),
            "escaped name: {text}"
        );
        assert!(text.contains("\\u{202e}"), "escaped alias: {text}");
        assert!(text.contains("\\u{2066}"), "escaped tag: {text}");

        // `complete` refuses the unsafe candidate outright (it runs on every
        // TAB press and the shell pastes its output into the command line).
        let completed = sandbox
            .cmd()
            .args(["complete", "hostile"])
            .output()
            .expect("complete");
        assert!(
            completed.stdout.is_empty(),
            "unsafe completion candidate: {:?}",
            completed.stdout
        );

        for args in [
            vec!["list"],
            vec!["list", "--debug"],
            vec!["info", "hostile"],
            vec!["doctor"],
            vec!["complete", "hostile"],
            vec!["goto", "--debug", "hostile"],
            vec!["alias", "add", "hostile", "fresh"],
            vec!["remove", "hostile", "--yes"],
        ] {
            let output = sandbox.cmd().args(&args).output().expect("run");
            assert_no_control_bytes(&output, &format!("{args:?}"));
        }
    });
}

/// The renderer invariant as a property: for *arbitrary* stored values, no
/// command may put an `ESC` or `BEL` on either stream (the sandbox pins
/// `NO_COLOR=1`, so any such byte would be a real leak). The planted-values
/// test above pins the known-bad shapes; this one fuzzes the renderers.
#[test]
fn arbitrary_stored_values_never_emit_a_terminal_escape() {
    use proptest::prelude::*;
    use proptest::test_runner::Config as ProptestConfig;

    // NUL is the one character argv cannot carry, and these values are also
    // passed as query arguments below.
    let clean = |chars: Vec<char>| -> String { chars.into_iter().filter(|c| *c != '\0').collect() };

    proptest!(ProptestConfig::with_cases(16), |(
        name in proptest::collection::vec(proptest::char::any(), 0..16),
        alias in proptest::collection::vec(proptest::char::any(), 0..16),
        tag in proptest::collection::vec(proptest::char::any(), 0..16),
        remote_name in proptest::collection::vec(proptest::char::any(), 0..8),
        remote_url in proptest::collection::vec(proptest::char::any(), 0..24),
    )| {
        let name = clean(name);
        let alias = clean(alias);
        let tag = clean(tag);
        let remote_name = clean(remote_name);
        let remote_url = clean(remote_url);

        let sandbox = Sandbox::new();
        let project = sandbox.project("probe");
        let canonical = sandbox.canonical(&project);
        write_json_index(
            &sandbox,
            serde_json::json!([{
                "id": "abc12345",
                "name": name,
                "path": canonical,
                "aliases": [alias],
                "tags": [tag],
                "created_at": "2026-09-28T00:00:00Z",
                "last_used_at": null,
                "use_count": 0,
                "pinned": false,
                "git": { "remote_name": remote_name, "remote_url": remote_url },
            }]),
        );

        for args in [
            vec!["list"],
            vec!["list", "--debug"],
            vec!["info", name.as_str()],
            vec!["doctor"],
            vec!["complete", ""],
            vec!["goto", "--debug", name.as_str()],
        ] {
            let output = sandbox.cmd().args(&args).output().expect("run");
            assert_no_control_bytes(&output, &format!("{args:?}"));
        }
    });
}

/// JSON-only on purpose: the entry must be hand-written, and Unix is the
/// platform where a directory name can carry an escape byte.
#[cfg(unix)]
#[test]
fn goto_refuses_a_path_a_terminal_would_mangle() {
    let sandbox = Sandbox::new();
    let dir = sandbox.projects.join("victim\u{1b}]52;c;QQ");
    fs::create_dir_all(&dir).expect("create hostile dir");
    write_json_index(
        &sandbox,
        serde_json::json!([{
            "id": "abc12345",
            "name": "victim",
            "path": sandbox.canonical(&dir),
            "aliases": [],
            "tags": [],
            "created_at": "2026-09-28T00:00:00Z",
            "last_used_at": null,
            "use_count": 0,
            "pinned": false,
            "git": null,
        }]),
    );

    let output = sandbox
        .cmd()
        .args(["goto", "victim"])
        .output()
        .expect("run");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty for the wrapper: {:?}",
        output.stdout
    );
    assert_no_control_bytes(&output, "goto");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing to print a path"),
        "stderr: {stderr}"
    );
}

/// JSON-only: the hostile name is Unix-only (Win32 forbids control bytes in
/// file names), and the config file is what turns the hint on.
#[cfg(unix)]
#[test]
fn hook_hint_escapes_a_hostile_directory_name() {
    let sandbox = Sandbox::new();
    let config = sandbox.write_config("[general]\nhint = true\n");
    let dir = sandbox.projects.join("watched\u{1b}]0;pwned");
    fs::create_dir_all(&dir).expect("create hostile dir");

    let mut last = None;
    for _ in 0..3 {
        last = Some(
            sandbox
                .cmd()
                .env("OVERDOSECD_CONFIG", &config)
                .arg("hook")
                .current_dir(&dir)
                .output()
                .expect("run hook"),
        );
    }
    let output = last.expect("hook ran three times");
    assert!(output.status.success(), "the hook never fails a cd");
    assert_no_control_bytes(&output, "hook hint");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("visits"), "the third visit hints: {stderr}");
    assert!(
        stderr.contains("\\u{1b}"),
        "the hostile name renders escaped: {stderr}"
    );
}

/// JSON-only on purpose: `add` refuses to store a dirty remote, so only a
/// planted index can exercise the stored-metadata check.
#[test]
fn doctor_reports_unsafe_git_metadata_from_a_planted_index() {
    let sandbox = Sandbox::new();
    let project = sandbox.project("planted");
    write_json_index(
        &sandbox,
        serde_json::json!([{
            "id": "abc12345",
            "name": "planted",
            "path": sandbox.canonical(&project),
            "aliases": [],
            "tags": [],
            "created_at": "2026-09-28T00:00:00Z",
            "last_used_at": null,
            "use_count": 0,
            "pinned": false,
            "git": { "remote_name": "origin", "remote_url": "git@host/\u{1b}[31mx" },
        }]),
    );

    let output = sandbox.cmd().arg("doctor").output().expect("run doctor");
    assert_eq!(output.status.code(), Some(1));
    assert_no_control_bytes(&output, "doctor");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("unsafe:"), "doctor reports it: {text}");
    assert!(
        text.contains("git remote URL"),
        "the field is named: {text}"
    );
}

// Data-directory trust: relative paths, symlinked store files, and
// non-UTF-8 paths are refused before they can choose an index.
// (The same-directory-vs-symlink cases are Unix-only where noted.)

#[test]
fn relative_data_dir_and_config_are_refused() {
    let sandbox = Sandbox::new();

    sandbox
        .cmd()
        .env("OVERDOSECD_DATA_DIR", ".")
        .arg("list")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("must be an absolute path"));

    sandbox
        .cmd()
        .args(["--data-dir", "relative/dir", "list"])
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("must be an absolute path"));

    sandbox
        .cmd()
        .env("OVERDOSECD_CONFIG", "config.toml")
        .arg("list")
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("must be an absolute path"));
}

/// JSON-only on purpose: the check is backend-independent, but the fixture
/// needs the JSON file name.
#[cfg(unix)]
#[test]
fn a_symlinked_index_is_refused_and_reported() {
    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("overdosecd"));

    let real = sandbox.data.join("real-index.json");
    fs::rename(sandbox.index_file(), &real).expect("move index aside");
    std::os::unix::fs::symlink(&real, sandbox.index_file()).expect("plant symlink");

    sandbox
        .cmd()
        .arg("list")
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains("symbolic link"));

    // `doctor` is the diagnostic: it reports the link and keeps going.
    sandbox.cmd().arg("doctor").assert().code(1).stdout(
        predicate::str::contains("symlink:").and(predicate::str::contains("projects.json")),
    );
}

/// SQLite-only: the planted database used to be written through the link.
#[cfg(unix)]
#[test]
fn a_symlinked_sqlite_database_is_refused() {
    let sandbox = Sandbox::with_backend(Backend::Sqlite);
    sandbox.add(&sandbox.project("overdosecd"));

    let real = sandbox.data.join("real-index.db");
    fs::rename(sandbox.index_file(), &real).expect("move database aside");
    std::os::unix::fs::symlink(&real, sandbox.index_file()).expect("plant symlink");

    sandbox
        .cmd()
        .arg("list")
        .assert()
        .failure()
        .code(3)
        .stderr(predicate::str::contains("symbolic link"));
}

/// JSON-only: the quarantine name is a JSON-file concern.
#[test]
fn two_corruptions_keep_both_forensic_copies() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(&sandbox.data).expect("create data dir");

    for body in ["GARBAGE-FIRST", "GARBAGE-SECOND"] {
        fs::write(sandbox.index_file(), body).expect("write garbage index");
        sandbox.cmd().arg("list").assert().code(3);
    }

    let mut quarantined: Vec<String> = fs::read_dir(&sandbox.data)
        .expect("read data dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("projects.json.corrupt-"))
        .collect();
    quarantined.sort();
    assert_eq!(quarantined.len(), 2, "both copies survive: {quarantined:?}");

    let bodies: Vec<String> = quarantined
        .iter()
        .map(|name| fs::read_to_string(sandbox.data.join(name)).expect("read backup"))
        .collect();
    assert!(bodies.iter().any(|body| body == "GARBAGE-FIRST"));
    assert!(bodies.iter().any(|body| body == "GARBAGE-SECOND"));
}

/// JSON-only on purpose: the sidecar lock exists only on this backend.
#[cfg(unix)]
#[test]
fn the_json_lock_file_is_private() {
    use std::os::unix::fs::PermissionsExt;

    let sandbox = Sandbox::new();
    sandbox.add(&sandbox.project("overdosecd"));
    let mode = fs::metadata(sandbox.data.join("projects.lock"))
        .expect("lock file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the lock file is private");
}

/// Unix-only: paths are arbitrary byte strings.
#[cfg(unix)]
#[test]
fn non_utf8_paths_are_refused_on_both_backends() {
    use std::os::unix::ffi::OsStringExt;

    dual!(sandbox, {
        let name = std::ffi::OsString::from_vec(b"latin\xffbyte".to_vec());
        let dir = sandbox.projects.join(&name);
        fs::create_dir_all(&dir).expect("create a non-UTF-8 directory");

        let output = sandbox.cmd().arg("add").arg(&dir).output().expect("run");
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("not valid UTF-8"),
            "refused with a clear message: {stderr}"
        );

        // Neither backend stored a row it could never load.
        sandbox
            .cmd()
            .arg("list")
            .assert()
            .success()
            .stdout(predicate::str::contains("no projects yet"));
    });
}

// Parser and walk bounds: third-party stores and repository metadata
// are read through bounded, regular-file-only handles, and the walk honours
// its cap and its multi-component skip rules.

/// Unix-only: FIFOs are a Unix concept.
#[cfg(unix)]
#[test]
fn import_refuses_a_fifo_store_instead_of_hanging() {
    use std::os::unix::ffi::OsStrExt;

    let sandbox = Sandbox::new();
    let dir = sandbox.root.join("autojump");
    fs::create_dir_all(&dir).expect("create store dir");
    let fifo = dir.join("autojump.txt");
    let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("path has no NUL");
    // SAFETY: `mkfifo` creates a named pipe at the given path.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);

    sandbox
        .cmd()
        .env("AUTOJUMP_DATA_DIR", &dir)
        .args(["import", "autojump"])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("not a regular file"));
}

/// Unix-only: the fixture is a symlink to a device.
#[cfg(unix)]
#[test]
fn import_refuses_a_device_store() {
    let sandbox = Sandbox::new();
    let dir = sandbox.root.join("autojump");
    fs::create_dir_all(&dir).expect("create store dir");
    std::os::unix::fs::symlink("/dev/zero", dir.join("autojump.txt")).expect("plant link");

    sandbox
        .cmd()
        .env("AUTOJUMP_DATA_DIR", &dir)
        .args(["import", "autojump"])
        .timeout(std::time::Duration::from_secs(10))
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("not a regular file"));
}

#[test]
fn import_ignores_relative_store_entries() {
    let sandbox = Sandbox::new();
    let dir = sandbox.root.join("autojump");
    fs::create_dir_all(&dir).expect("create store dir");
    // The entry names a directory that exists relative to the working
    // directory; resolving it against the cwd would index whatever happens
    // to be there, so a relative entry is malformed input.
    sandbox.project("relative-project");
    fs::write(dir.join("autojump.txt"), "9\trelative-project\n").expect("write store");

    let output = sandbox
        .cmd()
        .current_dir(&sandbox.projects)
        .env("AUTOJUMP_DATA_DIR", &dir)
        .args(["import", "autojump", "--dry-run"])
        .output()
        .expect("run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("nothing to import"), "{stdout}");
    assert!(stdout.contains("1 malformed"), "{stdout}");
}

#[test]
fn scan_skips_the_trash_by_builtin_rule() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(
        sandbox
            .root
            .join(".local/share/Trash/expired/secret-project"),
    )
    .expect("create trash tree");
    fs::create_dir_all(sandbox.root.join("code/visible-project")).expect("create project");

    let output = sandbox
        .cmd()
        .arg("scan")
        .arg(&sandbox.root)
        .output()
        .expect("run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("visible-project"), "{stdout}");
    assert!(
        !stdout.contains("secret-project") && !stdout.contains("Trash"),
        "trash must be skipped: {stdout}"
    );
}

#[test]
fn scan_honours_multi_component_skip_patterns() {
    let sandbox = Sandbox::new();
    let config = sandbox.write_config("[discovery]\nskip = [\"cache/private\"]\n");
    fs::create_dir_all(sandbox.root.join("cache/private/thing")).expect("create private");
    fs::create_dir_all(sandbox.root.join("cache/visible")).expect("create visible");

    let output = sandbox
        .cmd()
        .env("OVERDOSECD_CONFIG", &config)
        .arg("scan")
        .arg(&sandbox.root)
        .output()
        .expect("run");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("visible"), "{stdout}");
    assert!(!stdout.contains("private"), "skipped by pattern: {stdout}");
}

/// Unix-only: the fixture uses `set_len` to build a large sparse file.
#[cfg(unix)]
#[test]
fn add_survives_an_oversized_git_config() {
    let sandbox = Sandbox::new();
    let project = sandbox.project("hostile-repo");
    fs::create_dir_all(project.join(".git")).expect("create .git");
    let config = fs::File::create(project.join(".git/config")).expect("create config");
    config.set_len(64 << 20).expect("sparse size");
    fs::write(project.join(".git/HEAD"), "ref: refs/heads/main\n").expect("write HEAD");

    sandbox.cmd().arg("add").arg(&project).assert().success();

    // The repository is still recognized; only the oversized remote is
    // treated as absent.
    sandbox
        .cmd()
        .args(["info", "hostile-repo"])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("branch:    main")
                .and(predicate::str::contains("remote:    -")),
        );
}
