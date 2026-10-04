# Changelog

All notable changes to this project are documented in this file. The format is
based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this
project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- The palette's "add" action prefills its prompt (query, highlighted home
  result, or cwd) exactly like `Ctrl+a`, instead of opening it empty.
- `scan <root>...` resolves roots to their canonical path like `add`, so a
  symlinked root reports paths under its target.

### Changed

- Internals: deduplicated the storage, paths, and config layers (one backup
  namer, one rebuildable-JSON read, one index-file and one schema-version
  helper, shared directory validation for `add`/`scan`/picker/`doctor --fix`),
  merged the alias/tag commands behind one shape, and folded small renderer
  and picker helpers. The CLI surface is unchanged.
- `doctor` reports a read-only index as its mode (`0400`) instead of both a
  read-only line and a mode line.

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
