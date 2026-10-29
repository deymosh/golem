//! The job log's grammar: what marks one job, and what stamps a line.
//!
//! `logs/web-job.log` keeps every job there has been, appended one after
//! another. A job opens with a `===` header and closes with a `===` footer,
//! so a reader can show just the newest one, and every ordinary line carries
//! the local clock in front so the pane reads as a record. A `PROGRESS` line
//! is never stamped: the bar readers parse those lines exactly.

use crate::progress;

/// A job's first line: what ran, with which argument, on which day -
/// `=== tune ornith-1.5-9b · 2026-10-28 ===`.
pub fn header(action: &str, arg: &str, date: &str) -> String {
    match arg {
        "" => format!("=== {action} · {date} ==="),
        other => format!("=== {action} {other} · {date} ==="),
    }
}

/// A job's last line: the verdict and how long it took - `=== tune: done in
/// 2m 13s ===`, or `=== tune: failed (exit 1) after 5s ===`.
pub fn footer(action: &str, code: i32, secs: u64) -> String {
    let how_long = elapsed(secs);
    if code == 0 {
        format!("=== {action}: done in {how_long} ===")
    } else {
        format!("=== {action}: failed (exit {code}) after {how_long} ===")
    }
}

/// A duration as a person reads it: `5s`, `2m 13s`, `1h 2m 3s`.
fn elapsed(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h {}m {}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Whether `line` opens a job - the header, stamped or not.
///
/// A footer opens with `=== ` too, so the day at the end is what tells a
/// header from one.
pub fn is_header(line: &str) -> bool {
    let line = unstamped(line.trim_end());
    let Some(inner) = line.strip_prefix("=== ").and_then(|rest| rest.strip_suffix(" ===")) else {
        return false;
    };
    inner.rsplit_once(" · ").is_some_and(|(_, date)| is_date(date))
}

/// From the last header to the end: the lines of the newest job. A log with
/// no header is all one job, so the whole text is.
pub fn last_job(text: &str) -> &str {
    let mut start = 0;
    let mut offset = 0;
    for line in text.split('\n') {
        if is_header(line) {
            start = offset;
        }
        offset += line.len() + 1;
    }
    &text[start..]
}

/// The line as the log carries it: the local clock in front, except for a
/// `PROGRESS` line (the bar readers parse those exactly) and an empty one.
pub fn stamp(clock: &str, line: &str) -> String {
    if line.is_empty() || progress::is_line(line) {
        line.to_string()
    } else {
        format!("{clock} {line}")
    }
}

/// Whether `s` is the `YYYY-MM-DD` a header ends with.
fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    (0..10).filter(|i| *i != 4 && *i != 7).all(|i| b[i].is_ascii_digit())
}

/// Whether `b` is `HH:MM:SS` - two digits, a colon, twice.
fn is_clock(b: &[u8]) -> bool {
    b.len() == 8
        && b[2] == b':'
        && b[5] == b':'
        && (0..8).filter(|i| *i != 2 && *i != 5).all(|i| b[i].is_ascii_digit())
}

/// `line` with a leading `HH:MM:SS ` stamp taken off, if it carries one.
fn unstamped(line: &str) -> &str {
    let b = line.as_bytes();
    if b.len() > 9 && is_clock(&b[..8]) && b[8] == b' ' {
        &line[9..]
    } else {
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_names_the_action_and_the_day() {
        assert_eq!(
            header("tune", "ornith-1.5-9b", "2026-10-28"),
            "=== tune ornith-1.5-9b · 2026-10-28 ==="
        );
        // No argument: the verb alone.
        assert_eq!(header("bench", "", "2026-10-28"), "=== bench · 2026-10-28 ===");
    }

    #[test]
    fn a_footer_says_how_the_job_ended_and_how_long_it_took() {
        assert_eq!(footer("tune", 0, 133), "=== tune: done in 2m 13s ===");
        assert_eq!(footer("tune", 1, 5), "=== tune: failed (exit 1) after 5s ===");
        assert_eq!(footer("eval", 0, 3_723), "=== eval: done in 1h 2m 3s ===");
        assert_eq!(footer("bench", 0, 59), "=== bench: done in 59s ===");
    }

    #[test]
    fn a_header_is_known_stamped_and_unstamped_and_a_footer_is_not_one() {
        let header_line = header("tune", "ornith-1.5-9b", "2026-10-28");
        assert!(is_header(&header_line));
        assert!(is_header(&stamp("14:05:09", &header_line)), "the stamped one");
        assert!(is_header(&format!("14:05:09 {header_line}\r")), "a CRLF log");
        assert!(!is_header(&footer("tune", 0, 133)), "a footer is not a header");
        assert!(!is_header("=== no day here ==="));
        assert!(!is_header("[ OK ] done === maybe ==="));
    }

    #[test]
    fn the_last_job_is_from_its_header_to_the_end() {
        let log = "=== bench · 2026-10-27 ===\nold line\n\
                   === tune ornith · 2026-10-28 ===\n14:05:09 working\n=== tune: done in 5s ===\n";
        assert_eq!(
            last_job(log),
            "=== tune ornith · 2026-10-28 ===\n14:05:09 working\n=== tune: done in 5s ===\n"
        );
        // No header at all: everything is one job.
        assert_eq!(last_job("a\nb\n"), "a\nb\n");
        assert_eq!(last_job(""), "");
    }

    #[test]
    fn a_stamp_sits_in_front_of_an_ordinary_line_and_no_other() {
        assert_eq!(stamp("07:00:00", "[ OK ] loaded"), "07:00:00 [ OK ] loaded");
        // The bar readers parse a PROGRESS line exactly, and an empty line
        // is not worth stamping.
        assert_eq!(stamp("07:00:00", "PROGRESS 1/2 steps eval: one"), "PROGRESS 1/2 steps eval: one");
        assert_eq!(stamp("07:00:00", "PROGRESS -"), "PROGRESS -");
        assert_eq!(stamp("07:00:00", ""), "");
    }
}
