//! The two date stamps the tools leave behind.
//!
//! No calendar dependency: one file wants "today" for the comment it writes
//! above a preset, another wants an ISO timestamp for an overlay header, and
//! both are arithmetic on the seconds since the epoch, in UTC - so a stamp
//! made just after local midnight can carry the previous day. A comment is
//! not worth a timezone database; being one day out in one is.

use std::time::{SystemTime, UNIX_EPOCH};

/// UTC `YYYY-MM-DD`.
pub fn today() -> String {
    let (year, month, day) = civil_from_days(days_since_epoch());
    format!("{year:04}-{month:02}-{day:02}")
}

/// UTC `YYYY-MM-DDTHH:MMZ`, the stamp the tuner puts on an overlay.
pub fn now_iso() -> String {
    let secs = secs_since_epoch();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}Z", rem / 3600, (rem % 3600) / 60)
}

fn secs_since_epoch() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn days_since_epoch() -> i64 {
    (secs_since_epoch() / 86_400) as i64
}

/// Days since 1970-01-01 to a calendar date.
///
/// Howard Hinnant's `civil_from_days`: shift the epoch to 0000-03-01 so the
/// leap day lands at the end of the year, then the era (400 years), the year
/// of the era and the day of the year are each one division.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_calendar_survives_its_awkward_cases() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29), "a leap day");
        assert_eq!(civil_from_days(20_652), (2026, 7, 18));
        // A century that is a leap year, and one that is not.
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn the_stamps_are_the_shapes_the_files_want() {
        let today = today();
        assert_eq!(today.len(), 10, "{today}");
        assert_eq!(today.matches('-').count(), 2, "{today}");
        assert!(today.starts_with("20"), "{today}");
        let iso = now_iso();
        assert!(iso.ends_with('Z'), "{iso}");
        assert_eq!(&iso[..10], today, "the two agree about the date: {iso}");
    }
}
