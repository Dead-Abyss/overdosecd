# AGENTS.md

`overdosecd` is a small local-first Rust CLI (edition 2024, MSRV 1.99, binary-only
crate). `plan.md` is the forward-looking roadmap (milestones, decided calls,
known limitations); what already shipped is in `CHANGELOG.md`. Read both before
starting roadmap work.

## Commands

CI (`.github/workflows/ci.yml`) runs five ubuntu jobs, all `--locked` and
read-only (`permissions: contents: read`). The crate is Linux-only (a non-Linux
build fails with one documented compile error), and the project runs on a free
Actions plan (2,000 minutes/month), so the budget rule is strict: CI runs on
pull requests only — merging is gated by green PR checks, so pushes to `main`
do not rerun the same work — and only when `src/`, `tests/`, `benches/`,
`build.rs`, `Cargo.toml`, `Cargo.lock`, `rustfmt.toml`, `deny.toml`, or
`ci.yml` itself changed. A docs-only change starts nothing. Batch work locally
and push once; when a push would otherwise start a run it does not need (docs,
metadata), put `[skip ci]` in the commit message.

- `checks` (ubuntu, stable): `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`
- `test` (ubuntu, stable): `cargo test`
- `msrv` (ubuntu, 1.99, matching `rust-version` in `Cargo.toml`):
  `cargo check --all-targets`. The toolchain is passed as `with: toolchain:`
  and a step asserts `rustc --version` against `Cargo.toml`, because Dependabot
  collapses all `dtolnay/rust-toolchain` pins to one ref's SHA — pinning by ref
  name silently becomes "whatever stable is"
- `fish` (ubuntu, stable): builds the binary and smoke-tests the fish wrapper
  and its completion
- `deny` (ubuntu): `cargo deny` over the lockfile — advisories, licenses,
  bans, sources; exceptions live in `deny.toml` with a reason. Third-party
  Actions are pinned to commit SHAs in every workflow (Dependabot keeps them
  current, grouped), runtime values reach `run` scripts through `env`, never
  through a `${{ }}` expression, and `SECURITY.md` documents the threat model

Pushing a `v*` tag runs `.github/workflows/release.yml`: it checks the tag
against `Cargo.toml`, publishes to crates.io (`CARGO_REGISTRY_TOKEN`), and
creates the GitHub release from the CHANGELOG section matching the tag. No
pre-built binaries are produced: `cargo install overdosecd` builds from the
published source.

- `clippy` uses `-D warnings`: any warning fails CI.
- Keep `rustfmt.toml` on stable-rustfmt options only (no `imports_granularity` /
  `group_imports`), or `fmt --check` fails in CI.
- Single tests:

```sh
cargo test matcher::tests::exact_name_beats_prefix                 # unit test (module path)
cargo test --test cli ambiguous_query_lists_candidates_with_exit_1 # integration test
```

## Architecture facts that are easy to get wrong

- Binary-only crate: no lib target, modules declared in `src/main.rs`. Integration
  tests (`tests/cli.rs`) run the built binary through `assert_cmd`; they cannot call
  internal functions. Put testable logic in modules with `#[cfg(test)]` unit tests.
- **stdout contract:** `goto` prints only the path; all human output and errors go
  to stderr. The `ocd` wrapper does `cd "$(overdosecd goto …)"`, so any extra stdout
  breaks the primary use case. Emit command output with `output::print_line` /
  `output::print` (both broken-pipe safe), not `println!`. `--debug` listings go to
  stderr for the same reason.
- Tables measure display width (`unicode-width`), never `chars().count()`, so CJK
  and emoji align. `output::format_projects` takes the terminal width as an
  injected `Option<usize>`; `output::terminal_width()` is the only tty read and
  returns `None` when stdout is piped, so piped output is never truncated.
- Values that would break the line-oriented output or inject terminal escapes
  are refused at every entry point (`add` path/name/alias/tag, `rename`,
  `alias add`, `tag add`, `doctor --fix` replacement paths) via
  `project::reject_control_chars`: C0 controls (newlines and tab included),
  DEL, C1, bidi overrides/isolates and never-visible characters (zero-width
  space, word joiner, invisible operators, BOM) are all rejected, since a
  cloned repository can bring those characters in its directory names. ZWJ and
  ZWNJ stay legal (emoji and complex scripts need them). The error renders the
  value escaped so the message itself stays one line; the home walk and
  `load_cache` filter such names too, and `doctor` reports pre-guard entries
  (name/alias/tag/path **and** stored git remote name/URL) as `unsafe:`.
- Display safety is a property of the **renderer**, not of the stored value:
  entry-point guards cannot see a planted index, a live `.git` read, `$CWD`,
  or an environment variable. `src/sanitize.rs` is the single rule
  (`contains_dangerous`/`is_dangerous`, `text`/`path` for escaping,
  `quoted` for diagnostics); every human-facing sink escapes through it
  (`list`, `info`, `--debug`, `doctor`, picker rows/details/status, the hook's
  hint, every `Error` `Display`), while machine output **refuses** a dangerous
  value instead of escaping it: `goto` and `ui` print nothing and exit `1`
  (`Error::UnsafePath`, a sanitized path would be a wrong path), and
  `matcher::completion_candidates` drops dangerous names/aliases/tags because
  the shell pastes completion output into the command line. `git::read_remote`
  and `git::current_branch` refuse hostile metadata (the project still
  indexes), so `.git/config` and `.git/HEAD` can never reach a terminal.
  `output::print`/`print_line`/`print_stderr*` are the funnel: they reject any
  escape or BEL that is not one of overdosecd's own SGR colour changes
  (`sanitize::foreign_escape`), so a missed renderer fails closed with
  `Error::UnsafeOutput` (exit `3`) instead of leaking a sequence.
- `project::normalize` canonicalizes so stored paths stay `cd`-able.
- Exit codes live in `Error::exit_code()` (`src/error.rs`): 0 ok, 1 user errors
  (no/ambiguous match, missing or stale directory, duplicate, ...), 2 clap usage,
  3 I/O and storage failures (including lock timeouts and config errors).
- Data dir precedence: `--data-dir` > `$OVERDOSECD_DATA_DIR` > platform dir
  (`src/paths.rs`). The flag and the variable must be **absolute**: a relative
  value resolves against the current directory, so a planted folder could
  choose which index is read and written (`OVERDOSECD_DATA_DIR=.` used to create
  the index in the cwd, and the wrapper would `cd` wherever it said).
  `$OVERDOSECD_CONFIG` is absolute-only for the same reason; `config::load`
  refuses a relative one (strict commands exit `1`, `doctor` reports it as a
  config problem, `complete`/`hook`/`init` fall back to defaults as documented).
  Locations come from the hand-rolled `src/dirs.rs` (the unmaintained
  `directories` crate is gone): XDG directories under `$HOME`, with relative
  `XDG_*` values ignored per the spec. An unset `$HOME` errors instead of
  resolving through `getpwuid` (recorded trade-off).
- Store files are not trusted by name: every open refuses a symbolic link
  (`paths::refuse_symlink`) for the JSON index, SQLite database, home cache,
  visit counters, and the sidecar lock — a planted `projects.db -> /elsewhere`
  used to be written through, mode included. Rebuildable files (`home-dirs.json`,
  `hook-visits.json`) treat a link as absent and rescan instead of failing.
  `doctor` reports a link as `symlink:` for every store file, and survives a
  symlinked active index to do it. Paths entering the index must be valid UTF-8
  (`Error::NonUtf8Path`): the SQLite backend used to store a lossy `U+FFFD`
  row it could never load. `projects.lock` is created `0600`, quarantine and
  migration backups share `paths::sibling_backup` (a numeric suffix, so two
  events in the same second cannot overwrite the first forensic copy), and
  `write_private` fsyncs the parent directory so the rename itself is durable.
- Config file (`src/config.rs`): `~/.config/overdosecd/config.toml`, overridable with
  `$OVERDOSECD_CONFIG` (which must exist). Missing file = built-in defaults, malformed
  file = exit `3`; never a silent fallback. Settings resolve flag > env > config >
  default. `doctor` is deliberately tolerant: it reports a broken config as an issue
  instead of exiting `3`. `[general]` carries `color`, `hook`, and `hint`; both
  hook switches default to `false` and `init`/the hook read them leniently.
  Matcher knobs live in `MatcherConfig` (`src/matcher.rs`)
  and every `rank`/`best` call takes it explicitly.
- `projects.json` (version 1) is written atomically — tmp file + fsync + rename,
  mode 0600. Corrupt files are quarantined to `projects.json.corrupt-<ts>`, never
  wiped; a newer `version` is refused.
- `projects.db` (SQLite, `src/store/sqlite.rs`) is the second backend: schema v1
  with `meta` (`schema_version`), `projects`, and a `usage` jump log capped at
  `USAGE_KEEP = 500` rows per project (the oldest rows are pruned with each
  jump).
  WAL mode; the file is created mode 0600 and WAL/SHM inherit it. `load` never
  creates the file. `update` runs `BEGIN IMMEDIATE` (busy wait ~500 ms, then
  `Error::LockTimeout`, matching the JSON sidecar lock) and *diffs* the snapshot
  so a tracked jump updates one row instead of rewriting the index; `save`
  upserts and only removed ids cascade their `usage` rows away. `ensure_schema`
  must stay read-only once the schema exists — running DDL on every open would
  block readers behind another writer.
- `[storage] backend = "json" | "sqlite"` picks the index file (default `json`).
  `doctor` and `complete` are dispatched before the strict config load, so they
  resolve the backend leniently: a broken config falls back to the default
  backend instead of failing them.
- `doctor` reports the active backend and the SQLite schema version, checks the
  active file's permissions, the data directory's mode (`0700` expected on
  Unix), and indexed values for control characters, flags orphaned
  `projects.db-wal`/`-shm`/`-journal`
  files (only when the database is gone), a JSON+SQLite pair whose id sets
  differ, and an inactive index that cannot be read. It stays read-only: an
  inactive database goes through `SqliteStore::load_readonly` (read-only
  connection, no schema creation) and an inactive JSON is parsed directly, not
  through `JsonStore::load` (which would quarantine). Quarantine files keep
  the `projects.json.corrupt-` prefix whichever backend is active.
- `migrate` (`src/migrate.rs`) imports `projects.json` into `projects.db` and
  renames the original to `projects.json.migrated-<ts>` (a name that already
  exists gets a numeric suffix, so older backups are never overwritten). It is
  idempotent (`AlreadyMigrated` when the database is there and the json is
  gone), reuses an empty database, and refuses one that already holds projects
  (`MigrationClash`, exit 1). When the configured backend is still json it adds
  a note telling the user to set `[storage] backend = "sqlite"`.
- `overdosecd import zoxide|autojump|zsh-z` (`src/import.rs`) reads each tool's
  own store at its default location or the tool's env override (`_ZO_DATA_DIR`,
  `AUTOJUMP_DATA_DIR`, `$ZSHZ_DATA`), never a shell-out. zoxide's `db.zo` is
  parsed by hand — bincode 1.x legacy (fixint, little-endian) framing: `u32`
  version 3, `u64` count, then per entry `u64 len` + UTF-8 path + `f64` rank +
  `u64` epoch; `bincode` is a dev-dependency oracle for the fixtures, and the
  SQLite magic of pre-0.6 files is recognized only to say so. The store is
  first required to be a regular file (a FIFO or device is refused with a
  clear error instead of hanging or streaming), then read through one handle
  with the 32 MiB cap enforced by the read itself (`take(MAX + 1)`), so a file
  that grows after the check still cannot exceed it; every length prefix is
  bounds-checked, malformed records are counted instead of aborting, relative
  entries are malformed (they would resolve against the cwd), and
  `project::reject_control_chars` runs on every path.
  Imports set counters only (no `usage` rows), dedupe by normalized path
  against the index and within the source, plan against a snapshot, and apply
  in one `Store::update` that re-verifies each path (raced or vanished entries
  are reported, never resurrected). `--dry-run` renders the post-import `list`
  table with the same sort and width rules and writes nothing.
- `clap` definitions are the CLI source of truth. The `ocd` pass-through allowlist
  printed by `overdosecd init` is generated from clap in `src/output.rs`, so new
  subcommands appear automatically — do not hardcode shell allowlists. Hidden
  commands are filtered out of the allowlist, so a project named `complete` stays
  jumpable while the public `completions` is reserved.
- The completion helper (`overdosecd complete <prefix>`, hidden) is dispatched
  before the config load, like `doctor`: it runs on every TAB press and reads no
  matcher knobs, so a broken config must not break completion. Reuse
  `matcher::completion_candidates` (names + aliases + tags, prefix-matched,
  sorted, stale excluded) rather than writing a second filter.
- The cd hook (`src/hook.rs`, `overdosecd hook`, hidden) is dispatched before the
  strict config load like `doctor`/`complete`. `[general] hook = true` records
  a jump when the current directory is an indexed project (`Store::record_use`,
  so SQLite writes a real `usage` row); `[general] hint = true` counts visits
  to directories that are not indexed in `hook-visits.json` (0600, atomic,
  rebuildable, never quarantined — 3 visits, 14-day hint cooldown, 30-day
  decay, and a 500-entry cap, all fixed constants). The hook is silent on
  success, may print one stderr line, and never exits non-zero: it must not be
  able to break a `cd`. `overdosecd init` installs the wrapper whenever either
  knob is on, and all three wrappers jump with `builtin cd` so the hook can
  never double-record a `ocd` jump.
- The picker (`overdosecd ui`, `src/ui/`) draws on the controlling terminal
  (`/dev/tty`) and writes *only* the chosen path to
  stdout, so `cd "$(overdosecd ui)"` works even though stdout is a pipe —
  `open_terminal()` must never touch stdout. No terminal = `Error::NoTerminal`
  (exit 1), cancel = exit 1 with empty stdout. `src/ui/picker.rs` is the
  terminal-free state machine (`handle_key` → `Outcome`), `src/ui.rs` executes
  outcomes against the store, `src/ui/render.rs` only draws; keep interaction
  rules in the state machine so they stay unit-testable (CI has no pty).
- The picker draws *inline* (`Viewport::Inline`), anchored where the cursor
  was: no alternate screen, scrollback untouched, and exiting erases from the
  box origin downwards and parks the cursor there, so the shell's next prompt
  lands where the box was. Do not use `Terminal::clear()` for that: in ratatui
  0.30 it snapshots the cursor position, erases the viewport, then restores
  the snapshot — a box-height of blank lines between the `ocd` line and the
  next prompt — and it gates the erase behind a blocking cursor query, so a
  terminal that does not answer `ESC [ 6 n` kept the whole box on screen.
  `Session::drop` calls `Backend::set_cursor_position` +
  `Backend::clear_region(AfterCursor)` instead.
  `viewport_height` clamps `[ui] height` so the box never hides the whole
  terminal; the layout is list / input / hints, and `[ui] border = true` adds
  a frame. A terminal narrower than before the session makes ratatui clear the
  *visible screen* once (never the scrollback) to avoid wrapping the redraw;
  wider or same-width resizes redraw cleanly.
- The picker routes `SIGTERM`/`SIGINT` (`SIGHUP` on Unix too) through a
  `signal_hook` flag checked each loop (the poll timeout is 200 ms so a signal
  is noticed promptly) and exits via the cancel path. That is not politeness:
  a default signal disposition would skip every `Drop`, and the shell would
  keep a mouse-reporting terminal. Every exit path must run `Session::drop`'s
  restore (erase, raw mode off, bracketed paste off, mouse off, cursor show).
- `src/ui/backend.rs` exists for one reason: crossterm's cursor query writes
  `ESC [ 6 n` to *stdout*, which the wrapper captures with `$(...)`.
  `TtyBackend::get_cursor_position` sends the query on a read+write `/dev/tty`
  handle instead (1 s timeout). Never let
  ratatui's inline viewport reach crossterm's own position query — that bug was
  caught by the pty harness reading stdout.
- Mouse: `[ui] mouse = auto` captures in a plain terminal but leaves the mouse
  to tmux/zellij/screen (`in_multiplexer`: `TMUX`, `ZELLIJ`, `STY`), because
  capturing it suspends the multiplexer's own pane UI while the
  picker runs. Capture must be released on every exit path (`Session` and
  `PendingRestore` drops). The wheel moves three rows; clicks map back through
  the list geometry the renderer records (`set_list_geometry`).
- The picker's input modes are `Search` (typing filters; j/k are characters),
  `Nav` (Tab; j/k move) and `Prompt` (the one-line inputs behind `Ctrl+a` and
  the palette's rename/alias/tag/repath actions). `Ctrl+p` pins, `Ctrl+a`
  adds; both go through `Store::update` with a re-verify by id, and add reuses
  `project::insert` (the `cmd_add` path) after expanding `~` and normalizing.
  `matcher::highlight_indices` is display-only and must never feed
  `score`/`rank`, so highlighting cannot change ordering. Pasted text drops
  control characters: multiplexers wrap paste in bracketed-paste envelopes,
  and a pane-run harness proved those can carry escape bytes into a query
  otherwise.
- The picker's views are `Mode::{Search, Nav, Prompt, Palette, Help, Health}`;
  `handle_key` returns `Outcome::{Continue, Cancel, Run(RunAction)}` and
  `ui.rs` is the only place that touches the store. `src/ui/actions.rs` owns
  the palette catalog (`Action`), the prompt labels (`PromptKind`), the fuzzy
  `palette_matches`, and the `?` help lines. Prompts prefill per kind (rename
  with the current name, repath with the stale path); `Confirm` guards removals
  and rename collisions with `y/N`. Marks (`m`) turn pin/remove/alias/tag into
  batches: one `Store::update`, re-verify each id, skip vanished ones and
  report "applied N of M" exactly like `doctor --fix`.
- The health view is `doctor::inspect` (rows built in `ui.rs`) plus
  `doctor::validate_target` and `doctor::apply_repairs` for relocation, and
  removal for `x`. The detail pane takes rows *from the list* rather than
  growing the box: the inline viewport height is fixed at session start and
  must not resize mid-session. Sort uses `project::sort_key`, the same
  comparator as `list --sort`, so the two cannot drift. `ui --query <TEXT>`
  prefills the search box (the wrapper's no-confident-match fallback uses it).
- The pty smoke harness emulates the screen (cursor moves, `J`/`K` clears,
  SGR ignored) and **answers the cursor-position query with the emulated
  cursor's real position**. A hardcoded answer places the inline box wrongly
  and its exit-clear then wipes real output — that is what the `KEEP-ME-42`
  screen check guards.
- Empty-query ordering is `project::display_order` (pinned, then last used,
  then name — the same function backs `list`'s default sort); non-empty
  queries go through `matcher::rank`. A keystroke re-ranks 100k projects in
  ~37 ms (`ui::picker::tests::rank_latency_at_100k`, `#[ignore]`d, run with
  `--release -- --ignored --nocapture`). If that ever needs fixing, narrowing
  to the previous query's candidate set is provably equivalent when the new
  query extends the old one (every signal is subsequence-based).
- Bare `ocd` opens the picker: the shell wrappers in `src/output.rs` run
  `overdosecd ui` and `cd` to what it prints; `ocd ui` and `ocd --ui` are the
  same, and the usage text is gone. `ui` joins the subcommand names reserved
  by the wrapper.
- Subcommand names are reserved by the `ocd` wrapper, so a project literally named
  `tag` (or `ui`) must be jumped to via `ocd goto tag`.
- Git metadata: the remote is stored in `Project.git` at add time (`add --force`
  refreshes), while `info` reads the branch live. Every `.git` read is capped
  (`HEAD`/`gitdir:`/`commondir` 64 KiB, `config` 16 MiB) and only the first
  line of a `gitdir:`/`commondir` indirection is used, so a hostile repository
  cannot feed an unbounded read into `add`, `import`, or `doctor --refresh`. `project.git.is_some()` is the
  repository signal for ranking — do not reintroduce a per-query stat. `git.rs` is
  a hand-rolled reader (`.git` file indirection + `commondir`); no `git2`, no
  subprocess.
- Index writes go through `Store::update(|projects| …)` (trait in `src/store/mod.rs`),
  which owns lock → load → mutate → save. `JsonStore` serializes with the sidecar
  `projects.lock` (`fd-lock`, ~500 ms retry); `SqliteStore` uses one
  `BEGIN IMMEDIATE` transaction with a ~500 ms busy timeout. Route mutations
  through `update`; each store's `save` is an inherent method (migration and
  tests) that is deliberately not on the trait, so a generic caller cannot
  bypass the lock. Readers do not lock.
- Matcher fuzzy signals are hard-capped (name ≤ 400, alias ≤ 400, path ≤ 200,
  repo ≤ 300) and `signal_score()` skips one once the running best exceeds its
  cap: `improve` only replaces on a strict win and the signal order is
  unchanged, so scores and tie-break reasons stay identical (Phase 10e,
  output-identical optimization). Do not reorder the signals — `--debug`
  output and tie-breaks depend on the order.
- `matcher::rank` splits the query on whitespace. One term takes the original
  path exactly; several terms require every term to match (AND), sum the
  per-term signals, and apply the bonuses once — and the *whole* query still
  scores as one term, so names containing spaces match exactly (a tie goes to
  the whole query). `Match.terms` carries the per-term hits, `all_confident()`
  is the rule behind `goto --confident`/`ocd`, and `--debug` prints per-term
  lines only when more than one term contributed. Single-word scores, reasons,
  and ordering are bit-identical; `highlight_indices` unions every term
  (display-only, never fed back into scoring).
- `list` stats each project exactly once per run: `cmd_list` computes
  `missing: Vec<bool>`, hides by it, and hands it to
  `output::format_projects_with_missing`, which also derives the `!` footer. The
  plain `format_projects` wrapper is `#[cfg(test)]` and re-stats; production
  code must not call it.
- `Store::record_use` records a jump: JSON bumps the counters inside `update`;
  SQLite runs one `BEGIN IMMEDIATE` transaction (`MIN(use_count + 1, …)`) that
  also appends to `usage` and prunes that project's log. `recent_uses` returns
  newest-first timestamps on SQLite and an empty list on JSON. `goto` prints
  the path before recording usage, so a failed bookkeeping write only produces
  a stderr warning; keep it that way.
- `list --since <WHEN>` (`src/time.rs`) accepts durations (`30m`, `48h`, `7d`,
  `2w`, `1y`) and dates/RFC 3339 (`2026-09-01`); `now` is injected, invalid
  values exit 1 via `Error::InvalidValue`. It filters on the cached
  `last_used_at` before sorting, so it works on both backends and never-used
  projects are excluded, and its empty result prints its own message
  ("no projects jumped to since …") instead of the empty-index one.
- `info` appends a `history:` line (up to 5 most recent jumps, relative times)
  only when `recent_uses` is non-empty; the JSON backend never shows it.

- The home fallback (`src/discovery.rs`) never indexes anything: it walks
  `$HOME` within a depth cap and an entry budget applied *during* the walk
  (`max_entries * 8`, so a planted tree cannot grow the queue before the final
  truncate), without following symlinks, skipping `DEFAULT_SKIP` plus
  `[discovery] skip`, and scores the cached directories (name fuzzy,
  project-like bonus, depth penalty). A skip pattern containing `/` matches
  trailing path components, so `.local/share/Trash` actually skips; a bare
  pattern matches the directory name. `home-dirs.json` and `hook-visits.json`
  are read only when they are small regular files, so a planted giant file is
  ignored instead of parsed. Home rows appear only when
  the index has no match (or the palette's "also search home" forces them),
  carry no id, so `targets()` stays empty and `Enter` jumps without recording
  usage; `Ctrl+a` prefills the add prompt with the row's path. The event loop
  owns everything blocked: it loads `home-dirs.json` lazily via
  `picker.needs_home()`, scans on a thread when the cache is missing or older
  than `ttl_hours`, picks the result up on a 150 ms `event::poll` (blocking
  otherwise), and filters out the data dir before handing dirs to the picker.
- `home-dirs.json` is written by `store::json::write_private` (atomic, `0600`)
  and is *rebuildable*: a broken or older-version cache is ignored, never
  quarantined like the index. `overdosecd scan --home [--refresh] [--dry-run]`
  is the explicit control; `doctor` prints a `home cache:` line and reports a
  cache mode wider than `0600` (`Issue::CachePermissions`). `scan <root>...`
  previews explicit roots through the same walk and options (`discovery::
  best_first` orders them project-like first), writes neither the cache nor
  the index, and the mode is exclusive with `--home` via a clap arg group
  (`--refresh`/`--dry-run` are cache-only and conflict with roots). Measured
  on a real home (3,361 dirs, 344 project-like): warm scan ~56 ms, cold first
  scan ~12 s (I/O-bound), cache ~550 KB, parse ~a few ms.
- The empty-index behavior depends on the fallback: with `[discovery] home =
  true` (the default) `overdosecd ui` opens the picker with "type to search your
  home directory", and only `home = false` restores the `EmptyIndex` error.
- `ocd <query>` is jump-if-confident: `goto --confident` refuses matches that
  are only fuzzy (`Reason::is_confident`, a predicate over the documented
  classes — never a score) with `Error::WeakMatch` (exit 1, one line), and the
  wrapper answers *any* goto failure by opening `ui --query "$*"`, so
  ambiguous, missing, stale, and weak queries all become a picker. Its no-tty
  branch keeps plain `goto` with today's messages: scripts depend on that.
  `ocd --cmd …` runs the CLI only (subcommands pass through, anything else is
  `goto`) and never opens a picker or changes directory. Completions offer
  `--cmd` and the subcommands after it.

## Index mutations

`pin`/`unpin`, `rename`, and `alias`/`tag add|remove` follow one pattern: resolve
the target with `matcher::best` *outside* the lock, then re-verify by `id` inside
`Store::update` and fail with `NoLongerIndexed` if it vanished. Keep new
mutations on that path. `rename` refuses a case-insensitively taken name unless
`--force`; idempotent operations succeed with an "already …" note.

`doctor --fix` is the deliberate batch exception: it collects repairs
interactively outside the lock, then applies them in one `update` that
re-verifies each `id` and *skips* vanished or newly-colliding targets instead of
failing the whole session. `doctor` alone is read-only and exits `1` when it
finds issues; `doctor --refresh` rewrites stored git metadata under the lock.

## Testing conventions

- Integration tests: use the `Sandbox` helper in `tests/cli.rs` (canonicalized
  temp `HOME` + `OVERDOSECD_DATA_DIR` + isolated `XDG_CONFIG_HOME`, `NO_COLOR=1`).
  The canonical root matters: stored paths are canonical, so a raw tempdir
  path (when `/tmp` is symlinked, for example) breaks `~` assertions. Config
  tests point
  `OVERDOSECD_CONFIG` at a file inside the sandbox so they do not depend on
  platform paths.
- Behavior tests run against both backends: wrap the body in
  `dual!(sandbox, { … })`, which builds a fresh `Sandbox` per backend. A SQLite
  sandbox writes `[storage] backend = "sqlite"` (plus any test body config) and
  sets `OVERDOSECD_CONFIG`; `write_config` reuses that path. Tests that are
  backend-specific (quarantine files, the sidecar lock, `projects.json`
  permissions) stay on `Sandbox::new()` with a comment saying why.
- Edition 2024 makes `std::env::set_var` unsafe: never mutate env vars in tests.
  Use injected parameters instead (`expand_tilde_with`, `shorten_home(path, home)`,
  `relative_time(when, now)`, `format_projects(projects, home, width)`).
- Color assertions need `colored::control::set_override(false)` in-process because
  `colored` follows the tty.
- Permission tests are `#[cfg(unix)]`, restore their modes before the tempdir
  drops, and skip via `permissions_are_ignored` when running as root, where
  permission bits are not enforced.
- Lock and concurrency tests hold `projects.lock` in-process with `fd-lock`
  (dev-dependency); the SQLite analogue opens a second `rusqlite` connection
  and holds `BEGIN IMMEDIATE`. The stress test spawns the built binary via
  `env!("CARGO_BIN_EXE_overdosecd")`; `assert_cmd`'s `spawn` is private. Bench
  targets get the same variable, which is how `benches/cli.rs` finds the binary.
- Benchmarks: `cargo bench` runs `benches/cli.rs` (criterion, `harness = false`)
  over 1k/10k/100k fixtures on both backends, each iteration spawning the real
  binary, plus `import autojump-1k-json` (a fresh index per iteration, since
  imports are idempotent). CI does not run them; record numbers in the active
  milestone and `CHANGELOG.md` when they change. Fixtures hand-write
  `projects.json` and the SQLite fixture is built from it with one `migrate`
  run — never loop `add`.
- `proptest` (dev-only) guards the matcher — `rank` never panics on arbitrary
  queries and names, ranking is deterministic, a single contributing term
  keeps the old confidence rule — and the import parsers: arbitrary bytes are
  counted into exactly one bucket, and the bincode parser either rejects the
  frame or stays inside the buffer. Keep the properties cheap; they run on
  every `cargo test`.
- Fish is not installed on this machine; the fish wrapper and its completion
  are smoke-tested by the `fish` CI job and cannot be verified manually. Bash
  and zsh are available for manual shell tests.
- Completion smoke tests without a pty: for bash, call `_ocd` directly with
  `COMP_WORDS`/`COMP_CWORD` set and inspect `COMPREPLY`; for zsh, source the init
  script after `compinit -u` and assert `${_comps[ocd]}` is `_ocd`. Registering
  must stay eval-clean (no output at setup time) in every shell.
- Manual shell test: the wrapper calls `overdosecd` by name, so put the binary on
  `PATH` (e.g. `target/debug`) before `eval "$(overdosecd init zsh)"`.
- The picker's end-to-end behavior (jump, filter, Tab+j/k, pin, add, cancel,
  SIGTERM mid-session, mid-session resize, both wrappers, both backends) is
  checked by `scripts/picker-smoke.py`, which drives the real binary through a
  pty and reads the path from a redirected stdout. It answers the inline
  viewport's cursor query itself — *every* time it is asked, including the
  re-anchor after a resize; answering only the first query made a resize kill
  the picker with an I/O error, which is how that rule was learned. It drives
  real mouse sequences and asserts the escape stream carries no alternate
  screen, no scrollback clear, and no visible-screen clear except the one
  ratatui emits on a horizontal shrink. It also asserts that every exit path
  parks the emulated cursor at the box origin with no exit-time cursor query —
  the check that would have caught the ratatui 0.30 `clear()` gap. It is a
  manual check
  (python3 + bash + zsh), not part of CI; add a scenario there when picker
  interaction changes. Exercising the picker inside a tmux pane/split stays a
  manual check.
- Dependency note: the MSRV tracks current Rust stable (`rust-version` in
  `Cargo.toml`, currently 1.99), and moved there with the ratatui 0.30 stable
  upgrade — 0.30.2 pulls `ratatui-core 0.1.2` (`lru ^0.18`) and
  `ratatui-widgets 0.3.2` (`time ^0.3.47`), which retired the beta pin and
  both inherited advisories at once. Run `rustup update` before raising
  `rust-version`; local builds fail against a newer declaration. `bincode` is
  exact-pinned (`=1.3.3`) and must stay there: the crate is unmaintained and
  its 3.0.0 release is a lone `compile_error!` published to announce that, so
  it compiles nowhere (and no fixed 3.x is coming); 1.3.3 is the dev-only
  fixture oracle for the zoxide parser, not a runtime dependency.

## Git flow

Work on a feature branch and open a PR; `main` is expected to stay CI-green. CI
runs on pull requests only (see the quota rule under Commands), so a merge
starts no second run. Merging is the maintainer's call.

## Docs

- `README.md` — user-facing behavior, install, shell setup, exit codes.
- `plan.md` — the forward-looking roadmap: milestones with task lists and
  definitions of done, a `Decided` record, known limitations (each tagged with
  the milestone that fixes it), recorded performance numbers, and the release
  process. Tick checkboxes when a milestone ships and move the shipped work
  into `CHANGELOG.md`.
- `CHANGELOG.md` — what shipped, per release; the source for GitHub release notes.
- `scripts/picker-smoke.py` — manual pty end-to-end check for the picker.
