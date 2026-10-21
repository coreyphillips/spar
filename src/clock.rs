//! Dates and times without a date crate.
//!
//! spar needs a moment in four shapes: a file stamp that sorts, a header line
//! a person reads, the RFC 3339 form GitHub returns so a recorded time compares
//! with a comment's `created_at` as a string, and the HTTP date form a
//! conditional request sends back. The days to civil arithmetic is Howard
//! Hinnant's, valid for any date this will ever see.

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `yyyymmdd-hhmmss` in UTC, for a file name that sorts by when it was made.
pub fn file_stamp(secs: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil(secs);
    format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}")
}

/// `2026-09-12 14:03 UTC`, for the header a person reads.
pub fn date_line(secs: u64) -> String {
    let (y, m, d, hh, mm, _) = civil(secs);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02} UTC")
}

/// `2026-10-20T18:04:05Z`, the shape GitHub returns, so a time spar recorded
/// and a time GitHub reports compare as strings.
pub fn rfc3339(secs: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil(secs);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// `2026-10-20T18:04:05Z` back to seconds. A fractional second is accepted and
/// dropped; anything else that is not that shape, or is before 1970, is None.
pub fn parse_rfc3339(text: &str) -> Option<u64> {
    let text = text.trim().strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let time = time.split_once('.').map_or(time, |(whole, _)| whole);
    let mut ymd = date.split('-');
    let y: i64 = ymd.next()?.parse().ok()?;
    let m: u32 = ymd.next()?.parse().ok()?;
    let d: u32 = ymd.next()?.parse().ok()?;
    if ymd.next().is_some() {
        return None;
    }
    let mut hms = time.split(':');
    let hh: u64 = hms.next()?.parse().ok()?;
    let mm: u64 = hms.next()?.parse().ok()?;
    let ss: u64 = hms.next()?.parse().ok()?;
    if hms.next().is_some() {
        return None;
    }
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 59 {
        return None;
    }
    let days = days_from_civil(y, m, d);
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// `Tue, 20 Oct 2026 10:00:00 GMT`, the shape a `Last-Modified` header takes
/// and an `If-Modified-Since` header sends back.
pub fn http_date(secs: u64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, m, d, hh, mm, ss) = civil(secs);
    // The epoch fell on a Thursday.
    let weekday = DAYS[((secs / 86_400 + 4) % 7) as usize];
    let month = MONTHS[(m - 1) as usize];
    format!("{weekday}, {d:02} {month} {y:04} {hh:02}:{mm:02}:{ss:02} GMT")
}

/// Seconds since the epoch to a civil date and time.
pub(crate) fn civil(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// A civil date to days since the epoch, the inverse of `civil`.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + u64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_utc_and_sort_by_time() {
        let secs = 1_789_221_787;
        assert_eq!("20260912-140307", file_stamp(secs));
        assert_eq!("2026-09-12 14:03 UTC", date_line(secs));
        assert_eq!("19700101-000000", file_stamp(0));
        // The day after a leap day.
        assert_eq!("20240301-000000", file_stamp(1_709_251_200));
    }

    #[test]
    fn rfc3339_formats_the_epoch_as_github_does() {
        assert_eq!("1970-01-01T00:00:00Z", rfc3339(0));
        assert_eq!("2026-09-12T14:03:07Z", rfc3339(1_789_221_787));
        assert_eq!("2026-10-20T10:00:00Z", rfc3339(1_792_490_400));
    }

    #[test]
    fn unix_seconds_round_trip_through_github_timestamps() {
        for secs in [
            0,
            951_825_600,   // a leap day
            1_709_251_200, // the day after one
            1_789_221_787,
            4_102_444_799, // the last second of 2099
        ] {
            assert_eq!(Some(secs), parse_rfc3339(&rfc3339(secs)), "{secs}");
        }
        assert_eq!(Some(1_792_490_400), parse_rfc3339("2026-10-20T10:00:00Z"));
        assert_eq!(
            Some(1_792_490_400),
            parse_rfc3339("2026-10-20T10:00:00.123Z")
        );
        assert_eq!(
            Some(1_792_490_400),
            parse_rfc3339(" 2026-10-20T10:00:00Z\n")
        );
    }

    #[test]
    fn a_timestamp_that_is_not_one_is_none() {
        for junk in [
            "",
            "junk",
            "2026-10-20",
            "2026-10-20 10:00:00",
            "2026-10-20T10:00:00",
            "2026-13-01T00:00:00Z",
            "2026-10-20T24:00:00Z",
            "1969-12-31T23:59:59Z",
            "2026-10-20T10:00:00Z extra",
        ] {
            assert_eq!(None, parse_rfc3339(junk), "{junk:?}");
        }
    }

    #[test]
    fn an_http_date_reads_like_the_one_github_sends() {
        assert_eq!("Tue, 20 Oct 2026 10:00:00 GMT", http_date(1_792_490_400));
        assert_eq!("Thu, 01 Jan 1970 00:00:00 GMT", http_date(0));
        assert_eq!("Tue, 29 Feb 2000 12:00:00 GMT", http_date(951_825_600));
    }
}
