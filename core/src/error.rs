//! Error types for the OpenField bot SDK.
//!
//! Every failure surfaces as a single [`SdkError`] so that language bindings only
//! have to translate one shape. HTTP failures keep the status code and the parsed
//! server envelope (`{"error": "...", "permission": "..."}`), because the bot
//! documentation distinguishes a number of `400`/`403`/`409` cases by `error` text.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, SdkError>;

/// The server's error envelope, e.g. `{"error":"permission denied","permission":"chat.send"}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiErrorBody {
    /// Human readable error code/message. Absent when the body was not a JSON envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Missing permission key, present on `403` permission failures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
    /// Bot ceiling, present on `403 {"error":"bot limit reached"}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    /// Present on `403 {"error":"account is banned"}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banned: Option<bool>,
    /// Every other field the server chose to include.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl ApiErrorBody {
    /// Parse a response body as an error envelope. Never fails: non-JSON or
    /// non-object bodies are reported through [`ApiErrorBody::error`] verbatim.
    pub fn parse(bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            return Self::default();
        }
        match serde_json::from_slice::<serde_json::Value>(bytes) {
            Ok(serde_json::Value::Object(map)) => {
                let mut body: Self =
                    serde_json::from_value(serde_json::Value::Object(map)).unwrap_or_default();
                if body.error.is_none() {
                    body.error = Some(String::from_utf8_lossy(bytes).trim().to_string());
                }
                body
            }
            _ => Self {
                error: Some(String::from_utf8_lossy(bytes).trim().to_string()),
                ..Self::default()
            },
        }
    }

    /// The `error` text, or `""`.
    pub fn message(&self) -> &str {
        self.error.as_deref().unwrap_or("")
    }
}

impl fmt::Display for ApiErrorBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.error, &self.permission) {
            (Some(e), Some(p)) => write!(f, "{e} (permission: {p})"),
            (Some(e), None) => write!(f, "{e}"),
            (None, _) => write!(f, "<empty error body>"),
        }
    }
}

/// Everything that can go wrong while talking to an OpenField server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SdkError {
    /// The transport failed (DNS, TLS, connection reset, timeout).
    #[error("network error: {message}")]
    Network {
        /// Underlying transport description.
        message: String,
        /// True when the request never reached a usable response and may be retried.
        retryable: bool,
    },

    /// The server answered with a non-2xx status.
    #[error("http {status}: {body}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Parsed error envelope.
        body: ApiErrorBody,
        /// Suggestion from a `Retry-After` header, in seconds.
        retry_after_secs: Option<u64>,
    },

    /// The response body could not be decoded into the expected type.
    #[error("decode error: {message}")]
    Decode {
        /// Decoder description.
        message: String,
        /// The raw body, truncated, for debugging.
        raw: String,
    },

    /// A caller supplied value was rejected before any request was made.
    #[error("invalid argument: {message}")]
    InvalidArgument {
        /// What was wrong.
        message: String,
    },

    /// A sidecar transport failure (spawn, stdin/stdout, protocol).
    #[error("transport error: {message}")]
    Transport {
        /// Transport description.
        message: String,
    },
}

impl SdkError {
    /// Build a retryable network error.
    pub fn network(message: impl Into<String>) -> Self {
        Self::Network {
            message: message.into(),
            retryable: true,
        }
    }

    /// HTTP status code, when the failure was an HTTP status.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// The `error` string from the server envelope, when there was one.
    pub fn api_message(&self) -> Option<&str> {
        match self {
            Self::Http { body, .. } => body.error.as_deref(),
            _ => None,
        }
    }

    /// The missing permission key, when this was a `403 permission denied`.
    pub fn permission(&self) -> Option<&str> {
        match self {
            Self::Http { body, .. } => body.permission.as_deref(),
            _ => None,
        }
    }

    /// `Retry-After` hint, when the server sent one.
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            Self::Http {
                retry_after_secs, ..
            } => *retry_after_secs,
            _ => None,
        }
    }

    /// Whether retrying this operation could plausibly succeed.
    ///
    /// Mirrors the guidance in `bots.md` §15: retry `5xx`/`429` with backoff, never
    /// blindly retry `401`/`403`.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Network { retryable, .. } => *retryable,
            Self::Http { status, .. } => *status == 429 || (500..600).contains(status),
            Self::Transport { .. } => true,
            Self::Decode { .. } | Self::InvalidArgument { .. } => false,
        }
    }

    /// Classify common failures so callers can branch without string matching.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::InvalidArgument { .. } => ErrorKind::InvalidArgument,
            Self::Decode { .. } => ErrorKind::Decode,
            Self::Transport { .. } => ErrorKind::Transport,
            Self::Network { .. } => ErrorKind::Network,
            Self::Http { status, .. } => match status {
                400 => ErrorKind::BadRequest,
                401 => ErrorKind::Unauthorized,
                403 => ErrorKind::Forbidden,
                404 => ErrorKind::NotFound,
                409 => ErrorKind::Conflict,
                429 => ErrorKind::RateLimited,
                s if (500..600).contains(s) => ErrorKind::ServerError,
                _ => ErrorKind::Other,
            },
        }
    }
}

/// Coarse failure categories, stable across server versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Caller-supplied value rejected locally.
    InvalidArgument,
    /// Response could not be decoded.
    Decode,
    /// Sidecar transport failed.
    Transport,
    /// Transport failed before a response.
    Network,
    /// Malformed request.
    BadRequest,
    /// Missing, expired, reset, or banned token.
    Unauthorized,
    /// Authenticated but not permitted.
    Forbidden,
    /// Target missing or invisible to this bot.
    NotFound,
    /// Duplicate or already-pending.
    Conflict,
    /// Quota exhausted; honour `retry_after_secs`.
    RateLimited,
    /// Server-side failure; safe to retry with backoff.
    ServerError,
    /// Anything else.
    Other,
}

impl ErrorKind {
    /// Stable snake_case name, used by the sidecar protocol.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid_argument",
            Self::Decode => "decode",
            Self::Transport => "transport",
            Self::Network => "network",
            Self::BadRequest => "bad_request",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::RateLimited => "rate_limited",
            Self::ServerError => "server_error",
            Self::Other => "other",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_permission_envelope() {
        let body =
            ApiErrorBody::parse(br#"{"error":"permission denied","permission":"chat.send"}"#);
        assert_eq!(body.error.as_deref(), Some("permission denied"));
        assert_eq!(body.permission.as_deref(), Some("chat.send"));
        assert_eq!(
            body.to_string(),
            "permission denied (permission: chat.send)"
        );
    }

    #[test]
    fn parses_bot_limit_envelope() {
        let body = ApiErrorBody::parse(br#"{"error":"bot limit reached","limit":10}"#);
        assert_eq!(body.limit, Some(10));
        assert_eq!(body.extra.len(), 0);
    }

    #[test]
    fn keeps_unknown_fields() {
        let body = ApiErrorBody::parse(br#"{"error":"nope","weird":{"a":1}}"#);
        assert!(body.extra.contains_key("weird"));
    }

    #[test]
    fn non_json_body_becomes_message() {
        let body = ApiErrorBody::parse(b"<html>502 Bad Gateway</html>");
        assert_eq!(body.message(), "<html>502 Bad Gateway</html>");
    }

    #[test]
    fn empty_body_is_empty() {
        let body = ApiErrorBody::parse(b"");
        assert_eq!(body.message(), "");
    }

    #[test]
    fn retryability_matches_docs() {
        let forbidden = SdkError::Http {
            status: 403,
            body: ApiErrorBody::default(),
            retry_after_secs: None,
        };
        let unauthorized = SdkError::Http {
            status: 401,
            body: ApiErrorBody::default(),
            retry_after_secs: None,
        };
        let limited = SdkError::Http {
            status: 429,
            body: ApiErrorBody::default(),
            retry_after_secs: Some(60),
        };
        let server = SdkError::Http {
            status: 500,
            body: ApiErrorBody::default(),
            retry_after_secs: None,
        };
        assert!(!forbidden.is_retryable());
        assert!(!unauthorized.is_retryable());
        assert!(limited.is_retryable());
        assert!(server.is_retryable());
        assert_eq!(limited.retry_after_secs(), Some(60));
        assert_eq!(unauthorized.kind(), ErrorKind::Unauthorized);
        assert_eq!(server.kind(), ErrorKind::ServerError);
    }

    #[test]
    fn error_serializes_with_kind_tag() {
        let err = SdkError::Http {
            status: 403,
            body: ApiErrorBody::parse(
                br#"{"error":"permission denied","permission":"posts.create"}"#,
            ),
            retry_after_secs: None,
        };
        let json: serde_json::Value = serde_json::to_value(&err).unwrap();
        assert_eq!(json["kind"], "http");
        assert_eq!(json["status"], 403);
        assert_eq!(json["body"]["permission"], "posts.create");
        let back: SdkError = serde_json::from_value(json).unwrap();
        assert_eq!(back, err);
    }
}
