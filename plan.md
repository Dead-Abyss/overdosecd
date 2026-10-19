# overdosecd — Roadmap

> A small, moody project jumper for messy folders, forgotten branches, late-night
> coding, and the comfort of returning to projects that wait for you.

This file is forward-looking only. What already shipped lives in
[CHANGELOG.md](CHANGELOG.md); how the code works lives in [AGENTS.md](AGENTS.md)
and [README.md](README.md).

## Where we are

0.4.0 (code complete 2026-10-19) is the look-and-worktree release: a typed
theme (`bleed` by default, `base` to restore the 0.3.0 colors) and worktree
awareness with `ocd <project>:<branch>` as an exact lookup outside the
matcher. The tag and crates.io publish are the remaining release step.

0.3.0 shipped the single-backend release: `projects.db` is the only index (a
0.2.0 `projects.json` migrates once automatically, backup kept, and the retired
`[storage]` key parses and is ignored), projects carry single-line notes
(`ocd note`, schema v3) and an explicit entry profile applied by the `ocd`
wrapper behind `[general] hooks = true` (variables exported, one command run,
shown before it runs, refused rather than escaped when unquotable), and the
storage layer shed the JSON store, `AnyStore`/`Store` trait, and the sidecar
lock. The whole release rode a simplification-and-performance pass (shared
matcher, pre-lowered signals, cached sort keys, change-only redraws) whose
cuts are recorded in the release notes.

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

Order: v0.4.0 → v0.5.0.
The next milestone carries a full task list and definition of done; later ones
are sketches that get detailed when they become next.

### Left alone on purpose

Recorded so this is not reopened: the matcher's scoring and signal caps
(output-frozen), `format_projects_with_missing` (the tested layout engine),
`TtyBackend` and the `Session`/`PendingRestore` plumbing (each exists for a
recorded failure), the `Picker` state machine (wide, not deep), the three
import parsers, and the hand-rolled `git.rs` (v0.2.0 extends it; it
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

Goal: make a project remember what it was for — on one storage format. Shipped
2026-10-12; the details live in [CHANGELOG.md](CHANGELOG.md).

Tasks:

- [x] 1. **Task 0: simplify & squeeze** — the ponytail pass (shared
  `SkimMatcherV2`, `Lowered` signal views, `rank_indices`, cached sort keys,
  change-only redraws, unified output funnels/completion embedding, one
  UPSERT, linear duplicate-name tally and import planning, `git_dir` threaded
  through `info`/`add`, dead helpers deleted) plus the autojump XDG fix;
  output-identical, proven by the frozen tests and the pty smoke.
- [x] 2. **JSON retirement** — auto-migrate once at startup (`migrate::auto`;
  strict commands surface a clash/quarantine, doctor/complete/hook/init/enter
  warn and keep going), `JsonStore`/`AnyStore`/`Store`/`fd-lock`/the sidecar
  lock deleted, `store::json` shrunk to the legacy reader + `list --json`
  shape, `[storage] backend` parses-and-is-ignored with a `doctor` note, the
  60 `dual!` test bodies collapsed, benches and the picker smoke made
  SQLite-only.
- [x] 3. **Schema v3 + notes** — notes table (cascade, diffed through
  `update`), `env`/`enter_cmd` profile columns, v1/v2→v3 migration with the
  `VACUUM INTO` backup recipe, the by-path UNIQUE index pinned by an EXPLAIN
  QUERY PLAN test, `note add|list|remove`, `info`/detail-pane rendering,
  `doctor` note scanning.
- [x] 4. **Profiles + `enter`** — `profile env add|remove`, `profile run
  set|clear`, `profile show`; hidden `enter <shell> <path>` emitting quoted
  shell code (bash/zsh/fish) with show-before-run, warn-not-fail, refuse-not-
  escape; `[general] hooks` (default false) gates both emission and the
  wrapper line; SECURITY.md hook boundary; pty-smoke scenario.
- [x] 5. **Release** — CHANGELOG section, README/AGENTS.md facts, SECURITY.md
  boundary, this milestone ticked, version bumped.

### v0.4.0 — Worktrees & the house look

Goal: land in the right worktree, and look the way you want by default. Code
complete 2026-10-19; the tag/publish step is pending.

Tasks:

- [x] 1. **Theming** — `src/theme.rs`: eight role tokens (`accent`, `dim`,
  `good`, `bad`, `warn`, `info`, `border`, optional `text`), typed
  `ColorSpec` (a named ANSI color, `color0`–`color255`, or `#rrggbb`), and
  both renderers drawing through `theme::active()` so a theme value can only
  ever produce a foreground SGR. `bleed` is the bundled default — blackrose's
  rose/bone/ash look: rose `#e0455e` accents and borders, bone `#f5e9e2`
  picker text, ash `#6b5a5f` secondary text, ember `#ff8a5c` warnings,
  leaf `#7ee787` success. `base` restores the 0.3.0 colors, any role can be
  overridden inline in `[theme]`, and `$OVERDOSECD_THEME` selects a palette.
- [x] 2. **Worktree awareness** — `git::worktrees_in` reads
  `.git/worktrees/*/gitdir` and each admin `HEAD` under the existing caps;
  `info` lists siblings after the remote, and `ocd <project>:<branch>`
  resolves the project first, then the branch exactly (current checkout first,
  then siblings) before the matcher ever sees the query. Worktrees are live
  resolved, never indexed; a worktree jump records no usage row.
- [x] 3. **Docs** — README (palette, worktree queries), SECURITY.md (theme
  boundary), AGENTS.md facts, CHANGELOG section.
- [ ] 4. **Release** — bump `Cargo.toml`, tag, publish, GitHub release.

Definition of done:

- A fresh install renders `bleed`; `[theme] name = "base"` reproduces the
  0.3.0 colors; no theme value can put a non-SGR escape on any stream; the
  color mode, `NO_COLOR`, and `never` behave exactly as before.
- Ordinary queries stay bit-identical: the colon form is resolved outside the
  matcher, and scores, tie-breaks, and `--debug` never see it.
- Worktree reads stay capped, refuse non-regular files, skip vanished
  registrations, and every rendered value rides `sanitize`; a hostile
  admin directory cannot make `info` unbounded.
- `list`, `goto`, and `complete` hold the recorded floors; no new runtime
  dependency.

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

- tmux session switching inside `ocd` — built during 0.4.0 and cut before
  release (2026-10-19): the maintainer's `wilt` already owns session naming,
  the workspace picker, and state badges, so a second implementation in the
  jumper was surface without need. Revisit only if `wilt` is ever retired
  (and then tmux stays the only multiplexer target).
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
- The house look is `bleed` (blackrose's rose / bone / ash), and it is the
  default rather than an opt-in (2026-10-19): themes are a small built-in set
  plus inline `[theme]` role overrides, with `base` as the escape hatch. A
  theme-file directory or an `extends` engine stays out of scope — it is the
  plugin system this roadmap does not build. Colors are typed values rendered
  by overdosecd, never raw escape strings.
- `ocd <project>:<branch>` is an exact, pre-matcher lookup (2026-10-19): the
  project is resolved normally, then the branch is matched by name against
  the live current checkout and its sibling worktrees. The colon form never
  reaches `rank`/`best`, and worktrees are not index rows.
- The git status reader and its UI ship as v0.5.0, not v0.2.0 (2026-10-04
  re-scope): the object layer — `flate2`, packfile, delta chains — is the
  roadmap's riskiest parser work and would hold the orthogonal config-debt and
  typing wins hostage.
- Linux is the only supported platform (0.1.0): the crate refuses to build
  elsewhere, and no pre-built binaries are published — crates.io source
  installs are the delivery path.
- Notes are single-line and display-only, never a matcher signal
  (2026-10-12): a multi-line body would need a new escaped renderer, and a
  note signal would reopen the output-frozen scoring.
- Entry profiles ship as explicit hooks behind `[general] hooks = true`
  (default off, 2026-10-12): nothing imported or scanned can register one, the
  command is shown before it runs, an unquotable profile is refused, and a
  planted database plus the switch is code execution on the next wrapper jump
  — accepted, because the switch and the display notice are the boundary
  (SECURITY.md).
- The whole 0.3.0 surface lands in one schema bump (v3 carries notes and the
  profile columns, 2026-10-12): one migration, one backup, one re-read under
  the lock.
- A read-only data directory fails reads since 0.3.0 (SQLite's WAL needs write
  access beside the database); the jump-degrades-to-a-warning behavior was a
  JSON-store property and is not re-created (2026-10-12).

## Known limitations

| # | Limitation | Track |
| - | ---------- | ----- |
| 1 | Subcommand names are reserved by the `ocd` wrapper, so a project named `tag` needs `ocd goto tag` | by design |
| 2 | Ranking aggregates jumps into one `use_count` instead of weighting the history log | by design |
| 3 | Data directories, config paths, and store entries must be absolute; a relative value is refused | by design |

## Recorded numbers

`cargo bench` medians on Linux x86_64, release binary, measured 2026-10-12 on
the machine state of record. These are the floor: no later milestone may
regress them without recording why.

| metric | 1k | 10k | 100k |
| ------ | -- | --- | ---- |
| `list` | 5.3 ms | 38 ms | 356 ms |
| `goto` exact, untracked | 3.4 ms | 17 ms | 140 ms |
| `goto` exact, tracked | 3.9 ms | 18 ms | 142 ms |
| `complete` | 4.8 ms | 32 ms | 283 ms |

The 0.2.0 table (json and sqlite columns) predates the single-backend
retirement and the 0.3.0 measurements; absolute numbers drift with machine
conditions, so the 0.3.0 floor claims rest on a **same-session A/B against a
0.2.0 build** (2026-10-12): `list` 356 vs 399 ms, `goto` exact 141 vs 145 ms,
tracked 142 vs 138 ms, `complete` 275 vs 269 ms at 100k — parity within
noise except `list`, which is faster (the cached sort keys); the older
`goto` floor of 135 ms no longer reproduces even for the 0.2.0 baseline on
this machine state, so it is superseded by the A/B record rather than
treated as a regression.

Recorded during development:

- Picker: one keystroke re-ranks 100k projects in ~38 ms, a two-word
  keystroke ~46 ms, empty-query reset ~7 ms.
- Home scan: a warm scan (3,361 directories, 344 project-like) takes ~56 ms,
  the first cold scan ~12 s (I/O-bound, hence the background thread), and the
  cache is ~550 KB.
- Import: 1,000 autojump entries with a git read each take ~76 ms.
- cd hook: ~3.4 ms per recorded jump at 1,000 projects and ~2.9 ms while
  writing visit counters.
- Build: the `--release` binary is ~4.6 MB.

## Release process

1. Bump `Cargo.toml`, write the `CHANGELOG.md` section, and run
   `cargo publish --dry-run` (the tag push publishes for real).
2. Merge to `main`, then tag: `git tag -a vX.Y.Z -m "overdosecd vX.Y.Z: …"` and
   push it — the tag runs `.github/workflows/release.yml`, which publishes the
   crate and creates the GitHub release from the CHANGELOG section.
3. Verify crates.io (`cargo install overdosecd` gets the new version) and the
   GitHub release.
4. Delete the merged feature branch.
