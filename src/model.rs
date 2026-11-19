//! Public data types. Everything here crosses the FFI boundary, so it carries codes and values,
//! never user-facing sentences. Timestamps are Unix seconds; amounts are whole Colombian pesos.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum VehicleKind {
    Car,
    Moto,
}

impl VehicleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            VehicleKind::Car => "car",
            VehicleKind::Moto => "moto",
        }
    }
    pub fn parse(s: &str) -> Self {
        if s == "moto" {
            VehicleKind::Moto
        } else {
            VehicleKind::Car
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum PlateError {
    Empty,
    WrongLength,
    /// Expected `ABC123`.
    WrongFormatCar,
    /// Expected `ABC12D`.
    WrongFormatMoto,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, uniffi::Record)]
pub struct Vehicle {
    pub id: i64,
    pub plate: String,
    pub kind: VehicleKind,
    pub nickname: Option<String>,
    pub created_at: i64,
    /// Last time SIMIT answered successfully for this plate.
    pub last_checked_at: Option<i64>,
    /// Pending fines for this plate.
    pub pending_fines: u32,
    /// Pending fines the user has not opened yet ("1 nueva").
    pub new_fines: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum FineStatus {
    /// Pending payment on SIMIT.
    Pending,
    /// Gone from SIMIT or marked as paid by the user.
    Paid,
    /// Present on SIMIT with a status the core does not recognise (e.g. in dispute).
    Other,
}

impl FineStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FineStatus::Pending => "pending",
            FineStatus::Paid => "paid",
            FineStatus::Other => "other",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "pending" => FineStatus::Pending,
            "paid" => FineStatus::Paid,
            _ => FineStatus::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum DiscountStage {
    /// 50% off (requires the pedagogical course).
    Fifty,
    /// 25% off (requires the pedagogical course).
    TwentyFive,
    /// Full value; interest may apply.
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, uniffi::Record)]
pub struct DiscountSchedule {
    pub stage: DiscountStage,
    /// Last second (Bogotá) of the 50% window.
    pub fifty_until: i64,
    /// Last second (Bogotá) of the 25% window.
    pub twenty_five_until: i64,
    /// Business days left in the current discounted stage (0 when stage is Full).
    pub business_days_left: u32,
    /// Business days in the whole current window, for the progress bar (5 or 15; 0 when Full).
    pub window_business_days: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, uniffi::Record)]
pub struct Fine {
    pub id: i64,
    pub vehicle_id: i64,
    pub plate: String,
    pub simit_ref: String,
    pub infraction_code: String,
    /// Description as SIMIT returns it (Spanish). Shells may localize known codes instead.
    pub infraction_description: Option<String>,
    pub place: Option<String>,
    pub amount: i64,
    pub issued_at: i64,
    pub notified_at: Option<i64>,
    pub status: FineStatus,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    /// True until the user opens it (`acknowledge_fine`).
    pub is_new: bool,
    pub paid_by_user: bool,
    pub discount: DiscountSchedule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum CheckTrigger {
    Scheduled,
    Manual,
}

impl CheckTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            CheckTrigger::Scheduled => "scheduled",
            CheckTrigger::Manual => "manual",
        }
    }
    pub fn parse(s: &str) -> Self {
        if s == "manual" {
            CheckTrigger::Manual
        } else {
            CheckTrigger::Scheduled
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum RunResult {
    Ok,
    Offline,
    Blocked,
    Error,
}

impl RunResult {
    pub fn as_str(self) -> &'static str {
        match self {
            RunResult::Ok => "ok",
            RunResult::Offline => "offline",
            RunResult::Blocked => "blocked",
            RunResult::Error => "error",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "ok" => RunResult::Ok,
            "offline" => RunResult::Offline,
            "blocked" => RunResult::Blocked,
            _ => RunResult::Error,
        }
    }
    /// Severity used to combine plate outcomes into one run result.
    pub fn severity(self) -> u8 {
        match self {
            RunResult::Ok => 0,
            RunResult::Offline => 1,
            RunResult::Error => 2,
            RunResult::Blocked => 3,
        }
    }
}

/// Error classes recorded in `check_run.error_detail` and in plate outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Offline,
    Timeout,
    /// HTTP 403/429 or a captcha challenge.
    Blocked,
    /// HTTP status other than 2xx / 403 / 429.
    Http,
    /// SIMIT answered but the response did not match the expected shape (site probably changed).
    Parse,
    /// The previous run was killed before it finished.
    Interrupted,
    Storage,
}

impl ErrorClass {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorClass::Offline => "offline",
            ErrorClass::Timeout => "timeout",
            ErrorClass::Blocked => "blocked",
            ErrorClass::Http => "http",
            ErrorClass::Parse => "parse",
            ErrorClass::Interrupted => "interrupted",
            ErrorClass::Storage => "storage",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "offline" => ErrorClass::Offline,
            "timeout" => ErrorClass::Timeout,
            "blocked" => ErrorClass::Blocked,
            "http" => ErrorClass::Http,
            "parse" => ErrorClass::Parse,
            "interrupted" => ErrorClass::Interrupted,
            "storage" => ErrorClass::Storage,
            _ => return None,
        })
    }
    pub fn run_result(self) -> RunResult {
        match self {
            ErrorClass::Offline | ErrorClass::Timeout => RunResult::Offline,
            ErrorClass::Blocked => RunResult::Blocked,
            _ => RunResult::Error,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum PlateOutcomeKind {
    Checked,
    /// Skipped because it was checked too recently (rate limit / idempotency).
    SkippedRecent,
    /// Not reached before the time budget ran out; the next run starts with it.
    NotReached,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, uniffi::Record)]
pub struct PlateOutcome {
    pub vehicle_id: i64,
    pub plate: String,
    pub outcome: PlateOutcomeKind,
    pub error: Option<ErrorClass>,
    /// HTTP status, when the failure came with one.
    pub http_status: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum FineChangeKind {
    /// Amount, status, place or code changed on SIMIT.
    Updated,
    /// No longer listed on SIMIT (paid or resolved).
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, uniffi::Record)]
pub struct FineChange {
    pub fine_id: i64,
    pub kind: FineChangeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    NewFine,
    DiscountEnding,
    ChecksFailing,
}

impl AlertKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AlertKind::NewFine => "new_fine",
            AlertKind::DiscountEnding => "discount_ending",
            AlertKind::ChecksFailing => "checks_failing",
        }
    }
    pub fn parse(s: &str) -> Self {
        match s {
            "new_fine" => AlertKind::NewFine,
            "discount_ending" => AlertKind::DiscountEnding,
            _ => AlertKind::ChecksFailing,
        }
    }
}

/// Something the shell should notify about, at `due_at` (now or later). The shell posts it, using
/// `sappa-alert-<id>` as the notification identifier, then calls `mark_alerts_delivered`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, uniffi::Record)]
pub struct Alert {
    pub id: i64,
    pub kind: AlertKind,
    pub due_at: i64,
    pub fine_id: Option<i64>,
    pub vehicle_id: Option<i64>,
    pub plate: Option<String>,
    pub nickname: Option<String>,
    pub vehicle_kind: Option<VehicleKind>,
    pub infraction_code: Option<String>,
    pub place: Option<String>,
    pub amount: Option<i64>,
    pub discount_stage: Option<DiscountStage>,
    /// Business days left in the 50% window, counted at `due_at`.
    pub business_days_left: Option<u32>,
    pub discount_deadline: Option<i64>,
    /// For `checks_failing`: last successful check.
    pub last_ok_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, uniffi::Record)]
pub struct CheckReport {
    pub run_id: i64,
    pub result: RunResult,
    pub error: Option<ErrorClass>,
    pub started_at: i64,
    pub duration_ms: u64,
    pub outcomes: Vec<PlateOutcome>,
    /// True when the time budget ran out before every plate was reached.
    pub budget_exhausted: bool,
    pub new_fines: Vec<Fine>,
    pub changes: Vec<FineChange>,
    /// Every undelivered, uncancelled alert (due now or later).
    pub alerts: Vec<Alert>,
    /// Alerts cancelled during this run (fine resolved): remove their scheduled notifications.
    pub cancelled_alert_ids: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, uniffi::Record)]
pub struct RunSummary {
    pub id: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub trigger: CheckTrigger,
    pub result: Option<RunResult>,
    pub error: Option<ErrorClass>,
    pub plates_checked: u32,
    pub plates_remaining: u32,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, uniffi::Enum)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// No finished run yet.
    NoRuns,
    /// Last run ok.
    Good,
    /// Last run failed but there was a successful one in the last 24 h.
    Degraded,
    /// No successful run in the last 24 h.
    Failing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, uniffi::Record)]
pub struct Health {
    pub status: HealthStatus,
    pub last_ok_at: Option<i64>,
    pub last_run_at: Option<i64>,
    /// Most recent first, at most `limit`.
    pub runs: Vec<RunSummary>,
}
