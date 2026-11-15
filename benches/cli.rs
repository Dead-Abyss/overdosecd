//! End-to-end benchmarks for the CLI.
//!
//! Every iteration spawns the real binary, so the numbers include process
//! startup, index load, matching, and rendering — the cost a `ocd` jump
//! actually pays. Fixtures hand-write a legacy `projects.json` and build the
//! database from it by running `migrate` once, so they exercise the real
//! import path instead of duplicating the schema here.
//!
//! Run with `cargo bench`; the recorded numbers live in `plan.md` and
//! `CHANGELOG.md`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use chrono::{DateTime, Duration, Utc};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use serde::Serialize;

/// Index sizes worth tracking: personal scale, an order beyond it, and the
/// stress case that keeps partial updates honest.
const SIZES: [usize; 3] = [1_000, 10_000, 100_000];

/// A query matching a slice of the index, for prefix-ambiguity cost.
const PREFIX_QUERY: &str = "proj-005";

/// A query that matches nothing at all, forcing a full scan.
const NO_MATCH_QUERY: &str = "zzzqqq";

/// A generated index plus the real project directories it points at.
struct Fixture {
    root: tempfile::TempDir,
    data_dir: PathBuf,
    bin: PathBuf,
    /// The name of the project at `count / 2`, used for exact-match benches.
    exact: String,
    /// How many projects the fixture holds.
    count: usize,
}

/// On-disk envelope of `projects.json`, mirroring `store::json::StoreFile`.
#[derive(Serialize)]
struct FixtureFile {
    version: u32,
    projects: Vec<FixtureProject>,
}

/// Mirror of `project::Project` for fixture generation; field names and types
/// must stay parse-compatible with the real store.
#[derive(Serialize)]
struct FixtureProject {
    id: String,
    name: String,
    path: PathBuf,
    aliases: Vec<String>,
    tags: Vec<String>,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
    use_count: u64,
    pinned: bool,
    git: Option<FixtureGit>,
    kind: String,
    env: BTreeMap<String, String>,
    enter_cmd: Option<String>,
}

/// Mirror of `project::GitInfo`; no real `.git` directory is needed because
/// the binary reads the stored field rather than the filesystem.
#[derive(Serialize)]
struct FixtureGit {
    remote_name: Option<String>,
    remote_url: Option<String>,
}

impl Fixture {
    fn new(count: usize) -> Self {
        let root = tempfile::tempdir().expect("bench fixture temp dir");
        let data_dir = root.path().join("data");
        fs::create_dir_all(&data_dir).expect("create data dir");

        let now = Utc::now();
        let mut projects = Vec::with_capacity(count);
        for index in 0..count {
            let name = format!("proj-{index:05}");
            let path = root
                .path()
                .join("code")
                .join(format!("group-{:02}", index % 50))
                .join(&name);
            fs::create_dir_all(&path).expect("create project dir");

            projects.push(FixtureProject {
                id: format!("{index:08x}"),
                name,
                path,
                aliases: if index % 7 == 0 {
                    vec![format!("alias-{index:05}")]
                } else {
                    Vec::new()
                },
                tags: if index % 5 == 0 {
                    vec![format!("group-{:02}", index % 50)]
                } else {
                    Vec::new()
                },
                created_at: now - Duration::minutes(index as i64),
                last_used_at: (index % 3 == 0).then(|| now - Duration::hours((index % 200) as i64)),
                use_count: (index % 40) as u64,
                pinned: index % 500 == 0,
                git: (index % 10 == 0).then(|| FixtureGit {
                    remote_name: Some("origin".to_owned()),
                    remote_url: Some(format!("git@github.com:dead-abyss/proj-{index:05}.git")),
                }),
                kind: "unknown".to_owned(),
                env: BTreeMap::new(),
                enter_cmd: None,
            });
        }

        let mut body = serde_json::to_string_pretty(&FixtureFile {
            version: 1,
            projects,
        })
        .expect("serialize fixture");
        body.push('\n');
        fs::write(data_dir.join("projects.json"), body).expect("write fixture index");

        let fixture = Self {
            bin: PathBuf::from(env!("CARGO_BIN_EXE_overdosecd")),
            exact: format!("proj-{:05}", count / 2),
            count,
            data_dir,
            root,
        };

        // One real import, outside the measured region.
        let output = fixture.run(&["migrate"]);
        assert!(
            output.status.success(),
            "fixture migration failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        fixture.sanity_check();
        fixture
    }

    /// Fails loudly when the fixture does not round-trip, so a silently
    /// quarantined index cannot turn every benchmark into an error path.
    fn sanity_check(&self) {
        let output = checked(
            self.run(&["goto", &self.exact, "--no-track"]),
            "fixture goto",
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(&self.exact),
            "expected the exact query to resolve, got: {stdout}"
        );
        // A partially migrated fixture would still resolve one exact match;
        // the row count proves every project landed.
        let listed = checked(self.run(&["list", "--all"]), "fixture list");
        let lines = String::from_utf8_lossy(&listed.stdout).lines().count();
        assert_eq!(
            lines,
            self.count + 1,
            "the fixture must hold every project plus the header"
        );
    }

    /// Runs the binary with the fixture's environment.
    fn command(&self) -> Command {
        let mut command = Command::new(&self.bin);
        command
            .env("OVERDOSECD_DATA_DIR", &self.data_dir)
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("NO_COLOR", "1")
            .stdin(Stdio::null());
        command
    }

    /// A private copy of the index a mutating bench can write to, so the
    /// measured iterations do not drift the fixture they measure.
    fn fresh_data_dir(&self) -> PathBuf {
        let dir = self.root.path().join("mutable");
        remove_dir_if_present(&dir);
        fs::create_dir_all(&dir).expect("create mutable data dir");
        for suffix in ["", "-wal", "-shm"] {
            let mut from = self.data_dir.join("projects.db").into_os_string();
            from.push(suffix);
            let from = PathBuf::from(from);
            if from.exists() {
                let mut to = dir.join("projects.db").into_os_string();
                to.push(suffix);
                fs::copy(&from, PathBuf::from(to)).expect("copy the bench index");
            }
        }
        dir
    }

    /// [`Self::run`] against an explicit data directory (the mutable copy).
    fn run_in(&self, data_dir: &Path, args: &[&str]) -> Output {
        self.command()
            .env("OVERDOSECD_DATA_DIR", data_dir)
            .args(args)
            .output()
            .expect("run overdosecd")
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("run overdosecd")
    }
}

/// Fails loudly when the binary did not succeed: a silently failing command
/// would make the "faster" error path look like a benchmark win.
fn checked(output: Output, what: &str) -> Output {
    assert!(
        output.status.success(),
        "{what} failed (status {:?}): {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Asserts the command exited with `code` (used by the deliberate error-path
/// benchmark) and fails loudly with stderr when it did not.
fn expect_code(output: Output, what: &str, code: i32) -> Output {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{what} exited with the wrong code: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Removes a stale database file, but only tolerates "already gone": a
/// permission or mount error must fail the bench, not silently measure the
/// idempotent "nothing new" path.
fn remove_file_if_present(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => panic!("could not remove {}: {err}", path.display()),
    }
}

fn remove_dir_if_present(path: &Path) {
    match fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => panic!("could not remove {}: {err}", path.display()),
    }
}

fn bench_cli(c: &mut Criterion) {
    let largest = *SIZES.last().expect("at least one size");

    for count in SIZES {
        let fixture = Fixture::new(count);
        let mut group = c.benchmark_group(format!("sqlite-{count}-projects"));
        if count == largest {
            // A 100k index takes real time per iteration; keep the sample
            // budget bounded.
            group.sample_size(10);
            group.warm_up_time(std::time::Duration::from_secs(1));
            group.measurement_time(std::time::Duration::from_secs(5));
        }

        group.bench_function("list", |b| {
            b.iter(|| checked(fixture.run(&["list"]), "list"));
        });
        group.bench_function("goto-exact-no-track", |b| {
            b.iter(|| {
                checked(
                    fixture.run(&["goto", &fixture.exact, "--no-track"]),
                    "goto exact",
                )
            });
        });
        group.bench_function("goto-prefix-no-track", |b| {
            b.iter(|| {
                checked(
                    fixture.run(&["goto", PREFIX_QUERY, "--no-track"]),
                    "goto prefix",
                )
            });
        });
        group.bench_function("goto-no-match-no-track", |b| {
            b.iter(|| {
                // The full-scan error path: exit 1 is the expected result,
                // not a bench failure.
                expect_code(
                    fixture.run(&["goto", NO_MATCH_QUERY, "--no-track"]),
                    "goto no-match",
                    1,
                )
            });
        });
        // The tracked jump is the mutation budget: one row update plus a
        // usage row, under the write lock. It mutates its index, so run it
        // against a fresh copy per iteration (a 100k copy per iteration would
        // dominate the measured operation, so that size keeps the shortcut and
        // the drift is a bounded single-row update).
        if count == largest {
            // shortcut: the 100k tracked bench measures against a drifting
            // index; only use_count/last_used_at and one usage row change.
            group.bench_function("goto-exact-tracked", |b| {
                b.iter(|| checked(fixture.run(&["goto", &fixture.exact]), "goto tracked"));
            });
        } else {
            group.bench_function("goto-exact-tracked", |b| {
                b.iter_batched(
                    || fixture.fresh_data_dir(),
                    |data_dir| {
                        checked(
                            fixture.run_in(&data_dir, &["goto", &fixture.exact]),
                            "goto tracked",
                        )
                    },
                    BatchSize::SmallInput,
                );
            });
        }
        // One TAB press: the completion helper stats every project.
        group.bench_function("complete", |b| {
            b.iter(|| checked(fixture.run(&["complete", "proj"]), "complete"));
        });

        group.finish();
    }
}

/// `import autojump`: the one-shot adoption cost — parse the store, read git
/// metadata per new project, and write one batch. Imports are idempotent, so
/// each iteration starts from an empty index (the realistic first run).
fn bench_import(c: &mut Criterion) {
    const COUNT: usize = 1_000;
    let fixture = Fixture::new(COUNT);
    let store = fixture.root.path().join("autojump-store");
    fs::create_dir_all(&store).expect("create import source");
    let mut body = String::new();
    for index in 0..COUNT {
        let path = fixture
            .root
            .path()
            .join("code")
            .join(format!("group-{:02}", index % 50))
            .join(format!("proj-{index:05}"));
        body.push_str(&format!("5.0\t{}\n", path.display()));
    }
    fs::write(store.join("autojump.txt"), body).expect("write autojump fixture");

    let mut group = c.benchmark_group("import");
    group.bench_function("autojump-1k", |b| {
        b.iter_batched(
            || {
                // A fresh index per iteration: imports are idempotent.
                for suffix in ["", "-wal", "-shm", "-journal"] {
                    let mut name = fixture.data_dir.join("projects.db").into_os_string();
                    name.push(suffix);
                    remove_file_if_present(&PathBuf::from(name));
                }
            },
            |()| {
                let output = fixture
                    .command()
                    .env("AUTOJUMP_DATA_DIR", &store)
                    .args(["import", "autojump"])
                    .output()
                    .expect("run import");
                checked(output, "import autojump")
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_cli, bench_import);
criterion_main!(benches);
