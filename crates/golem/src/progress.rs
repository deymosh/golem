//! `PROGRESS` lines on a terminal: one bar, redrawn in place.
//!
//! The lines themselves are `golem_core::progress`'s. A verb that streams
//! through the service - `golem up` fetching a model it does not have yet -
//! sends them to whoever asked, and a terminal would show them as a column
//! of numbers. [`Bar`] sits between the stream and the terminal: a progress
//! line becomes the bar on the last row, every other line prints above it,
//! and the clear takes the bar away.

use std::io::{self, IsTerminal, Write};

use golem_core::progress::{self, Progress};

/// The bar's width in cells.
const WIDTH: usize = 28;

/// Erase the current row and go back to its start.
const ERASE: &[u8] = b"\r\x1b[2K";

/// A writer that draws `PROGRESS` lines as a bar on the row below the text.
pub struct Bar<W: Write> {
    inner: W,
    /// Bytes of a line not yet ended.
    pending: Vec<u8>,
    /// The bar on screen, to put back under each line that prints above it.
    shown: Option<Progress>,
}

impl<W: Write> Bar<W> {
    pub fn new(inner: W) -> Bar<W> {
        Bar { inner, pending: Vec::new(), shown: None }
    }

    fn line(&mut self, line: &str) -> io::Result<()> {
        let had_bar = self.shown.is_some();
        if progress::is_line(line) {
            self.shown = Progress::parse(line);
        }
        if had_bar {
            self.inner.write_all(ERASE)?;
        }
        if !progress::is_line(line) {
            writeln!(self.inner, "{line}")?;
        }
        if let Some(bar) = &self.shown {
            write!(self.inner, "{}", bar.text(WIDTH))?;
        }
        self.inner.flush()
    }
}

impl<W: Write> Write for Bar<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buf);
        while let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
            let bytes: Vec<u8> = self.pending.drain(..=end).collect();
            let text = String::from_utf8_lossy(&bytes[..end]).trim_end_matches('\r').to_string();
            self.line(&text)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Write> Drop for Bar<W> {
    /// A bar still showing when the stream ends stays, finished with its
    /// newline, and a partial last line is written as it came.
    fn drop(&mut self) {
        if !self.pending.is_empty() {
            let rest = String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned();
            let _ = self.line(&rest);
        }
        if self.shown.is_some() {
            let _ = self.inner.write_all(b"\n");
        }
        let _ = self.inner.flush();
    }
}

/// Stdout for a verb that may stream progress: the bar on a terminal, the
/// lines as they are anywhere else - a pipe or a log keeps every line.
pub fn stdout() -> Box<dyn Write> {
    if io::stdout().is_terminal() {
        Box::new(Bar::new(io::stdout()))
    } else {
        Box::new(io::stdout())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drawn(input: &str) -> String {
        let mut out = Vec::new();
        {
            let mut bar = Bar::new(&mut out);
            bar.write_all(input.as_bytes()).expect("write");
        }
        String::from_utf8(out).expect("utf-8")
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(drawn("one\ntwo\n"), "one\ntwo\n");
    }

    #[test]
    fn a_progress_line_is_a_bar_that_text_prints_above() {
        let bar = Progress::steps(1, 2, "pull: x").text(WIDTH);
        let out = drawn("start\nPROGRESS 1/2 steps pull: x\nmore\nPROGRESS -\nend\n");
        let erase = "\r\u{1b}[2K";
        assert_eq!(out, format!("start\n{bar}{erase}more\n{bar}{erase}end\n"));
        assert!(!out.contains("PROGRESS"), "{out}");
    }

    #[test]
    fn a_bar_left_at_the_end_keeps_its_row() {
        let bar = Progress::steps(2, 2, "tune: gate, load 2").text(WIDTH);
        assert_eq!(drawn("PROGRESS 2/2 steps tune: gate, load 2\n"), format!("{bar}\n"));
        // Lines split across writes, and a CRLF stream, still read as lines.
        let mut out = Vec::new();
        {
            let mut b = Bar::new(&mut out);
            b.write_all(b"hal").expect("write");
            b.write_all(b"f\r\nlast").expect("write");
        }
        assert_eq!(String::from_utf8(out).expect("utf-8"), "half\nlast\n");
    }
}
