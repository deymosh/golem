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
    local_date(secs_since_epoch(), 0)
}

/// UTC `YYYY-MM-DDTHH:MMZ`, the stamp the tuner puts on an overlay.
pub fn now_iso() -> String {
    iso(secs_since_epoch())
}

/// The `YYYY-MM-DDTHH:MMZ` stamp for seconds since the epoch.
fn iso(secs: u64) -> String {
    let rem = secs % 86_400;
    format!("{}T{:02}:{:02}Z", local_date(secs, 0), rem / 3600, (rem % 3600) / 60)
}

fn secs_since_epoch() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The local `HH:MM:SS` for seconds since the epoch, `offset_min` minutes
/// east of UTC. Past midnight the clock wraps and [`local_date`] carries the
/// day.
pub fn clock(secs_utc: u64, offset_min: i32) -> String {
    let secs = secs_utc as i64 + offset_min as i64 * 60;
    let rem = secs.rem_euclid(86_400) as u64;
    format!("{:02}:{:02}:{:02}", rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// The local `YYYY-MM-DD` for seconds since the epoch, `offset_min` minutes
/// east of UTC - the day a job header names.
pub fn local_date(secs_utc: u64, offset_min: i32) -> String {
    let secs = secs_utc as i64 + offset_min as i64 * 60;
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

/// The minutes east of UTC a `date +%z` line states: `+0100` is 60, `-0530`
/// is -330. `None` for anything else.
pub fn offset_minutes(zone: &str) -> Option<i32> {
    let (sign, rest) = match zone.as_bytes().first()? {
        b'+' => (1, &zone[1..]),
        b'-' => (-1, &zone[1..]),
        _ => return None,
    };
    if rest.len() != 4 || !rest.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hours: i32 = rest[..2].parse().ok()?;
    let mins: i32 = rest[2..].parse().ok()?;
    Some(sign * (hours * 60 + mins))
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
    fn the_overlay_stamp_is_the_utc_minute() {
        // 2026-10-28 23:30:59 UTC: the seconds are dropped, not rounded.
        let secs = 20_754 * 86_400 + 23 * 3_600 + 1_859;
        assert_eq!(iso(secs), "2026-10-28T23:30Z");
        assert_eq!(iso(0), "1970-01-01T00:00Z");
    }

    #[test]
    fn the_clock_and_the_local_date_follow_the_offset() {
        // 2026-10-28 23:30:00 UTC.
        let secs = 20_754 * 86_400 + 23 * 3_600 + 1_800;
        assert_eq!(clock(secs, 0), "23:30:00");
        assert_eq!(local_date(secs, 0), "2026-10-28");
        // Half an hour east is past midnight: the clock wraps and the date
        // moves with it.
        assert_eq!(clock(secs, 30), "00:00:00");
        assert_eq!(local_date(secs, 30), "2026-10-29");
        // Five hours west is still the same day, earlier.
        assert_eq!(clock(secs, -300), "18:30:00");
        assert_eq!(local_date(secs, -300), "2026-10-28");
    }

    #[test]
    fn the_zone_line_is_minutes_east() {
        assert_eq!(offset_minutes("+0000"), Some(0));
        assert_eq!(offset_minutes("+0100"), Some(60));
        assert_eq!(offset_minutes("-0530"), Some(-330));
        assert_eq!(offset_minutes("+0545"), Some(345));
        assert_eq!(offset_minutes("CET"), None);
        assert_eq!(offset_minutes("+010"), None);
        assert_eq!(offset_minutes(""), None);
    }
}
