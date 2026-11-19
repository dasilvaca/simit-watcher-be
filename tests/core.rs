//! End-to-end tests of the public API against recorded fixtures. No test calls SIMIT.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sappa_core::*;

const NOW: i64 = 1_794_999_720; // 2026-11-18 06:02 Bogotá
const H: i64 = 3600;
const BUDGET: u32 = 25_000;

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn ok(name: &str) -> Result<HttpResponse, TransportError> {
    Ok(HttpResponse {
        status: 200,
        body: fixture(name),
    })
}

#[derive(Default)]
struct FakeSimit {
    /// Per plate, a queue of responses; the last one repeats.
    responses: Mutex<HashMap<String, Vec<Result<HttpResponse, TransportError>>>>,
    calls: Mutex<Vec<String>>,
}

impl FakeSimit {
    fn set(&self, plate: &str, r: Vec<Result<HttpResponse, TransportError>>) {
        self.responses.lock().unwrap().insert(plate.into(), r);
    }
    fn calls(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl HttpTransport for FakeSimit {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        assert!(request.url.starts_with("https://"));
        assert_eq!(request.method, "POST");
        let body: serde_json::Value = serde_json::from_slice(&request.body.unwrap()).unwrap();
        let plate = body["filtro"].as_str().unwrap().to_string();
        self.calls.lock().unwrap().push(plate.clone());
        let mut map = self.responses.lock().unwrap();
        let q = map.get_mut(&plate).expect("unexpected plate");
        if q.len() > 1 {
            q.remove(0)
        } else {
            q[0].clone()
        }
    }
}

struct Env {
    _dir: tempfile::TempDir,
    db: String,
    core: Arc<SappaCore>,
    simit: Arc<FakeSimit>,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let db = dir
        .path()
        .join("sappa.sqlite")
        .to_string_lossy()
        .to_string();
    let simit = Arc::new(FakeSimit::default());
    let mut cfg = default_config(db.clone(), "SAPPA-test".into());
    cfg.jitter_min_ms = 0;
    cfg.jitter_max_ms = 0;
    cfg.retry_backoff_ms = vec![];
    cfg.min_plate_budget_ms = 10;
    let core = SappaCore::new(cfg, simit.clone()).unwrap();
    Env {
        _dir: dir,
        db,
        core,
        simit,
    }
}

fn kinds(alerts: &[Alert]) -> Vec<AlertKind> {
    alerts.iter().map(|a| a.kind).collect()
}

#[test]
fn parses_fixtures() {
    let one =
        sappa_core::simit::parse::parse_lookup(&fixture("one_pending.json"), "TTO08G").unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].infraction_code, "C02");
    assert_eq!(one[0].amount, 604_200);
    assert_eq!(one[0].place.as_deref(), Some("Bogotá D.C."));
    assert_eq!(one[0].status, FineStatus::Pending);

    let two = sappa_core::simit::parse::parse_lookup(
        &fixture("one_pending_updated_plus_new.json"),
        "TTO08G",
    )
    .unwrap();
    assert_eq!(two.len(), 2);
    assert_eq!(two[0].amount, 650_000);
    assert!(two[1].notified_at.is_some());

    assert!(
        sappa_core::simit::parse::parse_lookup(&fixture("no_fines.json"), "TTO08G")
            .unwrap()
            .is_empty()
    );
    // Items for other plates are ignored.
    assert!(
        sappa_core::simit::parse::parse_lookup(&fixture("one_pending.json"), "ABC123")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn vehicles_validate_and_dedupe() {
    let e = env();
    let v = e
        .core
        .add_vehicle(
            "tto08g".into(),
            VehicleKind::Moto,
            Some(" Moto principal ".into()),
            NOW,
        )
        .unwrap();
    assert_eq!(v.plate, "TTO08G");
    assert_eq!(v.nickname.as_deref(), Some("Moto principal"));
    assert_eq!(
        e.core
            .add_vehicle("TTO08G".into(), VehicleKind::Moto, None, NOW),
        Err(CoreError::DuplicatePlate)
    );
    assert_eq!(
        e.core
            .add_vehicle("TTO08G".into(), VehicleKind::Car, None, NOW),
        Err(CoreError::InvalidPlate {
            reason: PlateError::WrongFormatCar
        })
    );
    assert_eq!(
        validate_plate("abc 123".into(), VehicleKind::Car).error,
        None
    );
    assert_eq!(e.core.list_vehicles().unwrap().len(), 1);
    assert_eq!(e.core.schema_version().unwrap(), 1);
}

#[test]
fn baseline_then_new_fine_then_resolved() {
    let e = env();
    let v = e
        .core
        .add_vehicle("TTO08G".into(), VehicleKind::Moto, None, NOW)
        .unwrap();
    e.simit.set("TTO08G", vec![ok("one_pending.json")]);

    // First check = baseline: the existing fine is shown but not announced as new.
    let r1 = e
        .core
        .run_check(NOW, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    assert_eq!(r1.result, RunResult::Ok);
    assert!(r1.new_fines.is_empty());
    assert_eq!(kinds(&r1.alerts), vec![AlertKind::DiscountEnding]);
    let fines = e.core.list_fines(Some(v.id), NOW).unwrap();
    assert_eq!(fines.len(), 1);
    assert!(!fines[0].is_new);
    assert_eq!(fines[0].discount.stage, DiscountStage::Fifty);
    assert_eq!(fines[0].discount.business_days_left, 4);
    e.core
        .mark_alerts_delivered(r1.alerts.iter().map(|a| a.id).collect(), NOW)
        .unwrap();

    // Twelve hours later: amount changed and a new fine appeared.
    e.simit
        .set("TTO08G", vec![ok("one_pending_updated_plus_new.json")]);
    let t2 = NOW + 12 * H;
    let r2 = e
        .core
        .run_check(t2, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    assert_eq!(r2.new_fines.len(), 1);
    assert_eq!(r2.new_fines[0].infraction_code, "D12");
    assert!(r2.new_fines[0].is_new);
    assert_eq!(r2.changes.len(), 1);
    assert_eq!(r2.changes[0].kind, FineChangeKind::Updated);
    let mut k = kinds(&r2.alerts);
    k.sort_by_key(|k| k.as_str());
    assert_eq!(k, vec![AlertKind::DiscountEnding, AlertKind::NewFine]);
    let new_alert = r2
        .alerts
        .iter()
        .find(|a| a.kind == AlertKind::NewFine)
        .unwrap();
    assert_eq!(new_alert.plate.as_deref(), Some("TTO08G"));
    assert_eq!(new_alert.infraction_code.as_deref(), Some("D12"));
    assert_eq!(new_alert.discount_stage, Some(DiscountStage::Fifty));
    let v_now = e.core.get_vehicle(v.id).unwrap().unwrap();
    assert_eq!((v_now.pending_fines, v_now.new_fines), (2, 1));

    // Not acknowledged by the shell yet: a re-run (after the skip window) repeats them, nothing more.
    let r2b = e
        .core
        .run_check(t2 + 3 * H, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    assert_eq!(r2b.alerts.len(), 2);
    assert!(r2b.new_fines.is_empty());
    e.core
        .mark_alerts_delivered(r2b.alerts.iter().map(|a| a.id).collect(), t2)
        .unwrap();
    let r2c = e
        .core
        .run_check(t2 + 6 * H, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    assert!(r2c.alerts.is_empty(), "delivered alerts never come back");

    // Opening the fine clears "nueva".
    e.core.acknowledge_fine(r2.new_fines[0].id, t2).unwrap();
    assert_eq!(e.core.get_vehicle(v.id).unwrap().unwrap().new_fines, 0);

    // Both fines disappear from SIMIT: resolved, pending reminders cancelled.
    e.simit.set("TTO08G", vec![ok("no_fines.json")]);
    let r3 = e
        .core
        .run_check(t2 + 12 * H, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    assert_eq!(
        r3.changes
            .iter()
            .filter(|c| c.kind == FineChangeKind::Resolved)
            .count(),
        2
    );
    assert!(
        e.core
            .list_fines(Some(v.id), t2)
            .unwrap()
            .iter()
            .all(|f| f.status == FineStatus::Paid)
    );
}

#[test]
fn run_twice_in_a_row_is_a_no_op() {
    let e = env();
    e.core
        .add_vehicle("ABC123".into(), VehicleKind::Car, None, NOW)
        .unwrap();
    e.simit.set("ABC123", vec![ok("no_fines.json")]);
    e.core
        .run_check(NOW, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    let r = e
        .core
        .run_check(NOW + 60, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    assert_eq!(e.simit.calls(), 1);
    assert_eq!(r.outcomes[0].outcome, PlateOutcomeKind::SkippedRecent);
    assert!(r.alerts.is_empty());
}

#[test]
fn manual_checks_limited_to_one_per_ten_minutes() {
    let e = env();
    e.core
        .add_vehicle("ABC123".into(), VehicleKind::Car, None, NOW)
        .unwrap();
    e.simit.set("ABC123", vec![ok("no_fines.json")]);
    e.core.run_check(NOW, CheckTrigger::Manual, BUDGET).unwrap();
    e.core
        .run_check(NOW + 9 * 60, CheckTrigger::Manual, BUDGET)
        .unwrap();
    assert_eq!(e.simit.calls(), 1);
    e.core
        .run_check(NOW + 10 * 60, CheckTrigger::Manual, BUDGET)
        .unwrap();
    assert_eq!(e.simit.calls(), 2);
}

#[test]
fn budget_exhaustion_resumes_with_unreached_plates() {
    let e = env();
    for p in ["AAA111", "BBB222", "CCC333"] {
        e.core
            .add_vehicle(p.into(), VehicleKind::Car, None, NOW)
            .unwrap();
        e.simit.set(p, vec![ok("no_fines.json")]);
    }
    let r = e.core.run_check(NOW, CheckTrigger::Scheduled, 0).unwrap();
    assert!(r.budget_exhausted);
    assert_eq!(e.simit.calls(), 0);
    assert!(
        r.outcomes
            .iter()
            .all(|o| o.outcome == PlateOutcomeKind::NotReached)
    );
    let h = e.core.health(NOW, 14).unwrap();
    assert_eq!(h.runs[0].plates_remaining, 3);

    let r = e
        .core
        .run_check(NOW + 60, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    assert!(!r.budget_exhausted);
    assert_eq!(e.simit.calls(), 3);
}

#[test]
fn captcha_blocks_and_stops_the_run() {
    let e = env();
    e.core
        .add_vehicle("AAA111".into(), VehicleKind::Car, None, NOW)
        .unwrap();
    e.core
        .add_vehicle("BBB222".into(), VehicleKind::Car, None, NOW)
        .unwrap();
    e.simit.set("AAA111", vec![ok("captcha.html")]);
    e.simit.set("BBB222", vec![ok("no_fines.json")]);
    let r = e
        .core
        .run_check(NOW, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    assert_eq!(r.result, RunResult::Blocked);
    assert_eq!(r.error, Some(ErrorClass::Blocked));
    assert_eq!(e.simit.calls(), 1, "no more requests after a block");
    assert_eq!(r.outcomes[1].outcome, PlateOutcomeKind::NotReached);
    assert!(!r.budget_exhausted);
}

#[test]
fn error_classes() {
    let e = env();
    e.core
        .add_vehicle("AAA111".into(), VehicleKind::Car, None, NOW)
        .unwrap();
    e.simit.set("AAA111", vec![ok("unexpected_shape.json")]);
    let r = e.core.run_check(NOW, CheckTrigger::Manual, BUDGET).unwrap();
    assert_eq!(
        (r.result, r.error),
        (RunResult::Error, Some(ErrorClass::Parse))
    );

    e.simit.set(
        "AAA111",
        vec![Ok(HttpResponse {
            status: 503,
            body: vec![],
        })],
    );
    let r = e
        .core
        .run_check(NOW + 11 * 60, CheckTrigger::Manual, BUDGET)
        .unwrap();
    assert_eq!(r.error, Some(ErrorClass::Http));
    assert_eq!(r.outcomes[0].http_status, Some(503));

    e.simit.set(
        "AAA111",
        vec![Ok(HttpResponse {
            status: 429,
            body: vec![],
        })],
    );
    let r = e
        .core
        .run_check(NOW + 22 * 60, CheckTrigger::Manual, BUDGET)
        .unwrap();
    assert_eq!(r.result, RunResult::Blocked);

    e.simit.set("AAA111", vec![Err(TransportError::Offline)]);
    let r = e
        .core
        .run_check(NOW + 33 * 60, CheckTrigger::Manual, BUDGET)
        .unwrap();
    assert_eq!(r.result, RunResult::Offline);
}

#[test]
fn retries_transient_failures() {
    let e = {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("s.sqlite").to_string_lossy().to_string();
        let simit = Arc::new(FakeSimit::default());
        let mut cfg = default_config(db.clone(), "t".into());
        cfg.jitter_min_ms = 0;
        cfg.jitter_max_ms = 0;
        cfg.retry_backoff_ms = vec![1, 1];
        cfg.min_plate_budget_ms = 10;
        let core = SappaCore::new(cfg, simit.clone()).unwrap();
        Env {
            _dir: dir,
            db,
            core,
            simit,
        }
    };
    e.core
        .add_vehicle("AAA111".into(), VehicleKind::Car, None, NOW)
        .unwrap();
    e.simit.set(
        "AAA111",
        vec![Err(TransportError::Timeout), ok("no_fines.json")],
    );
    let r = e
        .core
        .run_check(NOW, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    assert_eq!(r.result, RunResult::Ok);
    assert_eq!(e.simit.calls(), 2);
}

#[test]
fn checks_failing_alert_once_per_streak() {
    let e = env();
    e.core
        .add_vehicle("AAA111".into(), VehicleKind::Car, None, NOW)
        .unwrap();
    e.simit.set("AAA111", vec![ok("no_fines.json")]);
    e.core
        .run_check(NOW, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    assert_eq!(e.core.health(NOW, 14).unwrap().status, HealthStatus::Good);

    e.simit.set("AAA111", vec![Err(TransportError::Offline)]);
    let r = e
        .core
        .run_check(NOW + 12 * H, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    assert!(r.alerts.is_empty());
    assert_eq!(
        e.core.health(NOW + 12 * H, 14).unwrap().status,
        HealthStatus::Degraded
    );

    let r = e
        .core
        .run_check(NOW + 25 * H, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    assert_eq!(kinds(&r.alerts), vec![AlertKind::ChecksFailing]);
    assert_eq!(r.alerts[0].last_ok_at, Some(NOW));
    assert_eq!(
        e.core.health(NOW + 25 * H, 14).unwrap().status,
        HealthStatus::Failing
    );
    e.core
        .mark_alerts_delivered(vec![r.alerts[0].id], NOW + 25 * H)
        .unwrap();

    let r = e
        .core
        .run_check(NOW + 37 * H, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    assert!(r.alerts.is_empty(), "one alert per failing streak");

    e.simit.set("AAA111", vec![ok("no_fines.json")]);
    let r = e
        .core
        .run_check(NOW + 49 * H, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    assert_eq!(r.result, RunResult::Ok);
    assert_eq!(
        e.core.health(NOW + 49 * H, 14).unwrap().status,
        HealthStatus::Good
    );
}

#[test]
fn mark_paid_and_reminder() {
    let e = env();
    let v = e
        .core
        .add_vehicle("TTO08G".into(), VehicleKind::Moto, None, NOW)
        .unwrap();
    e.simit.set("TTO08G", vec![ok("one_pending.json")]);
    let r = e
        .core
        .run_check(NOW, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    let reminder = r.alerts[0].clone();
    let fine = &e.core.list_fines(Some(v.id), NOW).unwrap()[0];

    let cancelled = e.core.mark_paid(fine.id, NOW).unwrap();
    assert_eq!(cancelled, vec![reminder.id]);
    assert!(e.core.pending_alerts(NOW).unwrap().is_empty());
    let f = e.core.get_fine(fine.id, NOW).unwrap().unwrap();
    assert!(f.paid_by_user && f.status == FineStatus::Paid);
    assert_eq!(e.core.remind_before_deadline(fine.id, NOW).unwrap(), None);

    // SIMIT still lists it: stays paid, no new alerts.
    let r = e
        .core
        .run_check(NOW + 12 * H, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    assert!(r.alerts.is_empty());
    assert_eq!(
        e.core.get_fine(fine.id, NOW).unwrap().unwrap().status,
        FineStatus::Paid
    );
}

#[test]
fn remind_rearms_a_cancelled_reminder() {
    let e = env();
    let v = e
        .core
        .add_vehicle("TTO08G".into(), VehicleKind::Moto, None, NOW)
        .unwrap();
    e.simit.set("TTO08G", vec![ok("one_pending.json")]);
    let r = e
        .core
        .run_check(NOW, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    e.core
        .mark_alerts_delivered(vec![r.alerts[0].id], NOW)
        .unwrap();
    let fine = &e.core.list_fines(Some(v.id), NOW).unwrap()[0];
    let a = e
        .core
        .remind_before_deadline(fine.id, NOW)
        .unwrap()
        .unwrap();
    assert_eq!(a.kind, AlertKind::DiscountEnding);
    assert_eq!(a.id, r.alerts[0].id, "same row, re-armed");
}

#[test]
fn remove_vehicle_returns_alerts_to_unschedule() {
    let e = env();
    let v = e
        .core
        .add_vehicle("TTO08G".into(), VehicleKind::Moto, None, NOW)
        .unwrap();
    e.simit.set("TTO08G", vec![ok("one_pending.json")]);
    let r = e
        .core
        .run_check(NOW, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    assert_eq!(e.core.remove_vehicle(v.id).unwrap(), vec![r.alerts[0].id]);
    assert!(e.core.list_fines(None, NOW).unwrap().is_empty());
    assert_eq!(e.core.remove_vehicle(v.id), Err(CoreError::NotFound));
}

#[test]
fn export_snapshot_and_delete_all() {
    let e = env();
    e.core
        .add_vehicle("TTO08G".into(), VehicleKind::Moto, Some("Moto".into()), NOW)
        .unwrap();
    e.simit.set("TTO08G", vec![ok("one_pending.json")]);
    e.core
        .run_check(NOW, CheckTrigger::Scheduled, BUDGET)
        .unwrap();

    let export: serde_json::Value =
        serde_json::from_str(&e.core.export_data(NOW).unwrap()).unwrap();
    assert_eq!(export["vehicles"].as_array().unwrap().len(), 1);
    assert_eq!(export["fines"][0]["infraction_code"], "C02");

    let snap: serde_json::Value =
        serde_json::from_str(&e.core.widget_snapshot(NOW, Some(NOW + 12 * H)).unwrap()).unwrap();
    assert_eq!(snap["status"], "fine");
    assert_eq!(snap["vehicles"][0]["plate"], "TTO08G");
    assert_eq!(snap["next_deadline"]["business_days_left"], 4);
    assert_eq!(snap["next_deadline"]["stage"], "fifty");

    e.core.delete_all().unwrap();
    assert!(std::path::Path::new(&e.db).exists());
    assert!(e.core.list_vehicles().unwrap().is_empty());
    assert_eq!(e.core.health(NOW, 14).unwrap().status, HealthStatus::NoRuns);
    assert_eq!(e.core.schema_version().unwrap(), 1);
    let snap: serde_json::Value =
        serde_json::from_str(&e.core.widget_snapshot(NOW, None).unwrap()).unwrap();
    assert_eq!(snap["status"], "empty");
}

#[test]
fn interrupted_run_is_closed_and_reopen_keeps_data() {
    let e = env();
    e.core
        .add_vehicle("AAA111".into(), VehicleKind::Car, None, NOW)
        .unwrap();
    // Simulate a run killed by the OS.
    {
        let conn = sappa_core::store::open(std::path::Path::new(&e.db)).unwrap();
        sappa_core::store::start_run(&conn, CheckTrigger::Scheduled, NOW - H).unwrap();
    }
    e.simit.set("AAA111", vec![ok("no_fines.json")]);
    e.core
        .run_check(NOW, CheckTrigger::Scheduled, BUDGET)
        .unwrap();
    let runs = e.core.health(NOW, 14).unwrap().runs;
    assert_eq!(runs[1].error, Some(ErrorClass::Interrupted));

    let core2 = SappaCore::new(default_config(e.db.clone(), "t".into()), e.simit.clone()).unwrap();
    assert_eq!(core2.list_vehicles().unwrap().len(), 1);
}

#[test]
fn rejects_plain_http() {
    let mut cfg = default_config("/tmp/never.sqlite".into(), "t".into());
    cfg.simit_base_url = Some("http://example.com".into());
    assert!(matches!(
        SappaCore::new(cfg, Arc::new(FakeSimit::default())),
        Err(CoreError::Config { .. })
    ));
}
