//! A long step's progress, as one line in a job's log.
//!
//! A pull, a tune and an eval run as children whose output lands in the job
//! log the console and the screen tail. A line that starts with `PROGRESS `
//! is how they say how far along they are:
//!
//! ```text
//! PROGRESS 4194304/20971520 bytes pull: Ornith-1.5-35B-Q4_K_M.gguf
//! PROGRESS 6/10 steps tune: context
//! PROGRESS -
//! ```
//!
//! The last one clears the bar: it is written when a job starts and when it
//! ends, so a finished job's bar never shows under the next one. The readers
//! draw the newest line as a bar and leave every such line out of the log
//! text they show.

use crate::json;

/// What every progress line starts with.
pub const PREFIX: &str = "PROGRESS ";

/// The line that clears the bar.
pub const CLEAR: &str = "PROGRESS -";

/// What `done` and `total` count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// A download: shown as gigabytes or megabytes.
    Bytes,
    /// Loads, sections, requests: shown as plain counts.
    Steps,
}

impl Unit {
    fn word(self) -> &'static str {
        match self {
            Unit::Bytes => "bytes",
            Unit::Steps => "steps",
        }
    }
}

/// How far a long step has got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
    pub unit: Unit,
    /// What is being done, short: `pull: <file>`, `tune: context`.
    pub label: String,
}

impl Progress {
    pub fn bytes(done: u64, total: u64, label: impl Into<String>) -> Progress {
        Progress { done, total, unit: Unit::Bytes, label: label.into() }
    }

    pub fn steps(done: u64, total: u64, label: impl Into<String>) -> Progress {
        Progress { done, total, unit: Unit::Steps, label: label.into() }
    }

    /// The log line that says this.
    pub fn line(&self) -> String {
        format!("{PREFIX}{}/{} {} {}", self.done, self.total, self.unit.word(), self.label)
    }

    /// The progress a line states, or `None` for any other line - the clear
    /// included.
    pub fn parse(line: &str) -> Option<Progress> {
        let rest = line.trim_end().strip_prefix(PREFIX)?;
        let mut words = rest.splitn(3, ' ');
        let (done, total) = words.next()?.split_once('/')?;
        let unit = match words.next()? {
            "bytes" => Unit::Bytes,
            "steps" => Unit::Steps,
            _ => return None,
        };
        Some(Progress {
            done: done.parse().ok()?,
            total: total.parse().ok()?,
            unit,
            label: words.next().unwrap_or("").to_string(),
        })
    }

    /// Done as a share of the total, in 0..=1. A total of zero is nothing
    /// done yet, not a division.
    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f64 / self.total as f64).min(1.0)
        }
    }

    /// The whole percent done, rounded down: a bar never says 100 before
    /// the step is.
    pub fn percent(&self) -> u32 {
        (self.fraction() * 100.0).floor() as u32
    }

    /// The amount, as a person reads it: `8.2 of 20.1 GB`, `6 of 10`.
    pub fn amount(&self) -> String {
        match self.unit {
            Unit::Steps => format!("{} of {}", self.done, self.total),
            Unit::Bytes if self.total >= 1 << 30 => {
                format!("{:.1} of {:.1} GB", self.done as f64 / GIB, self.total as f64 / GIB)
            }
            Unit::Bytes => format!("{} of {} MB", self.done >> 20, self.total >> 20),
        }
    }

    /// `width` cells: the done part and the rest, for a screen that colours
    /// them apart.
    pub fn cells(&self, width: usize) -> (usize, usize) {
        let filled = ((self.fraction() * width as f64).floor() as usize).min(width);
        (filled, width - filled)
    }

    /// One line of plain text for a terminal: the label, a `width`-cell bar,
    /// the percent and the amount.
    pub fn text(&self, width: usize) -> String {
        let (filled, rest) = self.cells(width);
        format!(
            "  {}  {}{} {:>3}%  {}",
            self.label,
            "\u{2501}".repeat(filled),
            "\u{2500}".repeat(rest),
            self.percent(),
            self.amount()
        )
    }

    /// The JSON object the console's state carries.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"done\": {}, \"total\": {}, \"percent\": {}, \"amount\": {}, \"label\": {}}}",
            self.done,
            self.total,
            self.percent(),
            json::quote(&self.amount()),
            json::quote(&self.label)
        )
    }
}

const GIB: f64 = 1_073_741_824.0;

/// Whether `line` is a progress line or the clear: the lines a log reader
/// leaves out of the text it shows.
pub fn is_line(line: &str) -> bool {
    let line = line.trim_end();
    line == CLEAR || line.starts_with(PREFIX)
}

/// The progress the newest progress line in `text` states, or `None` when
/// there is none or the newest is the clear.
pub fn latest(text: &str) -> Option<Progress> {
    text.lines().rev().find(|line| is_line(line)).and_then(Progress::parse)
}

/// `text` without its progress lines.
pub fn strip(text: &str) -> String {
    text.lines().filter(|line| !is_line(line)).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_reads_back_as_what_it_said() {
        let pull = Progress::bytes(4 << 20, 20 << 30, "pull: a file with spaces.gguf");
        assert_eq!(pull.line(), "PROGRESS 4194304/21474836480 bytes pull: a file with spaces.gguf");
        assert_eq!(Progress::parse(&pull.line()), Some(pull));
        let tune = Progress::steps(6, 10, "tune: context");
        assert_eq!(Progress::parse(&format!("{}\r", tune.line())), Some(tune), "a CRLF log");
    }

    #[test]
    fn anything_else_is_not_progress() {
        assert_eq!(Progress::parse(CLEAR), None);
        assert_eq!(Progress::parse("[ OK ] PROGRESS 1/2 steps"), None);
        assert_eq!(Progress::parse("PROGRESS 1/2 furlongs x"), None);
        assert_eq!(Progress::parse("PROGRESS a/2 steps x"), None);
        assert_eq!(Progress::parse("    PROGRESS 1/2 steps x"), None, "an indented copy is text");
    }

    #[test]
    fn the_newest_line_wins_and_the_clear_ends_it() {
        let log = "a\nPROGRESS 1/4 steps eval: one\nb\nPROGRESS 2/4 steps eval: two\nc";
        assert_eq!(latest(log), Some(Progress::steps(2, 4, "eval: two")));
        assert_eq!(latest(&format!("{log}\n{CLEAR}\nnext job")), None);
        assert_eq!(latest("no progress at all"), None);
        assert_eq!(strip(&format!("{log}\n{CLEAR}")), "a\nb\nc");
    }

    #[test]
    fn the_numbers_a_person_reads() {
        let gb = Progress::bytes(8_800_000_000, 21_600_000_000, "pull");
        assert_eq!(gb.amount(), "8.2 of 20.1 GB");
        assert_eq!(gb.percent(), 40);
        assert_eq!(Progress::bytes(300 << 20, 900 << 20, "pull").amount(), "300 of 900 MB");
        assert_eq!(Progress::steps(6, 10, "tune").amount(), "6 of 10");
        // Never over the end, never a division by zero, never 100 early.
        assert_eq!(Progress::steps(12, 10, "x").fraction(), 1.0);
        assert_eq!(Progress::steps(3, 0, "x").fraction(), 0.0);
        assert_eq!(Progress::steps(999, 1000, "x").percent(), 99);
    }

    #[test]
    fn the_bar_splits_its_width() {
        assert_eq!(Progress::steps(1, 4, "x").cells(20), (5, 15));
        assert_eq!(Progress::steps(9, 4, "x").cells(20), (20, 0));
        assert_eq!(Progress::steps(0, 0, "x").cells(20), (0, 20));
        assert_eq!(
            Progress::steps(1, 4, "tune: refine").text(8),
            "  tune: refine  \u{2501}\u{2501}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}  25%  1 of 4"
        );
    }

    #[test]
    fn the_console_gets_the_amount_already_worded() {
        assert_eq!(
            Progress::steps(3, 5, "eval: \"tools\"").to_json(),
            "{\"done\": 3, \"total\": 5, \"percent\": 60, \"amount\": \"3 of 5\", \"label\": \"eval: \\\"tools\\\"\"}"
        );
    }
}
