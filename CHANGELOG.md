# Changelog

All notable changes to this project are documented in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-10-04

Git configs are read the way git reads them, and projects remember what kind of
project they are.

### Features

- `.git/config` reads honor `include.path` and `includeIf` (`gitdir:` and
  `gitdir/i:`) under fixed caps (8 files, 16 MiB merged, regular files only),
  and `url.<base>.insteadOf` rewrites apply by longest prefix. The `gitdir:`
  pattern subset is `*`, `**`, and `?`; character classes and `onbranch:` never
  match.
- Bare repositories are detected through `core.bare` or the
  `HEAD` + `objects/` + `refs/` layout.
- Projects remember their kind (`rust`, `node`, `python`, `go`, `unknown`),
  detected once from marker files at `add` (`--force` refreshes it). `info`
  gains `type:`, `list --type T` filters, picker rows carry a badge, and
  `doctor --refresh` re-detects it.
- SQLite schema v2 stores the kind. A v1 database migrates on its first
  writable open, after a consistent `VACUUM INTO` backup
  (`projects.db.v1-<timestamp>`); a read-only open reports the old version
  instead of migrating, and `doctor` shows the schema version.

### Fixed

- The palette's "add" action prefills its prompt (query, highlighted home
  result, or cwd) exactly like `Ctrl+a`, instead of opening it empty.
- `scan <root>...` resolves roots to their canonical path like `add`, so a
  symlinked root reports paths under its target.
- A `.git` file that is a FIFO is refused instead of blocking the read.

### Changed

- Internals: deduplicated the storage, paths, and config layers (one backup
  namer, one rebuildable-JSON read, one index-file and one schema-version
  helper, shared directory validation for `add`/`scan`/picker/`doctor --fix`),
  merged the alias/tag commands behind one shape, and folded small renderer
  and picker helpers. The CLI surface is unchanged.
- `doctor` reports a read-only index as its mode (`0400`) instead of both a
  read-only line and a mode line; `--refresh` now refreshes stored metadata
  (git remote and project kind), and its wording says "metadata".

## [0.1.0] - 2026-10-03

The first release under the overdosecd name: a rename of the predecessor
project, whose crate no longer exists. The command is `overdosecd`, the shell
wrapper is `ocd`, and the feature set is the mature one the predecessor had
shipped.

### Features

- Index and organize projects: `add`, `remove`, `rename`, `alias`, `tag`,
  `pin`/`unpin`, with git-aware matching by name, alias, tag, path, and remote
  (`owner/repo` slugs included).
- Jump with `overdosecd goto` and the `ocd` shell wrapper (`ocd`, `ocd <query>`
  with a picker fallback, `ocd --cmd` for scripts).
- Inspect with `list` (sortable, `--since`, width-aware, missing markers),
  `info` (branch, remote, timestamps, jump history), and `doctor`.
- Search beyond the index: the bounded `$HOME` fallback in the picker and
  `overdosecd scan <root>...` / `scan --home`.
- Import existing jumpers: `import zoxide`, `import autojump`, `import zsh-z`.
- Interactive picker (`ui`): inline viewport, live filter, action palette,
  marked-row batches, health view, detail pane; stdout carries only the chosen
  path.
- Optional `cd` hook and home-visit hints (`[general] hook`, `[general] hint`).
- Two storage backends: JSON and SQLite (`[storage] backend`), with `migrate`
  and the `usage` jump log.
- Linux only: a crate-root guard refuses to build elsewhere; releases are the
  crates.io publish plus GitHub release notes (no pre-built binaries).
