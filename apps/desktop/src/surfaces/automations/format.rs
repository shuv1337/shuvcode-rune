//! Trigger, date, relative-time, status and ordering formatters for Automations
//! (packages/app/src/automations/format.ts, opencode#66393).

use std::cmp::Ordering;

use opencode_i18n::date::{self, Moment, Style, Unit, Width};
use opencode_i18n::{list, t};
use opencode_kit::model::{AutomationInfo, AutomationOutcome, AutomationStatus, AutomationTrigger};

/// Human-readable description of row.trigger (format.ts:triggerLabel).
pub fn trigger_label(row: &AutomationInfo) -> String {
    match &row.trigger {
        AutomationTrigger::At { at, .. } => {
            t!("automations.trigger.at", date = wall_clock(at, row))
        }
        AutomationTrigger::Every { every, .. } => {
            let interval = every.strip_prefix("1 ").unwrap_or(every.as_str());
            t!("automations.trigger.every", interval = interval)
        }
        AutomationTrigger::Cron { cron, .. } => {
            describe_cron(cron).unwrap_or_else(|| cron.clone())
        }
        AutomationTrigger::Event { server, event, .. } => {
            t!("automations.trigger.event", event = event, server = server)
        }
    }
}

/// Covers the common cron shapes ("every weekday at 9"); anything else keeps its raw cron text
/// (format.ts:describeCron).
fn describe_cron(cron: &str) -> Option<String> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 5 {
        return None;
    }
    if fields[2] != "*" || fields[3] != "*" {
        return None;
    }
    let minutes = parse_cron_field(fields[0], 0, 59, false)?;
    let hours = parse_cron_field(fields[1], 0, 23, false)?;
    let weekdays = parse_cron_field(fields[4], 0, 6, true)?;

    if minutes.len() == 1 && hours.len() == 1 {
        let moment = Moment {
            year: 2000,
            month: 1,
            day: 1,
            hour: hours[0],
            minute: minutes[0],
            offset: 0,
        };
        let time = date::format(Style::Time, moment);
        if weekdays.is_empty() {
            return Some(t!("automations.trigger.daily", time = time));
        }
        if weekdays == [1, 2, 3, 4, 5] {
            return Some(t!("automations.trigger.weekdays", time = time));
        }
        if weekdays == [0, 6] {
            return Some(t!("automations.trigger.weekends", time = time));
        }
        let names: Vec<&str> = weekdays.iter().map(|&d| date::weekday_name(d, Width::Long)).collect();
        let days = list(&names);
        return Some(t!("automations.trigger.days", days = days, time = time));
    }

    if !hours.is_empty() || !weekdays.is_empty() {
        return None;
    }
    if minutes.len() == 1 {
        let minute = format!("{:02}", minutes[0]);
        return Some(t!("automations.trigger.hourly", minute = minute));
    }
    if minutes.len() >= 2 {
        let step = minutes[1].saturating_sub(minutes[0]);
        if step > 0
            && minutes[0] == 0
            && 60 % step == 0
            && minutes.len() == (60 / step) as usize
            && minutes.iter().enumerate().all(|(i, &m)| m == i as u32 * step)
        {
            return Some(t!("automations.trigger.minutes", count = step));
        }
    }
    None
}

fn parse_cron_field(raw: &str, min: u32, max: u32, dow: bool) -> Option<Vec<u32>> {
    if raw == "*" {
        return Some(Vec::new());
    }
    let mut vals = Vec::new();
    for part in raw.split(',') {
        let (range_part, step) = match part.split_once('/') {
            Some((r, s)) => {
                let st: u32 = s.parse().ok()?;
                if st == 0 {
                    return None;
                }
                (r, st)
            }
            None => (part, 1),
        };
        let (start, end) = if range_part == "*" {
            (min, max)
        } else if let Some((a, b)) = range_part.split_once('-') {
            (parse_cron_atom(a, dow)?, parse_cron_atom(b, dow)?)
        } else {
            let v = parse_cron_atom(range_part, dow)?;
            if step == 1 { (v, v) } else { (v, max) }
        };
        if start < min || end > max || start > end {
            return None;
        }
        let mut cur = start;
        while cur <= end {
            vals.push(cur);
            cur = cur.saturating_add(step);
        }
    }
    vals.sort_unstable();
    vals.dedup();
    if vals.len() == (max - min + 1) as usize {
        return Some(Vec::new());
    }
    Some(vals)
}

fn parse_cron_atom(s: &str, dow: bool) -> Option<u32> {
    if dow {
        match s.to_ascii_uppercase().as_str() {
            "SUN" => return Some(0),
            "MON" => return Some(1),
            "TUE" => return Some(2),
            "WED" => return Some(3),
            "THU" => return Some(4),
            "FRI" => return Some(5),
            "SAT" => return Some(6),
            _ => {}
        }
    }
    let n: u32 = s.parse().ok()?;
    if dow && n == 7 { Some(0) } else { Some(n) }
}

fn wall_clock(at: &str, row: &AutomationInfo) -> String {
    if let Some(next) = row.next_due_at {
        return format_date(next, &row.tz);
    }
    if let Some(m) = parse_iso_wall_clock(at) {
        return date::format(Style::DateTime, m);
    }
    at.to_owned()
}

fn parse_iso_wall_clock(s: &str) -> Option<Moment> {
    let (d, t) = s.split_once('T')?;
    let mut dp = d.split('-');
    let year: i32 = dp.next()?.parse().ok()?;
    let month: u32 = dp.next()?.parse().ok()?;
    let day: u32 = dp.next()?.parse().ok()?;
    if dp.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let time_only = t.trim_end_matches('Z').split(['+', '-']).next()?;
    let mut tp = time_only.split(':');
    let hour: u32 = tp.next()?.parse().ok()?;
    let minute: u32 = tp.next()?.parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    Some(Moment { year, month, day, hour, minute, offset: 0 })
}

/// Formats Unix milliseconds as medium date + short time (format.ts:formatDate).
pub fn format_date(at_ms: i64, _tz: &str) -> String {
    let secs = at_ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400) as u32;
    let (year, month, day) = civil_from_days(days);
    let moment = Moment {
        year,
        month,
        day,
        hour: rem / 3600,
        minute: (rem % 3600) / 60,
        offset: 0,
    };
    date::format(Style::DateTime, moment)
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = y + i64::from(m <= 2);
    (y as i32, m, d)
}

/// Relative time matching Intl.RelativeTimeFormat(locale, { numeric: "auto" })
/// (format.ts:relativeTime).
pub fn relative_time(at_ms: i64, now_ms: i64) -> String {
    let seconds = (at_ms - now_ms) as f64 / 1000.0;
    let minutes = seconds / 60.0;
    let hours = minutes / 60.0;
    if seconds.abs() < 60.0 {
        return date::relative(0, Unit::Second);
    }
    if minutes.abs() < 60.0 {
        return date::relative(minutes.round() as i64, Unit::Minute);
    }
    if hours.abs() < 24.0 {
        return date::relative(hours.round() as i64, Unit::Hour);
    }
    let days = (hours / 24.0).round() as i64;
    date::relative_day(days)
        .map(str::to_owned)
        .unwrap_or_else(|| date::relative(days, Unit::Day))
}

/// Status string for the inspector subtitle (format.ts:statusLabel).
pub fn status_label(row: &AutomationInfo) -> &'static str {
    match (row.status, row.last_outcome) {
        (AutomationStatus::Paused, Some(AutomationOutcome::Failed)) => t!("automations.status.failed"),
        (AutomationStatus::Done, Some(AutomationOutcome::Expired)) => t!("automations.status.expired"),
        (AutomationStatus::Active, _) => t!("automations.status.active"),
        (AutomationStatus::Paused, _) => t!("automations.status.paused"),
        (AutomationStatus::Done, _) => t!("automations.status.done"),
    }
}

/// Row sort order: Active (earliest next_due_at first), then Paused, then Done; ties broken by
/// time.updated descending (format.ts:compareAutomations).
pub fn compare_automations(a: &AutomationInfo, b: &AutomationInfo) -> Ordering {
    let rank = |s: AutomationStatus| match s {
        AutomationStatus::Active => 0u8,
        AutomationStatus::Paused => 1u8,
        AutomationStatus::Done => 2u8,
    };
    rank(a.status)
        .cmp(&rank(b.status))
        .then_with(|| {
            if a.status == AutomationStatus::Active {
                a.next_due_at.unwrap_or(i64::MAX).cmp(&b.next_due_at.unwrap_or(i64::MAX))
            } else {
                Ordering::Equal
            }
        })
        .then_with(|| b.time.updated.cmp(&a.time.updated))
        .then_with(|| a.id.cmp(&b.id))
}
