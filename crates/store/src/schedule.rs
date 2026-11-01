//! Routine schedule math. Port of `nextRoutineTime`, `latestRoutineTime`, and the routine input
//! validation in `server/routines.ts`. CONTRACT (store agent).

use chrono::{DateTime, Utc};
use grok_bot_model::{HttpResult, RoutineInput, RoutineSchedule};

/// Validate and normalize a routine input (name, prompt, schedule bounds, IANA timezone).
pub fn validate_routine_input(input: RoutineInput) -> HttpResult<RoutineInput> {
    let _ = input;
    todo!("store agent")
}

/// Next run time strictly after `now` (or after `after` for recurring schedules).
pub fn next_routine_time(schedule: &RoutineSchedule, now: DateTime<Utc>, after: Option<&str>) -> HttpResult<String> {
    let _ = (schedule, now, after);
    todo!("store agent")
}

/// Latest scheduled time at or before `now`, starting from `next_run_at`.
pub fn latest_routine_time(schedule: &RoutineSchedule, next_run_at: &str, now: DateTime<Utc>) -> i64 {
    let _ = (schedule, next_run_at, now);
    todo!("store agent")
}
