//! End-to-end benchmarks for the CLI.
//!
//! Every iteration spawns the real binary, so the numbers include process
//! startup, index load, matching, and rendering — the cost a `wcd` jump
//! actually pays. Fixtures hand-write `projects.json` directly (mirroring the
//! version-1 shape in `src/store/json.rs`) instead of calling `add` per
//! project, because every `add` rewrites the whole index. The SQLite fixture
//! is built from that JSON by running `migrate` once, so it exercises the
//! real import path instead of duplicating the schema here.
//!
//! Run with `cargo bench`; the recorded numbers live in `plan.md` and
//! `CHANGELOG.md`.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use chrono::{DateTime, Duration, Utc};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use serde::Serialize;

/// Index sizes worth tracking: personal scale, an order beyond it, and the
/// stress case that keeps partial updates honest.
const SIZES: [usize; 3] = [1_000, 10_000, 100_000];

/// The storage backends, so the numbers contrast the JSON rewrite with the
/// SQLite row updates.
const BACKENDS: [Backend; 2] = [Backend::Json, Backend::Sqlite];

/// A query matching a slice of the index, for prefix-ambiguity cost.
const PREFIX_QUERY: &str = "proj-005";

/// A query that matches nothing at all, forcing a full scan.
const NO_MATCH_QUERY: &str = "zzzqqq";

/// Which backend a fixture indexes into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Json,
    Sqlite,
}

impl Backend {
    fn label(self) -> &'static str {
        match self {
            Backend::Json => "json",
            Backend::Sqlite => "sqlite",
        }
    }
}

/// A generated index plus the real project directories it points at.
struct Fixture {
    root: tempfile::TempDir,
    data_dir: PathBuf,
    /// The config that selects the backend, for everything but JSON (the
    /// built-in default).
    config: Option<PathBuf>,
    bin: PathBuf,
    /// The name of the project at `count / 2`, used for exact-match benches.
    exact: String,
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
}

/// Mirror of `project::GitInfo`; no real `.git` directory is needed because
/// the binary reads the stored field rather than the filesystem.
#[derive(Serialize)]
struct FixtureGit {
    remote_name: Option<String>,
    remote_url: Option<String>,
}

impl Fixture {
    fn new(count: usize, backend: Backend) -> Self {
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
            });
        }

        let mut body = serde_json::to_string_pretty(&FixtureFile {
            version: 1,
            projects,
        })
        .expect("serialize fixture");
        body.push('\n');
        fs::write(data_dir.join("projects.json"), body).expect("write fixture index");

        let config = match backend {
            Backend::Json => None,
            Backend::Sqlite => {
                let path = root.path().join("withercd-config.toml");
                fs::write(&path, "[storage]\nbackend = \"sqlite\"\n")
                    .expect("write fixture config");
                Some(path)
            }
        };

        let fixture = Self {
            bin: PathBuf::from(env!("CARGO_BIN_EXE_withercd")),
            exact: format!("proj-{:05}", count / 2),
            config,
            data_dir,
            root,
        };

        if backend == Backend::Sqlite {
            // One real import, outside the measured region.
            let output = fixture.run(&["migrate"]);
            assert!(
                output.status.success(),
                "fixture migration failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        fixture.sanity_check();
        fixture
    }

    /// Fails loudly when the fixture does not round-trip, so a silently
    /// quarantined index cannot turn every benchmark into an error path.
    fn sanity_check(&self) {
        let output = self.run(&["goto", &self.exact, "--no-track"]);
        assert!(
            output.status.success(),
            "fixture sanity check failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(&self.exact),
            "expected the exact query to resolve, got: {stdout}"
        );
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(&self.bin);
        command
            .args(args)
            .env("WITHERCD_DATA_DIR", &self.data_dir)
            .env("HOME", self.root.path())
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("NO_COLOR", "1")
            .stdin(Stdio::null());
        if let Some(config) = &self.config {
            command.env("WITHERCD_CONFIG", config);
        }
        command.output().expect("run withercd")
    }
}

fn bench_cli(c: &mut Criterion) {
    let largest = *SIZES.last().expect("at least one size");

    for backend in BACKENDS {
        for count in SIZES {
            let fixture = Fixture::new(count, backend);
            let mut group = c.benchmark_group(format!("{}-{count}-projects", backend.label()));
            if count == largest {
                // A 100k index takes real time per iteration; keep the
                // sample budget bounded.
                group.sample_size(10);
                group.warm_up_time(std::time::Duration::from_secs(1));
                group.measurement_time(std::time::Duration::from_secs(5));
            }

            group.bench_function("list", |b| {
                b.iter(|| fixture.run(&["list"]));
            });
            group.bench_function("goto-exact-no-track", |b| {
                b.iter(|| fixture.run(&["goto", &fixture.exact, "--no-track"]));
            });
            group.bench_function("goto-prefix-no-track", |b| {
                b.iter(|| fixture.run(&["goto", PREFIX_QUERY, "--no-track"]));
            });
            group.bench_function("goto-no-match-no-track", |b| {
                b.iter(|| fixture.run(&["goto", NO_MATCH_QUERY, "--no-track"]));
            });
            // The tracked jump is the mutation budget: JSON rewrites the whole
            // index under the lock; SQLite updates one row and appends a jump.
            group.bench_function("goto-exact-tracked", |b| {
                b.iter(|| fixture.run(&["goto", &fixture.exact]));
            });
            // One TAB press: the completion helper stats every project.
            group.bench_function("complete", |b| {
                b.iter(|| fixture.run(&["complete", "proj"]));
            });

            group.finish();
        }
    }
}

/// `import autojump`: the one-shot adoption cost — parse the store, read git
/// metadata per new project, and write one batch. Imports are idempotent, so
/// each iteration starts from an empty index (the realistic first run).
fn bench_import(c: &mut Criterion) {
    const COUNT: usize = 1_000;
    let fixture = Fixture::new(COUNT, Backend::Json);
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
    group.bench_function("autojump-1k-json", |b| {
        b.iter_batched(
            || {
                let _ = fs::remove_file(fixture.data_dir.join("projects.json"));
            },
            |()| {
                let output = Command::new(&fixture.bin)
                    .args(["import", "autojump"])
                    .env("WITHERCD_DATA_DIR", &fixture.data_dir)
                    .env("AUTOJUMP_DATA_DIR", &store)
                    .env("HOME", fixture.root.path())
                    .env("XDG_CONFIG_HOME", fixture.root.path().join("config"))
                    .env("NO_COLOR", "1")
                    .stdin(Stdio::null())
                    .output()
                    .expect("run import");
                assert!(
                    output.status.success(),
                    "import failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                output
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_cli, bench_import);
criterion_main!(benches);
