//! Routine schedule validation and next/latest occurrence calculations.
//!
//! Daily schedules preserve the prior `cron-parser` 5 and luxon search semantics (see `cron.rs`):
//! in New York a 02:30 schedule fires at 03:30 EDT
//! on the spring-forward day and once (at the first 01:30) on the fall-back day, while zones whose
//! transition skips local midnight (Santiago, Havana) skip that day's occurrence.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use grok_bot_model::{HttpError, HttpResult, RoutineInput, RoutineSchedule};

use crate::cron::{Daily, search_start};
use crate::validate::{Issues, internal, is_timestamp, iso_ms, js_trim, parse_ms};

pub const MAX_INTERVAL_MINUTES: u32 = 10080;

/// `Intl.DateTimeFormat` timezone acceptance (case-insensitive IANA names and links), excluding
/// offset strings like `+04:00` exactly like the Node refinement.
pub fn parse_timezone(value: &str) -> Option<Tz> {
    if value.starts_with('+') || value.starts_with('-') {
        return None;
    }
    Tz::from_str_insensitive(value).ok()
}

fn valid_time(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 5
        && bytes[2] == b':'
        && matches!((bytes[0], bytes[1]), (b'0'..=b'1', b'0'..=b'9') | (b'2', b'0'..=b'3'))
        && matches!((bytes[3], bytes[4]), (b'0'..=b'5', b'0'..=b'9'))
}

/// `routineScheduleSchema` issues; trims the timezone like the schema transform.
pub(crate) fn schedule_issues(issues: &mut Issues, schedule: &mut RoutineSchedule) {
    match schedule {
        RoutineSchedule::Once { at } => {
            if !is_timestamp(at) {
                issues.push("schedule.at", "Invalid ISO datetime");
            }
        }
        RoutineSchedule::Interval { minutes } => issues.number("schedule.minutes", u64::from(*minutes), 1, u64::from(MAX_INTERVAL_MINUTES)),
        RoutineSchedule::Daily { time, timezone } => {
            if !valid_time(time) {
                issues.push("schedule.time", "Use HH:mm in 24-hour time.");
            }
            *timezone = js_trim(timezone).to_owned();
            issues.length("schedule.timezone", timezone, 1, 100);
            if parse_timezone(timezone).is_none() {
                issues.push("schedule.timezone", "Choose a valid IANA timezone.");
            }
        }
    }
}

/// Validate and normalize a routine input (name, prompt, schedule bounds, IANA timezone).
pub fn validate_routine_input(mut input: RoutineInput) -> HttpResult<RoutineInput> {
    let mut issues = Issues::new();
    issues.uuid("botId", &input.bot_id);
    input.name = js_trim(&input.name).to_owned();
    issues.length("name", &input.name, 1, 100);
    input.prompt = js_trim(&input.prompt).to_owned();
    issues.length("prompt", &input.prompt, 1, 100000);
    schedule_issues(&mut issues, &mut input.schedule);
    issues.finish()?;
    Ok(input)
}

/// `dailyTimes` in routines.ts: the first occurrence after `now` and the latest one at or before it,
/// searching from the end of the day before yesterday in the schedule timezone. Errors where
/// `cron-parser` threw (its loop limit), which the Node routes surfaced as a 500.
fn daily_times(time: &str, timezone: &str, now: i64) -> HttpResult<(i64, Option<i64>)> {
    let tz = parse_timezone(timezone).ok_or_else(|| HttpError::bad_request("schedule.timezone: Choose a valid IANA timezone."))?;
    if !valid_time(time) {
        return Err(HttpError::bad_request("schedule.time: Use HH:mm in 24-hour time."));
    }
    let hour = time[..2].parse().map_err(|_| internal())?;
    let minute = time[3..].parse().map_err(|_| internal())?;
    let mut expression = Daily::new(hour, minute, search_start(now, tz), tz);
    let mut latest = None;
    loop {
        let occurrence = expression.next().ok_or_else(internal)?;
        if occurrence > now {
            return Ok((occurrence, latest));
        }
        latest = Some(occurrence);
    }
}

/// Next run time strictly after `now` (or after `after` for recurring schedules).
pub fn next_routine_time(schedule: &RoutineSchedule, now: DateTime<Utc>, after: Option<&str>) -> HttpResult<String> {
    let now = now.timestamp_millis();
    match schedule {
        RoutineSchedule::Once { at } => iso_ms(parse_ms(at).ok_or_else(internal)?),
        RoutineSchedule::Daily { time, timezone } => iso_ms(daily_times(time, timezone, now)?.0),
        RoutineSchedule::Interval { minutes } => {
            let period = i64::from(*minutes) * 60000;
            if period <= 0 {
                return Err(internal());
            }
            let anchor = match after {
                Some(previous) => parse_ms(previous).ok_or_else(internal)?,
                None => now,
            };
            let steps = ((now - anchor).div_euclid(period) + 1).max(1);
            iso_ms(anchor + steps * period)
        }
    }
}

/// Latest scheduled time at or before `now`, starting from `next_run_at`. Where JavaScript would
/// produce `NaN` or throw (unparsable `next_run_at`, a cron-parser failure) this returns `i64::MAX`
/// so the slot never counts as covered; `next_routine_time` then reports the error.
pub fn latest_routine_time(schedule: &RoutineSchedule, next_run_at: &str, now: DateTime<Utc>) -> i64 {
    let Some(due) = parse_ms(next_run_at) else { return i64::MAX };
    let now = now.timestamp_millis();
    match schedule {
        RoutineSchedule::Once { .. } => due,
        RoutineSchedule::Daily { time, timezone } => match daily_times(time, timezone, now) {
            Ok((_, Some(latest))) => due.max(latest),
            Ok((_, None)) => due,
            Err(_) => i64::MAX,
        },
        RoutineSchedule::Interval { minutes } => {
            let period = i64::from(*minutes) * 60000;
            if period <= 0 {
                return i64::MAX;
            }
            due + (now - due).div_euclid(period) * period
        }
    }
}
