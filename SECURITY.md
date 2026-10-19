# Security Policy

## Supported versions

Only the latest release on crates.io receives fixes. The repository's `main`
branch is the development line and is expected to be releasable at any time.

| Version | Supported |
| ------- | --------- |
| 0.4.x   | ✅        |
| < 0.4   | ❌        |

## Reporting a vulnerability

Use GitHub's private vulnerability reporting on this repository
(**Security → Report a vulnerability**). Please do not open a public issue for
a suspected vulnerability. Reports get an acknowledgement within a few days,
and the fix ships as a patch release with a `### Security` CHANGELOG entry.

## Threat model

overdosecd is a local-first tool: no network, no telemetry, no subprocesses, no
dynamic configuration beyond the local files it documents. The interesting
attack surfaces are all local:

- **(a) A cloned repository's contents.** Directory names, `.git/config`,
  `.git/HEAD`, and any file its `include.path`/`includeIf` points at come with
  the repository. They must never reach a terminal raw, must not make
  `overdosecd` read an unbounded amount of data, and must not choose where
  files are written. Config includes are followed under fixed caps (eight
  files, regular files only) and can steer only the sanitized remote display
  and the bare-repository heuristic, never a write path.
- **(b) A corrupted or planted data directory.** The database, the home cache,
  and the visit counters are loaded from disk and can predate the current
  guards. A value read from them is untrusted exactly like a value typed on
  the command line.
- **(c) Another local user.** The data directory is created `0700` and the
  store files `0600`, so a second user cannot read the index or plant files
  beside it. If those modes are lost, `doctor` reports it and store opens
  refuse a symbolic link.
- **(d) Environment variables.** `$OVERDOSECD_DATA_DIR`, `$OVERDOSECD_CONFIG`,
  `$OVERDOSECD_COLOR`, `_ZO_DATA_DIR`, `$AUTOJUMP_DATA_DIR`, and `$ZSHZ_DATA`
  steer where overdosecd reads and writes; path-shaped values must be absolute,
  and every path that appears in an error message is escaped.
- **(e) Text that reaches a terminal or a shell.** Display safety is a property
  of the renderer: human-facing output escapes control characters, bidi
  controls, and never-visible characters, machine output (`goto`'s stdout,
  completion candidates) refuses them outright, and the output funnel rejects
  any escape sequence that is not one of overdosecd's own colour changes. A
  project name, alias, tag, note, path, branch, or remote added by any route —
  typed, imported, or selected in the picker — obeys the same rule.
- **(f) Entry profiles.** `overdosecd profile` and the wrapper's `enter` step
  turn a stored value into shell code the wrapper evaluates. The boundary:
  profiles are explicit and user-authored — nothing imported or scanned can
  register one; the `[general] hooks` switch defaults to **off**, so a jump
  never evaluates a profile until the user asks for it; a command is shown on
  stderr before it runs; variable names must be shell identifiers and values
  and commands pass the control-character guard both when written and when
  rendered. A profile a planted database smuggles in is still arbitrary
  shell — the switch, explicit authoring, and the display-before-run notice
  are what keep that boundary, per the out-of-scope note below.
- **(g) Themes.** `[theme]` values are parsed into typed colors (a named
  ANSI color, `color0`–`color255`, or `#rrggbb`) at config load; only
  overdosecd renders them, and only as a foreground color change. A config
  file cannot use a theme to smuggle an arbitrary escape sequence, and
  `never`/`NO_COLOR` disable every color regardless of the palette.
  `$OVERDOSECD_THEME` selects a bundled palette and accepts only `base` or
  `bleed`.

### Out of scope

- An attacker who can already write to the `0700` data directory owns the
  index by design (they can index any path). The link and mode checks raise
  the cost of turning that into a jump to an attacker-chosen directory, they
  do not make it impossible; with `[general] hooks` on, a database write is
  code execution on the next wrapper jump, which is why the switch is
  explicit, off by default, and per-project profiles are shown as they run.
- `overdosecd import` reads third-party stores the user points it at; importing
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
- A theme can only change a foreground color; the output funnel still rejects
  every escape that is not an SGR sequence.
- Data directories, config paths, and store entries must be absolute; store
  files are never opened through a symbolic link.
- Every read of a file that arrives from outside the tool is capped and
  requires a regular file.
- `projects.db` is written through one transaction at a time and created
  privately (`0600`); a legacy `projects.json` is moved aside once, never
  wiped, and a corrupt file is quarantined so two events in the same second
  cannot overwrite the first backup.
- `enter` emits only quoted values it validated, and refuses — never escapes —
  a profile it cannot render; `[general] hooks` gates it, and a jump's exit
  status never depends on it.

## Supply chain

CI runs `cargo deny` over the lockfile (advisories, licenses, bans, sources);
exceptions live in [`deny.toml`](deny.toml) with a reason. Third-party GitHub
Actions are pinned to commit SHAs.
