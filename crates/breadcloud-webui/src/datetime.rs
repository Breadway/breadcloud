//! Small, dependency-free civil-calendar math for the file browser's
//! "modified" column and the calendar month view's grid layout.
//!
//! `breadcloud-webui` is meant to stay "genuinely lightweight" per the
//! CLOUD-12 issue -- pulling in a full date/time crate (`chrono`, `time`)
//! for "which weekday does the 1st fall on" and "format this mtime as
//! `YYYY-MM-DD HH:MM`" would be a lot of dependency for two small integer
//! computations, and nothing else in this workspace depends on one either
//! (see docs/DESIGN.md). This uses Howard Hinnant's `days_from_civil`
//! / `civil_from_days` algorithm (public domain, proleptic Gregorian,
//! correct for any year this server will ever see a file mtime or a
//! calendar event in): <http://howardhinnant.github.io/date_algorithms.html>.

/// Days since the Unix epoch (1970-01-01) for a given proleptic Gregorian
/// civil date. `month` is 1-12, `day` is 1-31.
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (month as i64 + 9) % 12; // [0, 11], Mar=0 .. Feb=11
    let doy = (153 * mp + 2) / 5 + day as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// The inverse of [`days_from_civil`]: the proleptic Gregorian civil date
/// (year, month, day) `z` days after the Unix epoch.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

/// Day of week for a civil date: `0` = Sunday .. `6` = Saturday.
pub fn day_of_week(year: i64, month: u32, day: u32) -> u32 {
    // 1970-01-01 (day 0) was a Thursday (weekday index 4).
    ((days_from_civil(year, month, day) + 4).rem_euclid(7)) as u32
}

pub fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Number of days in `month` (1-12) of `year`.
pub fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// Break a `SystemTime` down into civil date/time fields (UTC), for
/// rendering a file's mtime in the file browser. Times before the Unix
/// epoch (should not happen for a real file, but `SystemTime` does not
/// rule it out) clamp to the epoch rather than panicking.
pub fn civil_from_system_time(t: std::time::SystemTime) -> (i64, u32, u32, u32, u32, u32) {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let time_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = (time_of_day / 3600) as u32;
    let minute = ((time_of_day % 3600) / 60) as u32;
    let second = (time_of_day % 60) as u32;
    (year, month, day, hour, minute, second)
}

/// `YYYY-MM-DD HH:MM` rendering of a `SystemTime`, UTC.
pub fn format_mtime(t: std::time::SystemTime) -> String {
    let (year, month, day, hour, minute, _second) = civil_from_system_time(t);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// The current UTC (year, month) -- what the calendar month view defaults
/// to when the request has no `?year=&month=` query.
pub fn current_year_month() -> (i64, u32) {
    let (year, month, ..) = civil_from_system_time(std::time::SystemTime::now());
    (year, month)
}

pub const WEEKDAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
pub const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_epoch_roundtrips() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn known_date_roundtrips() {
        for (y, m, d) in [
            (2026, 1, 1),
            (2026, 2, 28),
            (2024, 2, 29), // leap day
            (2000, 2, 29), // divisible by 400: leap
            (1900, 2, 28), // divisible by 100 not 400: not leap
            (2026, 12, 31),
            (1969, 12, 31), // just before the epoch
        ] {
            let days = days_from_civil(y, m, d);
            assert_eq!(civil_from_days(days), (y, m, d), "for {y}-{m}-{d}");
        }
    }

    #[test]
    fn leap_year_detection() {
        assert!(is_leap_year(2024));
        assert!(is_leap_year(2000));
        assert!(!is_leap_year(1900));
        assert!(!is_leap_year(2026));
    }

    #[test]
    fn days_in_month_matches_leap_rules() {
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2026, 4), 30);
        assert_eq!(days_in_month(2026, 1), 31);
    }

    // 2026-01-01 is a real, checkable Thursday.
    #[test]
    fn day_of_week_matches_a_known_date() {
        assert_eq!(day_of_week(2026, 1, 1), 4); // Thursday
        assert_eq!(day_of_week(1970, 1, 1), 4); // also a Thursday
        assert_eq!(day_of_week(2026, 1, 4), 0); // the following Sunday
    }

    #[test]
    fn format_mtime_renders_utc_civil_time() {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_768_000_000);
        // 1768000000 unix seconds = 2026-01-09 23:06:40 UTC.
        assert_eq!(format_mtime(t), "2026-01-09 23:06");
    }
}
