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
use std::io::{self, Read};
use std::thread;
use std::time::{Duration, Instant};

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

/// How long the terminal gets to answer a cursor report before the query is
/// treated as unsupported. Generous on purpose: over a slow SSH link a round
/// trip can take a moment. It is paid at startup and again on every resize
/// (ratatui re-anchors the inline viewport).
const CURSOR_QUERY_TIMEOUT: Duration = Duration::from_millis(1000);

/// Longest buffer worth keeping for reports: two of `ESC [ 65535 ; 65535 R`.
const MAX_REPORT_BYTES: usize = 64;

pub struct TtyBackend {
    /// Drawing and every other escape sequence.
    inner: CrosstermBackend<File>,

    /// A second, readable handle to the same terminal, used only for the
    /// cursor query; opened non-blocking so an unanswering terminal cannot
    /// leave a reader behind.
    query: File,
}

impl TtyBackend {
    pub(crate) fn new(drawing: File, query: File) -> Self {
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
        match query_cursor_position(&mut self.query) {
            Ok(Some(position)) => Ok(position),
            // A terminal that does not answer must not end the session
            // (ratatui's inline viewport asks again on every resize). Anchor
            // at the bottom edge instead of failing the draw.
            Ok(None) => {
                let size = self.inner.size()?;
                Ok(Position {
                    x: 0,
                    y: size.height.saturating_sub(1),
                })
            }
            // A genuinely broken handle is different from a silent terminal:
            // hiding it as a normal position masked EIO on a dead tty.
            Err(err) => Err(err),
        }
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
/// `ESC [ row ; col R` reply. The handle is non-blocking (see `open_tty`), so
/// the timeout is a deadline loop rather than a reader thread: a thread could
/// outlive the query and steal the user's typed input.
///
/// Anything already queued is discarded *before* the query: a late answer to
/// an earlier (timed-out) query would otherwise be read as this one's reply
/// and the fresh answer would be left for crossterm to interpret as keys.
/// Reading stops after the first complete report so a keystroke arriving
/// right behind it is not swallowed.
fn query_cursor_position(tty: &mut File) -> io::Result<Option<Position>> {
    drain_queued(tty);

    // A full output queue (Ctrl-S flow control) is backpressure, not a dead
    // terminal: retry the remaining bytes under the shared query deadline
    // instead of abandoning a partial `ESC [ 6 n` in its parser.
    let deadline = Instant::now() + CURSOR_QUERY_TIMEOUT;
    if !write_query(tty, deadline)? {
        return Ok(None);
    }

    let mut reply = Vec::with_capacity(MAX_REPORT_BYTES);
    let mut byte = [0u8; 1];
    let mut saw_report = false;
    while reply.len() < MAX_REPORT_BYTES {
        match tty.read(&mut byte) {
            // EOF before any answer means an unsupported query; after one,
            // the last complete report is still usable.
            Ok(0) => break,
            Ok(_) => {
                reply.push(byte[0]);
                if byte[0] == b'R' {
                    saw_report = true;
                    break;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(2));
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(if saw_report {
        parse_cursor_report(&reply)
    } else {
        None
    })
}

/// Writes `ESC [ 6 n`, retrying a non-blocking writer until `deadline`.
///
/// Returns `false` when the whole sequence could not be written in time (or
/// the handle closed); the caller then treats the query as unanswered, so a
/// flow-controlled terminal degrades to the fallback anchor.
fn write_query(tty: &mut impl std::io::Write, deadline: Instant) -> io::Result<bool> {
    let query = b"\x1b[6n";
    let mut written = 0;
    while written < query.len() {
        match tty.write(&query[written..]) {
            Ok(0) => return Ok(false),
            Ok(count) => written += count,
            Err(err)
                if err.kind() == io::ErrorKind::WouldBlock
                    || err.kind() == io::ErrorKind::Interrupted =>
            {
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                thread::sleep(Duration::from_millis(2));
            }
            Err(err) => return Err(err),
        }
    }
    tty.flush()?;
    Ok(true)
}

/// Reads and discards up to one report's worth of bytes already queued on the
/// tty.
///
/// Only bytes that are already there are touched, so the drain cannot eat
/// input typed after the query; input queued *before* it may be discarded
/// (the read loop always tolerated junk in front of the report), and the
/// byte bound keeps a terminal spewing output from stalling the query.
fn drain_queued(tty: &mut File) {
    let mut byte = [0u8; 1];
    for _ in 0..MAX_REPORT_BYTES {
        match tty.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

/// Parses the last complete `ESC [ row ; col R` in `reply` into a zero-based
/// position. Every candidate is tried (an incomplete report before the answer
/// must not hide it), and taking the last one tolerates typed input that
/// reached the tty before the answer plus a second, stale report.
fn parse_cursor_report(reply: &[u8]) -> Option<Position> {
    let mut best = None;
    let mut search = 0;
    while let Some(offset) = reply[search..].windows(2).position(|pair| pair == b"\x1b[") {
        let start = search + offset + 2;
        if let Some(end) = reply[start..].iter().position(|&byte| byte == b'R')
            && let Ok(body) = std::str::from_utf8(&reply[start..start + end])
            && let Some((row, column)) = body.split_once(';')
            && let (Ok(row), Ok(column)) = (row.parse::<u16>(), column.parse::<u16>())
        {
            best = Some(Position {
                x: column.saturating_sub(1),
                y: row.saturating_sub(1),
            });
        }
        search = start;
    }
    best
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
    fn stray_input_before_the_report_is_tolerated() {
        assert_eq!(
            parse_cursor_report(b"gam\x1b[5;10R"),
            Some(Position { x: 9, y: 4 })
        );
        assert_eq!(
            parse_cursor_report(b"\x1b[1;1R\x1b[2;3R"),
            Some(Position { x: 2, y: 1 }),
            "the last report wins"
        );
        // A stale, incomplete report in front of the answer must not hide it
        // (the old parser took the last ESC[ and gave up on this buffer).
        assert_eq!(
            parse_cursor_report(b"\x1b[1;1\x1b[9;9R"),
            Some(Position { x: 8, y: 8 })
        );
        // ... and a partial report after it must not hide an earlier one.
        assert_eq!(
            parse_cursor_report(b"\x1b[4;7R\x1b[9;9"),
            Some(Position { x: 6, y: 3 })
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

    /// A writer that reports backpressure for the first `stalls` calls, then
    /// accepts at most `chunk` bytes per call.
    struct FlakyWriter {
        stalls: usize,
        chunk: usize,
        out: Vec<u8>,
    }

    impl std::io::Write for FlakyWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.stalls > 0 {
                self.stalls -= 1;
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            let count = buf.len().min(self.chunk);
            self.out.extend_from_slice(&buf[..count]);
            Ok(count)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn the_query_write_survives_backpressure() {
        let mut writer = FlakyWriter {
            stalls: 2,
            chunk: 2,
            out: Vec::new(),
        };
        assert!(
            write_query(&mut writer, Instant::now() + Duration::from_secs(1)).expect("write"),
            "a partial write is retried to completion"
        );
        assert_eq!(
            writer.out, b"\x1b[6n",
            "no truncated escape is left in the parser"
        );

        let mut stalled = FlakyWriter {
            stalls: usize::MAX,
            chunk: 1,
            out: Vec::new(),
        };
        assert!(
            !write_query(&mut stalled, Instant::now()).expect("timeout is not an error"),
            "an expired deadline means the query is unanswered"
        );
        assert!(stalled.out.is_empty(), "nothing partial is written");
    }
}
