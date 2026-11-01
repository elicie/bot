//! Daily schedule results compared against `cron-parser` 5.10.3 output recorded from the Node
//! implementation (`fixtures/daily-golden.json`: `[timezone, time, nowMs, nextIso | "ERR:…", latestMs]`).
//! Set `DAILY_GOLDEN=/path/to/file.json` to check a larger generated corpus.

use chrono::DateTime;
use grok_bot_model::RoutineSchedule;
use grok_bot_store::schedule::{latest_routine_time, next_routine_time};

type Case = (String, String, i64, String, Option<i64>);

fn check(cases: &[Case]) {
    let mut failures = Vec::new();
    for (timezone, time, now, next, latest) in cases {
        let schedule = RoutineSchedule::Daily { time: time.clone(), timezone: timezone.clone() };
        let at = DateTime::from_timestamp_millis(*now).unwrap();
        let actual = next_routine_time(&schedule, at, None);
        if next.starts_with("ERR:") {
            // cron-parser exhausted its loop limit; the Node route answered 500.
            if actual.as_ref().map_err(|error| error.status) != Err(500) {
                failures.push(format!("{timezone} {time} now={now}: expected cron-parser failure, got {actual:?}"));
            }
            continue;
        }
        // latestRoutineTime(schedule, due, now) = max(due, latest); probe with a due at the epoch.
        let actual_latest = latest_routine_time(&schedule, "1970-01-01T00:00:00.000Z", at);
        let expected_latest = latest.unwrap_or(0).max(0);
        if actual.as_ref() != Ok(next) || actual_latest != expected_latest {
            failures.push(format!("{timezone} {time} now={now}: next {actual:?} vs {next}, latest {actual_latest} vs {expected_latest}"));
        }
    }
    if let Ok(path) = std::env::var("DAILY_GOLDEN_FAILURES") {
        std::fs::write(path, failures.join("\n")).unwrap();
    }
    assert!(failures.is_empty(), "{} of {} cases differ:\n{}", failures.len(), cases.len(), failures.iter().take(30).cloned().collect::<Vec<_>>().join("\n"));
}

#[test]
fn daily_schedules_match_cron_parser_fixture() {
    let cases: Vec<Case> = serde_json::from_str(include_str!("fixtures/daily-golden.json")).unwrap();
    assert!(cases.len() > 1000);
    assert!(cases.iter().any(|case| case.3.starts_with("ERR:")));
    check(&cases);
}

#[test]
fn daily_schedules_match_external_corpus() {
    let Ok(path) = std::env::var("DAILY_GOLDEN") else { return };
    let cases: Vec<Case> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    check(&cases);
}
