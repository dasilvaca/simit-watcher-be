//! SQLite store: connection setup, numbered migrations, queries. Knows nothing about HTTP.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use crate::detector::{self, StoredFine};
use crate::model::*;
use crate::simit::SimitFine;

pub type Result<T> = std::result::Result<T, rusqlite::Error>;

/// Each entry is one migration, applied in order inside a transaction. Never edit a shipped one;
/// append a new one instead. `PRAGMA user_version` records how many have run.
const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    r#"
    CREATE TABLE vehicle (
        id              INTEGER PRIMARY KEY,
        plate           TEXT NOT NULL UNIQUE,
        kind            TEXT NOT NULL CHECK (kind IN ('car', 'moto')),
        nickname        TEXT,
        created_at      INTEGER NOT NULL,
        last_attempt_at INTEGER,
        last_checked_at INTEGER,
        baseline_done   INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE fine (
        id                     INTEGER PRIMARY KEY,
        vehicle_id             INTEGER NOT NULL REFERENCES vehicle(id) ON DELETE CASCADE,
        simit_ref              TEXT NOT NULL,
        infraction_code        TEXT NOT NULL,
        infraction_description TEXT,
        place                  TEXT,
        amount                 INTEGER NOT NULL,
        issued_at              INTEGER NOT NULL,
        notified_at            INTEGER,
        status                 TEXT NOT NULL,
        first_seen_at          INTEGER NOT NULL,
        last_seen_at           INTEGER NOT NULL,
        raw_hash               TEXT NOT NULL,
        acknowledged_at        INTEGER,
        paid_by_user_at        INTEGER,
        UNIQUE (vehicle_id, simit_ref)
    );
    CREATE TABLE check_run (
        id               INTEGER PRIMARY KEY,
        started_at       INTEGER NOT NULL,
        finished_at      INTEGER,
        trigger          TEXT NOT NULL CHECK (trigger IN ('scheduled', 'manual')),
        result           TEXT CHECK (result IN ('ok', 'offline', 'blocked', 'error')),
        error_detail     TEXT,
        plates_checked   INTEGER NOT NULL DEFAULT 0,
        plates_remaining INTEGER NOT NULL DEFAULT 0,
        duration_ms      INTEGER
    );
    CREATE TABLE alert (
        id           INTEGER PRIMARY KEY,
        fine_id      INTEGER REFERENCES fine(id) ON DELETE CASCADE,
        kind         TEXT NOT NULL CHECK (kind IN ('new_fine', 'discount_ending', 'checks_failing')),
        due_at       INTEGER NOT NULL,
        delivered_at INTEGER,
        cancelled_at INTEGER,
        created_at   INTEGER NOT NULL
    );
    CREATE UNIQUE INDEX alert_fine_kind ON alert (fine_id, kind) WHERE fine_id IS NOT NULL;
    CREATE INDEX fine_vehicle ON fine (vehicle_id);
    CREATE INDEX check_run_started ON check_run (started_at);
    "#,
];

/// Number of `check_run` rows kept (about a month of twice-daily runs).
pub const KEEP_RUNS: i64 = 60;

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(conn)
}

pub fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

pub fn schema_version(conn: &Connection) -> Result<i64> {
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
}

// ---------- vehicles ----------

const VEHICLE_SELECT: &str = r#"
    SELECT v.id, v.plate, v.kind, v.nickname, v.created_at, v.last_checked_at,
           (SELECT COUNT(*) FROM fine f WHERE f.vehicle_id = v.id AND f.status = 'pending'),
           (SELECT COUNT(*) FROM fine f WHERE f.vehicle_id = v.id AND f.status = 'pending'
                                          AND f.acknowledged_at IS NULL)
    FROM vehicle v
"#;

fn vehicle_row(r: &Row) -> rusqlite::Result<Vehicle> {
    Ok(Vehicle {
        id: r.get(0)?,
        plate: r.get(1)?,
        kind: VehicleKind::parse(&r.get::<_, String>(2)?),
        nickname: r.get(3)?,
        created_at: r.get(4)?,
        last_checked_at: r.get(5)?,
        pending_fines: r.get(6)?,
        new_fines: r.get(7)?,
    })
}

pub fn plate_exists(conn: &Connection, plate: &str) -> Result<bool> {
    conn.query_row(
        "SELECT 1 FROM vehicle WHERE plate = ?1",
        [plate],
        |_| Ok(()),
    )
    .optional()
    .map(|o| o.is_some())
}

pub fn insert_vehicle(
    conn: &Connection,
    plate: &str,
    kind: VehicleKind,
    nickname: Option<&str>,
    now: i64,
) -> Result<Vehicle> {
    conn.execute(
        "INSERT INTO vehicle (plate, kind, nickname, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![plate, kind.as_str(), nickname, now],
    )?;
    get_vehicle(conn, conn.last_insert_rowid()).map(|v| v.expect("just inserted"))
}

pub fn get_vehicle(conn: &Connection, id: i64) -> Result<Option<Vehicle>> {
    conn.query_row(
        &format!("{VEHICLE_SELECT} WHERE v.id = ?1"),
        [id],
        vehicle_row,
    )
    .optional()
}

pub fn list_vehicles(conn: &Connection) -> Result<Vec<Vehicle>> {
    let mut st = conn.prepare(&format!("{VEHICLE_SELECT} ORDER BY v.created_at, v.id"))?;
    st.query_map([], vehicle_row)?.collect()
}

pub fn update_nickname(conn: &Connection, id: i64, nickname: Option<&str>) -> Result<usize> {
    conn.execute(
        "UPDATE vehicle SET nickname = ?2 WHERE id = ?1",
        params![id, nickname],
    )
}

pub fn delete_vehicle(conn: &Connection, id: i64) -> Result<usize> {
    conn.execute("DELETE FROM vehicle WHERE id = ?1", [id])
}

/// Vehicles in check order: never-checked first, then oldest check first. This is what makes an
/// interrupted run resume with the plates it did not reach.
pub struct CheckTarget {
    pub id: i64,
    pub plate: String,
    pub last_attempt_at: Option<i64>,
    pub last_checked_at: Option<i64>,
    pub baseline_done: bool,
}

pub fn check_targets(conn: &Connection) -> Result<Vec<CheckTarget>> {
    let mut st = conn.prepare(
        "SELECT id, plate, last_attempt_at, last_checked_at, baseline_done FROM vehicle
         ORDER BY last_checked_at IS NOT NULL, last_checked_at, id",
    )?;
    st.query_map([], |r| {
        Ok(CheckTarget {
            id: r.get(0)?,
            plate: r.get(1)?,
            last_attempt_at: r.get(2)?,
            last_checked_at: r.get(3)?,
            baseline_done: r.get::<_, i64>(4)? != 0,
        })
    })?
    .collect()
}

pub fn mark_attempt(conn: &Connection, vehicle_id: i64, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE vehicle SET last_attempt_at = ?2 WHERE id = ?1",
        params![vehicle_id, now],
    )?;
    Ok(())
}

// ---------- fines ----------

const FINE_SELECT: &str = r#"
    SELECT f.id, f.vehicle_id, v.plate, f.simit_ref, f.infraction_code, f.infraction_description,
           f.place, f.amount, f.issued_at, f.notified_at, f.status, f.first_seen_at, f.last_seen_at,
           f.acknowledged_at, f.paid_by_user_at
    FROM fine f JOIN vehicle v ON v.id = f.vehicle_id
"#;

fn fine_row(r: &Row, now: i64) -> rusqlite::Result<Fine> {
    let issued_at: i64 = r.get(8)?;
    let notified_at: Option<i64> = r.get(9)?;
    let status = FineStatus::parse(&r.get::<_, String>(10)?);
    Ok(Fine {
        id: r.get(0)?,
        vehicle_id: r.get(1)?,
        plate: r.get(2)?,
        simit_ref: r.get(3)?,
        infraction_code: r.get(4)?,
        infraction_description: r.get(5)?,
        place: r.get(6)?,
        amount: r.get(7)?,
        issued_at,
        notified_at,
        status,
        first_seen_at: r.get(11)?,
        last_seen_at: r.get(12)?,
        is_new: status == FineStatus::Pending && r.get::<_, Option<i64>>(13)?.is_none(),
        paid_by_user: r.get::<_, Option<i64>>(14)?.is_some(),
        discount: detector::discount_schedule(issued_at, notified_at, now),
    })
}

pub fn get_fine(conn: &Connection, id: i64, now: i64) -> Result<Option<Fine>> {
    conn.query_row(&format!("{FINE_SELECT} WHERE f.id = ?1"), [id], |r| {
        fine_row(r, now)
    })
    .optional()
}

/// Fines of one vehicle (or all when `None`): pending first, newest first.
pub fn list_fines(conn: &Connection, vehicle_id: Option<i64>, now: i64) -> Result<Vec<Fine>> {
    let order = "ORDER BY f.status != 'pending', f.issued_at DESC, f.id DESC";
    match vehicle_id {
        Some(id) => {
            let mut st = conn.prepare(&format!("{FINE_SELECT} WHERE f.vehicle_id = ?1 {order}"))?;
            st.query_map([id], |r| fine_row(r, now))?.collect()
        }
        None => {
            let mut st = conn.prepare(&format!("{FINE_SELECT} {order}"))?;
            st.query_map([], |r| fine_row(r, now))?.collect()
        }
    }
}

pub fn stored_fines(conn: &Connection, vehicle_id: i64) -> Result<Vec<StoredFine>> {
    let mut st = conn.prepare(
        "SELECT id, simit_ref, raw_hash, status, paid_by_user_at FROM fine WHERE vehicle_id = ?1",
    )?;
    st.query_map([vehicle_id], |r| {
        Ok(StoredFine {
            id: r.get(0)?,
            simit_ref: r.get(1)?,
            raw_hash: r.get(2)?,
            status: FineStatus::parse(&r.get::<_, String>(3)?),
            paid_by_user: r.get::<_, Option<i64>>(4)?.is_some(),
        })
    })?
    .collect()
}

pub fn insert_fine(
    tx: &Transaction,
    vehicle_id: i64,
    f: &SimitFine,
    now: i64,
    acknowledged: bool,
) -> Result<i64> {
    tx.execute(
        "INSERT INTO fine (vehicle_id, simit_ref, infraction_code, infraction_description, place,
                           amount, issued_at, notified_at, status, first_seen_at, last_seen_at,
                           raw_hash, acknowledged_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11, ?12)",
        params![
            vehicle_id,
            f.simit_ref,
            f.infraction_code,
            f.infraction_description,
            f.place,
            f.amount,
            f.issued_at,
            f.notified_at,
            f.status.as_str(),
            now,
            f.raw_hash(),
            acknowledged.then_some(now),
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

/// Applies fresh SIMIT data to a stored fine. A fine the user marked as paid stays paid even if
/// SIMIT still lists it (SIMIT can take days to reflect a payment).
pub fn update_fine(tx: &Transaction, id: i64, f: &SimitFine, now: i64) -> Result<()> {
    tx.execute(
        "UPDATE fine SET infraction_code = ?2, infraction_description = ?3, place = ?4, amount = ?5,
                issued_at = ?6, notified_at = ?7,
                status = CASE WHEN paid_by_user_at IS NOT NULL THEN 'paid' ELSE ?8 END,
                last_seen_at = ?9, raw_hash = ?10
         WHERE id = ?1",
        params![
            id,
            f.infraction_code,
            f.infraction_description,
            f.place,
            f.amount,
            f.issued_at,
            f.notified_at,
            f.status.as_str(),
            now,
            f.raw_hash(),
        ],
    )?;
    Ok(())
}

pub fn touch_fine(tx: &Transaction, id: i64, now: i64) -> Result<()> {
    tx.execute(
        "UPDATE fine SET last_seen_at = ?2 WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

pub fn set_fine_status(conn: &Connection, id: i64, status: FineStatus) -> Result<usize> {
    conn.execute(
        "UPDATE fine SET status = ?2 WHERE id = ?1",
        params![id, status.as_str()],
    )
}

pub fn mark_paid_by_user(conn: &Connection, id: i64, now: i64) -> Result<usize> {
    conn.execute(
        "UPDATE fine SET status = 'paid', paid_by_user_at = COALESCE(paid_by_user_at, ?2),
                         acknowledged_at = COALESCE(acknowledged_at, ?2)
         WHERE id = ?1",
        params![id, now],
    )
}

pub fn acknowledge_fine(conn: &Connection, id: i64, now: i64) -> Result<usize> {
    conn.execute(
        "UPDATE fine SET acknowledged_at = COALESCE(acknowledged_at, ?2) WHERE id = ?1",
        params![id, now],
    )
}

pub fn finish_vehicle_check(tx: &Transaction, vehicle_id: i64, now: i64) -> Result<()> {
    tx.execute(
        "UPDATE vehicle SET last_checked_at = ?2, last_attempt_at = ?2, baseline_done = 1 WHERE id = ?1",
        params![vehicle_id, now],
    )?;
    Ok(())
}

// ---------- alerts ----------

pub fn insert_alert(
    conn: &Connection,
    fine_id: Option<i64>,
    kind: AlertKind,
    due_at: i64,
    now: i64,
) -> Result<Option<i64>> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO alert (fine_id, kind, due_at, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![fine_id, kind.as_str(), due_at, now],
    )?;
    Ok((n > 0).then(|| conn.last_insert_rowid()))
}

/// Re-arms a cancelled reminder (user tapped "Recordarme"), or creates it.
pub fn upsert_alert(
    conn: &Connection,
    fine_id: i64,
    kind: AlertKind,
    due_at: i64,
    now: i64,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO alert (fine_id, kind, due_at, created_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (fine_id, kind) WHERE fine_id IS NOT NULL
         DO UPDATE SET due_at = excluded.due_at, cancelled_at = NULL, delivered_at = NULL",
        params![fine_id, kind.as_str(), due_at, now],
    )?;
    conn.query_row(
        "SELECT id FROM alert WHERE fine_id = ?1 AND kind = ?2",
        params![fine_id, kind.as_str()],
        |r| r.get(0),
    )
}

/// Cancels a fine's pending alerts and returns their ids.
pub fn cancel_alerts_for_fine(conn: &Connection, fine_id: i64, now: i64) -> Result<Vec<i64>> {
    let mut st = conn.prepare(
        "UPDATE alert SET cancelled_at = ?2
         WHERE fine_id = ?1 AND delivered_at IS NULL AND cancelled_at IS NULL RETURNING id",
    )?;
    st.query_map(params![fine_id, now], |r| r.get(0))?.collect()
}

pub fn mark_alerts_delivered(conn: &Connection, ids: &[i64], now: i64) -> Result<()> {
    let mut st =
        conn.prepare("UPDATE alert SET delivered_at = COALESCE(delivered_at, ?2) WHERE id = ?1")?;
    for id in ids {
        st.execute(params![id, now])?;
    }
    Ok(())
}

pub fn last_checks_failing_alert_at(conn: &Connection) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT MAX(created_at) FROM alert WHERE kind = 'checks_failing'",
        [],
        |r| r.get(0),
    )
}

pub fn pending_alerts(conn: &Connection, now: i64) -> Result<Vec<Alert>> {
    let mut st = conn.prepare(
        "SELECT a.id, a.kind, a.due_at, a.fine_id, v.id, v.plate, v.nickname, v.kind,
                f.infraction_code, f.place, f.amount, f.issued_at, f.notified_at
         FROM alert a
         LEFT JOIN fine f ON f.id = a.fine_id
         LEFT JOIN vehicle v ON v.id = f.vehicle_id
         WHERE a.delivered_at IS NULL AND a.cancelled_at IS NULL
         ORDER BY a.due_at, a.id",
    )?;
    let last_ok = last_ok_at(conn)?;
    st.query_map([], |r| {
        let kind = AlertKind::parse(&r.get::<_, String>(1)?);
        let due_at: i64 = r.get(2)?;
        let issued_at: Option<i64> = r.get(11)?;
        let notified_at: Option<i64> = r.get(12)?;
        let sched = issued_at.map(|i| detector::discount_schedule(i, notified_at, due_at.max(now)));
        Ok(Alert {
            id: r.get(0)?,
            kind,
            due_at,
            fine_id: r.get(3)?,
            vehicle_id: r.get(4)?,
            plate: r.get(5)?,
            nickname: r.get(6)?,
            vehicle_kind: r
                .get::<_, Option<String>>(7)?
                .map(|k| VehicleKind::parse(&k)),
            infraction_code: r.get(8)?,
            place: r.get(9)?,
            amount: r.get(10)?,
            discount_stage: sched.as_ref().map(|s| s.stage),
            business_days_left: sched.as_ref().map(|s| s.business_days_left),
            discount_deadline: sched.as_ref().map(|s| match s.stage {
                DiscountStage::Fifty => s.fifty_until,
                _ => s.twenty_five_until,
            }),
            last_ok_at: (kind == AlertKind::ChecksFailing)
                .then_some(last_ok)
                .flatten(),
        })
    })?
    .collect()
}

// ---------- runs ----------

pub fn start_run(conn: &Connection, trigger: CheckTrigger, now: i64) -> Result<i64> {
    // A run left unfinished was killed by the OS: close it as interrupted.
    conn.execute(
        "UPDATE check_run SET finished_at = started_at, result = 'error', error_detail = 'interrupted'
         WHERE finished_at IS NULL",
        [],
    )?;
    conn.execute(
        "INSERT INTO check_run (started_at, trigger) VALUES (?1, ?2)",
        params![now, trigger.as_str()],
    )?;
    Ok(conn.last_insert_rowid())
}

#[allow(clippy::too_many_arguments)]
pub fn finish_run(
    conn: &Connection,
    id: i64,
    finished_at: i64,
    result: RunResult,
    error: Option<ErrorClass>,
    checked: u32,
    remaining: u32,
    duration_ms: u64,
) -> Result<()> {
    conn.execute(
        "UPDATE check_run SET finished_at = ?2, result = ?3, error_detail = ?4, plates_checked = ?5,
                plates_remaining = ?6, duration_ms = ?7 WHERE id = ?1",
        params![
            id,
            finished_at,
            result.as_str(),
            error.map(|e| e.as_str()),
            checked,
            remaining,
            duration_ms as i64
        ],
    )?;
    conn.execute(
        "DELETE FROM check_run WHERE id NOT IN (SELECT id FROM check_run ORDER BY started_at DESC, id DESC LIMIT ?1)",
        [KEEP_RUNS],
    )?;
    Ok(())
}

pub fn recent_runs(conn: &Connection, limit: u32) -> Result<Vec<RunSummary>> {
    let mut st = conn.prepare(
        "SELECT id, started_at, finished_at, trigger, result, error_detail, plates_checked,
                plates_remaining, duration_ms
         FROM check_run ORDER BY started_at DESC, id DESC LIMIT ?1",
    )?;
    st.query_map([limit], |r| {
        Ok(RunSummary {
            id: r.get(0)?,
            started_at: r.get(1)?,
            finished_at: r.get(2)?,
            trigger: CheckTrigger::parse(&r.get::<_, String>(3)?),
            result: r.get::<_, Option<String>>(4)?.map(|s| RunResult::parse(&s)),
            error: r
                .get::<_, Option<String>>(5)?
                .and_then(|s| ErrorClass::parse(&s)),
            plates_checked: r.get(6)?,
            plates_remaining: r.get(7)?,
            duration_ms: r.get::<_, Option<i64>>(8)?.map(|d| d as u64),
        })
    })?
    .collect()
}

pub fn last_ok_at(conn: &Connection) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT MAX(COALESCE(finished_at, started_at)) FROM check_run WHERE result = 'ok'",
        [],
        |r| r.get(0),
    )
}

pub fn first_run_at(conn: &Connection) -> Result<Option<i64>> {
    conn.query_row("SELECT MIN(started_at) FROM check_run", [], |r| r.get(0))
}

pub fn wipe(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "DELETE FROM alert; DELETE FROM fine; DELETE FROM check_run; DELETE FROM vehicle;",
    )
}
