//! Opt-in check against the LIVE SIMIT site. Never part of the normal suite (CLAUDE.md: tests do
//! not call SIMIT); run by hand, sparingly, for your own plate:
//!
//!   SAPPA_LIVE_PLATE=ABC123 cargo test --test live -- --ignored --nocapture
//!
//! `SAPPA_LIVE_DUMP=/path/dir` also saves the raw responses there, to make fixtures from (they
//! hold personal data: replace names and IDs before committing anything).
//! Requests go through the system `curl`, so the core gains no HTTP dependency.

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use sappa_core::*;

struct CurlTransport {
    dump: Option<std::path::PathBuf>,
    n: Mutex<u32>,
}

impl HttpTransport for CurlTransport {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        assert!(request.url.starts_with("https://"));
        let out = tempfile::NamedTempFile::new().unwrap();
        let mut cmd = Command::new("curl");
        cmd.args(["-sS", "-X", &request.method, "--max-time"])
            .arg(format!("{}", request.timeout_ms.div_ceil(1000)))
            .args(["-o"])
            .arg(out.path())
            .args(["-w", "%{http_code}"]);
        for h in &request.headers {
            cmd.arg("-H").arg(format!("{}: {}", h.name, h.value));
        }
        if request.body.is_some() {
            cmd.args(["--data-binary", "@-"]);
        }
        cmd.arg(&request.url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("curl");
        if let Some(body) = &request.body {
            child.stdin.take().unwrap().write_all(body).unwrap();
        }
        let res = child.wait_with_output().unwrap();
        if !res.status.success() {
            let err = String::from_utf8_lossy(&res.stderr).to_string();
            eprintln!("curl failed: {err}");
            return Err(if res.status.code() == Some(28) {
                TransportError::Timeout
            } else {
                TransportError::Offline
            });
        }
        let status: u16 = String::from_utf8_lossy(&res.stdout)
            .trim()
            .parse()
            .unwrap_or(0);
        let body = std::fs::read(out.path()).unwrap();
        let mut n = self.n.lock().unwrap();
        *n += 1;
        let short = request.url.rsplit('/').next().unwrap_or("");
        eprintln!("[{n}] POST …/{short} → HTTP {status}, {} bytes", body.len());
        if let Some(dir) = &self.dump {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join(format!("{n}-{short}.body")), &body).unwrap();
        }
        Ok(HttpResponse { status, body })
    }
}

#[test]
#[ignore = "calls the live SIMIT site; run by hand with SAPPA_LIVE_PLATE"]
fn live_lookup() {
    let plate = std::env::var("SAPPA_LIVE_PLATE").expect("set SAPPA_LIVE_PLATE");
    let kind = detect_vehicle_kind(plate.clone()).expect("valid plate");
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("live.sqlite").to_string_lossy().to_string();
    let transport = Arc::new(CurlTransport {
        dump: std::env::var_os("SAPPA_LIVE_DUMP").map(Into::into),
        n: Mutex::new(0),
    });
    let mut cfg = default_config(db, "SAPPA/0.1 (live test)".into());
    cfg.retry_backoff_ms = vec![]; // one attempt only: be polite
    let core = SappaCore::new(cfg, transport).unwrap();
    core.add_vehicle(plate.clone(), kind, None, now()).unwrap();

    let r = core.run_check(now(), CheckTrigger::Manual, 30_000).unwrap();
    eprintln!("result: {:?}, error: {:?}", r.result, r.error);
    for o in &r.outcomes {
        eprintln!(
            "  {} {:?} http={:?}",
            mask_plate(o.plate.clone()),
            o.outcome,
            o.http_status
        );
    }
    assert_eq!(r.result, RunResult::Ok, "live lookup did not succeed");
    let fines = core.list_fines(None, now()).unwrap();
    eprintln!("{} fines parsed for {}", fines.len(), mask_plate(plate));
    for f in &fines {
        eprintln!(
            "  {:?} {} ${} issued={} stage={:?} days_left={}",
            f.status,
            f.infraction_code,
            f.amount,
            f.issued_at,
            f.discount.stage,
            f.discount.business_days_left
        );
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
