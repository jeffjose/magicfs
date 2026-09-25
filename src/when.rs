//! `--when`: keeping only the files from a stretch of time.
//!
//! A window is written the way you would say it — `today`, `yesterday.morning`,
//! `3h`, `mon..wed`, `14:00..16:30` — and is re-read against the clock every
//! time the view is rebuilt, so a view of `today` is still today tomorrow.
//!
//! Everything is local time: "yesterday" means the one on your wall, which is
//! the only one anybody means. The calendar arithmetic is left to the C
//! library's `mktime`, which already knows the timezone and its DST rules, and
//! which normalises out-of-range fields — so "the 0th of March" or "hour 29"
//! land exactly where they should without any date code of our own.

use anyhow::{Result, bail};

use crate::entry::Entry;
use crate::spec::ViewSpec;

/// Seconds since the epoch.
pub type Secs = i64;

/// A parsed window: the union of its comma-separated parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    clauses: Vec<Clause>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Clause {
    /// Files stamped in `[start, end)`.
    Span(Secs, Secs),
}

/// Parts of the day. Night runs past midnight and belongs to the evening it
/// starts on, so `yesterday.night` is the one that ended this morning.
const PARTS: &[(&str, i32, i32)] = &[
    ("morning", 5, 12),
    ("afternoon", 12, 17),
    ("evening", 17, 22),
    ("night", 22, 29),
];

const WEEKDAYS: &[&[&str]] = &[
    &["sun", "sunday"],
    &["mon", "monday"],
    &["tue", "tues", "tuesday"],
    &["wed", "wednesday"],
    &["thu", "thur", "thurs", "thursday"],
    &["fri", "friday"],
    &["sat", "saturday"],
];

/// Every word a window can be built from, for suggesting a correction.
const WORDS: &[&str] = &[
    "today", "yesterday", "week", "lastweek", "month", "lastmonth", "morning", "afternoon",
    "evening", "night", "mon", "tue", "wed", "thu", "fri", "sat", "sun", "monday", "tuesday",
    "wednesday", "thursday", "friday", "saturday", "sunday",
];

impl Window {
    /// Read a window relative to `now`.
    pub fn parse(text: &str, now: Secs) -> Result<Window> {
        let mut clauses = Vec::new();
        for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            clauses.push(clause(&part.to_ascii_lowercase(), now)?);
        }
        if clauses.is_empty() {
            bail!("an empty --when window");
        }
        Ok(Window { clauses })
    }

    /// Whether a timestamp falls inside the window.
    fn contains(&self, t: Secs) -> bool {
        self.clauses.iter().any(|c| match *c {
            Clause::Span(start, end) => start <= t && t < end,
        })
    }
}

/// Drop every entry outside the spec's window.
pub fn retain(entries: &mut Vec<Entry>, spec: &ViewSpec) -> Result<()> {
    let Some(text) = &spec.when else { return Ok(()) };
    let window = Window::parse(text, now())?;
    entries.retain(|e| window.contains(stamp(e)));
    Ok(())
}

/// The moment a file counts as "from": when it was last written.
pub fn stamp(entry: &Entry) -> Secs {
    entry.mtime.div_euclid(1_000_000_000) as Secs
}

pub fn now() -> Secs {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as Secs)
        .unwrap_or(0)
}

fn clause(text: &str, now: Secs) -> Result<Clause> {
    if let Some((a, b)) = text.split_once("..") {
        return range(a, b, now);
    }
    if let Some(secs) = duration(text) {
        return Ok(Clause::Span(now - secs, Secs::MAX));
    }
    let (start, end) = span(text, now)?;
    Ok(Clause::Span(start, end))
}

/// `a..b`: clock times on one day, or from the start of one day to the end
/// of another. Either end may be left open.
fn range(a: &str, b: &str, now: Secs) -> Result<Clause> {
    // `14:00..16:30`, `yesterday.14:00..16:30`
    let (day, from) = match a.rsplit_once('.') {
        Some((day, t)) if clock(t).is_some() => (Some(day), t),
        _ => (None, a),
    };
    if let (Some(from), Some(to)) = (clock(from), clock(b)) {
        let midnight = match day {
            Some(day) => single_day(day, now)?,
            None => day_start(now, 0),
        };
        let start = at(midnight, from);
        let mut end = at(midnight, to);
        if end <= start {
            // `22:00..02:00` runs past midnight.
            end = at(midnight, (to.0 + 24, to.1));
        }
        return Ok(Clause::Span(start, end));
    }
    let start = if a.is_empty() { Secs::MIN } else { span(a, now)?.0 };
    let end = if b.is_empty() { Secs::MAX } else { span(b, now)?.1 };
    if start >= end {
        bail!("`{a}..{b}` ends before it starts");
    }
    Ok(Clause::Span(start, end))
}

/// A named stretch of time: a day, a part of one, a week or a month.
fn span(text: &str, now: Secs) -> Result<(Secs, Secs)> {
    if let Some((day, part)) = text.split_once('.')
        && let Some(&(_, from, to)) = PARTS.iter().find(|(n, ..)| *n == part)
    {
        let midnight = single_day(day, now)?;
        return Ok((at(midnight, (from, 0)), at(midnight, (to, 0))));
    }
    if let Some(&(_, from, to)) = PARTS.iter().find(|(n, ..)| *n == text) {
        // The latest one that has started: `morning` at 2am is yesterday's.
        let today = day_start(now, 0);
        let start = at(today, (from, 0));
        let midnight = if start <= now { today } else { day_start(now, 1) };
        return Ok((at(midnight, (from, 0)), at(midnight, (to, 0))));
    }
    let tm = local(now);
    match text {
        "week" | "thisweek" => {
            let back = monday_back(&tm);
            return Ok((day_start(now, back), day_start(now, back - 7)));
        }
        "lastweek" => {
            let back = monday_back(&tm);
            return Ok((day_start(now, back + 7), day_start(now, back)));
        }
        "month" | "thismonth" => {
            return Ok((make(tm.tm_year, tm.tm_mon, 1, 0, 0), make(tm.tm_year, tm.tm_mon + 1, 1, 0, 0)));
        }
        "lastmonth" => {
            return Ok((make(tm.tm_year, tm.tm_mon - 1, 1, 0, 0), make(tm.tm_year, tm.tm_mon, 1, 0, 0)));
        }
        _ => {}
    }
    let midnight = single_day(text, now)?;
    Ok((midnight, next_day(midnight)))
}

/// Midnight at the start of a single named day.
fn single_day(text: &str, now: Secs) -> Result<Secs> {
    let tm = local(now);
    match text {
        "" | "today" => return Ok(day_start(now, 0)),
        "yesterday" => return Ok(day_start(now, 1)),
        _ => {}
    }
    if let Some(wday) = WEEKDAYS.iter().position(|names| names.contains(&text)) {
        // The latest one, which is today if today is that day.
        let back = (tm.tm_wday - wday as i32).rem_euclid(7);
        return Ok(day_start(now, back as i64));
    }
    if let Some((y, m, d)) = date(text, tm.tm_year + 1900) {
        return Ok(make(y - 1900, m - 1, d, 0, 0));
    }
    let known = WORDS.iter().filter(|w| crate::invoke::distance(text, w) <= 2).min_by_key(|w| {
        crate::invoke::distance(text, w)
    });
    match known {
        Some(w) => bail!("`{text}` is not a time window — did you mean `{w}`?"),
        None => bail!(
            "`{text}` is not a time window\n\
             try: today, yesterday, mon, week, lastweek, month, 3h, 2d, \
             yesterday.morning, 14:00..16:30, 09-20..09-24"
        ),
    }
}

/// `2026-09-24` or `09-24` (this year).
fn date(text: &str, this_year: i32) -> Option<(i32, i32, i32)> {
    let nums: Vec<i32> = text.split('-').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let (y, m, d) = match nums[..] {
        [y, m, d] if y > 999 => (y, m, d),
        [m, d] => (this_year, m, d),
        _ => return None,
    };
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then_some((y, m, d))
}

/// `30m`, `2h`, `3d`, `1w` — the last that long.
fn duration(text: &str) -> Option<Secs> {
    let split = text.find(|c: char| !c.is_ascii_digit())?;
    let (n, unit) = text.split_at(split);
    let n: Secs = n.parse().ok()?;
    let unit = match unit {
        "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86_400,
        "w" | "wk" | "week" | "weeks" => 7 * 86_400,
        _ => return None,
    };
    Some(n * unit)
}

/// `14`, `14:30`, `9:05` — hours and minutes.
fn clock(text: &str) -> Option<(i32, i32)> {
    let (h, m) = text.split_once(':').unwrap_or((text, "0"));
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    ((0..=24).contains(&h) && (0..60).contains(&m)).then_some((h, m))
}

/// Days back to the most recent Monday — weeks start on Monday.
fn monday_back(tm: &libc::tm) -> i64 {
    ((tm.tm_wday + 6) % 7) as i64
}

fn local(t: Secs) -> libc::tm {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t = t as libc::time_t;
    unsafe { libc::localtime_r(&t, &mut tm) };
    tm
}

/// Local midnight `back` days before `now`'s day (negative: after).
fn day_start(now: Secs, back: i64) -> Secs {
    let tm = local(now);
    make(tm.tm_year, tm.tm_mon, tm.tm_mday - back as i32, 0, 0)
}

fn next_day(midnight: Secs) -> Secs {
    let tm = local(midnight);
    make(tm.tm_year, tm.tm_mon, tm.tm_mday + 1, 0, 0)
}

/// `hh:mm` on the day starting at `midnight`; hours past 24 run into the next.
fn at(midnight: Secs, (h, m): (i32, i32)) -> Secs {
    let tm = local(midnight);
    make(tm.tm_year, tm.tm_mon, tm.tm_mday, h, m)
}

/// Local time to seconds; `mktime` normalises out-of-range fields.
fn make(year: i32, mon: i32, mday: i32, hour: i32, min: i32) -> Secs {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = year;
    tm.tm_mon = mon;
    tm.tm_mday = mday;
    tm.tm_hour = hour;
    tm.tm_min = min;
    // Let the library decide whether DST applies on that date.
    tm.tm_isdst = -1;
    unsafe { libc::mktime(&mut tm) as Secs }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed "now": a Thursday afternoon, local time.
    fn thursday_3pm() -> Secs {
        make(2026 - 1900, 8, 24, 15, 0)
    }

    fn spans(text: &str) -> Vec<(Secs, Secs)> {
        Window::parse(text, thursday_3pm())
            .unwrap()
            .clauses
            .into_iter()
            .map(|Clause::Span(a, b)| (a, b))
            .collect()
    }

    fn one(text: &str) -> (Secs, Secs) {
        spans(text)[0]
    }

    /// (month, day, hour, minute) of a moment, local time.
    fn when(t: Secs) -> (i32, i32, i32, i32) {
        let tm = local(t);
        (tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
    }

    #[test]
    fn days_run_midnight_to_midnight() {
        let (a, b) = one("today");
        assert_eq!((when(a), when(b)), ((9, 24, 0, 0), (9, 25, 0, 0)));
        let (a, b) = one("yesterday");
        assert_eq!((when(a), when(b)), ((9, 23, 0, 0), (9, 24, 0, 0)));
    }

    #[test]
    fn a_weekday_is_the_latest_one() {
        assert_eq!(when(one("mon").0), (9, 21, 0, 0));
        assert_eq!(when(one("thursday").0), (9, 24, 0, 0), "today, when today is that day");
        assert_eq!(when(one("fri").0), (9, 18, 0, 0), "not tomorrow");
    }

    #[test]
    fn weeks_start_on_monday_and_months_on_the_first() {
        let (a, b) = one("week");
        assert_eq!((when(a), when(b)), ((9, 21, 0, 0), (9, 28, 0, 0)));
        let (a, b) = one("lastweek");
        assert_eq!((when(a), when(b)), ((9, 14, 0, 0), (9, 21, 0, 0)));
        let (a, b) = one("lastmonth");
        assert_eq!((when(a), when(b)), ((8, 1, 0, 0), (9, 1, 0, 0)));
    }

    #[test]
    fn parts_of_a_day() {
        let (a, b) = one("yesterday.morning");
        assert_eq!((when(a), when(b)), ((9, 23, 5, 0), (9, 23, 12, 0)));
        let (a, b) = one("yesterday.night");
        assert_eq!((when(a), when(b)), ((9, 23, 22, 0), (9, 24, 5, 0)), "into the next morning");
        // At 3pm, this morning has happened but tonight hasn't.
        assert_eq!(when(one("morning").0), (9, 24, 5, 0));
        assert_eq!(when(one("night").0), (9, 23, 22, 0));
    }

    #[test]
    fn durations_reach_back_from_now() {
        let now = thursday_3pm();
        assert_eq!(one("2h"), (now - 7200, Secs::MAX));
        assert_eq!(one("3d").0, now - 3 * 86_400);
    }

    #[test]
    fn clock_ranges_and_day_ranges() {
        let (a, b) = one("14:00..16:30");
        assert_eq!((when(a), when(b)), ((9, 24, 14, 0), (9, 24, 16, 30)));
        let (a, b) = one("yesterday.22..2");
        assert_eq!((when(a), when(b)), ((9, 23, 22, 0), (9, 24, 2, 0)));
        let (a, b) = one("mon..wed");
        assert_eq!((when(a), when(b)), ((9, 21, 0, 0), (9, 24, 0, 0)));
        let (a, b) = one("09-20..");
        assert_eq!((when(a), b), ((9, 20, 0, 0), Secs::MAX));
        assert_eq!(when(one("2026-01-05").0), (1, 5, 0, 0));
    }

    #[test]
    fn commas_are_a_union() {
        assert_eq!(spans("mon,wed").len(), 2);
        let w = Window::parse("mon,wed", thursday_3pm()).unwrap();
        assert!(w.contains(make(126, 8, 21, 10, 0)));
        assert!(!w.contains(make(126, 8, 22, 10, 0)), "tuesday");
        assert!(w.contains(make(126, 8, 23, 10, 0)));
    }

    #[test]
    fn a_typo_is_answered_with_the_word_it_meant() {
        let err = Window::parse("yesterdya", thursday_3pm()).unwrap_err().to_string();
        assert!(err.contains("did you mean `yesterday`"), "{err}");
        assert!(Window::parse("wed..mon", thursday_3pm()).is_err());
    }
}
