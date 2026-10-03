#!/usr/bin/env python3
"""Drive the interactive picker through a real pty and check the results.

CI cannot press keys in a terminal, so this is the end-to-end check for
`overdosecd ui`: it runs the real binary in a child with a controlling terminal
(the pty) while the child's stdout goes to a file -- exactly the wrapper's
`$(...)` situation -- and asserts on the path, the exit code, the index, and
the escape sequences the picker emits (no alternate screen, no full-screen
clear; the exit parks the cursor at the box origin with no cursor query, so
the next prompt lands where the box was).

    python3 scripts/picker-smoke.py                    # debug build, JSON backend
    python3 scripts/picker-smoke.py --release          # release build
    python3 scripts/picker-smoke.py --sqlite           # SQLite backend
    python3 scripts/picker-smoke.py --binary path/to/overdosecd

It covers jump, filter, Tab+j/k, pin, add, cancel, mouse scroll/click, the
palette's remove/rename/tag actions, batched pinning through marks, the detail
and health views, the help overlay, `ui --query`, the home-directory fallback
and `scan --home`, both wrappers, and both backends. Needs Linux (ptys),
python3, bash, and zsh. Not run by CI.
"""

import argparse
import codecs
import json
import os
import pty
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# SGR mouse: 1-based coordinates, 64/65 are the wheel, 0 the left button.
WHEEL_UP = b"\x1b[<64;10;5M"
WHEEL_DOWN = b"\x1b[<65;10;5M"


def click(row):
    return b"\x1b[<0;10;%dM" % (row + 1)


def release(row):
    return b"\x1b[<0;10;%dm" % (row + 1)


class Screen:
    """Just enough terminal emulation for what the picker emits: cursor moves,
    clears, SGR and mode escapes (ignored), and printable text.

    The picker's frames are cell diffs, so raw substrings do not appear
    contiguously in the stream; the screen is what the user actually sees.
    """

    def __init__(self, rows=30, cols=100):
        self.rows = rows
        self.cols = cols
        self.grid = [[" "] * cols for _ in range(rows)]
        self.row = 0
        self.col = 0
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, data):
        text = self.pending + self.decoder.decode(data)
        self.pending = ""
        index = 0
        while index < len(text):
            ch = text[index]
            if ch == "\x1b":
                end = self._escape_end(text, index)
                if end is None:
                    self.pending = text[index:]
                    return
                self._apply_escape(text, index, end)
                index = end
                continue
            if ch == "\r":
                self.col = 0
            elif ch == "\n":
                self.row = min(self.row + 1, self.rows - 1)
            elif ch >= " ":
                if self.col < self.cols:
                    self.grid[self.row][self.col] = ch
                self.col = min(self.col + 1, self.cols - 1)
            index += 1

    def lines(self):
        return ["".join(row).rstrip() for row in self.grid]

    def text(self):
        return "\n".join(self.lines())

    def _escape_end(self, text, start):
        """The index just past the escape sequence at `start`, or None when
        the sequence is not complete yet."""
        if start + 1 >= len(text):
            return None
        kind = text[start + 1]
        if kind == "[":
            index = start + 2
            while index < len(text) and not ("@" <= text[index] <= "~"):
                index += 1
            return None if index >= len(text) else index + 1
        if kind == "]":
            bel = text.find("\x07", start)
            st = text.find("\x1b\\", start)
            ends = []
            if bel != -1:
                ends.append(bel + 1)
            if st != -1:
                ends.append(st + 2)
            return min(ends) if ends else None
        return start + 2

    def _apply_escape(self, text, start, end):
        if text[start + 1] != "[":
            return
        params, final = text[start + 2 : end - 1], text[end - 1]
        if final in "Hf":
            parts = [part for part in params.split(";") if part]
            row = int(parts[0]) - 1 if parts else 0
            col = int(parts[1]) - 1 if len(parts) > 1 else 0
            self.row = max(0, min(row, self.rows - 1))
            self.col = max(0, min(col, self.cols - 1))
        elif final == "J":
            mode = int(params) if params.isdigit() else 0
            if mode == 0:
                for col in range(self.col, self.cols):
                    self.grid[self.row][col] = " "
                for row in range(self.row + 1, self.rows):
                    self.grid[row] = [" "] * self.cols
            elif mode == 2:
                self.grid = [[" "] * self.cols for _ in range(self.rows)]
        elif final == "K":
            for col in range(self.col, self.cols):
                self.grid[self.row][col] = " "


class Harness:
    def __init__(self, binary, sqlite):
        self.binary = binary
        self.sqlite = sqlite
        self.root = tempfile.mkdtemp(prefix="overdosecd-picker-")
        self.home = os.path.join(self.root, "home")
        self.data = os.path.join(self.root, "data")
        self.projects = os.path.join(self.root, "projects")
        self.config = os.path.join(self.root, "config")
        self.failures = []
        self.screen = Screen()
        self.snapshot = None
        self.final_screen = None
        os.makedirs(self.home)
        os.makedirs(self.config)
        for name in ("alpha", "beta", "gamma"):
            os.makedirs(os.path.join(self.projects, name))

    def env(self):
        env = dict(os.environ)
        env["OVERDOSECD_DATA_DIR"] = self.data
        env["HOME"] = self.home
        env["XDG_CONFIG_HOME"] = self.config
        env["NO_COLOR"] = "1"
        env["PATH"] = os.path.dirname(self.binary) + os.pathsep + env["PATH"]
        # The harness's own pty is not a multiplexer pane: scrub the session's
        # multiplexer variables so mouse `auto` resolves the same way it would
        # in a plain terminal.
        for key in list(env):
            if key in ("TMUX", "TMUX_PANE", "ZELLIJ", "STY"):
                env.pop(key)
        return env

    def add(self, path):
        subprocess.run(
            [self.binary, "add", path],
            env=self.env(),
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    def setup(self, names=("alpha", "beta", "gamma")):
        """Fresh index: no usage, no pins, so the order is alphabetical."""
        shutil.rmtree(self.data, ignore_errors=True)
        if self.sqlite:
            config_dir = os.path.join(self.config, "overdosecd")
            os.makedirs(config_dir, exist_ok=True)
            with open(os.path.join(config_dir, "config.toml"), "w") as handle:
                handle.write('[storage]\nbackend = "sqlite"\n')
        for name in names:
            os.makedirs(os.path.join(self.projects, name), exist_ok=True)
            self.add(os.path.join(self.projects, name))

    def index(self):
        """The index as a list of dicts, backend-agnostically."""
        if self.sqlite:
            import sqlite3

            conn = sqlite3.connect(os.path.join(self.data, "projects.db"))
            conn.row_factory = sqlite3.Row
            rows = [dict(row) for row in conn.execute("SELECT * FROM projects")]
            conn.close()
            for row in rows:
                row["pinned"] = bool(row["pinned"])
                for field in ("aliases", "tags"):
                    value = row.get(field)
                    if isinstance(value, str):
                        try:
                            row[field] = json.loads(value)
                        except json.JSONDecodeError:
                            pass
            return rows
        with open(os.path.join(self.data, "projects.json")) as handle:
            return json.load(handle)["projects"]

    def by_name(self, name):
        return next(p for p in self.index() if p["name"] == name)

    def run(
        self,
        argv,
        keys,
        cwd=None,
        shell=None,
        timeout=20,
        snapshot_at=None,
        signal_at=None,
        resize=None,
    ):
        """Run argv under a pty; stdin+stderr are the tty, stdout is a file.

        With `snapshot_at`, the emulated screen is captured after that many
        keys are sent (before the rest), so a view can be asserted while it is
        open rather than after it is cleared. With `signal_at`, SIGTERM is
        sent after that many keys (0 sends it right after startup); with
        `resize`, the pty is resized `(rows, cols)` once the picker is up.
        """
        out_path = tempfile.mktemp(dir=self.root)
        pid, fd = pty.fork()
        if pid == 0:
            # Deterministic pane geometry: rows=30, cols=100.
            import fcntl
            import struct
            import termios

            fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
            os.close(1)
            out_fd = os.open(out_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
            os.dup2(out_fd, 1)
            # Python opens files close-on-exec; dup2 leaves the flag alone when
            # out_fd is already 1, so clear it explicitly.
            os.set_inheritable(1, True)
            if cwd:
                os.chdir(cwd)
            env = self.env()
            if shell == "zsh":
                os.execve("/bin/zsh", ["zsh", "-f", "-c", argv], env)
            elif shell == "bash":
                os.execve("/bin/bash", ["bash", "--norc", "-c", argv], env)
            else:
                os.execve(argv[0], argv, env)
            os._exit(127)

        self.screen = Screen()
        self.snapshot = None
        self.final_screen = None
        pty_bytes = b""
        answered_at = 0

        def pump(wait):
            nonlocal pty_bytes, answered_at
            ready, _, _ = select.select([fd], [], [], wait)
            if not ready:
                return
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                return
            pty_bytes += chunk
            self.screen.feed(chunk)
            # A real terminal answers every cursor-position query with the
            # cursor's current position (1-based), and the inline viewport
            # asks again after a resize, not just at startup.
            while True:
                at = pty_bytes.find(b"\x1b[6n", answered_at)
                if at < 0:
                    break
                answered_at = at + 4
                try:
                    os.write(
                        fd,
                        b"\x1b[%d;%dR" % (self.screen.row + 1, self.screen.col + 1),
                    )
                except OSError:
                    return

        status = None

        # Warm-up: let the child start and answer its cursor-position query the
        # way a real terminal would (the inline viewport asks before its first
        # frame), so keys never race the query's reply. The reply is sent by
        # `pump` as soon as the query appears -- the picker waits, but not
        # forever.
        warmup = time.time() + 1.0
        while time.time() < warmup:
            done, raw = os.waitpid(pid, os.WNOHANG)
            if done:
                status = raw
                break
            pump(0.02)

        if status is None and resize is not None:
            import fcntl
            import struct
            import termios

            rows, cols = resize
            fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
            time.sleep(0.3)
            pump(0.2)

        if status is None:
            try:
                for index, key in enumerate(keys):
                    os.write(fd, key)
                    time.sleep(0.45)
                    if snapshot_at is not None and index + 1 == snapshot_at:
                        pump(0.3)
                        self.snapshot = self.screen.lines()
            except OSError:
                pass
            if signal_at is not None:
                try:
                    pump(0.2)
                    os.kill(pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass

        deadline = time.time() + timeout
        while status is None and time.time() < deadline:
            done, raw = os.waitpid(pid, os.WNOHANG)
            if done:
                status = raw
                break
            pump(0.2)
        if status is None:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
            self.final_screen = self.screen.lines()
            return None, "TIMEOUT", pty_bytes

        # The child is gone: drain whatever it left on the pty instead of
        # dropping the tail of the stream (reads hit EIO once the slave is
        # closed).
        while True:
            ready, _, _ = select.select([fd], [], [], 0.1)
            if not ready:
                break
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            pty_bytes += chunk
            self.screen.feed(chunk)

        self.final_screen = self.screen.lines()
        with open(out_path) as handle:
            return os.waitstatus_to_exitcode(status), handle.read(), pty_bytes

    def screen_has(self, needle, lines=None):
        """Whether the emulated screen (final by default) shows `needle`."""
        lines = self.final_screen if lines is None else lines
        return any(needle in line for line in (lines or []))

    def check(self, label, condition, detail=""):
        state = "ok" if condition else "FAIL"
        print(f"[{state}] {label}" + ("" if condition else f"  ({detail})"))
        if not condition:
            self.failures.append(label)

    def check_non_intrusive(self, pty):
        """The inline box must not take over the pane the way a full-screen
        TUI would."""
        for label, sequence in [
            ("no alternate screen", b"\x1b[?1049h"),
            ("no full-screen clear", b"\x1b[2J"),
            ("nothing cleared above the box", b"\x1b[1J"),
            ("scrollback left alone", b"\x1b[3J"),
        ]:
            self.check(label, sequence not in pty, f"found {sequence!r}")

    def check_exit_cursor(self, label, pty, anchor=(0, 0), queries=None):
        """The exit teardown must park the cursor at the box origin (the
        direct-`ui` scenarios anchor at the pty's top-left), so the shell's
        next prompt lands on the line the box occupied - no box-height gap
        and no zsh `%` marker. The stream must also carry no cursor query on
        the way out: `Terminal::clear()` in ratatui 0.30 restores the
        pre-clear cursor position and blocks on `ESC [ 6 n`, which left the
        gap whenever the terminal answered the query.
        """
        row, col = self.screen.row, self.screen.col
        self.check(
            f"{label} (cursor at the box origin)",
            (row, col) == anchor,
            f"cursor at ({row},{col}), expected {anchor}",
        )
        if queries is not None:
            found = pty.count(b"\x1b[6n")
            self.check(
                f"{label} (no extra cursor query)",
                found == queries,
                f"{found} cursor queries, expected {queries}",
            )

    def smoke(self):
        binary = self.binary
        alpha = os.path.join(self.projects, "alpha")
        beta = os.path.join(self.projects, "beta")
        delta = os.path.join(self.projects, "delta")
        gamma = os.path.join(self.projects, "gamma")

        # Enter jumps to the top row; the jump lands in the usage log.
        self.setup()
        code, out, pty = self.run([binary, "ui"], [b"\r"])
        self.check(
            "enter jumps to the first row",
            code == 0 and out == alpha + "\n",
            f"code={code} out={out!r}",
        )
        self.check("the jump is recorded", self.by_name("alpha")["use_count"] == 1)
        self.check("usage history exists", self.by_name("alpha")["last_used_at"] is not None)
        self.check_non_intrusive(pty)
        self.check_exit_cursor("enter jumps to the first row", pty, queries=1)

        # Typing filters.
        self.setup()
        code, out, pty = self.run([binary, "ui"], [b"gam", b"\r"])
        self.check(
            "typing filters and jumps",
            code == 0 and out == gamma + "\n",
            f"code={code} out={out!r}",
        )
        self.check_exit_cursor("typing filters", pty)

        # Tab switches to the list mode; j moves; Enter jumps to the second row.
        self.setup()
        code, out, pty = self.run([binary, "ui"], [b"\t", b"j", b"\r"])
        self.check(
            "tab + j moves and jumps",
            code == 0 and out == beta + "\n",
            f"code={code} out={out!r}",
        )
        self.check_exit_cursor("tab + j moves", pty)

        # Ctrl+p pins; Ctrl+c cancels with exit 1 and empty stdout.
        self.setup()
        code, out, pty = self.run([binary, "ui"], [b"\x10", b"\x03"])
        self.check(
            "ctrl-c cancels quietly", code == 1 and out == "", f"code={code} out={out!r}"
        )
        self.check(
            "ctrl-p pins the highlighted project", self.by_name("alpha")["pinned"] is True
        )
        self.check_exit_cursor("ctrl-c cancels", pty, queries=1)

        # SIGTERM mid-session must take the normal exit path: the terminal is
        # restored (mouse tracking released) instead of left broken.
        self.setup()
        code, out, raw = self.run([binary, "ui"], [], signal_at=0, timeout=10)
        self.check(
            "sigterm ends like a cancel", code == 1 and out == "", f"code={code} out={out!r}"
        )
        self.check(
            "sigterm releases the mouse",
            b"\x1b[?1006l" in raw,
            "mouse-off escape missing from the stream",
        )
        self.check_non_intrusive(raw)
        self.check_exit_cursor("sigterm ends like a cancel", raw, queries=1)

        # Resizing mid-session keeps the picker inline and functional. The
        # width grows here on purpose: ratatui clears the visible screen (2J,
        # never 3J -- scrollback survives) when the terminal narrows, to avoid
        # line wrapping during the redraw.
        self.setup()
        code, out, raw = self.run([binary, "ui"], [b"\r"], resize=(24, 120))
        self.check(
            "resize keeps the picker inline and jumping",
            code == 0 and out == alpha + "\n",
            f"code={code} out={out!r}",
        )
        self.check_non_intrusive(raw)
        # Two queries: the startup anchor and the resize re-anchor.
        self.check_exit_cursor("resize keeps the picker inline", raw, queries=2)

        # Ctrl+a adds the current directory.
        self.setup()
        fresh = os.path.join(self.projects, "fresh-one")
        os.makedirs(fresh)
        code, out, _ = self.run([binary, "ui"], [b"\x01", b"\r", b"\x03"], cwd=fresh)
        self.check(
            "ctrl-a adds the cwd",
            any(p["name"] == "fresh-one" for p in self.index()),
        )
        self.check("adding stays quiet on stdout", out == "", f"out={out!r}")

        # Mouse: one wheel notch moves three rows, a click selects its row.
        # The box spans rows 0..9 of the 30-row pty (list rows 0..7).
        mouse_setup = ("alpha", "beta", "delta", "gamma")
        self.setup(mouse_setup)
        code, out, _ = self.run([binary, "ui"], [WHEEL_DOWN, b"\r"])
        self.check(
            "wheel scrolls the list",
            code == 0 and out == gamma + "\n",
            f"code={code} out={out!r}",
        )

        self.setup(mouse_setup)
        code, out, _ = self.run([binary, "ui"], [click(2), release(2), b"\r"])
        self.check(
            "click selects the row under the pointer",
            code == 0 and out == delta + "\n",
            f"code={code} out={out!r}",
        )

        # `ui --query` prefills the search box (the wrapper's fallback uses it).
        self.setup()
        code, out, _ = self.run([binary, "ui", "--query", "gam"], [b"\r"])
        self.check(
            "ui --query prefills the search box",
            code == 0 and out == gamma + "\n",
            f"code={code} out={out!r}",
        )

        # The palette: remove, rename, and tag actions.
        self.setup()
        code, _, _ = self.run(
            [binary, "ui"],
            [b"\t", b":", b"remove from the index", b"\r", b"y", b"\x03"],
        )
        self.check(
            "palette remove drops the project",
            not any(p["name"] == "alpha" for p in self.index()),
            str([p["name"] for p in self.index()]),
        )

        self.setup()
        code, _, _ = self.run(
            [binary, "ui"],
            [b"\t", b":", b"rename", b"\r", b"\x7f" * 5, b"alpha-two", b"\r", b"\x03"],
        )
        self.check(
            "palette rename renames the project",
            any(p["name"] == "alpha-two" for p in self.index()),
            str([p["name"] for p in self.index()]),
        )

        self.setup()
        code, _, _ = self.run(
            [binary, "ui"],
            [b"\t", b":", b"add a tag", b"\r", b"rust", b"\r", b"\x03"],
        )
        self.check(
            "palette tag add updates the project",
            any(p["name"] == "alpha" and p.get("tags") == ["rust"] for p in self.index()),
            str(self.by_name("alpha")),
        )

        # Marks make pin/remove act on a batch.
        self.setup()
        code, _, _ = self.run([binary, "ui"], [b"\t", b"m", b"j", b"m", b"\x10", b"\x03"])
        self.check(
            "marked rows pin as a batch",
            all(
                p["pinned"]
                for p in self.index()
                if p["name"] in ("alpha", "beta")
            )
            and len([p for p in self.index() if p["pinned"]]) == 2,
            str([(p["name"], p["pinned"]) for p in self.index()]),
        )

        # The detail pane and the health view render their extra lines; the
        # screen is captured while the view is open (exiting clears the box).
        self.setup()
        code, _, _ = self.run(
            [binary, "ui"], [b"\t", b":", b"details", b"\r", b"\x03"], snapshot_at=4
        )
        self.check(
            "detail pane renders",
            self.screen_has("uses:", self.snapshot),
            "\n".join(self.snapshot or []),
        )

        self.setup()
        shutil.rmtree(os.path.join(self.projects, "alpha"))
        code, _, _ = self.run(
            [binary, "ui"], [b"\t", b":", b"health", b"\r", b"\x03"], snapshot_at=4
        )
        self.check(
            "health view lists the stale project",
            self.screen_has("stale `alpha`", self.snapshot),
            "\n".join(self.snapshot or []),
        )

        self.setup()
        code, _, _ = self.run([binary, "ui"], [b"\t", b"?", b"\x03"], snapshot_at=2)
        self.check(
            "help overlay renders",
            self.screen_has("half-page moves", self.snapshot),
            "\n".join(self.snapshot or []),
        )

        # The wrapper's jump semantics: a confident prefix jumps straight
        # through, a fuzzy-only query opens the picker prefilled, and
        # `ocd --cmd` runs the CLI without a picker or a cd.
        self.setup()
        for label, command, keys, expected in [
            (
                "confident ocd jumps without the picker",
                'eval "$(overdosecd init bash)"; ocd gamm; pwd',
                [],
                gamma,
            ),
            (
                "weak ocd opens the picker prefilled",
                'eval "$(overdosecd init bash)"; ocd gma; pwd',
                [b"\r"],
                gamma,
            ),
        ]:
            code, out, pty = self.run(command, keys, cwd=self.root, shell="bash")
            lines = [line for line in out.splitlines() if line.strip()]
            self.check(
                label,
                code == 0 and bool(lines) and lines[-1] == expected,
                f"code={code} out={out!r}",
            )
            if label.startswith("confident"):
                self.check(
                    "no picker was drawn for the confident jump",
                    b"type to filter" not in pty,
                    "the hint line appeared",
                )

        self.setup()
        code, out, _ = self.run(
            'eval "$(overdosecd init bash)"; ocd --cmd list',
            [],
            cwd=self.root,
            shell="bash",
        )
        self.check(
            "ocd --cmd runs the CLI",
            code == 0 and "alpha" in out and "beta" in out,
            f"code={code} out={out!r}",
        )

        # Home discovery: a directory outside the index is offered by the
        # fallback and Enter jumps to it (nothing is recorded).
        self.setup()
        hidden = os.path.join(self.home, "hidden-notes")
        os.makedirs(hidden)
        code, out, _ = self.run([binary, "ui"], [b"hidden", b"\r"])
        self.check(
            "home fallback jumps to an unindexed directory",
            code == 0 and out == hidden + "\n",
            f"code={code} out={out!r}",
        )

        scan = subprocess.run(
            [binary, "scan", "--home", "--dry-run"],
            env=self.env(),
            capture_output=True,
            text=True,
        )
        self.check(
            "scan --home --dry-run counts directories",
            scan.returncode == 0 and "would cache" in scan.stdout,
            f"code={scan.returncode} out={scan.stdout!r} err={scan.stderr!r}",
        )

        # The inline box must not take the pane over: no alternate screen, no
        # full-screen clear, and text above the box survives the session.
        self.setup()
        command = (
            "printf 'KEEP-ME-42\\n' >/dev/tty; "
            'eval "$(overdosecd init bash)"; ocd; printf "AFTER\\n"'
        )
        code, _, pty = self.run(command, [b"\r"], cwd=self.root, shell="bash")
        self.check("kept the pane content", b"KEEP-ME-42" in pty, f"code={code}")
        self.check(
            "kept the pane content (screen)",
            self.screen_has("KEEP-ME-42"),
            "\n".join(self.final_screen or []),
        )
        self.check_non_intrusive(pty)

        # The wrappers: bare `ocd`, `ocd --ui`, and zsh all open the picker
        # and cd into the choice (`pwd` prints it after the picker exits).
        for label, shell, command in [
            ("bash bare ocd", "bash", 'eval "$(overdosecd init bash)"; ocd; pwd'),
            ("bash ocd --ui", "bash", 'eval "$(overdosecd init bash)"; ocd --ui; pwd'),
            ("zsh bare ocd", "zsh", 'eval "$(overdosecd init zsh)"; ocd; pwd'),
        ]:
            self.setup()
            code, out, _ = self.run(command, [b"\r"], cwd=self.root, shell=shell)
            lines = [line for line in out.splitlines() if line.strip()]
            self.check(
                f"{label} jumps",
                code == 0 and bool(lines) and lines[-1] == alpha,
                f"code={code} out={out!r}",
            )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary",
        default=os.path.join(REPO, "target", "debug", "overdosecd"),
        help="the overdosecd binary to drive (default: target/debug/overdosecd)",
    )
    parser.add_argument("--release", action="store_true", help="use target/release/overdosecd")
    parser.add_argument("--sqlite", action="store_true", help="run against the SQLite backend")
    args = parser.parse_args()

    binary = args.binary
    if args.release:
        binary = os.path.join(REPO, "target", "release", "overdosecd")
    if not os.path.exists(binary):
        parser.error(f"{binary} does not exist; build it first")

    harness = Harness(binary, args.sqlite)
    harness.smoke()

    print()
    if harness.failures:
        print("FAILURES:", ", ".join(harness.failures))
        sys.exit(1)
    backend = "sqlite" if args.sqlite else "json"
    print(f"all picker smoke checks passed ({backend}, {binary})")


if __name__ == "__main__":
    main()
