//! A ratatui [`Backend`] that keeps every escape sequence on the picker's own
//! terminal handle.
//!
//! `CrosstermBackend` answers ratatui's cursor query through crossterm's
//! `cursor::position()`, which writes `ESC [ 6 n` to *stdout*. The wrapper
//! captures stdout with `$(...)`, so that query would land in the command
//! substitution and break `cd "$(overdosecd ui)"`. The query is therefore
//! reimplemented here against the tty handle, with a timeout — a terminal
//! that never answers must not hang the picker.

use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

/// How long the terminal gets to answer a cursor report before the query is
/// treated as unsupported. Generous on purpose: over a slow SSH link a round
/// trip can take a moment, and this is paid once at startup.
const CURSOR_QUERY_TIMEOUT: Duration = Duration::from_millis(1000);

/// Longest cursor report worth buffering: `ESC [ 65535 ; 65535 R`.
const MAX_REPORT_BYTES: usize = 32;

pub struct TtyBackend {
    /// Drawing and every other escape sequence.
    inner: CrosstermBackend<File>,

    /// A second, readable handle to the same terminal, used only for the
    /// cursor query (the reader may still be blocked when the main thread
    /// times out).
    query: File,
}

impl TtyBackend {
    pub fn new(drawing: File, query: File) -> Self {
        Self {
            inner: CrosstermBackend::new(drawing),
            query,
        }
    }
}

impl Backend for TtyBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        query_cursor_position(&mut self.query)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

/// Asks the terminal for a cursor position report (`ESC [ 6 n`) and parses the
/// `ESC [ row ; col R` reply. It needs a handle that can read the answer back,
/// which is why [`open_terminal_read_write`] exists.
fn query_cursor_position(tty: &mut File) -> io::Result<Position> {
    tty.write_all(b"\x1b[6n")?;
    tty.flush()?;

    // The read blocks until the terminal answers, so a reader thread enforces
    // the timeout without `poll`/`libc` plumbing on the main path.
    let mut reader = tty.try_clone()?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reply = Vec::with_capacity(MAX_REPORT_BYTES);
        let mut byte = [0u8; 1];
        while reply.len() < MAX_REPORT_BYTES {
            match reader.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    reply.push(byte[0]);
                    if byte[0] == b'R' {
                        break;
                    }
                }
            }
        }
        let _ = sender.send(parse_cursor_report(&reply));
    });

    match receiver.recv_timeout(CURSOR_QUERY_TIMEOUT) {
        Ok(Some(position)) => Ok(position),
        Ok(None) | Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the terminal did not answer the cursor position query",
        )),
    }
}

/// Parses a `ESC [ row ; col R` cursor report into a zero-based position.
fn parse_cursor_report(reply: &[u8]) -> Option<Position> {
    let reply = std::str::from_utf8(reply).ok()?;
    let body = reply.strip_prefix("\x1b[")?.strip_suffix('R')?;
    let (row, column) = body.split_once(';')?;
    let row: u16 = row.parse().ok()?;
    let column: u16 = column.parse().ok()?;
    Some(Position {
        x: column.saturating_sub(1),
        y: row.saturating_sub(1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_reports_parse_to_zero_based_positions() {
        assert_eq!(
            parse_cursor_report(b"\x1b[1;1R"),
            Some(Position { x: 0, y: 0 })
        );
        assert_eq!(
            parse_cursor_report(b"\x1b[24;80R"),
            Some(Position { x: 79, y: 23 })
        );
        assert_eq!(
            parse_cursor_report(b"\x1b[5;10R"),
            Some(Position { x: 9, y: 4 })
        );
    }

    #[test]
    fn malformed_cursor_reports_are_ignored() {
        for reply in [
            b"".as_slice(),
            b"junk",
            b"\x1b[1;1",
            b"\x1b[;R",
            b"\x1b[a;bR",
            b"\x1b[0;0R", // parses, but saturates to the origin
        ] {
            let parsed = parse_cursor_report(reply);
            if reply == b"\x1b[0;0R" {
                assert_eq!(parsed, Some(Position { x: 0, y: 0 }));
            } else {
                assert_eq!(parsed, None, "reply: {reply:?}");
            }
        }
    }
}
