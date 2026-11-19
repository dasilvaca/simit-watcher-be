//! Check runner: one full pass over all plates. It does not decide *when* to run; the OS does.
//!
//! Guarantees:
//! * Idempotent: plates checked recently are skipped, alerts are unique per (fine, kind), and the
//!   report lists undelivered alerts until the shell acknowledges them.
//! * Resumable: each plate is committed on its own; plates are visited oldest-check first, so a
//!   run cut short by the time budget resumes with the plates it did not reach.
//! * Polite: random jitter between plates, bounded retries with backoff, and a hard stop on the
//!   first "blocked" answer.

use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::detector;
use crate::model::*;
use crate::simit::{LookupError, SimitClient};
use crate::store;

pub const DAY: i64 = 86_400;
/// Manual checks: at most one per plate every 10 minutes.
pub const MANUAL_MIN_INTERVAL_S: i64 = 10 * 60;
/// Scheduled checks skip plates checked successfully in the last 2 hours (re-runs are no-ops).
pub const SCHEDULED_MIN_INTERVAL_S: i64 = 2 * 3600;
/// After this long without a successful run, emit a `checks_failing` alert.
pub const FAILING_AFTER_S: i64 = DAY;

#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub jitter_min_ms: u64,
    pub jitter_max_ms: u64,
    pub retry_backoff_ms: Vec<u64>,
    /// Do not start a plate with less than this left in the budget.
    pub min_plate_budget_ms: u64,
    /// Lookup timeout for a plate's first (baseline) check, which may bring its whole history at
    /// once. Capped by the budget left. Later checks use the normal HTTP timeout: SIMIT answers
    /// fast for small changes, and a slow answer then is a real failure.
    pub first_check_timeout_ms: u64,
}

struct Budget {
    start: Instant,
    total: Duration,
}

impl Budget {
    fn left_ms(&self) -> u64 {
        self.total.saturating_sub(self.start.elapsed()).as_millis() as u64
    }
    fn elapsed_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

fn sleep_ms(ms: u64) {
    if ms > 0 {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

fn storage(e: rusqlite::Error) -> crate::CoreError {
    crate::CoreError::Storage {
        detail: e.to_string(),
    }
}

pub fn run(
    conn: &mut Connection,
    client: &SimitClient,
    cfg: &RunnerConfig,
    now: i64,
    trigger: CheckTrigger,
    budget_ms: u32,
) -> Result<CheckReport, crate::CoreError> {
    let budget = Budget {
        start: Instant::now(),
        total: Duration::from_millis(budget_ms as u64),
    };
    let run_id = store::start_run(conn, trigger, now).map_err(storage)?;
    let targets = store::check_targets(conn).map_err(storage)?;

    let mut outcomes = Vec::with_capacity(targets.len());
    let mut new_fine_ids = Vec::new();
    let mut changes = Vec::new();
    let mut cancelled = Vec::new();
    let mut checked = 0u32;
    let mut remaining = 0u32;
    let mut budget_exhausted = false;
    let mut stop = false;
    let mut worst: Option<ErrorClass> = None;
    let mut storage_error: Option<crate::CoreError> = None;
    let mut contacted_any = false;

    for t in &targets {
        let outcome = |kind, error: Option<&LookupError>| PlateOutcome {
            vehicle_id: t.id,
            plate: t.plate.clone(),
            outcome: kind,
            error: error.map(|e| e.class),
            http_status: error.and_then(|e| e.http_status),
        };

        let recent = match trigger {
            CheckTrigger::Manual => t
                .last_attempt_at
                .is_some_and(|a| now - a < MANUAL_MIN_INTERVAL_S),
            CheckTrigger::Scheduled => t
                .last_checked_at
                .is_some_and(|c| now - c < SCHEDULED_MIN_INTERVAL_S),
        };
        if recent {
            outcomes.push(outcome(PlateOutcomeKind::SkippedRecent, None));
            continue;
        }
        if stop || budget.left_ms() < cfg.min_plate_budget_ms {
            budget_exhausted |= !stop;
            remaining += 1;
            outcomes.push(outcome(PlateOutcomeKind::NotReached, None));
            continue;
        }

        if contacted_any {
            let jitter =
                fastrand::u64(cfg.jitter_min_ms..=cfg.jitter_max_ms.max(cfg.jitter_min_ms));
            sleep_ms(jitter.min(budget.left_ms().saturating_sub(cfg.min_plate_budget_ms)));
        }
        contacted_any = true;
        if let Err(e) = store::mark_attempt(conn, t.id, now) {
            storage_error = Some(storage(e));
            break;
        }

        let lookup = || {
            let timeout = (!t.baseline_done).then(|| {
                cfg.first_check_timeout_ms
                    .min(budget.left_ms())
                    .min(u32::MAX as u64) as u32
            });
            client.lookup(&t.plate, now + (budget.elapsed_ms() / 1000) as i64, timeout)
        };
        let mut result = lookup();
        for backoff in &cfg.retry_backoff_ms {
            match &result {
                Err(e) if e.retryable() && budget.left_ms() > backoff + cfg.min_plate_budget_ms => {
                    sleep_ms(*backoff);
                    result = lookup();
                }
                _ => break,
            }
        }

        match result {
            Ok(fines) => match apply(conn, t, &fines, now) {
                Ok(applied) => {
                    checked += 1;
                    new_fine_ids.extend(applied.new_alerting);
                    changes.extend(applied.changes);
                    cancelled.extend(applied.cancelled);
                    outcomes.push(outcome(PlateOutcomeKind::Checked, None));
                }
                Err(e) => {
                    storage_error = Some(storage(e));
                    break;
                }
            },
            Err(e) => {
                if worst.is_none_or(|w| e.class.run_result().severity() > w.run_result().severity())
                {
                    worst = Some(e.class);
                }
                if e.class == ErrorClass::Blocked {
                    stop = true; // be polite: no more requests this run
                }
                outcomes.push(outcome(PlateOutcomeKind::Failed, Some(&e)));
            }
        }
    }

    if let Some(err) = storage_error {
        let _ = store::finish_run(
            conn,
            run_id,
            now + (budget.elapsed_ms() / 1000) as i64,
            RunResult::Error,
            Some(ErrorClass::Storage),
            checked,
            remaining,
            budget.elapsed_ms(),
        );
        return Err(err);
    }

    let result = worst.map(|w| w.run_result()).unwrap_or(RunResult::Ok);
    let duration_ms = budget.elapsed_ms();
    let finished_at = now + (duration_ms / 1000) as i64;
    store::finish_run(
        conn,
        run_id,
        finished_at,
        result,
        worst,
        checked,
        remaining,
        duration_ms,
    )
    .map_err(storage)?;

    // Checks-failing alert, at most once per failing streak.
    if result == RunResult::Ok {
        let mut st = conn
            .prepare(
                "UPDATE alert SET cancelled_at = ?1 WHERE kind = 'checks_failing'
                 AND delivered_at IS NULL AND cancelled_at IS NULL RETURNING id",
            )
            .map_err(storage)?;
        let ids: Vec<i64> = st
            .query_map([now], |r| r.get(0))
            .map_err(storage)?
            .collect::<Result<_, _>>()
            .map_err(storage)?;
        cancelled.extend(ids);
    } else {
        let reference = match store::last_ok_at(conn).map_err(storage)? {
            Some(ok) => Some(ok),
            None => store::first_run_at(conn).map_err(storage)?,
        };
        if let Some(reference) = reference {
            let already = store::last_checks_failing_alert_at(conn)
                .map_err(storage)?
                .is_some_and(|a| a >= reference);
            if now - reference >= FAILING_AFTER_S && !already {
                store::insert_alert(conn, None, AlertKind::ChecksFailing, now, now)
                    .map_err(storage)?;
            }
        }
    }

    let new_fines = new_fine_ids
        .iter()
        .filter_map(|id| store::get_fine(conn, *id, now).transpose())
        .collect::<Result<Vec<_>, _>>()
        .map_err(storage)?;
    let alerts = store::pending_alerts(conn, now).map_err(storage)?;

    Ok(CheckReport {
        run_id,
        result,
        error: worst,
        started_at: now,
        duration_ms,
        outcomes,
        budget_exhausted,
        new_fines,
        changes,
        alerts,
        cancelled_alert_ids: cancelled,
    })
}

struct Applied {
    /// New fines that raised a `new_fine` alert (baseline fines do not).
    new_alerting: Vec<i64>,
    changes: Vec<FineChange>,
    cancelled: Vec<i64>,
}

/// Stores one plate's fresh results in a single transaction.
fn apply(
    conn: &mut Connection,
    t: &store::CheckTarget,
    fetched: &[crate::simit::SimitFine],
    now: i64,
) -> rusqlite::Result<Applied> {
    let stored = store::stored_fines(conn, t.id)?;
    let diff = detector::diff(&stored, fetched);
    let baseline = !t.baseline_done;
    let tx = conn.transaction()?;
    let mut out = Applied {
        new_alerting: vec![],
        changes: vec![],
        cancelled: vec![],
    };

    for f in &diff.new {
        // Fines found on the very first check of a plate are the baseline: shown, not announced.
        let id = store::insert_fine(&tx, t.id, f, now, baseline)?;
        if f.status != FineStatus::Pending {
            continue;
        }
        if !baseline {
            store::insert_alert(&tx, Some(id), AlertKind::NewFine, now, now)?;
            out.new_alerting.push(id);
        }
        if let Some(at) = detector::discount_reminder_at(f.issued_at, f.notified_at, now) {
            store::insert_alert(&tx, Some(id), AlertKind::DiscountEnding, at, now)?;
        }
    }
    for (id, f) in &diff.updated {
        store::update_fine(&tx, *id, f, now)?;
        out.changes.push(FineChange {
            fine_id: *id,
            kind: FineChangeKind::Updated,
        });
        if f.status != FineStatus::Pending {
            out.cancelled
                .extend(store::cancel_alerts_for_fine(&tx, *id, now)?);
        }
    }
    for id in &diff.unchanged {
        store::touch_fine(&tx, *id, now)?;
    }
    for id in &diff.resolved {
        store::set_fine_status(&tx, *id, FineStatus::Paid)?;
        out.changes.push(FineChange {
            fine_id: *id,
            kind: FineChangeKind::Resolved,
        });
        out.cancelled
            .extend(store::cancel_alerts_for_fine(&tx, *id, now)?);
    }
    store::finish_vehicle_check(&tx, t.id, now)?;
    tx.commit()?;
    Ok(out)
}
