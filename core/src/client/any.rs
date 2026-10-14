//! A client that is either authenticated or anonymous, and can be re-tokenised.
//!
//! [`AnyClient`] exists for the language bindings. The Rust API deliberately splits
//! [`BaseClient`](super::BaseClient) and [`AuthenticatedClient`](super::AuthenticatedClient)
//! because a Rust program knows at compile time which one it has; the FFI and the
//! sidecar do not, because their callers decide at runtime and may swap the token in
//! place after `POST /bots/:id/regenerate` revokes the old one.
//!
//! It owns one connection pool and rebuilds it only when the token actually changes.

use crate::client::{AuthenticatedClient, BaseClient, Request};
use crate::config::{BotToken, Config};
use crate::error::{Result, SdkError};
use crate::events::{EventFilter, EventStream};
use crate::http::HttpResponse;
use crate::retry::ReconnectPolicy;

/// A client that works anonymously or as a bot, chosen from the configuration.
#[derive(Debug)]
pub struct AnyClient {
    config: Config,
    auth: Option<AuthenticatedClient>,
    anon: Option<BaseClient>,
}

impl AnyClient {
    /// Prepare a client from a configuration. The transport is built lazily on the
    /// first call, so this does not touch the network.
    pub fn new(config: Config) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            auth: None,
            anon: None,
        })
    }

    /// The configuration in force.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Whether a token is configured. Does not prove the token still works.
    pub fn has_token(&self) -> bool {
        self.config.token.is_some()
    }

    /// Replace the token, or pass `None` to become anonymous.
    ///
    /// The transport is dropped so the next call is made with the new credential. That
    /// matters after a `regenerate`, which invalidates the previous token instantly.
    pub fn set_token(&mut self, token: Option<BotToken>) -> Result<()> {
        if self.config.token == token {
            return Ok(());
        }
        self.config.token = token;
        self.auth = None;
        self.anon = None;
        Ok(())
    }

    /// Ensure the transport matches the current token.
    fn sync(&mut self) -> Result<()> {
        match (&self.config.token, &self.auth) {
            (Some(token), Some(existing)) if existing.token() == token => Ok(()),
            (Some(_), _) => {
                self.auth = Some(AuthenticatedClient::with_config(self.config.clone())?);
                self.anon = None;
                Ok(())
            }
            (None, _) => {
                if self.anon.is_some() {
                    return Ok(());
                }
                let mut anonymous = self.config.clone();
                anonymous.token = None;
                self.anon = Some(BaseClient::with_config(anonymous)?);
                self.auth = None;
                Ok(())
            }
        }
    }

    /// Issue a request, returning the response whatever its status.
    pub fn request(&mut self, request: &Request) -> Result<HttpResponse> {
        self.sync()?;
        if let Some(client) = &self.auth {
            return client.request(request);
        }
        match &self.anon {
            Some(client) => client.request(request),
            None => Err(SdkError::InvalidArgument {
                message: "client has neither a token nor an anonymous transport".into(),
            }),
        }
    }

    /// The authenticated client, or an error when no token is configured.
    ///
    /// Used by the bindings for the calls that only make sense for a bot.
    pub fn authenticated(&mut self) -> Result<&AuthenticatedClient> {
        self.sync()?;
        self.auth.as_ref().ok_or_else(|| SdkError::InvalidArgument {
            message: "this call needs a bot token; the client is anonymous".into(),
        })
    }

    /// The anonymous client, which exists in both modes because the token is only
    /// cleared for the anonymous transport.
    pub fn anonymous(&mut self) -> Result<&BaseClient> {
        self.sync()?;
        if self.anon.is_none() {
            // Only reached when an authenticated transport was built: its config carries
            // the token, so a token-free clone is needed for the plain base client.
            let mut anonymous = self.config.clone();
            anonymous.token = None;
            self.anon = Some(BaseClient::with_config(anonymous)?);
        }
        self.anon.as_ref().ok_or_else(|| SdkError::InvalidArgument {
            message: "could not build an anonymous transport".into(),
        })
    }

    /// Subscribe to the event channel, which requires a token.
    pub fn subscribe(&mut self, filter: EventFilter) -> Result<EventStream> {
        let client = self.authenticated()?;
        EventStream::subscribe(client, filter)
    }

    /// Subscribe with an explicit reconnect policy and queue capacity.
    pub fn subscribe_full(
        &mut self,
        filter: EventFilter,
        reconnect: ReconnectPolicy,
        queue_capacity: usize,
    ) -> Result<EventStream> {
        let client = self.authenticated()?;
        EventStream::subscribe_full(client, filter, reconnect, queue_capacity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(token: Option<&str>) -> Config {
        let token = token.map(|raw| BotToken::new(raw.to_string()).unwrap());
        Config::new("http://127.0.0.1:9", token)
    }

    const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn an_anonymous_client_has_no_token() {
        let mut client = AnyClient::new(config(None)).unwrap();
        assert!(!client.has_token());
        assert!(client.authenticated().is_err());
        assert!(client.anonymous().is_ok());
    }

    #[test]
    fn an_authenticated_client_reports_its_token() {
        let mut client = AnyClient::new(config(Some(&format!("ofb_{HEX}")))).unwrap();
        assert!(client.has_token());
        assert!(client.authenticated().is_ok());
    }

    #[test]
    fn setting_a_token_switches_modes() {
        let mut client = AnyClient::new(config(None)).unwrap();
        assert!(client.authenticated().is_err());

        client
            .set_token(Some(BotToken::new(format!("ofb_{HEX}")).unwrap()))
            .unwrap();
        assert!(client.has_token());
        assert!(client.authenticated().is_ok());

        client.set_token(None).unwrap();
        assert!(!client.has_token());
        assert!(client.authenticated().is_err());
        assert!(client.anonymous().is_ok());
    }

    #[test]
    fn a_replaced_token_rebuilds_the_client() {
        let mut client = AnyClient::new(config(Some(&format!("ofb_{HEX}")))).unwrap();
        let first = client.authenticated().unwrap().token().expose().to_string();

        let other = format!("ofb_{}", "a".repeat(64));
        client
            .set_token(Some(BotToken::new(other.clone()).unwrap()))
            .unwrap();
        assert_eq!(client.authenticated().unwrap().token().expose(), other);
        assert_ne!(first, other);
    }

    #[test]
    fn setting_the_same_token_is_a_no_op() {
        let token = format!("ofb_{HEX}");
        let mut client = AnyClient::new(config(Some(&token))).unwrap();
        let _ = client.authenticated().unwrap();
        client
            .set_token(Some(BotToken::new(token).unwrap()))
            .unwrap();
        assert!(client.authenticated().is_ok());
    }

    #[test]
    fn an_invalid_config_is_rejected_up_front() {
        assert!(AnyClient::new(Config::new("", None)).is_err());
        assert!(AnyClient::new(Config::new("not-a-url", None)).is_err());
    }

    #[test]
    fn subscribing_without_a_token_explains_why() {
        let mut client = AnyClient::new(config(None)).unwrap();
        let err = client.subscribe(EventFilter::all()).unwrap_err();
        assert!(err.to_string().contains("needs a bot token"), "got: {err}");
    }

    #[test]
    fn a_request_without_a_transport_is_reachable() {
        // With a token, the request goes out and fails on the closed port; the point is
        // that the dispatch path is exercised rather than panicking.
        let mut client = AnyClient::new(config(Some(&format!("ofb_{HEX}")))).unwrap();
        let request = Request::get("/api/v1/users/me");
        let err = client.request(&request).unwrap_err();
        assert!(matches!(err.kind(), crate::ErrorKind::Network));
    }
}
