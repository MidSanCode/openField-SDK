//! HTTP transport shared by every endpoint module.
//!
//! One [`HttpClient`] owns a [`ureq::Agent`], which pools connections and therefore
//! must be reused rather than rebuilt per request. Agents are internally
//! reference-counted and `Send + Sync`, so the client is cheap to clone and safe to
//! share across threads and bindings.
//!
//! ## Status handling
//!
//! The transport is split in two on purpose:
//!
//! * [`HttpClient::send_raw`] hands back the response **whatever its status**, so the
//!   raw-request escape hatch and the health probe can read a `400` or a `503` body;
//! * the typed helpers ([`HttpClient::json_raw`], [`HttpClient::empty`], …) call
//!   [`HttpResponse::error_for_status`], so endpoint code gets an [`SdkError::Http`].
//!
//! ## Retry
//!
//! `429` is retried for every verb — the server explicitly asked for a slower retry and
//! never processed the request — while `5xx` is retried for `GET`/`HEAD` only, so a
//! `POST /posts` cannot be duplicated by a retry.

use std::time::Duration;

use serde::de::DeserializeOwned;
use ureq::http::HeaderMap;

use crate::config::Config;
use crate::error::{ApiErrorBody, Result, SdkError};

/// Header name used for `Retry-After`, matching the HTTP spelling.
const RETRY_AFTER: &str = "retry-after";

/// A decoded HTTP response. Non-2xx statuses are representable, not exceptions, so
/// callers can inspect `401` vs `403` themselves.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers as received.
    pub headers: HeaderMap,
    /// Raw body bytes.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// Whether the status is in the 2xx range.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Parse the body as JSON, returning [`serde_json::Value::Null`] for an empty body.
    pub fn json(&self) -> Result<serde_json::Value> {
        if self.body.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(&self.body).map_err(|err| SdkError::Decode {
            message: err.to_string(),
            raw: truncate(&self.body),
        })
    }

    /// Parse the body into a type.
    pub fn decode<T: DeserializeOwned>(&self) -> Result<T> {
        if self.body.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Err(SdkError::Decode {
                message: format!(
                    "expected a {} body for HTTP {}, got an empty response",
                    std::any::type_name::<T>(),
                    self.status
                ),
                raw: String::new(),
            });
        }
        serde_json::from_slice(&self.body).map_err(|err| SdkError::Decode {
            message: err.to_string(),
            raw: truncate(&self.body),
        })
    }

    /// Turn this response into an [`SdkError::Http`].
    pub fn to_error(&self) -> SdkError {
        SdkError::Http {
            status: self.status,
            body: ApiErrorBody::parse(&self.body),
            retry_after_secs: self.retry_after_secs(),
        }
    }

    /// Parse a `Retry-After` header, supporting both the delta-seconds and HTTP-date
    /// forms.
    pub fn retry_after_secs(&self) -> Option<u64> {
        let raw = self.headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
        if let Ok(secs) = raw.parse::<u64>() {
            return Some(secs);
        }
        let target = parse_http_date(raw)?;
        let now = std::time::SystemTime::now();
        Some(target.duration_since(now).map(|d| d.as_secs()).unwrap_or(0))
    }

    /// Return `Ok(self)` for 2xx, `Err(SdkError::Http)` otherwise.
    pub fn error_for_status(self) -> Result<Self> {
        if self.is_success() {
            Ok(self)
        } else {
            Err(self.to_error())
        }
    }

    /// The decoded value for a 2xx response.
    pub fn into_result<T: DeserializeOwned>(self) -> Result<T> {
        self.error_for_status()?.decode()
    }
}

fn truncate(bytes: &[u8]) -> String {
    const LIMIT: usize = 2048;
    let slice = if bytes.len() > LIMIT {
        &bytes[..LIMIT]
    } else {
        bytes
    };
    let mut text = String::from_utf8_lossy(slice).to_string();
    if bytes.len() > LIMIT {
        text.push_str("…(truncated)");
    }
    text
}

/// Minimal IMF-fixdate parser for `Retry-After: Sun, 06 Nov 1994 08:49:37 GMT`.
///
/// Hand-rolled to keep the dependency set small; returns `None` for anything that is
/// not a valid date, in which case the caller falls back to its own backoff.
fn parse_http_date(raw: &str) -> Option<std::time::SystemTime> {
    use std::time::{Duration, UNIX_EPOCH};

    let parts: Vec<&str> = raw.split_whitespace().collect();
    // ["Sun,", "06", "Nov", "1994", "08:49:37", "GMT"]
    if parts.len() < 5 {
        return None;
    }
    let day: u64 = parts[1].parse().ok()?;
    let month = match parts[2] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: u64 = parts[3].parse().ok()?;
    let clock: Vec<&str> = parts[4].split(':').collect();
    if clock.len() != 3 {
        return None;
    }
    let hour: u64 = clock[0].parse().ok()?;
    let minute: u64 = clock[1].parse().ok()?;
    let second: u64 = clock[2].parse().ok()?;
    if day == 0 || day > 31 || hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    // Days since the Unix epoch: whole years, then whole months, then days.
    let mut days: i64 = 0;
    for year_iter in 1970..year {
        days += if is_leap(year_iter) { 366 } else { 365 };
    }
    let lengths = [
        31,
        if is_leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    for length in lengths.iter().take(month as usize - 1) {
        days += *length as i64;
    }
    days += day as i64 - 1;

    let secs = days * 86_400 + (hour * 3_600 + minute * 60 + second) as i64;
    Some(UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64))
}

fn is_leap(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Body shape for a request.
enum Body {
    None,
    Json(serde_json::Value),
    Bytes(Vec<u8>),
}

/// A transport bound to one [`Config`].
#[derive(Debug, Clone)]
pub struct HttpClient {
    config: Config,
    agent: ureq::Agent,
}

impl HttpClient {
    /// Build a transport, validating the [`Config`] first.
    pub fn new(config: Config) -> Result<Self> {
        config.validate()?;
        let agent = ureq::config::Config::builder()
            .timeout_global(Some(config.timeout))
            .timeout_connect(Some(config.connect_timeout))
            .http_status_as_error(false)
            .build()
            .new_agent();
        Ok(Self { config, agent })
    }

    /// The configuration in force.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The bearer header value, when a token is configured.
    fn authorization(&self) -> Option<String> {
        self.config
            .token
            .as_ref()
            .map(|token| format!("Bearer {}", token.expose()))
    }

    /// Send a JSON request and decode a 2xx body into a type.
    pub(crate) fn json<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T> {
        self.json_raw(method, path, body, &[])?.decode()
    }

    /// Send a request with an optional JSON body and query pairs, as a checked response.
    pub(crate) fn json_raw(
        &self,
        method: &str,
        path: &str,
        body: Option<&serde_json::Value>,
        query: &[(String, String)],
    ) -> Result<HttpResponse> {
        let body = match body {
            Some(value) => Body::Json(value.clone()),
            None => Body::None,
        };
        self.send_with_retry(method, path, query, body)?
            .error_for_status()
    }

    /// Send a request with an empty body, as a checked response.
    pub(crate) fn empty(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
    ) -> Result<HttpResponse> {
        self.send_with_retry(method, path, query, Body::None)?
            .error_for_status()
    }

    /// Send a `multipart/form-data` request, as a checked response.
    ///
    /// `fields` become text parts and `files` become file parts; the OpenField
    /// attachment endpoint requires the file part to be named `file` (`bots.md` §6.5),
    /// and the avatar/banner handlers read `file` too.
    pub(crate) fn multipart(
        &self,
        method: &str,
        path: &str,
        fields: &[(String, String)],
        files: &[MultipartFile],
    ) -> Result<HttpResponse> {
        let url = self.config.resolve(path);
        let mut builder = self.builder(method, &url);
        for (name, value) in &self.config.headers {
            builder = builder.header(name, value);
        }
        if let Some(auth) = self.authorization() {
            builder = builder.header("Authorization", auth);
        }

        // `Form` and `Part` borrow their inputs, so the parts are built from the
        // caller's slices rather than cloned.
        let mut form = ureq::unversioned::multipart::Form::new();
        for (name, value) in fields {
            form = form.text(name, value);
        }
        for file in files {
            let part =
                ureq::unversioned::multipart::Part::bytes(&file.bytes).file_name(&file.file_name);
            let part = match &file.mime {
                Some(mime) => part
                    .mime_str(mime)
                    .map_err(|err| SdkError::InvalidArgument {
                        message: format!("invalid mime type `{mime}`: {err}"),
                    })?,
                None => part,
            };
            form = form.part(&file.field, part);
        }
        let raw = builder.send(form).map_err(map_transport_error)?;
        let (status, headers, body) = split_response(raw, self.config.max_response_bytes)?;
        HttpResponse {
            status,
            headers,
            body,
        }
        .error_for_status()
    }

    /// Issue a fully described [`crate::client::Request`] **without** checking the
    /// status, so a caller can read an error body.
    pub(crate) fn send_raw(&self, request: &crate::client::Request) -> Result<HttpResponse> {
        use crate::client::RequestBody;
        request.validate()?;
        match &request.body {
            RequestBody::Multipart { fields, files } => {
                let url = self.config.resolve(&request.path);
                let mut builder = self.builder(&request.method, &url);
                for (name, value) in &self.config.headers {
                    builder = builder.header(name, value);
                }
                if let Some(auth) = self.authorization() {
                    builder = builder.header("Authorization", auth);
                }
                for (name, value) in &request.query {
                    builder = builder.query(name, value);
                }
                let mut form = ureq::unversioned::multipart::Form::new();
                for (name, value) in fields {
                    form = form.text(name, value);
                }
                for file in files {
                    let part = ureq::unversioned::multipart::Part::bytes(&file.bytes)
                        .file_name(&file.file_name);
                    let part = match &file.mime {
                        Some(mime) => {
                            part.mime_str(mime)
                                .map_err(|err| SdkError::InvalidArgument {
                                    message: format!("invalid mime type `{mime}`: {err}"),
                                })?
                        }
                        None => part,
                    };
                    form = form.part(&file.field, part);
                }
                let raw = builder.send(form).map_err(map_transport_error)?;
                let (status, headers, body) = split_response(raw, self.config.max_response_bytes)?;
                Ok(HttpResponse {
                    status,
                    headers,
                    body,
                })
            }
            RequestBody::Json(value) => self.send_with_retry(
                &request.method,
                &request.path,
                &request.query,
                Body::Json(value.clone()),
            ),
            RequestBody::Bytes(bytes) => self.send_with_retry(
                &request.method,
                &request.path,
                &request.query,
                Body::Bytes(bytes.clone()),
            ),
            RequestBody::None => {
                self.send_with_retry(&request.method, &request.path, &request.query, Body::None)
            }
        }
    }

    /// The retry loop. Returns the response whatever its status once retrying stops.
    fn send_with_retry(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        body: Body,
    ) -> Result<HttpResponse> {
        let url = self.config.resolve(path);
        let policy = &self.config.retry;
        // Retrying a mutating verb can duplicate a post or message, so mutations are
        // only retried when the server explicitly signals throttling with 429.
        let method_retryable = matches!(method.to_ascii_uppercase().as_str(), "GET" | "HEAD");
        let mut attempt = 1u32;

        loop {
            match self.send_once(method, &url, query, &body) {
                Ok(response) if response.is_success() => return Ok(response),
                Ok(response) => {
                    if attempt >= policy.max_attempts {
                        return Ok(response);
                    }
                    let status = response.status;
                    let retryable =
                        status == 429 || (method_retryable && (500..600).contains(&status));
                    if !retryable {
                        return Ok(response);
                    }
                    let mut delay = policy.delay_for_attempt(attempt + 1);
                    if policy.respect_retry_after {
                        if let Some(secs) = response.retry_after_secs() {
                            delay = Duration::from_secs(secs)
                                .min(Duration::from_millis(policy.max_delay_ms));
                        }
                    }
                    if !delay.is_zero() {
                        std::thread::sleep(delay);
                    }
                    attempt += 1;
                }
                Err(err) => {
                    if attempt >= policy.max_attempts || !(method_retryable && err.is_retryable()) {
                        return Err(err);
                    }
                    let mut delay = policy.delay_for_attempt(attempt + 1);
                    if policy.respect_retry_after {
                        if let Some(secs) = err.retry_after_secs() {
                            delay = Duration::from_secs(secs)
                                .min(Duration::from_millis(policy.max_delay_ms));
                        }
                    }
                    if !delay.is_zero() {
                        std::thread::sleep(delay);
                    }
                    attempt += 1;
                }
            }
        }
    }

    fn send_once(
        &self,
        method: &str,
        url: &str,
        query: &[(String, String)],
        body: &Body,
    ) -> Result<HttpResponse> {
        let mut builder = self.builder(method, url);
        for (name, value) in &self.config.headers {
            builder = builder.header(name, value);
        }
        if let Some(auth) = self.authorization() {
            builder = builder.header("Authorization", auth);
        }
        for (name, value) in query {
            builder = builder.query(name, value);
        }

        let raw = match body {
            Body::Json(value) => builder.send_json(value),
            Body::Bytes(bytes) => builder.send(bytes.as_slice()),
            Body::None => builder.send_empty(),
        };
        let raw = raw.map_err(map_transport_error)?;
        let (status, headers, body) = split_response(raw, self.config.max_response_bytes)?;
        // The status is deliberately not checked here: retry policy needs to see the
        // code, and the caller decides whether to raise.
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }

    /// Build a request builder that can carry a body.
    ///
    /// `ureq` types the bodyless verbs (`GET`, `HEAD`, `DELETE`, `OPTIONS`) differently
    /// from the body-carrying ones, so `force_send_body` normalises them; the practical
    /// effect is that a `GET` carries `Content-Length: 0`, which every server accepts.
    /// One builder type also means the retry loop can be written once.
    fn builder(&self, method: &str, url: &str) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
        let builder = match method.to_ascii_uppercase().as_str() {
            "GET" => self.agent.get(url).force_send_body(),
            "POST" => self.agent.post(url),
            "PUT" => self.agent.put(url),
            "PATCH" => self.agent.patch(url),
            "DELETE" => self.agent.delete(url).force_send_body(),
            "HEAD" => self.agent.head(url).force_send_body(),
            "OPTIONS" => self.agent.options(url).force_send_body(),
            other => {
                debug_assert!(false, "unsupported HTTP method `{other}`");
                self.agent.post(url)
            }
        };
        builder.header("User-Agent", self.config.user_agent.clone())
    }
}

/// One file part of a multipart upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartFile {
    /// Form field name; the attachment, avatar and banner handlers all expect `file`.
    pub field: String,
    /// File name reported to the server.
    pub file_name: String,
    /// Optional MIME type override.
    pub mime: Option<String>,
    /// File contents.
    pub bytes: Vec<u8>,
}

impl MultipartFile {
    /// Build a file part, guessing the MIME type from the extension.
    pub fn new(file_name: impl Into<String>, bytes: Vec<u8>) -> Self {
        let file_name = file_name.into();
        Self {
            field: "file".to_string(),
            mime: guess_mime(&file_name),
            file_name,
            bytes,
        }
    }

    /// Override the form field name.
    pub fn with_field(mut self, field: impl Into<String>) -> Self {
        self.field = field.into();
        self
    }

    /// Override the MIME type.
    pub fn with_mime(mut self, mime: impl Into<String>) -> Self {
        self.mime = Some(mime.into());
        self
    }
}

/// Tiny extension-to-MIME table. The server decides what it accepts; this only avoids
/// the generic `application/octet-stream` for common uploads.
fn guess_mime(file_name: &str) -> Option<String> {
    let ext = std::path::Path::new(file_name)
        .extension()?
        .to_string_lossy()
        .to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" | "log" | "md" => "text/plain",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        _ => return None,
    };
    Some(mime.to_string())
}

/// Split a ureq response into status, headers and a fully-read body.
///
/// Streaming is deliberately avoided: every OpenField response is a small JSON
/// document, so buffering keeps both the retry loop and the bindings simple.
fn split_response(
    raw: ureq::http::Response<ureq::Body>,
    limit: u64,
) -> Result<(u16, HeaderMap, Vec<u8>)> {
    let (parts, body) = raw.into_parts();
    let status = parts.status.as_u16();
    let headers = parts.headers;
    let bytes = body
        .into_with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(map_transport_error)?;
    Ok((status, headers, bytes))
}

fn map_transport_error(err: ureq::Error) -> SdkError {
    match err {
        ureq::Error::StatusCode(code) => SdkError::Http {
            status: code,
            body: ApiErrorBody::default(),
            retry_after_secs: None,
        },
        other => SdkError::Network {
            message: other.to_string(),
            retryable: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses_mime_types() {
        assert_eq!(
            MultipartFile::new("chart.png", vec![]).mime.as_deref(),
            Some("image/png")
        );
        assert_eq!(
            MultipartFile::new("a.JPEG", vec![]).mime.as_deref(),
            Some("image/jpeg")
        );
        assert_eq!(
            MultipartFile::new("notes.md", vec![]).mime.as_deref(),
            Some("text/plain")
        );
        assert_eq!(MultipartFile::new("mystery", vec![]).mime, None);
        assert_eq!(MultipartFile::new("x.unknown", vec![]).mime, None);
    }

    #[test]
    fn multipart_defaults_to_the_file_field() {
        let file = MultipartFile::new("a.png", vec![1, 2, 3]);
        assert_eq!(file.field, "file");
        let renamed = file.with_field("avatar");
        assert_eq!(renamed.field, "avatar");
    }

    #[test]
    fn parses_retry_after_seconds_and_dates() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, "42".parse().unwrap());
        let response = HttpResponse {
            status: 429,
            headers,
            body: vec![],
        };
        assert_eq!(response.retry_after_secs(), Some(42));

        let mut headers = HeaderMap::new();
        headers.insert(
            RETRY_AFTER,
            "Sun, 06 Nov 1994 08:49:37 GMT".parse().unwrap(),
        );
        let response = HttpResponse {
            status: 429,
            headers,
            body: vec![],
        };
        // A past date clamps to zero rather than underflowing.
        assert_eq!(response.retry_after_secs(), Some(0));

        let response = HttpResponse {
            status: 429,
            headers: HeaderMap::new(),
            body: vec![],
        };
        assert_eq!(response.retry_after_secs(), None);
    }

    #[test]
    fn parses_future_http_dates() {
        let future = std::time::SystemTime::now() + Duration::from_secs(120);
        let secs = future
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let days = secs / 86_400;
        let time = secs % 86_400;
        let (year, month, day) = civil_from_days(days as i64);
        let raw = format!(
            "Sun, {:02} {} {} {:02}:{:02}:{:02} GMT",
            day,
            MONTHS[month as usize - 1],
            year,
            time / 3_600,
            (time % 3_600) / 60,
            time % 60
        );
        let parsed = parse_http_date(&raw).expect("date should parse");
        let delta = parsed
            .duration_since(std::time::SystemTime::now())
            .unwrap()
            .as_secs();
        assert!((118..=120).contains(&delta), "unexpected delta {delta}");
    }

    #[test]
    fn rejects_nonsense_dates() {
        assert!(parse_http_date("not a date").is_none());
        assert!(parse_http_date("Sun, 99 Nov 1994 08:49:37 GMT").is_none());
        assert!(parse_http_date("Sun, 06 Xxx 1994 08:49:37 GMT").is_none());
        assert!(parse_http_date("Sun, 06 Nov 1994 99:49:37 GMT").is_none());
    }

    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    /// Days-since-epoch to (year, month, day), Howard Hinnant's algorithm.
    fn civil_from_days(z: i64) -> (i64, u32, u32) {
        let z = z + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        (if m <= 2 { y + 1 } else { y }, m, d)
    }

    #[test]
    fn decodes_bodies_and_reports_decode_errors() {
        let ok = HttpResponse {
            status: 200,
            headers: HeaderMap::new(),
            body: br#"{"id":7}"#.to_vec(),
        };
        let value: serde_json::Value = ok.decode().unwrap();
        assert_eq!(value["id"], 7);

        let bad = HttpResponse {
            status: 200,
            headers: HeaderMap::new(),
            body: b"not json".to_vec(),
        };
        let err = bad.decode::<serde_json::Value>().unwrap_err();
        assert_eq!(err.kind(), crate::error::ErrorKind::Decode);

        let empty = HttpResponse {
            status: 204,
            headers: HeaderMap::new(),
            body: vec![],
        };
        assert_eq!(empty.json().unwrap(), serde_json::Value::Null);
        assert!(
            empty.decode::<serde_json::Value>().is_err(),
            "a typed decode of an empty body is an error, not a silent default"
        );
    }

    #[test]
    fn error_for_status_preserves_the_envelope() {
        let response = HttpResponse {
            status: 403,
            headers: HeaderMap::new(),
            body: br#"{"error":"permission denied","permission":"chat.send"}"#.to_vec(),
        };
        let err = response.clone().error_for_status().unwrap_err();
        assert_eq!(err.status(), Some(403));
        assert_eq!(err.permission(), Some("chat.send"));
        assert_eq!(err.kind(), crate::error::ErrorKind::Forbidden);
        assert!(!err.is_retryable(), "403 must never be retried");
        assert!(response.error_for_status().is_err());
    }

    #[test]
    fn a_503_body_is_still_readable() {
        // The health endpoint answers 503 with a parseable body; the raw path keeps it.
        let response = HttpResponse {
            status: 503,
            headers: HeaderMap::new(),
            body: br#"{"all_healthy":false,"services":{}}"#.to_vec(),
        };
        assert!(!response.is_success());
        let report: crate::models::HealthReport = response.decode().unwrap();
        assert!(!report.all_healthy);
        assert_eq!(response.to_error().status(), Some(503));
        assert!(response.to_error().is_retryable(), "503 is worth retrying");
    }

    #[test]
    fn client_rejects_bad_config() {
        assert!(HttpClient::new(Config::new("", None)).is_err());
        assert!(HttpClient::new(Config::new("http://localhost:8080", None)).is_ok());
    }
}
