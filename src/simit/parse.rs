//! Parser for SIMIT's "estado de cuenta" JSON.
//!
//! The exact shape is not documented. This parser accepts the field names observed on the public
//! site's JSON (`multas` / `comparendos`, `numeroComparendo`, `infracciones[].codigoInfraccion`,
//! `valorPagar`, ...) and a few aliases, and fails with `ParseError::Shape` on anything else so a
//! site change shows up as its own error class instead of "no fines".

use chrono::{NaiveDate, NaiveDateTime};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::calendar;
use crate::model::FineStatus;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimitFine {
    pub simit_ref: String,
    pub infraction_code: String,
    pub infraction_description: Option<String>,
    pub place: Option<String>,
    pub amount: i64,
    pub issued_at: i64,
    pub notified_at: Option<i64>,
    pub status: FineStatus,
}

impl SimitFine {
    /// Hash of the fields that matter, to detect changes without storing raw responses.
    pub fn raw_hash(&self) -> String {
        let canon = format!(
            "{}|{}|{}|{}|{}|{}|{}",
            self.simit_ref,
            self.infraction_code,
            self.place.as_deref().unwrap_or(""),
            self.amount,
            self.issued_at,
            self.notified_at.map(|n| n.to_string()).unwrap_or_default(),
            self.status.as_str()
        );
        let digest = Sha256::digest(canon.as_bytes());
        digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The response asks for a captcha / human verification.
    Captcha,
    /// Unexpected JSON shape; the string names what was missing (for debug builds only).
    Shape(String),
}

const LIST_KEYS: &[&str] = &["multas", "comparendos", "comparendosMultas"];

fn first<'a>(obj: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .find_map(|k| obj.get(*k))
        .filter(|v| !v.is_null())
}

fn as_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn as_amount(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.round() as i64)),
        Value::String(s) => {
            // "$ 1.234.567" or "1234567.00"
            let s = s.trim().trim_start_matches('$').trim();
            let s = match s.rsplit_once(',') {
                Some((int, dec)) if dec.len() <= 2 => int.to_string(),
                _ => s.to_string(),
            };
            let s = match s.rsplit_once('.') {
                Some((int, dec)) if dec.len() <= 2 => int.to_string(),
                _ => s,
            };
            let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
            digits.parse().ok()
        }
        _ => None,
    }
}

/// Accepts "dd/mm/yyyy", "dd/mm/yyyy HH:MM[:SS]", "yyyy-mm-dd", "yyyy-mm-ddTHH:MM:SS[...]" and
/// Unix milliseconds. Dates without a time are taken as noon Bogotá.
pub fn parse_date(v: &Value) -> Option<i64> {
    if let Value::Number(n) = v {
        let ms = n.as_i64()?;
        return Some(if ms > 10_000_000_000 { ms / 1000 } else { ms });
    }
    let s = v.as_str()?.trim();
    let head = s.get(..19.min(s.len()))?;
    for fmt in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%d/%m/%Y %H:%M:%S",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(head, fmt) {
            return Some(
                calendar::ts_at(dt.date(), 0, 0)
                    + dt.time()
                        .signed_duration_since(chrono::NaiveTime::MIN)
                        .num_seconds(),
            );
        }
    }
    let date_part = s.split([' ', 'T']).next()?;
    for fmt in ["%d/%m/%Y", "%Y-%m-%d", "%d-%m-%Y"] {
        if let Ok(d) = NaiveDate::parse_from_str(date_part, fmt) {
            return Some(calendar::ts_at(d, 12, 0));
        }
    }
    None
}

fn parse_status(v: Option<&Value>) -> FineStatus {
    let Some(s) = v.and_then(as_text) else {
        return FineStatus::Pending;
    };
    let s = s.to_lowercase();
    if s.contains("pend") || s.contains("vigente") || s.contains("cobro") || s.contains("impag") {
        FineStatus::Pending
    } else if s.contains("pagad") || s.contains("cancelad") {
        FineStatus::Paid
    } else {
        FineStatus::Other
    }
}

fn parse_fine(item: &Value) -> Result<SimitFine, ParseError> {
    let shape = |what: &str| ParseError::Shape(what.to_string());
    let simit_ref = first(
        item,
        &["numeroComparendo", "comparendo", "numero", "nroComparendo"],
    )
    .and_then(as_text)
    .ok_or_else(|| shape("numeroComparendo"))?;

    let infraccion = first(item, &["infracciones"])
        .and_then(|v| v.as_array())
        .and_then(|a| a.first());
    let code_src = infraccion.unwrap_or(item);
    let infraction_code = first(code_src, &["codigoInfraccion", "infraccion", "codigo"])
        .and_then(as_text)
        .ok_or_else(|| shape("codigoInfraccion"))?;
    let infraction_description =
        first(code_src, &["descripcionInfraccion", "descripcion"]).and_then(as_text);

    let amount = first(
        item,
        &["valorPagar", "valorAPagar", "valor", "saldo", "total"],
    )
    .or_else(|| infraccion.and_then(|i| first(i, &["valorInfraccion", "valor"])))
    .and_then(as_amount)
    .ok_or_else(|| shape("valorPagar"))?;

    let issued_at = first(item, &["fechaComparendo", "fechaInfraccion", "fecha"])
        .and_then(parse_date)
        .ok_or_else(|| shape("fechaComparendo"))?;
    let notified_at = first(item, &["fechaNotificacion"]).and_then(parse_date);

    let place = first(
        item,
        &[
            "ciudad",
            "municipio",
            "secretaria",
            "organismoTransito",
            "lugar",
        ],
    )
    .and_then(|v| {
        // Some responses nest the office: { "secretaria": { "nombre": "..." } }
        as_text(v).or_else(|| v.get("nombre").and_then(as_text))
    });

    let status = parse_status(first(
        item,
        &["estadoComparendo", "estado", "estadoCartera"],
    ));

    Ok(SimitFine {
        simit_ref,
        infraction_code: infraction_code.to_uppercase(),
        infraction_description,
        place,
        amount,
        issued_at,
        notified_at,
        status,
    })
}

/// Parses a lookup response. Returns only fines for `plate` when items carry a plate.
/// Reads the captcha service answer `{"error": false, "data": {"question": "<hex>", …}}`.
/// `error: true` means the service refused us, which is treated like a block.
pub fn parse_question(body: &[u8]) -> Result<String, ParseError> {
    let v: Value = serde_json::from_slice(body).map_err(|e| ParseError::Shape(e.to_string()))?;
    if v.get("error").and_then(Value::as_bool) == Some(true) {
        return Err(ParseError::Captcha);
    }
    let q = v
        .pointer("/data/question")
        .and_then(Value::as_str)
        .ok_or_else(|| ParseError::Shape("no data.question".into()))?;
    if q.is_empty() || q.len() > 256 || !q.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(ParseError::Shape("bad question".into()));
    }
    Ok(q.to_string())
}

pub fn parse_lookup(body: &[u8], plate: &str) -> Result<Vec<SimitFine>, ParseError> {
    let text = String::from_utf8_lossy(body);
    let value: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            let lower = text.to_lowercase();
            return Err(
                if lower.contains("captcha") || lower.contains("recaptcha") {
                    ParseError::Captcha
                } else {
                    ParseError::Shape("not json".into())
                },
            );
        }
    };
    if let Some(obj) = value.as_object() {
        let captcha_flag = obj.iter().any(|(k, v)| {
            k.to_lowercase().contains("captcha") && (v.as_bool() == Some(true) || v.is_string())
        });
        if captcha_flag && !LIST_KEYS.iter().any(|k| obj.contains_key(*k)) {
            return Err(ParseError::Captcha);
        }
    }

    let list = match &value {
        Value::Array(a) => a,
        Value::Object(_) => {
            let container = value
                .get("data")
                .filter(|d| d.is_object())
                .unwrap_or(&value);
            match first(container, LIST_KEYS) {
                Some(Value::Array(a)) => a,
                // Explicit "no fines" shapes.
                None if container.get("totalGeneral").is_some()
                    || container.get("paz_salvo").is_some()
                    || container.get("pazYSalvo").is_some() =>
                {
                    return Ok(vec![]);
                }
                _ => return Err(ParseError::Shape("multas".into())),
            }
        }
        _ => return Err(ParseError::Shape("root".into())),
    };

    let mut fines = Vec::with_capacity(list.len());
    for item in list {
        if let Some(p) = first(item, &["placa", "placaVehiculo"]).and_then(as_text)
            && crate::plate::normalize(&p) != crate::plate::normalize(plate)
        {
            continue;
        }
        fines.push(parse_fine(item)?);
    }
    Ok(fines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn amounts() {
        assert_eq!(as_amount(&json!(604200)), Some(604200));
        assert_eq!(as_amount(&json!(604200.0)), Some(604200));
        assert_eq!(as_amount(&json!("$ 604.200")), Some(604200));
        assert_eq!(as_amount(&json!("604200.00")), Some(604200));
        assert_eq!(as_amount(&json!("$604.200,50")), Some(604200));
    }

    #[test]
    fn dates() {
        let noon = calendar::ts_at(NaiveDate::from_ymd_opt(2026, 11, 13).unwrap(), 12, 0);
        assert_eq!(parse_date(&json!("13/11/2026")), Some(noon));
        assert_eq!(parse_date(&json!("2026-11-13")), Some(noon));
        assert_eq!(
            parse_date(&json!("2026-11-13T12:00:00.000-05:00")),
            Some(noon)
        );
        assert_eq!(parse_date(&json!("13/11/2026 12:00:00")), Some(noon));
        assert_eq!(parse_date(&json!(noon * 1000)), Some(noon));
        assert_eq!(parse_date(&json!("ayer")), None);
    }

    #[test]
    fn question() {
        let ok = br#"{"error":false,"data":{"question":"51105c0550b068e4620513eea19d07a3","recommended_difficulty":2}}"#;
        assert_eq!(
            parse_question(ok).unwrap(),
            "51105c0550b068e4620513eea19d07a3"
        );
        assert_eq!(
            parse_question(br#"{"error":true,"data":null}"#),
            Err(ParseError::Captcha)
        );
        assert!(matches!(
            parse_question(br#"{"error":false,"data":{}}"#),
            Err(ParseError::Shape(_))
        ));
        assert!(matches!(
            parse_question(br#"{"error":false,"data":{"question":"a\"b"}}"#),
            Err(ParseError::Shape(_))
        ));
        assert!(matches!(
            parse_question(b"<html>"),
            Err(ParseError::Shape(_))
        ));
    }

    #[test]
    fn captcha_and_shape() {
        assert_eq!(
            parse_lookup(b"<html>reCAPTCHA</html>", "ABC123"),
            Err(ParseError::Captcha)
        );
        assert_eq!(
            parse_lookup(br#"{"captchaRequired": true}"#, "ABC123"),
            Err(ParseError::Captcha)
        );
        assert!(matches!(
            parse_lookup(br#"{"foo": 1}"#, "ABC123"),
            Err(ParseError::Shape(_))
        ));
        assert_eq!(parse_lookup(br#"{"multas": []}"#, "ABC123"), Ok(vec![]));
    }
}
