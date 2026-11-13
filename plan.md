# overdosecd — Roadmap

> A small, moody project jumper for messy folders, forgotten branches, late-night
> coding, and the comfort of returning to projects that wait for you.

This file is forward-looking only. What already shipped lives in
[CHANGELOG.md](CHANGELOG.md); how the code works lives in [AGENTS.md](AGENTS.md)
and [README.md](README.md).

## Current state

0.4.1 (2026-11-13) is the latest release — the dependency refresh (rusqlite
0.42, bundled SQLite 3.54.1) and the MSRV/toolchain move to 1.100. 0.4.0 added
themes and worktrees.

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

Order: v0.5.0. Beyond v0.5.0 the roadmap is deliberately empty —
features enter this file once the maintainer decides they are worth building;
the backlog below records the only two deferred ideas. The next milestone
carries a full task list and definition of done; later ones are sketches that
get detailed when they become next.

### Left alone on purpose

Recorded so this is not reopened: the matcher's scoring and signal caps
(output-frozen), `format_projects_with_missing` (the tested layout engine),
`TtyBackend` and the `Session`/`PendingRestore` plumbing (each exists for a
recorded failure), the `Picker` state machine (wide, not deep), the three
import parsers, and the hand-rolled `git.rs` (v0.5.0 extends it; it is never
replaced by `git2`/`gix`/subprocess).

### v0.5.0 — Git status & health UI

Goal: see the state of a project before jumping into it. (Deferred from v0.2.0
on 2026-10-04; promoted to the next milestone on 2026-11-13.)

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

Tasks:

- [ ] 1. **Status reader** — the three layers in `git.rs`, fail-closed,
  per-parse caps, fixtures, proptests, and an `#[ignore]`d accuracy check
  against real `git status` on generated repositories.
- [ ] 2. **Status UI** — status in `info` and the picker detail pane (single
  project, always); `list --status` behind a flag plus `[general] status =
  false` (off by default; flag > env > config > default). `remote_name` is read
  so the upstream renders as `origin/main`. A pty-smoke scenario for the detail
  pane; `--status` bench numbers recorded.
- [ ] 3. **Release** — CHANGELOG, README, AGENTS.md facts (status semantics,
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
- The house look is `bleed`, selected by default; themes are a small built-in
  set plus inline `[theme]` role overrides, with `base` as the escape hatch. A
  theme-file directory or an `extends` engine stays out of scope — it is the
  plugin system this roadmap does not build. Colors are typed values rendered
  by overdosecd, never raw escape strings.
- `ocd <project>:<branch>` is an exact, pre-matcher lookup: the project is
  resolved normally, then the branch is matched by name against the live
  current checkout and its sibling worktrees. The colon form never reaches
  `rank`/`best`, and worktrees are not index rows.
- The git status reader and its UI ship as one milestone (v0.5.0, a 2026-10-04
  re-scope): the object layer — `flate2`, packfile, delta chains — is the
  roadmap's riskiest parser work and gets the full task list rather than a
  corner of another release.
- Linux is the only supported platform: the crate refuses to build elsewhere,
  and no pre-built binaries are published — crates.io source installs are the
  delivery path.
- Notes are single-line and display-only, never a matcher signal: a multi-line
  body would need a new escaped renderer, and a note signal would reopen the
  output-frozen scoring.
- Entry profiles ship as explicit hooks behind `[general] hooks = true`
  (default off): nothing imported or scanned can register one, the command is
  shown before it runs, an unquotable profile is refused, and a planted
  database plus the switch is code execution on the next wrapper jump —
  accepted, because the switch and the display notice are the boundary
  (SECURITY.md).

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
