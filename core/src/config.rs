//! Client configuration and token handling.
//!
//! [`BotToken`] encodes the rules from `bots.md` §3.1 (`ofb_` + 64 lowercase hex,
//! 68 characters total) and deliberately has no `Display`/`Serialize` that would
//! leak the secret, so a token cannot end up in a log line or a JSON payload by
//! accident (`bots.md` §15.1).

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Result, SdkError};
use crate::retry::{ReconnectPolicy, RetryPolicy};

/// Prefix that makes the gateway take the bot-token branch instead of JWT.
pub const BOT_TOKEN_PREFIX: &str = "ofb_";
/// Number of lowercase hex characters after [`BOT_TOKEN_PREFIX`].
pub const BOT_TOKEN_HEX_LEN: usize = 64;
/// Total token length: prefix plus 32 random bytes rendered as hex.
pub const BOT_TOKEN_LEN: usize = BOT_TOKEN_PREFIX.len() + BOT_TOKEN_HEX_LEN;
/// Default server address used by the sample code in `bots.md` §2.
pub const DEFAULT_BASE_URL: &str = "http://localhost:8080";

/// A validated `ofb_…` bot token.
///
/// Debug and Display output is redacted; use [`BotToken::expose`] when the raw
/// value is genuinely required (an `Authorization` header, or persistence).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct BotToken(String);

impl BotToken {
    /// Bare 64-character hex token (no prefix), as returned by the API without `ofb_`.
    pub fn from_hex(hex: &str) -> Result<Self> {
        Self::new(format!("{BOT_TOKEN_PREFIX}{}", hex.trim()))
    }

    /// Validate and wrap a full `ofb_…` token.
    pub fn new(token: impl Into<String>) -> Result<Self> {
        let token = token.into();
        let trimmed = token.trim();
        if trimmed.is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "bot token is empty".into(),
            });
        }
        if let Some(rest) = trimmed.strip_prefix(BOT_TOKEN_PREFIX) {
            if rest.len() != BOT_TOKEN_HEX_LEN {
                return Err(SdkError::InvalidArgument {
                    message: format!(
                        "bot token must be {BOT_TOKEN_PREFIX} followed by \
                         {BOT_TOKEN_HEX_LEN} hex characters ({BOT_TOKEN_LEN} total), got {}",
                        trimmed.len()
                    ),
                });
            }
            if !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(SdkError::InvalidArgument {
                    message: "bot token body must be hexadecimal".into(),
                });
            }
            // The token is generated lowercase; accept uppercase input but normalise
            // it so the header always matches the generated form.
            return Ok(Self(format!(
                "{BOT_TOKEN_PREFIX}{}",
                rest.to_ascii_lowercase()
            )));
        }
        Err(SdkError::InvalidArgument {
            message: format!("bot token must start with `{BOT_TOKEN_PREFIX}`"),
        })
    }

    /// Raw token value. Handle with care: this is the credential itself.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Redacted form suitable for logs and error messages.
    pub fn redacted(&self) -> String {
        let tail = &self.0[self.0.len().saturating_sub(4)..];
        format!("{BOT_TOKEN_PREFIX}…{tail}")
    }

    /// The 64 hex characters without the `ofb_` prefix.
    pub fn hex_body(&self) -> &str {
        &self.0[BOT_TOKEN_PREFIX.len()..]
    }
}

impl fmt::Debug for BotToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BotToken({})", self.redacted())
    }
}

impl fmt::Display for BotToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.redacted())
    }
}

impl Serialize for BotToken {
    /// Serialises to the redacted form on purpose: a token must never travel in a
    /// serialised config or get logged by a binding (`bots.md` §15.1).
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.redacted())
    }
}

impl<'de> Deserialize<'de> for BotToken {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        BotToken::new(raw).map_err(serde::de::Error::custom)
    }
}

impl std::str::FromStr for BotToken {
    type Err = SdkError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Everything needed to talk to one OpenField server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Server root, e.g. `https://openfield.example.com`. All calls join `/api/v1`.
    pub base_url: String,
    /// Bot credential. `None` builds an anonymous client (public endpoints only).
    pub token: Option<BotToken>,
    /// Per-request wall-clock ceiling.
    pub timeout: Duration,
    /// Connection establishment ceiling.
    pub connect_timeout: Duration,
    /// `User-Agent` header sent with every request.
    pub user_agent: String,
    /// Extra headers merged into every request (e.g. a reverse-proxy cookie).
    pub headers: Vec<(String, String)>,
    /// Retry schedule for REST calls.
    pub retry: RetryPolicy,
    /// Reconnection schedule for the event stream.
    pub reconnect: ReconnectPolicy,
    /// Ceiling on a decoded response body, to bound memory on a hostile response.
    pub max_response_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            token: None,
            timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            user_agent: default_user_agent(),
            headers: Vec::new(),
            retry: RetryPolicy::default(),
            reconnect: ReconnectPolicy::default(),
            max_response_bytes: 32 * 1024 * 1024,
        }
    }
}

fn default_user_agent() -> String {
    format!("openfield-bot-sdk/{}", env!("CARGO_PKG_VERSION"))
}

impl Config {
    /// Config for a server with an optional token.
    pub fn new(base_url: impl Into<String>, token: Option<BotToken>) -> Self {
        Self {
            base_url: base_url.into(),
            token,
            ..Self::default()
        }
    }

    /// Config for a server, authenticated as this bot.
    pub fn authenticated(base_url: impl Into<String>, token: impl Into<BotToken>) -> Self {
        Self::new(base_url, Some(token.into()))
    }

    /// Builder: replace the token.
    pub fn with_token(mut self, token: impl Into<BotToken>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Builder: drop the token, producing an anonymous client.
    pub fn anonymous(mut self) -> Self {
        self.token = None;
        self
    }

    /// Builder: set the request timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Builder: set the retry policy.
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Builder: set the reconnect policy.
    pub fn with_reconnect(mut self, reconnect: ReconnectPolicy) -> Self {
        self.reconnect = reconnect;
        self
    }

    /// Builder: add one header to every request.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Normalised base URL with any trailing slash removed.
    pub fn normalized_base_url(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }

    /// Resolve `path` against the base URL. Absolute `http(s)://` paths win.
    pub(crate) fn resolve(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            return path.to_string();
        }
        let base = self.normalized_base_url();
        if path.starts_with('/') {
            format!("{base}{path}")
        } else {
            format!("{base}/{path}")
        }
    }

    /// Reject configurations that cannot work, before any network call.
    pub fn validate(&self) -> Result<()> {
        let base = self.base_url.trim();
        if base.is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "base_url is empty".into(),
            });
        }
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            return Err(SdkError::InvalidArgument {
                message: format!("base_url must start with http:// or https://, got `{base}`"),
            });
        }
        if base.contains(' ') {
            return Err(SdkError::InvalidArgument {
                message: "base_url must not contain spaces".into(),
            });
        }
        if self.user_agent.trim().is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "user_agent is empty".into(),
            });
        }
        for (name, _) in &self.headers {
            if name.trim().is_empty() {
                return Err(SdkError::InvalidArgument {
                    message: "custom header name is empty".into(),
                });
            }
        }
        if self.timeout.is_zero() {
            return Err(SdkError::InvalidArgument {
                message: "timeout must be greater than zero".into(),
            });
        }
        self.retry.validate()?;
        self.reconnect.validate()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn accepts_a_generated_token() {
        let token = BotToken::new(format!("ofb_{HEX}")).unwrap();
        assert_eq!(token.expose().len(), BOT_TOKEN_LEN);
        assert_eq!(token.expose().len(), 68);
        assert_eq!(token.hex_body(), HEX);
    }

    #[test]
    fn normalises_uppercase_hex() {
        let token = BotToken::new(format!("ofb_{}", HEX.to_uppercase())).unwrap();
        assert_eq!(token.expose(), format!("ofb_{HEX}"));
    }

    #[test]
    fn from_hex_adds_prefix() {
        let token = BotToken::from_hex(HEX).unwrap();
        assert_eq!(token.expose(), format!("ofb_{HEX}"));
    }

    #[test]
    fn redacts_in_debug_display_and_json() {
        let token = BotToken::new(format!("ofb_{HEX}")).unwrap();
        assert_eq!(token.redacted(), "ofb_…cdef");
        assert_eq!(token.to_string(), "ofb_…cdef");
        assert_eq!(format!("{token:?}"), "BotToken(ofb_…cdef)");
        let json = serde_json::to_string(&token).unwrap();
        assert_eq!(json, "\"ofb_…cdef\"");
        assert!(!json.contains(HEX), "token body must never be serialised");
    }

    #[test]
    fn rejects_malformed_tokens() {
        let cases = [
            String::new(),
            "not-a-token".to_string(),
            format!("ofb_{}", &HEX[..63]),         // too short
            format!("ofb_{}0", HEX),               // too long
            format!("ofb_{}", "z".repeat(64)),     // not hex
            format!("jwt_{HEX}"),                  // wrong prefix
            format!("ofb_{}", HEX.to_uppercase()), // valid hex, normalised
        ];
        for (index, case) in cases.iter().enumerate() {
            let result = BotToken::new(case.clone());
            if index == cases.len() - 1 {
                assert!(result.is_ok(), "uppercase hex is accepted and normalised");
            } else {
                assert!(result.is_err(), "expected rejection for `{case}`");
            }
        }
    }

    #[test]
    fn base_url_normalisation_and_joining() {
        let config = Config::new("https://example.com/", None);
        assert_eq!(config.normalized_base_url(), "https://example.com");
        assert_eq!(
            config.resolve("/api/v1/users/me"),
            "https://example.com/api/v1/users/me"
        );
        assert_eq!(
            config.resolve("api/v1/posts"),
            "https://example.com/api/v1/posts"
        );
        assert_eq!(
            config.resolve("https://other.example/x"),
            "https://other.example/x"
        );
    }

    #[test]
    fn validates_base_url() {
        assert!(Config::new("", None).validate().is_err());
        assert!(Config::new("example.com", None).validate().is_err());
        assert!(Config::new("ftp://example.com", None).validate().is_err());
        assert!(Config::new("https://example.com", None).validate().is_ok());
        assert!(Config::new("http://localhost:8080", None)
            .validate()
            .is_ok());
        assert!(Config::new("http://a b.com", None).validate().is_err());
    }

    #[test]
    fn config_round_trips_and_defaults_partially() {
        let config = Config::new("https://example.com", None);
        let json = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.base_url, config.base_url);
        assert_eq!(back.timeout, config.timeout);

        let partial: Config = serde_json::from_str(r#"{"base_url":"https://x.dev"}"#).unwrap();
        assert_eq!(partial.base_url, "https://x.dev");
        assert_eq!(partial.timeout, Duration::from_secs(30));
        assert!(partial.token.is_none());
        assert_eq!(partial.retry.max_attempts, 3);
    }

    #[test]
    fn default_user_agent_names_the_crate() {
        assert!(Config::default()
            .user_agent
            .starts_with("openfield-bot-sdk/"));
    }
}
