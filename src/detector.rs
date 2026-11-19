//! Change detection: diff freshly fetched fines against stored ones, and compute discount
//! schedules. Pure functions, no I/O.

use std::collections::{HashMap, HashSet};

use crate::calendar::{self, FIFTY_PERCENT_BUSINESS_DAYS, TWENTY_FIVE_PERCENT_BUSINESS_DAYS};
use crate::model::{DiscountSchedule, DiscountStage, FineStatus};
use crate::simit::SimitFine;

/// What the store knows about one fine of a vehicle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFine {
    pub id: i64,
    pub simit_ref: String,
    pub raw_hash: String,
    pub status: FineStatus,
    pub paid_by_user: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Diff {
    /// Not stored yet.
    pub new: Vec<SimitFine>,
    /// Stored, but SIMIT's data changed: (stored id, fresh data).
    pub updated: Vec<(i64, SimitFine)>,
    /// Stored, unchanged: refresh `last_seen_at` only.
    pub unchanged: Vec<i64>,
    /// Stored as pending/other but no longer listed on SIMIT.
    pub resolved: Vec<i64>,
}

pub fn diff(stored: &[StoredFine], fetched: &[SimitFine]) -> Diff {
    let by_ref: HashMap<&str, &StoredFine> =
        stored.iter().map(|s| (s.simit_ref.as_str(), s)).collect();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out = Diff::default();

    for f in fetched {
        if !seen.insert(f.simit_ref.as_str()) {
            continue; // duplicated row in the response
        }
        match by_ref.get(f.simit_ref.as_str()) {
            None => out.new.push(f.clone()),
            Some(s) if s.raw_hash != f.raw_hash() => out.updated.push((s.id, f.clone())),
            Some(s) => out.unchanged.push(s.id),
        }
    }
    for s in stored {
        if !seen.contains(s.simit_ref.as_str()) && s.status != FineStatus::Paid {
            out.resolved.push(s.id);
        }
    }
    out
}

/// Discount schedule for a fine, as of `now`. The windows count business days from the
/// notification date when known (photo fines), else from the date of the comparendo.
pub fn discount_schedule(issued_at: i64, notified_at: Option<i64>, now: i64) -> DiscountSchedule {
    let base = calendar::date_of(notified_at.unwrap_or(issued_at));
    let fifty_day = calendar::add_business_days(base, FIFTY_PERCENT_BUSINESS_DAYS);
    let twenty_five_day = calendar::add_business_days(base, TWENTY_FIVE_PERCENT_BUSINESS_DAYS);
    let fifty_until = calendar::end_of_day(fifty_day);
    let twenty_five_until = calendar::end_of_day(twenty_five_day);
    let today = calendar::date_of(now);

    let (stage, left, window) = if now <= fifty_until {
        (
            DiscountStage::Fifty,
            calendar::business_days_between(today, fifty_day),
            FIFTY_PERCENT_BUSINESS_DAYS,
        )
    } else if now <= twenty_five_until {
        (
            DiscountStage::TwentyFive,
            calendar::business_days_between(today, twenty_five_day),
            TWENTY_FIVE_PERCENT_BUSINESS_DAYS - FIFTY_PERCENT_BUSINESS_DAYS,
        )
    } else {
        (DiscountStage::Full, 0, 0)
    };
    DiscountSchedule {
        stage,
        fifty_until,
        twenty_five_until,
        business_days_left: left,
        window_business_days: window,
    }
}

/// When to remind about the 50% window closing: 9:00 Bogotá on the business day before the last
/// day. `None` when the window is already closed at `now`.
pub fn discount_reminder_at(issued_at: i64, notified_at: Option<i64>, now: i64) -> Option<i64> {
    let sched = discount_schedule(issued_at, notified_at, now);
    if sched.stage != DiscountStage::Fifty {
        return None;
    }
    let last_day = calendar::date_of(sched.fifty_until);
    let remind = calendar::ts_at(calendar::previous_business_day(last_day), 9, 0);
    Some(remind.max(now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn fine(r: &str, amount: i64) -> SimitFine {
        SimitFine {
            simit_ref: r.into(),
            infraction_code: "C02".into(),
            infraction_description: None,
            place: Some("Bogotá".into()),
            amount,
            issued_at: 1_000,
            notified_at: None,
            status: FineStatus::Pending,
        }
    }

    fn stored(id: i64, f: &SimitFine, status: FineStatus) -> StoredFine {
        StoredFine {
            id,
            simit_ref: f.simit_ref.clone(),
            raw_hash: f.raw_hash(),
            status,
            paid_by_user: false,
        }
    }

    #[test]
    fn diff_new_updated_unchanged_resolved() {
        let a = fine("A", 100);
        let b = fine("B", 200);
        let c = fine("C", 300);
        let d = fine("D", 400);
        let st = vec![
            stored(1, &a, FineStatus::Pending),
            stored(2, &b, FineStatus::Pending),
            stored(3, &c, FineStatus::Pending),
            stored(4, &d, FineStatus::Paid),
        ];
        let b2 = fine("B", 250);
        let e = fine("E", 500);
        let out = diff(&st, &[a.clone(), b2.clone(), e.clone(), e.clone()]);
        assert_eq!(out.new, vec![e]);
        assert_eq!(out.updated, vec![(2, b2)]);
        assert_eq!(out.unchanged, vec![1]);
        assert_eq!(out.resolved, vec![3]); // D was already paid
    }

    #[test]
    fn diff_is_stable_when_nothing_changes() {
        let a = fine("A", 100);
        let out = diff(&[stored(1, &a, FineStatus::Pending)], &[a]);
        assert!(out.new.is_empty() && out.updated.is_empty() && out.resolved.is_empty());
    }

    #[test]
    fn discount_stages() {
        // Comparendo Friday 2026-11-13; Mon 16 is a holiday -> 50% until Mon 23, 25% until Tue Dec 15
        // (Dec 8 holiday).
        let issued = calendar::ts_at(NaiveDate::from_ymd_opt(2026, 11, 13).unwrap(), 10, 0);
        let now = calendar::ts_at(NaiveDate::from_ymd_opt(2026, 11, 18).unwrap(), 6, 2);
        let s = discount_schedule(issued, None, now);
        assert_eq!(s.stage, DiscountStage::Fifty);
        assert_eq!(
            calendar::date_of(s.fifty_until),
            NaiveDate::from_ymd_opt(2026, 11, 23).unwrap()
        );
        assert_eq!(
            calendar::date_of(s.twenty_five_until),
            NaiveDate::from_ymd_opt(2026, 12, 15).unwrap()
        );
        assert_eq!(s.business_days_left, 4); // 18, 19, 20, 23
        assert_eq!(s.window_business_days, 5);

        let later = calendar::ts_at(NaiveDate::from_ymd_opt(2026, 11, 24).unwrap(), 6, 0);
        assert_eq!(
            discount_schedule(issued, None, later).stage,
            DiscountStage::TwentyFive
        );
        let much_later = calendar::ts_at(NaiveDate::from_ymd_opt(2027, 1, 30).unwrap(), 6, 0);
        let full = discount_schedule(issued, None, much_later);
        assert_eq!(full.stage, DiscountStage::Full);
        assert_eq!(full.business_days_left, 0);

        let remind = discount_reminder_at(issued, None, now).unwrap();
        assert_eq!(
            remind,
            calendar::ts_at(NaiveDate::from_ymd_opt(2026, 11, 20).unwrap(), 9, 0)
        );
        assert_eq!(discount_reminder_at(issued, None, later), None);
    }
}
