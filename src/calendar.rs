//! Colombian business-day calendar (Bogotá time, UTC-5, no DST) and discount deadlines.
//!
//! Holidays follow Ley 51 de 1983 ("Ley Emiliani"): some are fixed, some move to the next Monday,
//! and some depend on Easter.

use chrono::{Datelike, Duration, FixedOffset, NaiveDate, NaiveTime, TimeZone, Weekday};

pub fn bogota() -> FixedOffset {
    FixedOffset::west_opt(5 * 3600).expect("valid offset")
}

pub fn date_of(ts: i64) -> NaiveDate {
    bogota()
        .timestamp_opt(ts, 0)
        .single()
        .expect("valid timestamp")
        .date_naive()
}

/// Unix timestamp of `date` at `hour:minute` Bogotá time.
pub fn ts_at(date: NaiveDate, hour: u32, minute: u32) -> i64 {
    let t = NaiveTime::from_hms_opt(hour, minute, 0).expect("valid time");
    bogota()
        .from_local_datetime(&date.and_time(t))
        .single()
        .expect("fixed offset is unambiguous")
        .timestamp()
}

/// Last second of `date` in Bogotá.
pub fn end_of_day(date: NaiveDate) -> i64 {
    ts_at(date, 0, 0) + 86_400 - 1
}

fn easter(year: i32) -> NaiveDate {
    // Anonymous Gregorian algorithm.
    let a = year % 19;
    let b = year / 100;
    let c = year % 100;
    let d = b / 4;
    let e = b % 4;
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15) % 30;
    let i = c / 4;
    let k = c % 4;
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let m = (a + 11 * h + 22 * l) / 451;
    let month = (h + l - 7 * m + 114) / 31;
    let day = (h + l - 7 * m + 114) % 31 + 1;
    NaiveDate::from_ymd_opt(year, month as u32, day as u32).expect("valid easter")
}

fn next_monday(d: NaiveDate) -> NaiveDate {
    let mut d = d;
    while d.weekday() != Weekday::Mon {
        d += Duration::days(1);
    }
    d
}

pub fn holidays(year: i32) -> Vec<NaiveDate> {
    let ymd = |m, d| NaiveDate::from_ymd_opt(year, m, d).expect("valid date");
    let e = easter(year);
    let mut v = vec![
        ymd(1, 1),
        ymd(5, 1),
        ymd(7, 20),
        ymd(8, 7),
        ymd(12, 8),
        ymd(12, 25),
        e - Duration::days(3), // Jueves Santo
        e - Duration::days(2), // Viernes Santo
    ];
    for (m, d) in [
        (1, 6),
        (3, 19),
        (6, 29),
        (8, 15),
        (10, 12),
        (11, 1),
        (11, 11),
    ] {
        v.push(next_monday(ymd(m, d)));
    }
    v.push(e + Duration::days(43)); // Ascensión
    v.push(e + Duration::days(64)); // Corpus Christi
    v.push(e + Duration::days(71)); // Sagrado Corazón
    v.sort();
    v
}

pub fn is_business_day(d: NaiveDate) -> bool {
    !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) && !holidays(d.year()).contains(&d)
}

/// The `n`-th business day strictly after `from`.
pub fn add_business_days(from: NaiveDate, n: u32) -> NaiveDate {
    let mut d = from;
    let mut left = n;
    while left > 0 {
        d += Duration::days(1);
        if is_business_day(d) {
            left -= 1;
        }
    }
    d
}

/// The business day strictly before `d`.
pub fn previous_business_day(d: NaiveDate) -> NaiveDate {
    let mut d = d - Duration::days(1);
    while !is_business_day(d) {
        d -= Duration::days(1);
    }
    d
}

/// Business days from `today` (inclusive, if it is one) to `deadline` (inclusive).
pub fn business_days_between(today: NaiveDate, deadline: NaiveDate) -> u32 {
    if deadline < today {
        return 0;
    }
    let mut n = 0;
    let mut d = today;
    while d <= deadline {
        if is_business_day(d) {
            n += 1;
        }
        d += Duration::days(1);
    }
    n
}

/// Days after the comparendo (or its notification) for each discount (Art. 136 Código Nacional
/// de Tránsito, as amended by Ley 1383 de 2010): 50% within 5 business days, 25% within 20.
pub const FIFTY_PERCENT_BUSINESS_DAYS: u32 = 5;
pub const TWENTY_FIVE_PERCENT_BUSINESS_DAYS: u32 = 20;

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn easter_dates() {
        assert_eq!(easter(2025), d(2025, 4, 20));
        assert_eq!(easter(2026), d(2026, 4, 5));
    }

    #[test]
    fn colombian_holidays_2026() {
        let h = holidays(2026);
        for x in [
            d(2026, 1, 1),
            d(2026, 1, 12),  // Reyes moved
            d(2026, 3, 23),  // San José moved
            d(2026, 4, 2),   // Jueves Santo
            d(2026, 4, 3),   // Viernes Santo
            d(2026, 5, 18),  // Ascensión
            d(2026, 6, 8),   // Corpus Christi
            d(2026, 6, 15),  // Sagrado Corazón
            d(2026, 6, 29),  // San Pedro (Monday already)
            d(2026, 11, 2),  // Todos los Santos moved
            d(2026, 11, 16), // Independencia de Cartagena moved
            d(2026, 12, 8),
        ] {
            assert!(h.contains(&x), "missing {x}");
        }
        assert_eq!(h.len(), 18);
    }

    #[test]
    fn business_day_math() {
        // Friday 2026-11-13; Monday 16 is a holiday.
        let fri = d(2026, 11, 13);
        assert_eq!(add_business_days(fri, 1), d(2026, 11, 17));
        assert_eq!(add_business_days(fri, 5), d(2026, 11, 23));
        assert_eq!(previous_business_day(d(2026, 11, 17)), fri);
        assert_eq!(business_days_between(d(2026, 11, 17), d(2026, 11, 23)), 5);
        assert_eq!(business_days_between(d(2026, 11, 24), d(2026, 11, 23)), 0);
    }

    #[test]
    fn timestamps_are_bogota() {
        let ts = ts_at(d(2026, 11, 18), 6, 0);
        assert_eq!(ts, 1_794_999_600); // 2026-11-18T11:00:00Z
        assert_eq!(date_of(ts), d(2026, 11, 18));
        assert_eq!(date_of(end_of_day(d(2026, 11, 18))), d(2026, 11, 18));
    }
}
