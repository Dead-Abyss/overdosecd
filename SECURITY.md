# Security Policy

## Supported versions

Only the latest release on crates.io receives fixes. The repository's `main`
branch is the development line and is expected to be releasable at any time.

| Version | Supported |
| ------- | --------- |
| 0.6.x   | ✅        |
| < 0.6   | ❌        |

## Reporting a vulnerability

Use GitHub's private vulnerability reporting on this repository
(**Security → Report a vulnerability**). Please do not open a public issue for
a suspected vulnerability. Reports get an acknowledgement within a few days,
and the fix ships as a patch release with a `### Security` CHANGELOG entry.

## Threat model

withercd is a local-first tool: no network, no telemetry, no subprocesses, no
dynamic configuration beyond the local files it documents. The interesting
attack surfaces are all local:

- **(a) A cloned repository's contents.** Directory names, `.git/config`, and
  `.git/HEAD` come with the repository. They must never reach a terminal raw,
  must not make `withercd` read an unbounded amount of data, and must not
  choose where files are written.
- **(b) A corrupted or planted data directory.** The index, the SQLite
  database, the home cache, and the visit counters are loaded from disk and
  can predate the current guards. A value read from them is untrusted exactly
  like a value typed on the command line.
- **(c) Another local user.** The data directory is created `0700` and the
  store files `0600`, so a second user cannot read the index or plant files
  beside it. If those modes are lost, `doctor` reports it and store opens
  refuse a symbolic link.
- **(d) Environment variables.** `$WITHERCD_DATA_DIR`, `$WITHERCD_CONFIG`,
  `$WITHERCD_COLOR`, `_ZO_DATA_DIR`, `$AUTOJUMP_DATA_DIR`, and `$ZSHZ_DATA`
  steer where withercd reads and writes; path-shaped values must be absolute,
  and every path that appears in an error message is escaped.
- **(e) Text that reaches a terminal or a shell.** Display safety is a property
  of the renderer: human-facing output escapes control characters, bidi
  controls, and never-visible characters, machine output (`goto`'s stdout,
  completion candidates) refuses them outright, and the output funnel rejects
  any escape sequence that is not one of withercd's own colour changes. A
  project name, alias, tag, path, branch, or remote added by any route — typed,
  imported, or selected in the picker — obeys the same rule.

### Out of scope

- An attacker who can already write to the `0700` data directory owns the
  index by design (they can index any path). The link and mode checks raise
  the cost of turning that into a jump to an attacker-chosen directory, they
  do not make it impossible.
- `withercd import` reads third-party stores the user points it at; importing
  a store the user chose is an explicit, documented action.
- Denial of service that requires active racing against the user's own
  process (for example replacing a store file between two checks) is not
  considered a vulnerability: the reads stay capped either way.

## Invariants

The rules that keep the guarantees above are recorded in
[AGENTS.md](AGENTS.md) and enforced by tests:

- `goto`/`ui` print only a path on stdout; a path a terminal would mangle is
  refused with exit `1` instead of escaped.
- No command's stdout or stderr carries a terminal control byte for any stored
  or live value (`NO_COLOR=1` in tests).
- Data directories, config paths, and store entries must be absolute; store
  files are never opened through a symbolic link.
- Every read of a file that arrives from outside the tool is capped and
  requires a regular file.
- `projects.json` and `projects.db` are written atomically and privately;
  corrupt files are quarantined, never wiped, and two events in the same
  second cannot overwrite the first backup.

## Supply chain

CI runs `cargo deny` over the lockfile (advisories, licenses, bans, sources);
exceptions live in [`deny.toml`](deny.toml) with a reason. Third-party GitHub
Actions are pinned to commit SHAs.
