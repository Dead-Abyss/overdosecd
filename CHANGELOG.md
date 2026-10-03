# Changelog

All notable changes to this project are documented in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.6.5] - 2026-10-03

Linux is the only supported platform.

### Changed

- The crate refuses to build on non-Linux targets with a single documented
  error instead of carrying cfg branches that are never exercised.
- Releases publish to crates.io and create the GitHub release; the six-target
  build matrix and pre-built binaries are gone (`cargo install withercd`
  builds from source).

### Removed

- The macOS and Windows code paths: platform data directories, the `\\?\`
  verbatim-prefix rewrite in `project::normalize`, the Windows console cursor
  query, and the `tests/cli.rs` mirror of the conversion.
- herdr from the picker's multiplexer detection (`HERDR_ENV`) and from the
  docs; tmux, zellij, and screen stay.

## [0.6.4] - 2026-10-02

The picker hands the terminal back the way it found it, and the README is a
reference now.

### Fixed

- Exiting the picker no longer leaves a box-height of blank lines between the
  `wcd` line and the next prompt (and zsh's `%` marker for the unterminated
  line). `Session::drop` erases from the box origin downwards and parks the
  cursor there instead of calling `Terminal::clear()`, whose ratatui 0.30
  semantics snapshot the cursor, erase the viewport, and restore the snapshot
  — inside the erased box — and which gates the erase behind a blocking
  cursor query, so a terminal that never answered it kept the whole box on
  screen. Every exit path (jump, cancel, signals, panic) shares the teardown.
- `scripts/picker-smoke.py` asserts the exit cursor position and query count
  for the jump, cancel, SIGTERM, and resize scenarios, so the gap cannot
  return silently.

### Changed

- The README is a concise reference: a wrapper grammar table, a command and
  flag table (checked against `--help`), and the picker key table carry the
  lookup content; duplicated explanations collapse to one home each, and
  rationale prose that lives in `AGENTS.md` is no longer repeated.

## [0.6.3] - 2026-10-01

Simplify: no flag, knob, output line, or exit code moved.

### Changed

- The duplicated rules behind the CLI and the picker have one home each:
  `TermKind`, `set_term`, `sanitize_value`, and the re-verify-by-id check
  live in `project.rs`; `cmd_info` renders one row table; the branch label
  and the relative-time history line are shared with the picker.
- `Store::save` left the trait (it remains an inherent method for migrations
  and tests), so a bare save on a generic store no longer exists — the lock
  is part of `update` by construction.
- Build diet: `terminal_size` is gone (crossterm already reports the size),
  `rusqlite`'s unused `chrono` feature is off, and `toml` builds without
  default features.
- Crate-only items are private or `pub(crate)` now; dead items are gone
  (`HomeMatch.project_like`, `Default for MatcherConfig`, `Error::InvalidName`,
  stale doc lines, the half-finished CHANGELOG link refs).
- The test suite uses plain JSON sandboxes where the backend is not the
  subject, merges the two lock tests and the duplicated picker helpers, and
  folds the duplicated confident-goto coverage — with identical assertions.

## [0.6.2] - 2026-10-01

Trust boundaries: nothing withercd reads — a cloned repository's `.git`, a
jumper's store, a data directory, the current directory, an environment
variable — can put an escape sequence on the terminal, forge an output line,
choose which index is read, or destroy a forensic copy. The threat model
lives in [SECURITY.md](SECURITY.md).

### Added

- `SECURITY.md` documents the threat model: what a cloned repository, a
  planted data directory, another local user, and hostile environment
  variables can and cannot do — and what is explicitly out of scope.
- CI runs `cargo deny` over the lockfile (advisories, licenses, bans,
  sources), and every third-party Action is pinned to a commit SHA;
  Dependabot keeps both current in weekly grouped pull requests.

### Changed

- Display safety is now a property of the renderer, not of the stored
  value: a value read from disk, from `$CWD`, or from a live `.git` file
  never passed an entry-point guard, so every human-facing sink — `list`,
  `info`, `--debug`, `doctor`, ambiguity candidates, the hook's hint, and
  the picker — escapes control, bidi, and never-visible characters. Machine
  output refuses instead of escaping: `goto` and `ui` print nothing and
  exit `1` when the chosen path is unsafe, and completion drops unsafe
  candidates (the shell pastes them into the command line).
- `--data-dir`, `$WITHERCD_DATA_DIR`, and `$WITHERCD_CONFIG` must be
  absolute paths; a relative value is refused with one clear line. Store
  entries must be absolute too, on both backends, and paths entering the
  index must be valid UTF-8 (SQLite no longer stores a lossy `U+FFFD` row
  it could never load).
- The unmaintained `directories` crate is gone, replaced by a hand-rolled
  resolver with identical locations on all three platforms. Recorded
  trade-off: with `HOME`/`USERPROFILE` unset, withercd now errors instead
  of falling back to `getpwuid`.

### Fixed

- A cloned repository can no longer render escapes: `.git/config` and
  `.git/HEAD` are refused when dirty (at the entry points and on live
  reads alike), `doctor` reports such metadata, and every `.git` read is
  capped (config 16 MiB; HEAD, `gitdir:`, and `commondir` 64 KiB, with only
  the first line of an indirection used).
- A planted index (JSON or SQLite) no longer renders escapes in `list`,
  `remove`, `doctor`, or completion: entries are escaped, or refused on the
  machine-output paths — never silently hidden.
- Symbolic links where store files should be (index, database, lock, both
  caches) are refused on open instead of written through, and `doctor`
  reports them; quarantine and migration backups can no longer overwrite an
  earlier copy within the same second.
- Imports read through one handle with a hard 32 MiB cap: FIFOs and device
  files are refused instead of hanging or streaming, and records with
  relative paths count as malformed instead of resolving against the
  current directory.
- Skip patterns containing `/` (`[discovery] skip` and the built-in
  `.local/share/Trash`) now match trailing path components, so trashed
  directories stay out of the home cache; the home walk is bounded while
  it walks.
- Bidi overrides/isolates and never-visible characters (zero-width space,
  word joiner, invisible operators, BOM) are rejected at every entry
  point, so a name can no longer render as a different string than the one
  typed or stored. ZWJ and ZWNJ stay legal: emoji sequences and complex
  scripts need them.

## [0.6.1] - 2026-10-01

Daily-driver parity: withercd learns from plain `cd`, takes multi-word
queries, ships prebuilt binaries, and can no longer leave the terminal broken.

### Added

- `[general] hook = true` installs a `cd` wrapper (bash, zsh, fish) and
  records a jump whenever a plain `cd` lands inside an indexed project, so
  recency and frequency reflect how you actually move around. `[general] hint
  = true` additionally counts visits to directories that are not indexed and,
  on the third visit in the window, prints one line suggesting `wcd add`
  (14-day per-path cooldown, 30-day decay; nothing is ever indexed
  automatically). The hook is silent on success, never changes `cd`'s
  behavior or exit status, and costs a few milliseconds.
- Multi-word queries: `wcd rust web` requires every word to match, and a
  project name containing spaces still matches the whole query exactly.
  `goto`/`info`/`pin`/`unpin`/`remove` accept the words as separate
  arguments; `--debug` lists what each word contributed. For `wcd`, a
  multi-word query jumps without asking only when every word lands in a
  confident class.
- Prebuilt binaries on every release: Linux (x86_64 glibc and static musl,
  aarch64), macOS (Intel and Apple Silicon), and Windows archives with a
  `sha256sums.txt`; `cargo binstall withercd` installs from them.

### Fixed

- `SIGTERM`/`SIGINT`/`SIGHUP` during the picker now exit through the normal
  restore path instead of skipping it: the terminal no longer keeps mouse
  reporting enabled after a signal.

### Changed

- `rename` and `alias`/`tag` still take the project as one (quoted) argument —
  a variadic argument in the middle of a command line would be ambiguous.

## [0.6.0] - 2026-10-01

Import: arrive with an index instead of building one by hand, and preview
directories without indexing them.

### Added

- `withercd import zoxide|autojump|zsh-z [--dry-run] [--min-score N]
  [--limit N]`: reads each jumper's own store — zoxide's bincode `db.zo`
  (format v3, parsed by hand against the exact layout), autojump's
  `autojump.txt` (negative weights are blacklist entries), and zsh-z's
  `path|rank|epoch` data file (parsed from the right, so `|` in paths
  survives) — and maps its frecency onto `use_count`/`last_used_at`. The
  stores are read-only and treated as hostile: a 32 MiB cap, bounds-checked
  length prefixes, malformed records counted instead of aborting. Entries
  dedupe by canonical path, so re-running an import only reports what is
  already indexed; counters only (no `usage` rows), so `info` history stays
  truthful. `--dry-run` prints the exact `list` table the import would
  produce.
- `withercd scan <root>...`: previews explicit roots through the same bounded
  walk as the home search (skip list, depth and entry caps, no symlink
  following), project-like candidates first, writing neither the cache nor
  the index.

### Changed

- `scan` requires an explicit target now: `scan <root>...` or `scan --home`
  (`--refresh`/`--dry-run` are cache-only and exclusive with roots). A bare
  `scan` used to silently scan `$HOME`, and is a usage error now.

## [0.5.2] - 2026-10-01

Dependency refresh: ratatui 0.30 stable, the two inherited advisories resolved,
and the MSRV moves to 1.99.

### Changed

- The minimum supported Rust version is 1.99, the current stable, up from 1.85.
- The ratatui beta pin is gone. 0.30 stable pulls `ratatui-core 0.1.2`
  (`lru ^0.18`) and `ratatui-widgets 0.3.2` (`time ^0.3.47`), so
  RUSTSEC-2026-0253 and RUSTSEC-2026-0009 no longer apply.
- `colored` moves to 3.x and `criterion` to 0.8. Nested conditionals were
  rewritten as let-chains (the new toolchain's clippy asks for them); no
  behavior changed, and the matcher's scores and tie-breaks are untouched.

## [0.5.1] - 2026-10-01

Security hardening on top of v0.5.0: terminal escapes are refused at the entry
points, the data directory is private from the first run, and `doctor` reports
both.

### Security

- Names, aliases, tags, and paths must be printable now: control characters —
  newlines, tab, DEL, C1, and escape sequences — are refused at every entry
  point, the home search never offers them, and `doctor` reports entries that
  predate the guard. A directory name carried in by a cloned repository can no
  longer feed OSC 52 or CSI sequences to the terminal.
- The data directory is created `0700` (Unix), and the JSON index's temporary
  file uses a process-specific name and `create_new`, so a planted symlink in
  a writable data directory can no longer truncate another file. `doctor`
  reports a data directory wider than `0700`.

### Changed

- Test fixtures no longer embed the maintainer's home directory in the
  published crate sources.

## [0.5.0] - 2026-10-01

The interactive picker: choose without knowing the name, keep the index tidy in
place, and search `$HOME` when the index has no answer.

### Added

- `withercd ui [--query <TEXT>] [--no-track]`: an interactive picker that
  filters with the normal ranked matcher as you type. It draws inline where the
  cursor was — no alternate screen, scrollback untouched, the box cleared on
  exit — and prints only the chosen path, so `cd "$(withercd ui)"` composes like
  `goto`. Without a controlling terminal it refuses with exit `1`.
- Bare `wcd` opens the picker and `cd`s into the choice; `wcd ui` and `wcd --ui`
  are the same thing, and the old usage text is gone.
- Picker keys: `Tab` toggles the `j`/`k` list mode, `Ctrl+p` pins, `Ctrl+a` adds
  a directory, `Ctrl+x` removes behind `y/N`, `m` marks rows, the wheel and
  clicks work, and `?` opens a help overlay.
- Action palette (`Ctrl+Space`, or `:` in list mode, fuzzy-filtered): rename,
  alias and tag add/remove, sort cycle, missing toggle, detail pane (path,
  aliases, tags, remote, use count, live branch, recent jumps), and the health
  view. Marked rows batch pin/remove/alias/tag into one re-verified update.
- Health view over `doctor`'s findings: `Enter` relocates a stale project, `x`
  removes it, `r` re-checks.
- Home search: when the index has no match, a bounded `$HOME` walk offers
  candidate rows (`~` and `home` marked; `Enter` jumps without recording,
  `Ctrl+a` prefills the add prompt). The walk skips symlinks, caches, and build
  trees, caps depth and entries, and caches to a rebuildable `home-dirs.json`
  (`0600`, atomic), loaded lazily and refreshed in the background past
  `ttl_hours`.
- `withercd scan --home [--refresh] [--dry-run]` builds or refreshes that cache;
  `doctor` reports the cache and its permissions.
- `[ui]` (`height`, `border`, `mouse`) and `[discovery]` (`home`, `max_depth`,
  `max_entries`, `ttl_hours`, `skip`) configuration.
- `wcd --cmd …` runs the CLI without the picker or a `cd`: subcommands pass
  through, anything else is `goto`. Completions offer `--cmd` and the
  subcommands after it.
- `withercd goto --confident` refuses fuzzy-only matches (exit `1`, one line);
  it is the flag behind the wrapper's decision.

### Changed

- **Breaking (wrapper):** `wcd <query>` answers an unconfident match — fuzzy
  only, ambiguous, missing, or stale — by opening the picker at
  `ui --query "$*"` instead of printing an error. Scripts keep the old behavior:
  when stdin is not a terminal the wrapper runs plain `goto` with its messages
  and exit codes.
- **Breaking (wrapper):** bare `wcd` opens the picker instead of printing usage,
  and `wcd --cmd` makes `--cmd` a reserved word like the subcommand names.
- Mouse input is captured in a plain terminal; inside a multiplexer (herdr,
  tmux, zellij, screen) `mouse = "auto"` leaves it to the multiplexer, since
  capturing it suspends the pane UI. `always` forces it on.
- The release binary is 4.3 MB (v0.4.0: 3.9 MB). A keystroke re-ranks 100k
  projects in ~37 ms; the empty-query reset takes ~4 ms.

## [0.4.0] - 2026-09-30

SQLite storage: jump history, richer queries, and partial index updates.

### Added

- Optional SQLite backend, selected with `[storage] backend = "json" | "sqlite"`
  in `config.toml` (default `json`). `projects.db` uses schema v1 (`meta`,
  `projects`, and a `usage` jump log), runs in WAL mode, and is created `0600`
  like the JSON index.
- `withercd migrate`: imports `projects.json` into `projects.db` in one
  transaction and keeps the original as `projects.json.migrated-<timestamp>`.
  Idempotent; refuses to overwrite a database that already holds projects, and
  reuses an empty one. Prints the `[storage]` snippet when the configured
  backend is still `json`.
- `list --since <WHEN>`: a duration (`30m`, `48h`, `7d`, `2w`, `1y`) or a date
  (`2026-09-01`, or any RFC 3339 timestamp) filters the list to projects jumped
  to in that window. Works on both backends.
- `info` gains a `history:` line with the five most recent jumps on the SQLite
  backend; the `usage` log keeps the latest 500 jumps per project.
- `doctor` reports the active backend and schema version and checks the SQLite
  side: orphaned `projects.db-wal`/`-shm`/`-journal` files, a `projects.json`
  and `projects.db` that hold different projects, and an unreadable inactive
  index. All checks stay read-only.

### Changed

- A tracked jump no longer rewrites the whole index on SQLite: it updates one
  row and appends to the `usage` log in a single transaction. At 10k projects a
  tracked `goto` drops from ~34 ms (JSON) to ~17 ms (SQLite), and at 100k from
  ~304 ms to ~131 ms. Read paths (`list`, `complete`) are unchanged on both
  backends.
- The `Store` seam owns `update`, `record_use`, `recent_uses`, and
  `schema_version`, so every command runs against either backend, and the
  integration suite runs each behavior test against both.
- Benchmarks cover both backends at 1k/10k/100k projects. The release binary
  with bundled SQLite is 3.9 MB (v0.3.1: 1.6 MB, LTO + strip), and the MSRV
  stays at 1.85.

## [0.3.1] - 2026-09-30

Release automation, platform CI, and a Windows path fix.

### Added

- Pushing a `v*` tag now publishes the crate and creates the GitHub release
  from that version's CHANGELOG section, with no local commands.
- CI runs `cargo test` on Linux, macOS, and Windows, and smoke-tests the fish
  wrapper and its completion on Linux — the fish integration is verified now,
  not just shipped.

### Changed

- `actions/checkout` bumped to v6 after GitHub removed the Node 20 runtime
  from Actions runners.

### Fixed

- Windows: `normalize` strips the `\\?\` verbatim prefix from stored paths so
  they stay `cd`-able (`\\?\UNC\` is rewritten to `\\server\share`).

## [0.3.0] - 2026-09-30

Git awareness, index health, configuration, and shell completions.

### Added

- `withercd doctor [--fix] [--refresh]`: reports stale paths, duplicate names,
  quarantined files, and permission problems. `--fix` repairs stale entries
  interactively (skip / remove / quit / replacement path) and `--refresh`
  re-reads stored git remotes.
- Optional `~/.config/withercd/config.toml` with `[general] color` and the
  `[matching]` knobs (fuzzy toggle, recency/frequency/pin bonuses, ambiguity
  margin, depth penalty). Settings resolve flag > environment > config >
  built-in defaults, unknown keys and bad values are exit-`3` errors, and
  `$WITHERCD_CONFIG` points at another file.
- Global `--color auto|always|never` and `$WITHERCD_COLOR`, above the config
  file's `general.color`.
- Global `--debug`: lists every ranked candidate for a query on stderr with its
  match reason and score, while stdout keeps the machine-readable contract.
- `withercd completions <bash|zsh|fish>` prints static completion scripts, and
  `wcd <TAB>` completes indexed names, aliases, and tags through the hidden
  `withercd complete <prefix>` helper.
- Git awareness: `add` snapshots the preferred remote, matching accepts the
  repository name and the exact `owner/repo` slug, and `info` shows the live
  branch and remote (worktrees and submodules included).

### Changed

- `list` measures display width (`unicode-width`), so CJK and emoji names align,
  and elides long paths only on a terminal: piped output is never truncated.
- `info`, `doctor`, and ambiguity candidates are colored, still honoring
  `NO_COLOR`.
- Performance work measured at 1,000 and 10,000 projects: `list` stats each
  project once per run and the matcher skips fuzzy signals that cannot beat the
  running best. All recorded budgets pass, so the SQLite storage upgrade stays
  unplanned while they hold.
- `doctor` reports the effective config path and treats an unreadable config as
  an issue instead of failing.

### Fixed

- Paths, names, aliases, and tags containing newlines are refused at every
  entry point instead of breaking `cd "$(withercd goto …)"`.
- Ambiguity candidates and `--debug` output shorten the home directory.

## [0.2.0] - 2026-09-28

Usage tracking and index mutations.

### Added

- Every successful `goto` records `last_used_at` and `use_count`, and recency
  and frequency feed the ranking. `goto --no-track` skips recording for one
  jump.
- `pin`/`unpin` (pinned projects float to the top of `list` with `★`), `rename`,
  `alias add|remove`, and `tag add|remove`.
- A sidecar `projects.lock` serializes writers, and jumps never fail because
  bookkeeping failed: a failed usage write is a warning.

### Fixed

- An empty index now points at `withercd add <path>` instead of a generic
  no-match error.

## [0.1.0] - 2026-09-28

First usable version.

### Added

- `add`, `list`, `goto`, `info`, and `remove`, with exact, alias, prefix, tag,
  and fuzzy matching plus an ambiguity guard that lists candidates.
- A versioned JSON index (`projects.json`) with atomic writes, `0600`
  permissions, corruption quarantine, and `--data-dir`/`$WITHERCD_DATA_DIR`.
- The `wcd` shell wrapper via `withercd init bash|zsh|fish`.
