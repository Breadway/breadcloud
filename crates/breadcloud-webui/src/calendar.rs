//! Calendar view: a read-only month grid across every one of the
//! authenticated user's calendars (personal/family/school, CLOUD-6).
//!
//! Read-only per the issue's own framing ("just... a calendar view," not
//! a calendar editor) -- creating/editing events is left to a real CalDAV
//! client (DAVx5, iOS Calendar, any desktop client already pointed at
//! this server per CLOUD-6), which already has to exist for this
//! deployment to have events at all. Documented explicitly here since the
//! issue leaves it as a judgment call: a write path would mean either
//! reusing `breadcloud-caldav`'s PUT handler (a second, form-shaped
//! entry point into the same iCalendar-object logic, more surface than a
//! "minimal" UI needs) or a second event-authoring implementation, and a
//! read-only grid alone already answers "what's on the calendar this
//! month" -- the thing the issue's own "desktop Firefox access pattern"
//! framing is about.
//!
//! Reuses `breadcloud_caldav::ical::summarize` (a small CLOUD-12 addition
//! to that module, see its own doc comment) for the same "wall-clock
//! digits, not tz-aware" event reading the CalDAV surface's own
//! `time-range` filtering already relies on, rather than a second
//! iCalendar parser.

use std::collections::BTreeMap;

use axum::response::Response;
use breadcloud_caldav::{ical, AppState as CaldavState};
use breadcloud_storage::{CalendarInfo, DavPath};

use crate::datetime::{
    civil_from_system_time, current_year_month, day_of_week, days_in_month, MONTH_NAMES,
    WEEKDAY_NAMES,
};
use crate::form::parse_urlencoded;
use crate::template::{html_escape, html_response, page};

pub async fn month_view(caldav: &CaldavState, user: &str, query: &str) -> Response {
    let (year, month) = parse_year_month(query).unwrap_or_else(current_year_month);

    let calendars = match caldav.calendars.list_calendars(user).await {
        Ok(c) => c,
        Err(err) => return breadcloud_webdav::error::storage_error_response(err),
    };

    let mut events_by_day: BTreeMap<u32, Vec<(String, ical::EventSummary)>> = BTreeMap::new();
    for cal in &calendars {
        let Ok(events_storage) = caldav.calendars.events_storage(user, &cal.id).await else {
            continue;
        };
        let Ok(entries) = events_storage.list_dir(&DavPath::root()).await else {
            continue;
        };
        for entry in entries {
            if entry.metadata.is_dir() {
                continue;
            }
            let Ok(bytes) = events_storage.read_bytes(&entry.path).await else {
                continue;
            };
            let Some(summary) = ical::summarize(&bytes) else {
                continue;
            };
            if summary.year as i64 == year && summary.month == month {
                events_by_day
                    .entry(summary.day)
                    .or_default()
                    .push((cal.display_name.clone(), summary));
            }
        }
    }

    let (today_year, today_month, today_day, ..) =
        civil_from_system_time(std::time::SystemTime::now());
    let title = format!("{} {year}", MONTH_NAMES[(month - 1) as usize]);
    let body = render_month(
        year,
        month,
        &calendars,
        &events_by_day,
        (today_year, today_month, today_day),
    );
    html_response(page(&title, user, "calendar", &body))
}

fn parse_year_month(query: &str) -> Option<(i64, u32)> {
    let params = parse_urlencoded(query.as_bytes());
    let year: i64 = params.get("year")?.parse().ok()?;
    let month: u32 = params.get("month")?.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }
    Some((year, month))
}

/// `month` shifted by `delta` months (positive or negative), rolling the
/// year over correctly in either direction.
fn adjacent_month(year: i64, month: u32, delta: i64) -> (i64, u32) {
    let total = year * 12 + (month as i64 - 1) + delta;
    let new_year = total.div_euclid(12);
    let new_month = (total.rem_euclid(12) + 1) as u32;
    (new_year, new_month)
}

fn render_month(
    year: i64,
    month: u32,
    calendars: &[CalendarInfo],
    events_by_day: &BTreeMap<u32, Vec<(String, ical::EventSummary)>>,
    today: (i64, u32, u32),
) -> String {
    let (prev_year, prev_month) = adjacent_month(year, month, -1);
    let (next_year, next_month) = adjacent_month(year, month, 1);

    let mut body = String::new();
    body.push_str(&format!(
        r#"<div class="month-nav">
<a class="btn" href="/ui/calendar?year={prev_year}&amp;month={prev_month}">&larr; Prev</a>
<h1>{} {year}</h1>
<a class="btn" href="/ui/calendar?year={next_year}&amp;month={next_month}">Next &rarr;</a>
</div>"#,
        MONTH_NAMES[(month - 1) as usize]
    ));

    if calendars.is_empty() {
        body.push_str(r#"<p class="empty">No calendars yet.</p>"#);
    } else {
        body.push_str(r#"<ul class="cal-list">"#);
        for cal in calendars {
            body.push_str(&format!("<li>{}</li>", html_escape(&cal.display_name)));
        }
        body.push_str("</ul>");
    }

    let first_weekday = day_of_week(year, month, 1);
    let total_days = days_in_month(year, month);
    let days_in_prev = days_in_month(prev_year, prev_month);

    let mut cells: Vec<String> = Vec::new();
    for i in 0..first_weekday {
        let day = days_in_prev - first_weekday + 1 + i;
        cells.push(format!(
            r#"<td class="outside"><div class="daynum">{day}</div></td>"#
        ));
    }
    for day in 1..=total_days {
        let is_today = today == (year, month, day);
        let cell_class = if is_today { r#" class="today""# } else { "" };
        let mut cell = format!(r#"<td{cell_class}><div class="daynum">{day}</div>"#);
        if let Some(events) = events_by_day.get(&day) {
            let mut sorted: Vec<&(String, ical::EventSummary)> = events.iter().collect();
            sorted.sort_by_key(|entry| (entry.1.hour, entry.1.minute));
            for (cal_name, summary) in sorted {
                let label = if summary.all_day {
                    summary.summary.clone()
                } else {
                    format!(
                        "{:02}:{:02} {}",
                        summary.hour, summary.minute, summary.summary
                    )
                };
                let title = format!("{cal_name}: {}", summary.summary);
                cell.push_str(&format!(
                    r#"<span class="event" title="{}">{}</span>"#,
                    html_escape(&title),
                    html_escape(&label)
                ));
            }
        }
        cell.push_str("</td>");
        cells.push(cell);
    }
    let mut trailing_day = 1u32;
    while !cells.len().is_multiple_of(7) {
        cells.push(format!(
            r#"<td class="outside"><div class="daynum">{trailing_day}</div></td>"#
        ));
        trailing_day += 1;
    }

    body.push_str(r#"<table class="month"><thead><tr>"#);
    for name in WEEKDAY_NAMES {
        body.push_str(&format!("<th>{name}</th>"));
    }
    body.push_str("</tr></thead><tbody>");
    for chunk in cells.chunks(7) {
        body.push_str("<tr>");
        for cell in chunk {
            body.push_str(cell);
        }
        body.push_str("</tr>");
    }
    body.push_str("</tbody></table>");

    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjacent_month_rolls_forward_and_back_across_year_boundaries() {
        assert_eq!(adjacent_month(2026, 12, 1), (2027, 1));
        assert_eq!(adjacent_month(2026, 1, -1), (2025, 12));
        assert_eq!(adjacent_month(2026, 6, 1), (2026, 7));
        assert_eq!(adjacent_month(2026, 6, -1), (2026, 5));
    }

    #[test]
    fn parse_year_month_rejects_an_out_of_range_month() {
        assert_eq!(parse_year_month("year=2026&month=13"), None);
        assert_eq!(parse_year_month("year=2026&month=0"), None);
        assert_eq!(parse_year_month("year=2026&month=6"), Some((2026, 6)));
    }

    #[test]
    fn render_month_marks_todays_cell_only_when_it_falls_in_the_rendered_month() {
        let empty = BTreeMap::new();
        let in_month = render_month(2026, 6, &[], &empty, (2026, 6, 15));
        assert!(in_month.contains(r#"<td class="today"><div class="daynum">15</div>"#));

        // A different month never marks any cell as today, even though
        // day 15 still exists in it.
        let other_month = render_month(2026, 7, &[], &empty, (2026, 6, 15));
        assert!(!other_month.contains("class=\"today\""));
    }

    #[test]
    fn parse_year_month_is_none_without_both_params() {
        assert_eq!(parse_year_month(""), None);
        assert_eq!(parse_year_month("year=2026"), None);
    }
}
