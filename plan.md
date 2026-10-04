# overdosecd — Roadmap

> A small, moody project jumper for messy folders, forgotten branches, late-night
> coding, and the comfort of returning to projects that wait for you.

This file is forward-looking only. What already shipped lives in
[CHANGELOG.md](CHANGELOG.md); how the code works lives in [AGENTS.md](AGENTS.md)
and [README.md](README.md).

## Where we are

0.2.0 shipped the first slice of the git-health work: `.git/config` reads honor
`include`/`includeIf` and `url.<base>.insteadOf`, bare repositories are
detected, and projects remember their kind (`info`'s `type:`, `list --type`,
a picker badge, SQLite schema v2 with a backed-up migration). The status
reader and its UI moved to v0.5.0 by a 2026-10-04 re-scope: the object layer
(flate2, packfile, delta chains) is the riskiest parser work on this roadmap
and does not need to hold the config debt and typing hostage.

0.1.0 was the first release under the overdosecd name — a rename of the
predecessor project, whose crate no longer exists. It carries the whole shipped
feature set: `add`/`list`/`goto`/`info`/`remove` with alias/tag/pin/rename and
git-aware matching; `config.toml`, width-aware output, `--debug`, completions,
`doctor`; the JSON and SQLite backends with `migrate` and the `usage` jump log
(`list --since`, `info` history); the inline picker (`ui`) with the action
palette, marked-row batches, health view, detail pane, and the bounded `$HOME`
fallback (`home-dirs.json`, `scan --home`); `overdosecd import
zoxide|autojump|zsh-z` and `scan <root>...`; the opt-in `cd` hook
(`[general] hook`, visit hints under `[general] hint`) and multi-word queries;
and the `ocd` shell wrapper. It is Linux-only — a crate-root guard refuses to
build elsewhere — and releases are a crates.io publish plus GitHub release
notes: no pre-built binaries, no platform matrix.

The CLI is complete for its original job — index, organize, inspect, jump.
Everything below adds new capability or pays down an accepted debt.

## Guiding principles

- **Local-first**: no cloud, no account, no telemetry.
- **Fast**: jumping should feel instant.
- **Shell-friendly**: it should integrate naturally with `cd`.
- **Predictable**: manual control should always exist.
- **Extensible**: start simple, but leave room for smarter behavior.
- **Rusty**: strong types, good error handling, and tests.

Three invariants outlive every milestone (details in `AGENTS.md`): `goto` prints
only the path on stdout; no command does filesystem work per query that it does
not need; and display safety is a property of the **renderer** — no command's
stdout or stderr can carry a terminal escape from any stored or live value.

## Non-goals

- Cloud sync, accounts, telemetry.
- Multi-user sharing.
- Discovery never indexes the filesystem: projects enter the index only
  through an explicit `add`. The picker may *search* bounded roots (`$HOME` by
  default) from a rebuildable cache, with a depth cap, a skip list, and an off
  switch.
- GUI app.
- Plugin system.
- Support beyond Linux: the crate builds on Linux only and refuses to build
  elsewhere.

## Roadmap

Order: v0.2.0 → v0.3.0 → v0.4.0 → v0.5.0.
The next milestone carries a full task list and definition of done; later ones
are sketches that get detailed when they become next.

### Left alone on purpose

Recorded so this is not reopened: the matcher's scoring and signal caps
(output-frozen), `format_projects_with_missing` (the tested layout engine),
`TtyBackend` and the `Session`/`PendingRestore` plumbing (each exists for a
recorded failure), the `Picker` state machine (wide, not deep), `AnyStore`
(the trait is not object-safe; it retires with the JSON backend in v0.3.0),
the three import parsers, and the hand-rolled `git.rs` (v0.2.0 extends it; it
is never replaced by `git2`/`gix`/subprocess).

### v0.2.0 — Config debt & project typing

Goal: make the git reads honest about real-world configs, and say what kind of
project an entry is.

Deferred on 2026-10-04 to v0.5.0: the `git::status()` reader and its UI. The
object layer (flate2, packfile, delta chains) is the riskiest parser work on
this roadmap; the config debt and project typing are orthogonal to it and ship
first.

Tasks:

- [x] 1. **Git-config debt** — `include.path` and `includeIf.gitdir[:/]`
  (`~` and config-relative expansion, ≤8 files, caps, regular files only),
  `url.<base>.insteadOf` longest-prefix rewrite, and bare-repository detection
  (`core.bare`, or `HEAD` + `objects/` + `refs/`). Hostile-include tests; a
  SECURITY.md note that a cloned config can steer only the sanitized remote
  display. Removed the stale config limitation.
- [x] 2. **Schema v2** — `schema_version` 1→2 with `ALTER TABLE projects ADD
  COLUMN kind TEXT`, migrating only on the first writable open; `load_readonly`
  reports an old version instead of migrating, and the pre-migration copy goes
  through `paths::sibling_backup`. JSON gets `#[serde(default)] kind`. `doctor`
  reports the version.
- [x] 3. **Project typing** — `project::kind_of` from marker files
  (`Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`) with a stable
  precedence: `rust | node | python | go | unknown`. Detected once at `add` and
  stored — never stat'd per query. `info` gains `type:`, `list --type <t>`
  filters, picker rows carry a badge, and `doctor --refresh` re-detects. Kind
  is display and filter, never a matcher signal.
- [x] 4. **Release** — CHANGELOG section, README reference, AGENTS.md facts
  (include semantics and budgets, the schema-v2 rule, the kind rule), tick this
  milestone, delete the fixed limitation's row, bump `Cargo.toml`, tag → publish
  + GitHub release.

Definition of done:

- `include`/`includeIf`/`insteadOf`/bare detection run under the stated budgets,
  refuse non-regular files, and no hostile repository escapes a cap or reaches
  a terminal unsanitized; the old config limitation is gone.
- Schema v1→2 is non-destructive, keeps a backup, runs only on writable open,
  `load_readonly` reports the old version instead of migrating, and `doctor`
  reports it.
- `kind` is detected once at `add` and never on the query path; ranking and
  `--debug` output stay bit-identical — kind is display and filter only.
- `list`, `goto`, and `complete` hold the recorded floors at 1k/10k/100k.
- No new runtime dependency; every new parser is proptested.

### v0.3.0 — Notes, entry hooks & single backend

Goal: make a project remember what it was for — on one storage format.

- JSON retirement: when `projects.json` exists, one-shot auto-migrate (reuse
  the `migrate` machinery, keep the `.migrated-<ts>` backup), then delete
  `JsonStore`, `AnyStore`, the `[storage] backend` key, and the `dual!` test
  machinery; `doctor` sheds the JSON↔SQLite disagreement check and keeps
  quarantine reporting. SQLite becomes the only backend.
- `ocd note add|list|remove <project>` — notes stored with the project; the
  schema moves to v3, once, SQLite-only.
- Per-project profiles: environment variables exported on jump, and an optional
  run-on-enter command. Security rule: hooks are explicit, per-project, and
  user-authored. Nothing imported or scanned may register a hook, and a hook is
  shown before it runs. SECURITY.md grows the hook boundary.
- To decide when detailed: notes shape (one body vs ordered list) and profile
  storage (columns vs a JSON blob).

### v0.4.0 — tmux & worktree integration

Goal: land in the right session and the right worktree.

- `ocd <project>` inside tmux attaches the project's session or creates one
  rooted there, following the maintainer's tmux session conventions (their
  setup is the reference environment, not a dependency; outside tmux this
  degrades to today's behavior).
- Worktree-aware indexing: sibling worktrees of one repository (read through
  `.git/worktrees/`, still hand-rolled) become related entries, queryable as
  `ocd <repo>:<branch>`, and `info` lists them.
- To decide when detailed: session-per-project vs a window in one session; flag
  name (`--tmux` vs the reserved subcommand namespace); how `:` queries
  negotiate with name matching.

### v0.5.0 — Git status & health UI

Goal: see the state of a project before jumping into it. (Deferred from v0.2.0
on 2026-10-04; gets full build-ready detail when it becomes next.)

`git::status()` is built in three layers of cost, each with budgets, over the
hand-rolled reader:

- **Free layer** — plain text: mid-operation (`MERGE_HEAD`, `CHERRY_PICK_HEAD`,
  `REVERT_HEAD`, bisect, rebase), unmerged index stages (conflicts), stash
  count, upstream configured.
- **Stat layer** — an index v2/v3 parser plus one `stat()` per entry with early
  exit on the first mismatch: `dirty`, git's own racy heuristic.
- **Object layer** — `flate2` (backend choice recorded when this milestone
  turns next; expected `rust_backend` so `cargo deny`, the MSRV job, and source
  installs stay clean), loose objects, packed-idx v2, pack v2 with bounded
  delta chains, a capped walk (200, rendered `200+`), and a visited-set guard:
  ahead/behind.

Fail-closed: an unreadable or hostile index/object store never reports "clean" —
it reports nothing. Every new parser gets caps, bounds checks, and proptests,
and every rendered string rides `sanitize` + `output::print*`. Documented
non-goals: untracked files (`.gitignore` matching is a project of its own),
staged-vs-HEAD counts (tree diffing), and index v4/split-index (reported as
`unknown`).

Tasks (sketch):

- [ ] Status reader — the three layers in `git.rs`, fail-closed, per-parse
  caps, fixtures, proptests, and an `#[ignore]`d accuracy check against real
  `git status` on generated repositories.
- [ ] Status UI — status in `info` and the picker detail pane (single project,
  always); `list --status` behind a flag plus `[general] status = false` (off
  by default; flag > env > config > default). `remote_name` is read so the
  upstream renders as `origin/main`. A pty-smoke scenario for the detail pane;
  `--status` bench numbers recorded.
- [ ] Release — CHANGELOG, README, AGENTS.md facts (status semantics,
  per-layer budgets), tick, bump, tag.

Definition of done:

- Every field `status()` reports is true or absent; no hostile input can make
  it report "clean".
- Plain `list` does no status read; `list`, `goto`, and `complete` hold the
  recorded floors at 1k/10k/100k; `--status` numbers recorded.
- Ranking and `--debug` output stay bit-identical — status never reaches the
  matcher.
- `cargo deny` is clean with `flate2`; the pty smoke harness covers the detail
  pane; all new parsers are proptested.

### Backlog (unscheduled)

- Multi-machine sync through a plain file (git or a synced folder): only if a
  concrete need shows up; it needs a merge policy for usage data and stable ids
  across machines.

### Decided (do not reopen)

- `ocd goto foo` prints the path; the `ocd` wrapper performs the `cd`, and
  `ocd --cmd` is the script path.
- No search daemon: process startup is a few milliseconds and exact `goto` is
  3 ms at 1k.
- No matcher early exit: it cannot change the winner and would thin `--debug`
  listings; revisit only if the recorded numbers demand it.
- The JSON backend retires in v0.3.0, with a one-shot auto-migrate and a kept
  backup.
- The multiplexer target is tmux only.
- The git status reader and its UI ship as v0.5.0, not v0.2.0 (2026-10-04
  re-scope): the object layer — `flate2`, packfile, delta chains — is the
  roadmap's riskiest parser work and would hold the orthogonal config-debt and
  typing wins hostage.
- Linux is the only supported platform (0.1.0): the crate refuses to build
  elsewhere, and no pre-built binaries are published — crates.io source
  installs are the delivery path.

## Known limitations

| # | Limitation | Track |
| - | ---------- | ----- |
| 1 | Subcommand names are reserved by the `ocd` wrapper, so a project named `tag` needs `ocd goto tag` | by design |
| 2 | Ranking aggregates jumps into one `use_count` instead of weighting the history log | by design |
| 3 | The JSON backend rewrites the whole index file on every mutation (SQLite updates single rows) | by design (json); moot at v0.3.0 |
| 4 | Data directories, config paths, and store entries must be absolute; a relative value is refused | by design |

## Recorded numbers

`cargo bench` medians on Linux x86_64, release binary. These are the floor:
no later milestone may regress them without recording why.

| metric | json 1k | json 10k | json 100k | sqlite 1k | sqlite 10k | sqlite 100k |
| ------ | ------- | -------- | --------- | --------- | ---------- | ----------- |
| `list` | 5.0 ms | 39 ms | 385 ms | 5.5 ms | 39 ms | 379 ms |
| `goto` exact, untracked | 3.0 ms | 17 ms | 139 ms | 3.4 ms | 17 ms | 135 ms |
| `goto` exact, tracked | 5.2 ms | 34 ms | 304 ms | 3.7 ms | 17 ms | 131 ms |
| `complete` | 4.6 ms | 32 ms | 287 ms | 4.8 ms | 32 ms | 281 ms |

Recorded during development:

- Picker: one keystroke re-ranks 100k projects in ~37 ms, empty-query reset
  ~4 ms.
- Home scan: a warm scan (3,361 directories, 344 project-like) takes ~56 ms,
  the first cold scan ~12 s (I/O-bound, hence the background thread), and the
  cache is ~550 KB.
- Import: 1,000 autojump entries with a git read each take ~84 ms.
- cd hook: ~3.4 ms per recorded jump at 1,000 projects and ~2.9 ms while
  writing visit counters.
- Multi-word queries: a two-word keystroke re-ranks 100k in ~48.9 ms
  (single-word ~38.6 ms).
- Build: the `--release` binary is ~4.4 MB.

## Release process

1. Bump `Cargo.toml`, write the `CHANGELOG.md` section, and run
   `cargo publish --dry-run` (the tag push publishes for real).
2. Merge to `main`, then tag: `git tag -a vX.Y.Z -m "overdosecd vX.Y.Z: …"` and
   push it — the tag runs `.github/workflows/release.yml`, which publishes the
   crate and creates the GitHub release from the CHANGELOG section.
3. Verify crates.io (`cargo install overdosecd` gets the new version) and the
   GitHub release.
4. Delete the merged feature branch.
