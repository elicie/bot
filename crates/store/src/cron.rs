//! Port of the parts of `cron-parser` 5.10.3 (`CronExpression#findSchedule`, `CronDate`) and luxon
//! 3.7 (`DateTime#plus/set/startOf/endOf`, `fixOffset`) that `0 M H * * *` exercises, so daily
//! routines land on exactly the instants the Node scheduler produced, including its handling of
//! skipped and repeated wall-clock time around DST transitions.

use chrono::{DateTime, Datelike, NaiveDate, Offset, TimeZone, Timelike};
use chrono_tz::Tz;

const LOOP_LIMIT: u32 = 10000;
const MINUTE: i64 = 60_000;
const DAY: i64 = 86_400_000;

/// Luxon `DateTime`: an instant plus the offset (minutes) luxon associated with it.
#[derive(Clone, Copy)]
struct Lux {
    ts: i64,
    o: i64,
    tz: Tz,
}

#[derive(Clone, Copy)]
struct Fields {
    year: i32,
    month: u32,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    millisecond: i64,
}

fn zone_offset(tz: Tz, ts: i64) -> i64 {
    let utc = DateTime::from_timestamp_millis(ts).unwrap_or_default().naive_utc();
    i64::from(tz.offset_from_utc_datetime(&utc).fix().local_minus_utc()).div_euclid(60)
}

fn days_in_month(year: i32, month: u32) -> i64 {
    let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    match (NaiveDate::from_ymd_opt(year, month, 1), NaiveDate::from_ymd_opt(next_year, next_month, 1)) {
        (Some(start), Some(end)) => (end - start).num_days(),
        _ => 31,
    }
}

/// `objToLocalTS` (`Date.UTC` with overflow normalization).
fn local_ts(fields: Fields) -> i64 {
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default();
    let first = NaiveDate::from_ymd_opt(fields.year, fields.month, 1).unwrap_or(epoch);
    let days = (first - epoch).num_days() + fields.day - 1;
    days * DAY + fields.hour * 3_600_000 + fields.minute * MINUTE + fields.second * 1000 + fields.millisecond
}

/// luxon `fixOffset`.
fn fix_offset(local: i64, o: i64, tz: Tz) -> (i64, i64) {
    let mut guess = local - o * MINUTE;
    let o2 = zone_offset(tz, guess);
    if o == o2 {
        return (guess, o);
    }
    guess -= (o2 - o) * MINUTE;
    let o3 = zone_offset(tz, guess);
    if o2 == o3 {
        return (guess, o2);
    }
    (local - o2.min(o3) * MINUTE, o2.max(o3))
}

impl Lux {
    fn from_millis(ts: i64, tz: Tz) -> Self {
        Self { ts, o: zone_offset(tz, ts), tz }
    }

    fn fields(&self) -> Fields {
        let local = DateTime::from_timestamp_millis(self.ts + self.o * MINUTE).unwrap_or_default().naive_utc();
        Fields {
            year: local.year(),
            month: local.month(),
            day: i64::from(local.day()),
            hour: i64::from(local.hour()),
            minute: i64::from(local.minute()),
            second: i64::from(local.second()),
            millisecond: i64::from(local.and_utc().timestamp_subsec_millis()),
        }
    }

    fn with(&self, fields: Fields) -> Self {
        let (ts, o) = fix_offset(local_ts(fields), self.o, self.tz);
        Self { ts, o, tz: self.tz }
    }

    /// `plus` with time units only: absolute milliseconds.
    fn plus_ms(&self, ms: i64) -> Self {
        let (ts, _) = fix_offset(self.ts + self.o * MINUTE, self.o, self.tz);
        let ts = ts + ms;
        Self { ts, o: zone_offset(self.tz, ts), tz: self.tz }
    }

    /// `plus({ days })`: calendar arithmetic on the wall clock.
    fn plus_days(&self, days: i64) -> Self {
        let mut fields = self.fields();
        fields.day = fields.day.min(days_in_month(fields.year, fields.month)) + days;
        self.with(fields)
    }

    /// `set`, keeping the day clamped to the month like luxon when the day is not set.
    fn set(&self, hour: Option<i64>, minute: Option<i64>, second: Option<i64>, millisecond: Option<i64>) -> Self {
        let mut fields = self.fields();
        fields.hour = hour.unwrap_or(fields.hour);
        fields.minute = minute.unwrap_or(fields.minute);
        fields.second = second.unwrap_or(fields.second);
        fields.millisecond = millisecond.unwrap_or(fields.millisecond);
        fields.day = fields.day.min(days_in_month(fields.year, fields.month));
        self.with(fields)
    }

    fn start_of_day(&self) -> Self {
        self.set(Some(0), Some(0), Some(0), Some(0))
    }
    fn start_of_hour(&self) -> Self {
        self.set(None, Some(0), Some(0), Some(0))
    }
    fn start_of_minute(&self) -> Self {
        self.set(None, None, Some(0), Some(0))
    }
    fn end_of_day(&self) -> Self {
        self.plus_days(1).start_of_day().plus_ms(-1)
    }
}

/// `CronDate` with its DST bookkeeping.
#[derive(Clone, Copy)]
struct CronDate {
    date: Lux,
    dst_start: Option<i64>,
    dst_start_landing_hour: Option<i64>,
    dst_end: Option<i64>,
}

enum Unit {
    Second,
    Minute,
    Hour,
    Day,
}

impl CronDate {
    fn new(ts: i64, tz: Tz) -> Self {
        Self { date: Lux::from_millis(ts, tz), dst_start: None, dst_start_landing_hour: None, dst_end: None }
    }
    fn hours(&self) -> i64 {
        self.date.fields().hour
    }
    fn minutes(&self) -> i64 {
        self.date.fields().minute
    }
    fn seconds(&self) -> i64 {
        self.date.fields().second
    }
    fn set_hours(&mut self, hour: i64) {
        self.date = self.date.set(Some(hour), None, None, None);
    }
    fn set_minutes(&mut self, minute: i64) {
        self.date = self.date.set(None, Some(minute), None, None);
    }
    fn set_seconds(&mut self, second: i64) {
        self.date = self.date.set(None, None, Some(second), None);
    }
    fn clear_dst_start(&mut self) {
        self.dst_start = None;
        self.dst_start_landing_hour = None;
    }

    fn add(&mut self, unit: &Unit) {
        self.date = match unit {
            Unit::Second => self.date.plus_ms(1000),
            Unit::Minute => self.date.plus_ms(MINUTE).start_of_minute(),
            Unit::Hour => self.date.plus_ms(60 * MINUTE).start_of_hour(),
            Unit::Day => self.date.plus_days(1).start_of_day(),
        };
    }

    /// `applyDateOperation(Add, unit, 1)` (one hour value in the expression).
    fn apply_add(&mut self, unit: Unit) {
        if matches!(unit, Unit::Day) {
            self.add(&unit);
            return;
        }
        let previous_hour = self.hours();
        let previous_offset = self.date.o;
        self.add(&unit);
        let current_hour = self.hours();
        let advance = (current_hour - previous_hour + 24) % 24;
        if self.date.o > previous_offset && advance >= 2 {
            self.dst_start = Some((previous_hour + 1) % 24);
            self.dst_start_landing_hour = Some(current_hour);
        } else if advance == 0 && self.minutes() == 0 && self.seconds() == 0 {
            self.dst_end = Some(current_hour);
        }
    }
}

/// `CronExpression` for `0 minute hour * * *`.
pub(crate) struct Daily {
    hour: i64,
    minute: i64,
    current: CronDate,
}

impl Daily {
    pub(crate) fn new(hour: u32, minute: u32, current_ts: i64, tz: Tz) -> Self {
        Self { hour: i64::from(hour), minute: i64::from(minute), current: CronDate::new(current_ts, tz) }
    }

    fn is_dst_transition_day(date: &CronDate) -> bool {
        date.date.start_of_day().o != date.date.end_of_day().o
    }

    fn match_hour(&self, date: &mut CronDate) -> bool {
        let current_hour = date.hours();
        let is_match = current_hour == self.hour;
        let is_dst_end = date.dst_end == Some(current_hour);
        if let (Some(start), Some(landing)) = (date.dst_start, date.dst_start_landing_hour)
            && landing == current_hour
        {
            let mut skipped = start;
            while skipped != current_hour {
                if skipped == self.hour {
                    return true;
                }
                skipped = (skipped + 1) % 24;
            }
        }
        if is_dst_end {
            date.dst_end = None;
            date.apply_add(Unit::Hour);
            return false;
        }
        if is_match {
            return true;
        }
        date.clear_dst_start();
        if self.hour <= current_hour {
            date.apply_add(Unit::Day);
            return false;
        }
        let next_hour = self.hour;
        if Self::is_dst_transition_day(date) {
            for _ in 0..next_hour - current_hour {
                date.apply_add(Unit::Hour);
                if date.hours() >= next_hour {
                    break;
                }
            }
        } else {
            date.set_hours(next_hour);
        }
        date.set_minutes(self.minute);
        date.set_seconds(0);
        false
    }

    /// `next()`: the following occurrence, or `None` when cron-parser exceeds its loop limit.
    pub(crate) fn next(&mut self) -> Option<i64> {
        let mut date = self.current;
        let start = date.date.ts;
        if date.date.fields().millisecond > 0 {
            date.date = date.date.set(None, None, None, Some(0));
            date.apply_add(Unit::Second);
        }
        let mut steps = 0;
        loop {
            steps += 1;
            if steps >= LOOP_LIMIT {
                return None;
            }
            if !self.match_hour(&mut date) {
                continue;
            }
            if date.minutes() != self.minute {
                if self.minute > date.minutes() {
                    date.set_minutes(self.minute);
                    date.set_seconds(0);
                } else {
                    date.apply_add(Unit::Hour);
                    date.set_minutes(self.minute);
                    date.set_seconds(0);
                }
                continue;
            }
            if date.seconds() != 0 {
                date.apply_add(Unit::Minute);
                date.set_seconds(0);
                continue;
            }
            if date.date.ts == start {
                date.apply_add(Unit::Second);
                continue;
            }
            break;
        }
        self.current = date;
        Some(date.date.ts)
    }
}

/// The `dailyTimes` search start: `CronDate(now)` → start of day → minus one day (end of that
/// day, whole second) → start of day → minus one second.
pub(crate) fn search_start(now: i64, tz: Tz) -> i64 {
    let day = Lux::from_millis(now, tz).start_of_day();
    let yesterday = day.plus_days(-1).end_of_day().set(None, None, None, Some(0));
    yesterday.start_of_day().plus_ms(-1000).ts
}
