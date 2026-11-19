# sappa_be — SAPPA shared core

Rust library with all of SAPPA's logic: SIMIT plate lookup, response parsing, change detection,
discount deadlines and the on-device SQLite store. No UI and no scheduling: the iOS app (and later
Android, or a server) wakes it, calls it, and acts on what it returns. Bindings come from
[UniFFI](https://mozilla.github.io/uniffi-rs/). SAPPA is not affiliated with SIMIT or the
Federación Colombiana de Municipios.

## Layout

| Path | What |
| --- | --- |
| `src/plate.rs` | Plate validation (car `ABC123`, moto `ABC12D`), masking (`TTO***`) |
| `src/calendar.rs` | Colombian business days and holidays (Ley Emiliani, Easter-based) |
| `src/simit/` | Request building, `HttpTransport` foreign trait, tolerant JSON parser |
| `src/detector.rs` | Pure diff (new / updated / unchanged / resolved) and 50% / 25% discount windows |
| `src/store.rs` | SQLite: WAL, busy timeout, numbered migrations (`PRAGMA user_version`) |
| `src/runner.rs` | `run_check`: idempotent, resumable, jitter, retries, stop on block, checks-failing alert |
| `src/lib.rs` | Public API (`SappaCore` object + free functions) |
| `fixtures/` | SIMIT responses used by tests (partly real, partly synthetic: see `fixtures/README.md`) |
| `tests/core.rs` | End-to-end API tests with a fake transport |

## Build and test

```sh
cargo test                 # unit + integration tests, never hits SIMIT
cargo clippy --all-targets
scripts/build-ios.sh       # xcframework + Swift bindings for the SappaCore Swift package
```

Requires rustup (`brew install rustup`, then `rustup default stable`). The script adds the iOS
targets.

### Swift package

This repo is also the Swift package **SappaCore** (`Package.swift`, iOS 26+):

- binary target `sappa_coreFFI` = `dist/ios/SappaCore.xcframework` (static Rust library + C header
  and module map);
- Swift target `SappaCore` = `swift/Sources/SappaCore/SappaCore.swift`, the generated UniFFI API
  (committed; never edit by hand).

Apps add it as a local package (`../sappa_be`) and `import SappaCore`. The xcframework is not
committed (`dist/` is ignored), so run `scripts/build-ios.sh` before building the app and after
every core change. To use it as a remote package later, publish the zipped xcframework as a
release asset and switch the binary target to `url:` + `checksum:`.

## Rules

- The core returns codes and values (`AlertKind`, `ErrorClass`, `DiscountStage`, timestamps,
  pesos), never sentences.
- Network goes through `HttpTransport`, implemented by the shell with the platform's default TLS.
  `simit_base_url` and `captcha_url` must be https.
- Each lookup is two requests, as on SIMIT's own page (confirmed from browser traffic, Nov 2026):
  1. `POST https://qxcaptcha.fcm.org.co/api.php`, multipart form `endpoint=question` →
     `{"error":false,"data":{"question":"<hex>","recommended_difficulty":2}}`;
  2. the core finds a prime `nonce` with `sha256('{"question":…,"time":<unix s>,"nonce":N}')`
     starting with `0000` (`src/simit/pow.rs`, ~30 ms in release; the page's difficulty is 1 and
     `recommended_difficulty` is ignored, as the page does), then
     `POST …/estadocuenta/consulta` with
     `{"filtro":"<plate>","reCaptchaDTO":{"response":"[{question,time,nonce}]","consumidor":"1"}}`.

  This is a cost-per-request proof of work the page solves without user input. If SIMIT switches
  to a human check (its config already lists Cloudflare Turnstile and mosparo, both off), or the
  service answers `error: true`, the core reports `blocked`; it does not try to get around it.
- `run_check(now, trigger, budget_ms)`:
  - scheduled runs skip plates checked successfully in the last 2 h; manual runs skip plates tried
    in the last 10 min;
  - each plate commits on its own, oldest-check first, so a run cut by the budget resumes;
  - 403/429/captcha page/captcha-service refusal → `blocked`, and no further requests that run;
  - the first check of a new plate is a *baseline*: fines are stored but raise no `new_fine`;
  - alerts are unique per (fine, kind) and keep coming back in reports until the shell calls
    `mark_alerts_delivered`. Notification id: `sappa-alert-<id>`.
- Widgets read `widget_snapshot()` JSON, never the database.

## Open questions

- **Live check:** `SAPPA_LIVE_PLATE=ABC123 cargo test --test live -- --ignored --nocapture`
  or `cargo test --test live live_lookup_abc123 -- --ignored --nocapture`
  (opt-in, one attempt, never in the normal suite). Verified 2026-11-18: SIMIT accepts the proof
  of work; the empty response is `fixtures/no_fines.json`. A lookup returns `multas`,
  `comparendos`, `resoluciones`, `acuerdosPago`, `cursos` and totals.
- **Big records are slow.** ABC123 returns 1,050 records (1.9 MB) in ~20 s; parsing takes
  ~33 ms. So a plate's first lookup (its baseline) gets `first_check_timeout_ms` (30 s, capped by
  the budget left); every later lookup keeps `http_timeout_ms` (10 s), since between periodic
  checks a record only changes by a few fines.
- **SIMIT response format.** Confirmed with real data (`fixtures/real_three_fines.json`); the
  remaining synthetic fixtures still use guessed field names in places (the page reads `multas`, `infracciones[].codigoInfraccion`,
  `numeroComparendo` and `personasMismoDocumento`, which matches). Record a real response for your
  own plate and replace the fixtures. SIMIT's terms of use for automated lookups are not reviewed.
- Discount windows use Art. 136 CNT (5 / 20 business days from the comparendo, or from
  notification when SIMIT gives that date). Verify against SIMIT's own values once real data is in.
