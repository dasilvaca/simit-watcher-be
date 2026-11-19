//! SIMIT client: builds the plate lookup request, sends it through the shell-provided transport,
//! and parses the answer. It never touches the database.

pub mod parse;
pub mod pow;

use std::sync::Arc;

use crate::model::ErrorClass;
pub use parse::SimitFine;

/// Default SIMIT host and the plate lookup path used by the public "Estado de cuenta" page
/// (confirmed from browser traffic, November 2026). It is not a documented API.
pub const DEFAULT_BASE_URL: &str = "https://consultasimit.fcm.org.co";
pub const LOOKUP_PATH: &str = "/simit/microservices/estado-cuenta-simit/estadocuenta/consulta";
/// Proof-of-work service the page asks for a `question` right before each lookup.
pub const DEFAULT_CAPTCHA_URL: &str = "https://qxcaptcha.fcm.org.co/api.php";
/// The page's configured difficulty (`weHateCaptchaConfig.difficulty`); it is also sent as
/// `reCaptchaDTO.consumidor`. The service's `recommended_difficulty` is ignored, as the page does.
pub const DEFAULT_CAPTCHA_DIFFICULTY: u32 = 1;
const FORM_BOUNDARY: &str = "----SappaFormBoundary7MA4YWxkTrZu0gW";

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
    captcha_url: String,
    captcha_difficulty: u32,
    user_agent: String,
    timeout_ms: u32,
}

impl SimitClient {
    pub fn new(
        transport: Arc<dyn HttpTransport>,
        base_url: String,
        captcha_url: String,
        captcha_difficulty: u32,
        user_agent: String,
        timeout_ms: u32,
    ) -> Self {
        Self {
            transport,
            base_url: base_url.trim_end_matches('/').to_string(),
            captcha_url,
            captcha_difficulty: captcha_difficulty.max(1),
            user_agent,
            timeout_ms,
        }
    }

    fn header(name: &str, value: &str) -> HttpHeader {
        HttpHeader {
            name: name.into(),
            value: value.into(),
        }
    }

    /// `multipart/form-data` with the single field `endpoint=question`, as the page sends it.
    pub fn build_question_request(&self) -> HttpRequest {
        let body = format!(
            "--{FORM_BOUNDARY}\r\nContent-Disposition: form-data; name=\"endpoint\"\r\n\r\nquestion\r\n--{FORM_BOUNDARY}--\r\n"
        );
        HttpRequest {
            method: "POST".into(),
            url: self.captcha_url.clone(),
            headers: vec![
                Self::header(
                    "Content-Type",
                    &format!("multipart/form-data; boundary={FORM_BOUNDARY}"),
                ),
                Self::header("Accept", "application/json"),
                Self::header("User-Agent", &self.user_agent),
            ],
            body: Some(body.into_bytes()),
            timeout_ms: self.timeout_ms,
        }
    }

    /// `timeout_ms` overrides the client's default (used for a plate's first check).
    pub fn build_request(
        &self,
        plate: &str,
        solutions: &[pow::Solution],
        timeout_ms: Option<u32>,
    ) -> HttpRequest {
        let response = serde_json::to_string(solutions).expect("plain structs");
        let body = serde_json::json!({
            "filtro": plate,
            "reCaptchaDTO": {
                "response": response,
                "consumidor": self.captcha_difficulty.to_string(),
            },
        });
        HttpRequest {
            method: "POST".into(),
            url: format!("{}{}", self.base_url, LOOKUP_PATH),
            headers: vec![
                Self::header("Content-Type", "application/json"),
                Self::header("Accept", "application/json"),
                Self::header("User-Agent", &self.user_agent),
            ],
            body: Some(serde_json::to_vec(&body).expect("static json")),
            timeout_ms: timeout_ms.unwrap_or(self.timeout_ms).max(self.timeout_ms),
        }
    }

    fn send(&self, request: HttpRequest) -> Result<HttpResponse, LookupError> {
        let resp = self.transport.send(request).map_err(|e| match e {
            TransportError::Offline => LookupError::new(ErrorClass::Offline, None),
            TransportError::Timeout => LookupError::new(ErrorClass::Timeout, None),
            TransportError::Other { .. } => LookupError::new(ErrorClass::Offline, None),
        })?;
        match resp.status {
            200..=299 => Ok(resp),
            403 | 429 => Err(LookupError::new(ErrorClass::Blocked, Some(resp.status))),
            s => Err(LookupError::new(ErrorClass::Http, Some(s))),
        }
    }

    /// Asks the captcha service for a fresh question. Each question is used for one lookup.
    fn question(&self) -> Result<String, LookupError> {
        let resp = self.send(self.build_question_request())?;
        parse::parse_question(&resp.body).map_err(|e| match e {
            parse::ParseError::Captcha => LookupError::new(ErrorClass::Blocked, Some(resp.status)),
            parse::ParseError::Shape(_) => LookupError::new(ErrorClass::Parse, Some(resp.status)),
        })
    }

    /// Looks up one plate: fetch a question, solve it, then query. `now` (unix seconds) is the
    /// `time` the solution is bound to. `timeout_ms` overrides the lookup timeout (never below the
    /// default; the question request always uses the default). Never retries; the check runner
    /// owns retries and backoff.
    pub fn lookup(
        &self,
        plate: &str,
        now: i64,
        timeout_ms: Option<u32>,
    ) -> Result<Vec<SimitFine>, LookupError> {
        let question = self.question()?;
        let solutions = pow::solve(&question, now, self.captcha_difficulty);
        let resp = self.send(self.build_request(plate, &solutions, timeout_ms))?;
        parse::parse_lookup(&resp.body, plate).map_err(|e| match e {
            parse::ParseError::Captcha => LookupError::new(ErrorClass::Blocked, Some(resp.status)),
            parse::ParseError::Shape(_) => LookupError::new(ErrorClass::Parse, Some(resp.status)),
        })
    }
}
