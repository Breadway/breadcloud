//! Minimal iCalendar (RFC 5545) reading, just deep enough for what the
//! CalDAV surface needs: pulling a `VEVENT`/`VTODO`'s `UID` out of a PUT
//! body, and a best-effort `DTSTART`/`DTEND` time-range check for
//! `calendar-query` filtering (RFC 4791 9.9). This is deliberately not a
//! general iCalendar parser/writer -- events are stored and served as
//! opaque bytes (see `breadcloud_storage::CalendarStore`); this module
//! only ever reads just enough of that byte content to answer "what UID
//! is this" and "does this event's time range overlap the one the client
//! asked for".
//!
//! **Scope decisions, both documented in the workspace README too:**
//!
//! - **Timezones are compared as wall-clock digits, not resolved against
//!   a real tz database.** `DTSTART;TZID=Australia/Sydney:20260115T090000`
//!   and a UTC `time-range` bound are compared by their digit strings
//!   after stripping the `TZID` parameter, not by actually resolving
//!   `Australia/Sydney` and adjusting for offset/DST. For a personal
//!   deployment where every real calendar's events and every real
//!   client's query windows are in the same one or two timezones, this is
//!   accurate enough in practice and avoids pulling in a full tz database
//!   dependency for a pre-filter that exists to bound "give me roughly
//!   this window", not to be the final word on inclusion -- real clients
//!   (DAVx5, iOS) always re-filter and re-render what the server sends
//!   them against their own tz-aware calendar logic anyway, so a
//!   pre-filter that is occasionally a few hours too wide costs nothing
//!   (a client just sees one extra event outside its exact window, still
//!   correctly rendered by the client itself); a pre-filter that is too
//!   narrow would actually lose data, which the "always include on
//!   ambiguity" rule below exists to prevent.
//! - **Recurring events (`RRULE` present) always pass a time-range
//!   filter**, rather than this module expanding the recurrence rule to
//!   find actual occurrence instances. RFC 5545 recurrence expansion
//!   (`FREQ`/`INTERVAL`/`BYDAY`/`COUNT`/`UNTIL`/exceptions) is
//!   substantially more machinery than this issue's ask (calendar
//!   collections, multi-calendar, sync-collection/ctag), and getting it
//!   subtly wrong would silently drop a recurring event from a client's
//!   view of a queried window -- a real correctness bug a user would not
//!   notice until a meeting they expected to see was missing. Always
//!   including it instead means `calendar-query` with a time-range on a
//!   recurring event is a superset filter (a client may see the master
//!   instance's data even for a query window it does not actually recur
//!   into), never a subset that silently loses data. Real clients treat
//!   `calendar-query`'s time-range as a hint, not a hard contract, for
//!   exactly this reason (RFC 4791 9.9 explicitly allows a server to
//!   return a superset), and both DAVx5 and iOS already do their own
//!   recurrence expansion client-side for display regardless of what the
//!   server pre-filtered.

/// Unfold RFC 5545 folded lines (a CRLF or LF immediately followed by a
/// single space or tab continues the previous line) and normalize line
/// endings, so the rest of this module can work line-by-line without
/// worrying about a property value having been wrapped mid-word.
fn unfold(ics: &str) -> String {
    let normalized = ics.replace("\r\n", "\n");
    let mut out = String::with_capacity(normalized.len());
    for line in normalized.split('\n') {
        if (line.starts_with(' ') || line.starts_with('\t')) && !out.is_empty() {
            out.push_str(&line[1..]);
        } else {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(line);
        }
    }
    out
}

/// One unfolded content line split into its property name (including any
/// `;PARAM=...` suffixes, e.g. `DTSTART;TZID=Australia/Sydney`) and
/// value.
fn split_line(line: &str) -> Option<(&str, &str)> {
    line.split_once(':')
}

fn property_name(name_and_params: &str) -> &str {
    name_and_params
        .split_once(';')
        .map(|(name, _)| name)
        .unwrap_or(name_and_params)
}

/// Extract the `UID` of the first `VEVENT`/`VTODO`/`VJOURNAL` component in
/// an iCalendar object. `None` if there is no `UID` line at all, which
/// callers treat as an invalid calendar object (RFC 5545 requires `UID`
/// on every one of these component types).
pub fn extract_uid(ics: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(ics);
    let unfolded = unfold(&text);
    for line in unfolded.lines() {
        if let Some((name, value)) = split_line(line) {
            if property_name(name).eq_ignore_ascii_case("UID") {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// Extract the `SUMMARY` of the first component in an iCalendar object.
/// `None` if there is no `SUMMARY` line (RFC 5545 does not require one,
/// unlike `UID`).
pub fn extract_summary(ics: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(ics);
    let unfolded = unfold(&text);
    for line in unfolded.lines() {
        if let Some((name, value)) = split_line(line) {
            if property_name(name).eq_ignore_ascii_case("SUMMARY") {
                let value = value.trim();
                if value.is_empty() {
                    return None;
                }
                return Some(value.to_string());
            }
        }
    }
    None
}

/// A minimal event summary extracted for display purposes (`breadcloud-webui`'s
/// calendar month view, CLOUD-12): just enough to place an event on a
/// specific day and show a short label, without this module becoming a
/// general iCalendar library. Reuses exactly the same "wall-clock digits,
/// not tz-aware" `DTSTART` reading [`time_range_overlaps`] already uses
/// (see this module's doc comment for why), just exposed as calendar
/// fields instead of folded into a range comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventSummary {
    pub uid: String,
    /// `SUMMARY`, or a placeholder when the event has none.
    pub summary: String,
    pub year: u32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    /// `true` for a `DTSTART;VALUE=DATE` (all-day) event.
    pub all_day: bool,
}

/// Pull a display summary out of `ics`: `None` if there is no `UID` or no
/// parseable `DTSTART` (the same "cannot confidently place this in time"
/// cases [`time_range_overlaps`] treats as "always include" for a filter,
/// but for a calendar view there is no day to place an unparseable event
/// on, so it is left out instead).
pub fn summarize(ics: &[u8]) -> Option<EventSummary> {
    let text = String::from_utf8_lossy(ics);
    let unfolded = unfold(&text);
    let component = first_component_properties(&unfolded, &["VEVENT"])?;
    let mut uid = None;
    let mut summary = None;
    let mut dtstart_raw: Option<String> = None;
    let mut is_date_only = false;
    for line in component {
        let Some((name, value)) = split_line(line) else {
            continue;
        };
        match property_name(name).to_ascii_uppercase().as_str() {
            "UID" => uid = Some(value.trim().to_string()),
            "SUMMARY" => {
                let value = value.trim();
                if !value.is_empty() {
                    summary = Some(value.to_string());
                }
            }
            "DTSTART" => {
                dtstart_raw = Some(value.trim().to_string());
                is_date_only = name.to_ascii_uppercase().contains("VALUE=DATE")
                    && !name.to_ascii_uppercase().contains("VALUE=DATE-TIME");
            }
            _ => {}
        }
    }
    let uid = uid?;
    let summary = summary.unwrap_or_else(|| "(untitled)".to_string());
    let dtstart_raw = dtstart_raw?;
    let digits: String = dtstart_raw
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(14)
        .collect();
    if digits.len() < 8 {
        return None;
    }
    let year: u32 = digits[0..4].parse().ok()?;
    let month: u32 = digits[4..6].parse().ok()?;
    let day: u32 = digits[6..8].parse().ok()?;
    let (hour, minute) = if digits.len() >= 12 {
        (
            digits[8..10].parse().unwrap_or(0),
            digits[10..12].parse().unwrap_or(0),
        )
    } else {
        (0, 0)
    };

    Some(EventSummary {
        uid,
        summary,
        year,
        month,
        day,
        hour,
        minute,
        all_day: is_date_only,
    })
}

/// Whether the object contains a component of the given type (compared
/// case-insensitively), e.g. `has_component(ics, "VEVENT")`.
pub fn has_component(ics: &[u8], component: &str) -> bool {
    let text = String::from_utf8_lossy(ics);
    let unfolded = unfold(&text);
    unfolded.lines().any(|line| {
        line.split_once(':')
            .map(|(name, value)| {
                property_name(name).eq_ignore_ascii_case("BEGIN")
                    && value.trim().eq_ignore_ascii_case(component)
            })
            .unwrap_or(false)
    })
}

/// Convert an iCalendar `DATE` (`YYYYMMDD`) or `DATE-TIME`
/// (`YYYYMMDDTHHMMSS[Z]`) value to seconds from the Unix epoch, ignoring
/// any `TZID` parameter (see the module doc comment on why). A bare date
/// is treated as midnight at the start of that day.
fn normalize_datetime(raw: &str) -> Option<i64> {
    let digits: String = raw
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(14)
        .collect();
    if digits.len() < 8 {
        return None;
    }
    let mut padded = digits;
    while padded.len() < 14 {
        padded.push('0');
    }
    let year = padded[0..4].parse::<i64>().ok()?;
    let month = padded[4..6].parse::<i64>().ok()?;
    let day = padded[6..8].parse::<i64>().ok()?;
    let hour = padded[8..10].parse::<i64>().ok()?;
    let minute = padded[10..12].parse::<i64>().ok()?;
    let second = padded[12..14].parse::<i64>().ok()?;

    if !(1..=12).contains(&month) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        2 if leap_year => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day < 1 || day > days_in_month {
        return None;
    }

    // Gregorian civil date to days since 1970-01-01. Keeping this local
    // avoids adding a date library for a best-effort query pre-filter.
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year / 400
    } else {
        (adjusted_year - 399) / 400
    };
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Properties at the top level of the first matching component. Nested
/// components such as VALARM are excluded, as are sibling components such
/// as VTIMEZONE.
fn first_component_properties<'a>(unfolded: &'a str, wanted: &[&str]) -> Option<Vec<&'a str>> {
    let mut stack = Vec::<String>::new();
    let mut target_depth = None;
    let mut properties = Vec::new();

    for line in unfolded.lines() {
        if let Some((name, value)) = split_line(line) {
            let property = property_name(name);
            if property.eq_ignore_ascii_case("BEGIN") {
                stack.push(value.trim().to_ascii_uppercase());
                if target_depth.is_none()
                    && stack.len() == 2
                    && wanted
                        .iter()
                        .any(|kind| stack[1].eq_ignore_ascii_case(kind))
                {
                    target_depth = Some(stack.len());
                }
                continue;
            }
            if property.eq_ignore_ascii_case("END") {
                let depth = stack.len();
                let closes_target = target_depth == Some(depth)
                    && stack
                        .last()
                        .is_some_and(|component| component.eq_ignore_ascii_case(value.trim()));
                stack.pop();
                if closes_target {
                    return Some(properties);
                }
                continue;
            }
            if target_depth == Some(stack.len()) {
                properties.push(line);
            }
        }
    }
    None
}

/// Best-effort parse of an ISO 8601 duration as used in iCalendar
/// `DURATION` (`PnDTnHnMnS`), returned as a count of seconds. Supports
/// the day/hour/minute/second fields real calendar exports actually use;
/// weeks (`PnW`) and the date/time-only edge cases RFC 5545 technically
/// allows are not needed here since `DURATION` is only ever used as a
/// fallback when `DTEND` is absent, and real client exports (Google,
/// Apple, DAVx5 test events) always send an explicit `DTEND` instead.
fn parse_duration_seconds(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    let raw = raw.strip_prefix('+').unwrap_or(raw);
    let (negative, raw) = match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    let raw = raw.strip_prefix('P')?;
    let (date_part, time_part) = raw.split_once('T').unwrap_or((raw, ""));

    let mut seconds: i64 = 0;
    let mut num = String::new();
    for c in date_part.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else if c == 'D' {
            seconds += num.parse::<i64>().unwrap_or(0) * 86_400;
            num.clear();
        } else {
            num.clear();
        }
    }
    num.clear();
    for c in time_part.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else if c == 'H' {
            seconds += num.parse::<i64>().unwrap_or(0) * 3_600;
            num.clear();
        } else if c == 'M' {
            seconds += num.parse::<i64>().unwrap_or(0) * 60;
            num.clear();
        } else if c == 'S' {
            seconds += num.parse::<i64>().unwrap_or(0);
            num.clear();
        } else {
            num.clear();
        }
    }
    Some(if negative { -seconds } else { seconds })
}

/// Whether the first VEVENT/VTODO in `ics` overlaps `[start, end)` (RFC
/// 4791 9.9's half-open interval: an event starting exactly at `end`, or
/// ending exactly at `start`, does not overlap). `start`/`end` are the
/// raw iCalendar-format bounds from the `calendar-query` REPORT's
/// `time-range` element (`None` means unbounded on that side).
///
/// Always returns `true` (never filters out) when:
/// - the object has no parseable `DTSTART` at all (safer to show an
///   object this module can't confidently place in time than to hide it);
/// - the object has an `RRULE` (see the module doc comment).
pub fn time_range_overlaps(ics: &[u8], start: Option<&str>, end: Option<&str>) -> bool {
    if start.is_none() && end.is_none() {
        return true;
    }

    let text = String::from_utf8_lossy(ics);
    let unfolded = unfold(&text);

    let Some(component) = first_component_properties(&unfolded, &["VEVENT", "VTODO"]) else {
        return true;
    };
    if component.iter().any(|line| {
        line.split_once(':')
            .is_some_and(|(n, _)| property_name(n).eq_ignore_ascii_case("RRULE"))
    }) {
        return true;
    }

    let mut dtstart_raw: Option<String> = None;
    let mut dtend_raw: Option<String> = None;
    let mut duration_raw: Option<String> = None;
    let mut is_date_only = false;

    for line in component {
        let Some((name, value)) = split_line(line) else {
            continue;
        };
        let prop = property_name(name);
        if prop.eq_ignore_ascii_case("DTSTART") {
            dtstart_raw = Some(value.trim().to_string());
            is_date_only = name.to_ascii_uppercase().contains("VALUE=DATE")
                && !name.to_ascii_uppercase().contains("VALUE=DATE-TIME");
        } else if prop.eq_ignore_ascii_case("DTEND") {
            dtend_raw = Some(value.trim().to_string());
        } else if prop.eq_ignore_ascii_case("DURATION") {
            duration_raw = Some(value.trim().to_string());
        }
    }

    let Some(dtstart) = dtstart_raw.as_deref().and_then(normalize_datetime) else {
        return true;
    };

    let dtend = if let Some(raw) = dtend_raw.as_deref() {
        normalize_datetime(raw)
    } else if let Some(raw) = duration_raw.as_deref() {
        parse_duration_seconds(raw).and_then(|secs| dtstart.checked_add(secs))
    } else if is_date_only {
        // All-day event with no explicit DTEND: RFC 5545 treats it as a
        // single day, i.e. ending at the start of the next day.
        dtstart.checked_add(86_400)
    } else {
        // Zero-duration event: treat DTEND as DTSTART itself for overlap
        // purposes.
        Some(dtstart)
    };
    let Some(dtend) = dtend else {
        return true;
    };

    let range_start = start.and_then(normalize_datetime);
    let range_end = end.and_then(normalize_datetime);

    let starts_before_range_end = range_end.is_none_or(|re| dtstart < re);
    let ends_after_range_start = range_start.is_none_or(|rs| dtend > rs);
    starts_before_range_end && ends_after_range_start
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(dtstart: &str, dtend: Option<&str>) -> Vec<u8> {
        let mut s =
            format!("BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:abc-123\r\nDTSTART:{dtstart}\r\n");
        if let Some(end) = dtend {
            s.push_str(&format!("DTEND:{end}\r\n"));
        }
        s.push_str("SUMMARY:Test\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
        s.into_bytes()
    }

    #[test]
    fn extracts_uid() {
        let ics = event("20260115T090000Z", Some("20260115T100000Z"));
        assert_eq!(extract_uid(&ics).as_deref(), Some("abc-123"));
    }

    #[test]
    fn missing_uid_is_none() {
        let ics =
            b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nSUMMARY:x\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(extract_uid(ics), None);
    }

    #[test]
    fn unfolds_wrapped_lines() {
        let ics = b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:abc-\r\n 123\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(extract_uid(ics).as_deref(), Some("abc-123"));
    }

    #[test]
    fn detects_component_type() {
        let ics = event("20260115T090000Z", None);
        assert!(has_component(&ics, "VEVENT"));
        assert!(!has_component(&ics, "VTODO"));
    }

    #[test]
    fn overlapping_range_matches() {
        let ics = event("20260115T090000Z", Some("20260115T100000Z"));
        assert!(time_range_overlaps(
            &ics,
            Some("20260101T000000Z"),
            Some("20260201T000000Z")
        ));
    }

    #[test]
    fn non_overlapping_range_does_not_match() {
        let ics = event("20260115T090000Z", Some("20260115T100000Z"));
        assert!(!time_range_overlaps(
            &ics,
            Some("20260201T000000Z"),
            Some("20260301T000000Z")
        ));
    }

    #[test]
    fn half_open_interval_excludes_exact_boundary() {
        let ics = event("20260115T090000Z", Some("20260115T100000Z"));
        // Event ends exactly at range start: no overlap.
        assert!(!time_range_overlaps(
            &ics,
            Some("20260115T100000Z"),
            Some("20260116T000000Z")
        ));
        // Event starts exactly at range end: no overlap.
        assert!(!time_range_overlaps(
            &ics,
            Some("20260114T000000Z"),
            Some("20260115T090000Z")
        ));
    }

    #[test]
    fn all_day_event_with_no_dtend_spans_one_day() {
        let mut ics =
            b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART;VALUE=DATE:20260115\r\n".to_vec();
        ics.extend_from_slice(b"SUMMARY:day\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n");
        assert!(time_range_overlaps(
            &ics,
            Some("20260115T000000Z"),
            Some("20260116T000000Z")
        ));
        assert!(!time_range_overlaps(
            &ics,
            Some("20260116T000000Z"),
            Some("20260117T000000Z")
        ));
    }

    #[test]
    fn recurring_event_always_matches() {
        let ics = b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART:20200101T090000Z\r\nRRULE:FREQ=WEEKLY\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(time_range_overlaps(
            ics,
            Some("20260101T000000Z"),
            Some("20260102T000000Z")
        ));
    }

    #[test]
    fn no_bounds_always_matches() {
        let ics = event("20260115T090000Z", Some("20260115T100000Z"));
        assert!(time_range_overlaps(&ics, None, None));
    }

    #[test]
    fn summarize_extracts_uid_summary_and_wall_clock_start() {
        let ics = b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:abc-123\r\nDTSTART:20260115T090000Z\r\nSUMMARY:Team sync\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let s = summarize(ics).unwrap();
        assert_eq!(s.uid, "abc-123");
        assert_eq!(s.summary, "Team sync");
        assert_eq!((s.year, s.month, s.day), (2026, 1, 15));
        assert_eq!((s.hour, s.minute), (9, 0));
        assert!(!s.all_day);
    }

    #[test]
    fn summarize_marks_all_day_events() {
        let ics = b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART;VALUE=DATE:20260301\r\nSUMMARY:Public holiday\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let s = summarize(ics).unwrap();
        assert_eq!((s.year, s.month, s.day), (2026, 3, 1));
        assert!(s.all_day);
    }

    #[test]
    fn summarize_defaults_missing_summary_to_placeholder() {
        let ics = b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART:20260115T090000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let s = summarize(ics).unwrap();
        assert_eq!(s.summary, "(untitled)");
    }

    #[test]
    fn summarize_is_none_without_uid_or_dtstart() {
        let no_uid = b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nDTSTART:20260115T090000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(summarize(no_uid).is_none());

        let no_dtstart =
            b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nSUMMARY:y\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(summarize(no_dtstart).is_none());
    }

    #[test]
    fn extract_summary_ignores_an_empty_value() {
        let ics = b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nSUMMARY:\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(extract_summary(ics), None);
    }

    #[test]
    fn duration_based_event_computes_end() {
        let ics = b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART:20260115T090000Z\r\nDURATION:PT1H\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(time_range_overlaps(
            ics,
            Some("20260115T093000Z"),
            Some("20260115T110000Z")
        ));
        assert!(!time_range_overlaps(
            ics,
            Some("20260115T100000Z"),
            Some("20260115T110000Z")
        ));
    }

    #[test]
    fn timezone_properties_do_not_replace_or_expand_the_event() {
        let ics = b"BEGIN:VCALENDAR\r\nBEGIN:VTIMEZONE\r\nTZID:Example/Zone\r\nBEGIN:STANDARD\r\nDTSTART:19700101T000000\r\nRRULE:FREQ=YEARLY\r\nEND:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:event-1\r\nDTSTART:20260115T090000\r\nDTEND:20260115T100000\r\nSUMMARY:Current event\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

        let summary = summarize(ics).unwrap();
        assert_eq!((summary.year, summary.month, summary.day), (2026, 1, 15));
        assert_eq!(summary.summary, "Current event");
        assert!(!time_range_overlaps(
            ics,
            Some("20260201T000000Z"),
            Some("20260301T000000Z")
        ));
    }

    #[test]
    fn summarize_uses_properties_from_the_same_event() {
        let ics = b"BEGIN:VCALENDAR\r\nBEGIN:VTODO\r\nUID:task-1\r\nSUMMARY:Task summary\r\nDTSTART:20260110T090000Z\r\nEND:VTODO\r\nBEGIN:VEVENT\r\nUID:event-1\r\nSUMMARY:Event summary\r\nDTSTART:20260115T090000Z\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

        let summary = summarize(ics).unwrap();
        assert_eq!(summary.uid, "event-1");
        assert_eq!(summary.summary, "Event summary");
        assert_eq!((summary.year, summary.month, summary.day), (2026, 1, 15));
    }

    #[test]
    fn duration_arithmetic_carries_minutes_hours_and_calendar_days() {
        let ics = b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:x\r\nDTSTART:20261231T234500Z\r\nDURATION:PT30M\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert!(time_range_overlaps(
            ics,
            Some("20270101T000000Z"),
            Some("20270101T001000Z")
        ));
        assert!(!time_range_overlaps(
            ics,
            Some("20270101T001500Z"),
            Some("20270101T020000Z")
        ));
    }
}
