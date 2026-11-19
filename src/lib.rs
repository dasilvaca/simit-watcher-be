//! SAPPA shared core.
//!
//! Owns the SIMIT lookup, parsing, change detection and the on-device SQLite file. It draws no UI
//! and does no scheduling: a platform shell wakes it, calls it, and acts on what it returns.
//! Everything returned is codes and values; shells turn them into localized text.

pub mod calendar;
pub mod detector;
pub mod model;
pub mod plate;
pub mod runner;
pub mod simit;
pub mod store;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::Connection;
use serde::Serialize;

pub use model::*;
pub use simit::{HttpHeader, HttpRequest, HttpResponse, HttpTransport, TransportError};

uniffi::setup_scaffolding!();

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum CoreError {
    #[error("invalid plate: {reason:?}")]
    InvalidPlate { reason: PlateError },
    #[error("plate already registered")]
    DuplicatePlate,
    #[error("not found")]
    NotFound,
    #[error("a check is already running")]
    AlreadyRunning,
    #[error("storage: {detail}")]
    Storage { detail: String },
    #[error("config: {detail}")]
    Config { detail: String },
}

impl From<rusqlite::Error> for CoreError {
    fn from(e: rusqlite::Error) -> Self {
        CoreError::Storage {
            detail: e.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CoreConfig {
    /// Absolute path of the SQLite file. The shell picks the directory (App Group container on
    /// iOS) and its file protection.
    pub db_path: String,
    /// `None` uses the default SIMIT host. Must be https.
    pub simit_base_url: Option<String>,
    /// `None` uses SIMIT's proof-of-work service. Must be https.
    pub captcha_url: Option<String>,
    /// Proof-of-work solutions per lookup; 0 uses the page's default (1).
    pub captcha_difficulty: u32,
    pub user_agent: String,
    pub http_timeout_ms: u32,
    pub jitter_min_ms: u32,
    pub jitter_max_ms: u32,
    pub retry_backoff_ms: Vec<u32>,
    pub min_plate_budget_ms: u32,
}

/// Sensible production defaults.
#[uniffi::export]
pub fn default_config(db_path: String, user_agent: String) -> CoreConfig {
    CoreConfig {
        db_path,
        simit_base_url: None,
        captcha_url: None,
        captcha_difficulty: 0,
        user_agent,
        http_timeout_ms: 10_000,
        jitter_min_ms: 400,
        jitter_max_ms: 1_800,
        retry_backoff_ms: vec![1_500, 4_000],
        min_plate_budget_ms: 6_000,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PlateValidation {
    /// Uppercase, without spaces or hyphens.
    pub normalized: String,
    /// `None` when valid.
    pub error: Option<PlateError>,
}

/// Live validation for the plate input.
#[uniffi::export]
pub fn validate_plate(plate: String, kind: VehicleKind) -> PlateValidation {
    match plate::validate(&plate, kind) {
        Ok(normalized) => PlateValidation {
            normalized,
            error: None,
        },
        Err(e) => PlateValidation {
            normalized: plate::normalize(&plate),
            error: Some(e),
        },
    }
}

#[uniffi::export]
pub fn detect_vehicle_kind(plate: String) -> Option<VehicleKind> {
    plate::detect_kind(&plate)
}

/// "TTO08G" -> "TTO***", for anything shareable.
#[uniffi::export]
pub fn mask_plate(plate: String) -> String {
    plate::mask(&plate)
}

#[uniffi::export]
pub fn discount_schedule(issued_at: i64, notified_at: Option<i64>, now: i64) -> DiscountSchedule {
    detector::discount_schedule(issued_at, notified_at, now)
}

#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[derive(uniffi::Object)]
pub struct SappaCore {
    db_path: PathBuf,
    conn: Mutex<Connection>,
    run_lock: Mutex<()>,
    client: simit::SimitClient,
    runner: runner::RunnerConfig,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn open_migrated(path: &Path) -> Result<Connection, CoreError> {
    let mut conn = store::open(path)?;
    store::migrate(&mut conn)?;
    Ok(conn)
}

#[derive(Serialize)]
struct ExportDoc {
    format: &'static str,
    version: u32,
    exported_at: i64,
    schema_version: i64,
    vehicles: Vec<Vehicle>,
    fines: Vec<Fine>,
    runs: Vec<RunSummary>,
}

#[derive(Serialize)]
struct SnapshotVehicle {
    id: i64,
    plate: String,
    kind: VehicleKind,
    nickname: Option<String>,
    pending_fines: u32,
    new_fines: u32,
    last_checked_at: Option<i64>,
}

#[derive(Serialize)]
struct SnapshotDeadline {
    fine_id: i64,
    plate: String,
    stage: DiscountStage,
    deadline: i64,
    business_days_left: u32,
    amount: i64,
}

#[derive(Serialize)]
struct WidgetSnapshot {
    version: u32,
    generated_at: i64,
    /// "clear" | "fine" | "failing" | "empty"
    status: &'static str,
    last_ok_at: Option<i64>,
    next_check_at: Option<i64>,
    new_fines: u32,
    vehicles: Vec<SnapshotVehicle>,
    next_deadline: Option<SnapshotDeadline>,
}

#[uniffi::export]
impl SappaCore {
    /// Opens (or creates) the database and runs pending migrations.
    #[uniffi::constructor]
    pub fn new(
        config: CoreConfig,
        transport: Arc<dyn HttpTransport>,
    ) -> Result<Arc<Self>, CoreError> {
        let base = config
            .simit_base_url
            .clone()
            .unwrap_or_else(|| simit::DEFAULT_BASE_URL.into());
        let captcha_url = config
            .captcha_url
            .clone()
            .unwrap_or_else(|| simit::DEFAULT_CAPTCHA_URL.into());
        if !base.starts_with("https://") || !captcha_url.starts_with("https://") {
            return Err(CoreError::Config {
                detail: "simit_base_url and captcha_url must be https".into(),
            });
        }
        let difficulty = match config.captcha_difficulty {
            0 => simit::DEFAULT_CAPTCHA_DIFFICULTY,
            d => d.min(5),
        };
        let db_path = PathBuf::from(&config.db_path);
        let conn = open_migrated(&db_path)?;
        Ok(Arc::new(Self {
            db_path,
            conn: Mutex::new(conn),
            run_lock: Mutex::new(()),
            client: simit::SimitClient::new(
                transport,
                base,
                captcha_url,
                difficulty,
                config.user_agent,
                config.http_timeout_ms,
            ),
            runner: runner::RunnerConfig {
                jitter_min_ms: config.jitter_min_ms as u64,
                jitter_max_ms: config.jitter_max_ms as u64,
                retry_backoff_ms: config.retry_backoff_ms.iter().map(|b| *b as u64).collect(),
                min_plate_budget_ms: config.min_plate_budget_ms as u64,
            },
        }))
    }

    pub fn schema_version(&self) -> Result<i64, CoreError> {
        Ok(store::schema_version(&lock(&self.conn))?)
    }

    // ----- vehicles -----

    pub fn add_vehicle(
        &self,
        plate: String,
        kind: VehicleKind,
        nickname: Option<String>,
        now: i64,
    ) -> Result<Vehicle, CoreError> {
        let plate =
            plate::validate(&plate, kind).map_err(|reason| CoreError::InvalidPlate { reason })?;
        let nickname = nickname
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty());
        let conn = lock(&self.conn);
        if store::plate_exists(&conn, &plate)? {
            return Err(CoreError::DuplicatePlate);
        }
        Ok(store::insert_vehicle(
            &conn,
            &plate,
            kind,
            nickname.as_deref(),
            now,
        )?)
    }

    pub fn rename_vehicle(&self, id: i64, nickname: Option<String>) -> Result<Vehicle, CoreError> {
        let nickname = nickname
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty());
        let conn = lock(&self.conn);
        if store::update_nickname(&conn, id, nickname.as_deref())? == 0 {
            return Err(CoreError::NotFound);
        }
        store::get_vehicle(&conn, id)?.ok_or(CoreError::NotFound)
    }

    /// Removes a vehicle and its fines. Returns the ids of alerts the shell should unschedule.
    pub fn remove_vehicle(&self, id: i64) -> Result<Vec<i64>, CoreError> {
        let conn = lock(&self.conn);
        let mut st = conn.prepare(
            "SELECT a.id FROM alert a JOIN fine f ON f.id = a.fine_id
             WHERE f.vehicle_id = ?1 AND a.delivered_at IS NULL AND a.cancelled_at IS NULL",
        )?;
        let ids = st
            .query_map([id], |r| r.get(0))?
            .collect::<Result<Vec<i64>, _>>()?;
        drop(st);
        if store::delete_vehicle(&conn, id)? == 0 {
            return Err(CoreError::NotFound);
        }
        Ok(ids)
    }

    pub fn list_vehicles(&self) -> Result<Vec<Vehicle>, CoreError> {
        Ok(store::list_vehicles(&lock(&self.conn))?)
    }

    pub fn get_vehicle(&self, id: i64) -> Result<Option<Vehicle>, CoreError> {
        Ok(store::get_vehicle(&lock(&self.conn), id)?)
    }

    // ----- fines -----

    /// Fines of one vehicle, or of all vehicles when `vehicle_id` is `None`.
    pub fn list_fines(&self, vehicle_id: Option<i64>, now: i64) -> Result<Vec<Fine>, CoreError> {
        Ok(store::list_fines(&lock(&self.conn), vehicle_id, now)?)
    }

    pub fn get_fine(&self, id: i64, now: i64) -> Result<Option<Fine>, CoreError> {
        Ok(store::get_fine(&lock(&self.conn), id, now)?)
    }

    /// The user opened the fine: it no longer counts as "nueva".
    pub fn acknowledge_fine(&self, id: i64, now: i64) -> Result<(), CoreError> {
        if store::acknowledge_fine(&lock(&self.conn), id, now)? == 0 {
            return Err(CoreError::NotFound);
        }
        Ok(())
    }

    /// "Ya la pagué". Returns the ids of alerts the shell should unschedule.
    pub fn mark_paid(&self, fine_id: i64, now: i64) -> Result<Vec<i64>, CoreError> {
        let conn = lock(&self.conn);
        if store::mark_paid_by_user(&conn, fine_id, now)? == 0 {
            return Err(CoreError::NotFound);
        }
        Ok(store::cancel_alerts_for_fine(&conn, fine_id, now)?)
    }

    /// "Recordarme antes del vencimiento": (re)arms the discount-ending reminder. Returns `None`
    /// when no discounted window is left.
    pub fn remind_before_deadline(
        &self,
        fine_id: i64,
        now: i64,
    ) -> Result<Option<Alert>, CoreError> {
        let conn = lock(&self.conn);
        let fine = store::get_fine(&conn, fine_id, now)?.ok_or(CoreError::NotFound)?;
        if fine.status != FineStatus::Pending {
            return Ok(None);
        }
        let due = match detector::discount_reminder_at(fine.issued_at, fine.notified_at, now) {
            Some(at) => at,
            None if fine.discount.stage == DiscountStage::TwentyFive => {
                let last = calendar::date_of(fine.discount.twenty_five_until);
                calendar::ts_at(calendar::previous_business_day(last), 9, 0).max(now)
            }
            None => return Ok(None),
        };
        let id = store::upsert_alert(&conn, fine_id, AlertKind::DiscountEnding, due, now)?;
        Ok(store::pending_alerts(&conn, now)?
            .into_iter()
            .find(|a| a.id == id))
    }

    // ----- checks -----

    /// Runs one pass over all plates within `budget_ms`. Safe to call repeatedly; see `runner`.
    pub fn run_check(
        &self,
        now: i64,
        trigger: CheckTrigger,
        budget_ms: u32,
    ) -> Result<CheckReport, CoreError> {
        let _guard = self
            .run_lock
            .try_lock()
            .map_err(|_| CoreError::AlreadyRunning)?;
        // Own connection, so the UI keeps reading and writing while the network is slow (WAL).
        let mut conn = open_migrated(&self.db_path)?;
        runner::run(
            &mut conn,
            &self.client,
            &self.runner,
            now,
            trigger,
            budget_ms,
        )
    }

    /// Undelivered, uncancelled alerts (for re-scheduling after app launch).
    pub fn pending_alerts(&self, now: i64) -> Result<Vec<Alert>, CoreError> {
        Ok(store::pending_alerts(&lock(&self.conn), now)?)
    }

    /// The shell posted (or scheduled) these notifications.
    pub fn mark_alerts_delivered(&self, ids: Vec<i64>, now: i64) -> Result<(), CoreError> {
        Ok(store::mark_alerts_delivered(&lock(&self.conn), &ids, now)?)
    }

    pub fn health(&self, now: i64, limit: u32) -> Result<Health, CoreError> {
        let conn = lock(&self.conn);
        let runs = store::recent_runs(&conn, limit)?;
        let last_ok_at = store::last_ok_at(&conn)?;
        let last_finished = runs.iter().find(|r| r.result.is_some());
        let status = match last_finished {
            None => HealthStatus::NoRuns,
            Some(r) => {
                let reference = last_ok_at.or(store::first_run_at(&conn)?).unwrap_or(now);
                if r.result == Some(RunResult::Ok) {
                    HealthStatus::Good
                } else if now - reference < runner::FAILING_AFTER_S {
                    HealthStatus::Degraded
                } else {
                    HealthStatus::Failing
                }
            }
        };
        Ok(Health {
            status,
            last_ok_at,
            last_run_at: runs.first().map(|r| r.started_at),
            runs,
        })
    }

    // ----- data -----

    /// Everything the user stored, as JSON.
    pub fn export_data(&self, now: i64) -> Result<String, CoreError> {
        let conn = lock(&self.conn);
        let doc = ExportDoc {
            format: "sappa-export",
            version: 1,
            exported_at: now,
            schema_version: store::schema_version(&conn)?,
            vehicles: store::list_vehicles(&conn)?,
            fines: store::list_fines(&conn, None, now)?,
            runs: store::recent_runs(&conn, store::KEEP_RUNS as u32)?,
        };
        serde_json::to_string_pretty(&doc).map_err(|e| CoreError::Storage {
            detail: e.to_string(),
        })
    }

    /// Removes the database file and recreates an empty schema.
    pub fn delete_all(&self) -> Result<(), CoreError> {
        let _guard = self
            .run_lock
            .try_lock()
            .map_err(|_| CoreError::AlreadyRunning)?;
        let mut conn = lock(&self.conn);
        let old = std::mem::replace(&mut *conn, Connection::open_in_memory()?);
        old.close().map_err(|(_, e)| CoreError::from(e))?;
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.db_path.clone().into_os_string();
            p.push(suffix);
            match std::fs::remove_file(&p) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(CoreError::Storage {
                        detail: e.to_string(),
                    });
                }
            }
        }
        *conn = open_migrated(&self.db_path)?;
        Ok(())
    }

    /// Small JSON document for widgets/extensions, which never open the database. The shell
    /// writes it to shared storage after each run and passes the next scheduled check time.
    pub fn widget_snapshot(
        &self,
        now: i64,
        next_check_at: Option<i64>,
    ) -> Result<String, CoreError> {
        let conn = lock(&self.conn);
        let vehicles = store::list_vehicles(&conn)?;
        let fines = store::list_fines(&conn, None, now)?;
        let last_ok_at = store::last_ok_at(&conn)?;
        let failing = last_ok_at
            .or(store::first_run_at(&conn)?)
            .is_some_and(|r| now - r >= runner::FAILING_AFTER_S);
        let next_deadline = fines
            .iter()
            .filter(|f| f.status == FineStatus::Pending && f.discount.stage != DiscountStage::Full)
            .min_by_key(|f| match f.discount.stage {
                DiscountStage::Fifty => f.discount.fifty_until,
                _ => f.discount.twenty_five_until,
            })
            .map(|f| SnapshotDeadline {
                fine_id: f.id,
                plate: f.plate.clone(),
                stage: f.discount.stage,
                deadline: match f.discount.stage {
                    DiscountStage::Fifty => f.discount.fifty_until,
                    _ => f.discount.twenty_five_until,
                },
                business_days_left: f.discount.business_days_left,
                amount: f.amount,
            });
        let pending: u32 = vehicles.iter().map(|v| v.pending_fines).sum();
        let status = if vehicles.is_empty() {
            "empty"
        } else if pending > 0 {
            "fine"
        } else if failing {
            "failing"
        } else {
            "clear"
        };
        let snap = WidgetSnapshot {
            version: 1,
            generated_at: now,
            status,
            last_ok_at,
            next_check_at,
            new_fines: vehicles.iter().map(|v| v.new_fines).sum(),
            vehicles: vehicles
                .into_iter()
                .map(|v| SnapshotVehicle {
                    id: v.id,
                    plate: v.plate,
                    kind: v.kind,
                    nickname: v.nickname,
                    pending_fines: v.pending_fines,
                    new_fines: v.new_fines,
                    last_checked_at: v.last_checked_at,
                })
                .collect(),
            next_deadline,
        };
        serde_json::to_string(&snap).map_err(|e| CoreError::Storage {
            detail: e.to_string(),
        })
    }
}
