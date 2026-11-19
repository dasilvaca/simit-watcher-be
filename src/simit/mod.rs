//! SIMIT client: builds the plate lookup request, sends it through the shell-provided transport,
//! and parses the answer. It never touches the database.

pub mod parse;

use std::sync::Arc;

use crate::model::ErrorClass;
pub use parse::SimitFine;

/// Default SIMIT host. The plate lookup path below is the one used by the public
/// "Estado de cuenta" page; it is NOT a documented API and must be confirmed against real traffic
/// (see README "Open questions").
pub const DEFAULT_BASE_URL: &str = "https://consultasimit.fcm.org.co";
pub const LOOKUP_PATH: &str = "/simit/microservices/estado-cuenta-simit/estadocuenta/consulta";

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct HttpHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<HttpHeader>,
    pub body: Option<Vec<u8>>,
    pub timeout_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum TransportError {
    #[error("offline")]
    Offline,
    #[error("timeout")]
    Timeout,
    #[error("transport: {detail}")]
    Other { detail: String },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for TransportError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> Self {
        TransportError::Other { detail: e.reason }
    }
}

/// Network interface implemented by each shell (URLSession on iOS, OkHttp/HttpURLConnection on
/// Android, a real HTTP client on a server). It is called from a background thread and may block.
/// It must use the platform's default TLS validation.
#[uniffi::export(with_foreign)]
pub trait HttpTransport: Send + Sync {
    fn send(&self, request: HttpRequest) -> Result<HttpResponse, TransportError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupError {
    pub class: ErrorClass,
    pub http_status: Option<u16>,
}

impl LookupError {
    fn new(class: ErrorClass, http_status: Option<u16>) -> Self {
        Self { class, http_status }
    }
    /// Worth retrying within the same run.
    pub fn retryable(&self) -> bool {
        matches!(self.class, ErrorClass::Offline | ErrorClass::Timeout)
            || (self.class == ErrorClass::Http && self.http_status.is_some_and(|s| s >= 500))
    }
}

pub struct SimitClient {
    transport: Arc<dyn HttpTransport>,
    base_url: String,
    user_agent: String,
    timeout_ms: u32,
}

impl SimitClient {
    pub fn new(
        transport: Arc<dyn HttpTransport>,
        base_url: String,
        user_agent: String,
        timeout_ms: u32,
    ) -> Self {
        Self {
            transport,
            base_url: base_url.trim_end_matches('/').to_string(),
            user_agent,
            timeout_ms,
        }
    }

    pub fn build_request(&self, plate: &str) -> HttpRequest {
        let body = serde_json::json!({ "filtro": plate });
        HttpRequest {
            method: "POST".into(),
            url: format!("{}{}", self.base_url, LOOKUP_PATH),
            headers: vec![
                HttpHeader {
                    name: "Content-Type".into(),
                    value: "application/json".into(),
                },
                HttpHeader {
                    name: "Accept".into(),
                    value: "application/json".into(),
                },
                HttpHeader {
                    name: "User-Agent".into(),
                    value: self.user_agent.clone(),
                },
            ],
            body: Some(serde_json::to_vec(&body).expect("static json")),
            timeout_ms: self.timeout_ms,
        }
    }

    /// Looks up one plate. Never retries; the check runner owns retries and backoff.
    pub fn lookup(&self, plate: &str) -> Result<Vec<SimitFine>, LookupError> {
        let resp = self
            .transport
            .send(self.build_request(plate))
            .map_err(|e| match e {
                TransportError::Offline => LookupError::new(ErrorClass::Offline, None),
                TransportError::Timeout => LookupError::new(ErrorClass::Timeout, None),
                TransportError::Other { .. } => LookupError::new(ErrorClass::Offline, None),
            })?;
        match resp.status {
            200..=299 => {}
            403 | 429 => return Err(LookupError::new(ErrorClass::Blocked, Some(resp.status))),
            s => return Err(LookupError::new(ErrorClass::Http, Some(s))),
        }
        parse::parse_lookup(&resp.body, plate).map_err(|e| match e {
            parse::ParseError::Captcha => LookupError::new(ErrorClass::Blocked, Some(resp.status)),
            parse::ParseError::Shape(_) => LookupError::new(ErrorClass::Parse, Some(resp.status)),
        })
    }
}
