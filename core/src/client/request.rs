//! The raw request escape hatch.
//!
//! [`Request`] exists so that a bot developer is never blocked by a missing typed
//! wrapper: any `/api/v1/...` endpoint reachable by a bot token can be called
//! through it, and the language bindings expose it as `request_raw`.

use serde::Serialize;

use crate::error::{Result, SdkError};
use crate::http::MultipartFile;

/// Body of a raw request.
#[derive(Debug, Clone, PartialEq)]
pub enum RequestBody {
    /// No body at all.
    None,
    /// A JSON document.
    Json(serde_json::Value),
    /// Raw bytes, sent as-is.
    Bytes(Vec<u8>),
    /// A `multipart/form-data` payload. The file part must be named `file` for
    /// `POST /attachments` (`bots.md` §6.5).
    Multipart {
        /// Text fields.
        fields: Vec<(String, String)>,
        /// File parts.
        files: Vec<MultipartFile>,
    },
}

/// A server-relative request.
///
/// `path` must be relative (`/api/v1/posts`); absolute URLs are rejected so a bot
/// cannot be steered into leaking its token to another host.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// HTTP method, upper case by convention.
    pub method: String,
    /// Server-relative path.
    pub path: String,
    /// Query parameters appended in order.
    pub query: Vec<(String, String)>,
    /// Request body.
    pub body: RequestBody,
}

impl Request {
    /// Start building a request.
    pub fn new(method: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            method: method.into().to_ascii_uppercase(),
            path: path.into(),
            query: Vec::new(),
            body: RequestBody::None,
        }
    }

    /// `GET` request.
    pub fn get(path: impl Into<String>) -> Self {
        Self::new("GET", path)
    }

    /// `POST` request.
    pub fn post(path: impl Into<String>) -> Self {
        Self::new("POST", path)
    }

    /// `PUT` request.
    pub fn put(path: impl Into<String>) -> Self {
        Self::new("PUT", path)
    }

    /// `DELETE` request.
    pub fn delete(path: impl Into<String>) -> Self {
        Self::new("DELETE", path)
    }

    /// Append a query parameter.
    pub fn query(mut self, key: impl Into<String>, value: impl std::fmt::Display) -> Self {
        self.query.push((key.into(), value.to_string()));
        self
    }

    /// Append a query parameter, skipping it when `value` is `None`.
    pub fn query_opt<T: std::fmt::Display>(self, key: impl Into<String>, value: Option<T>) -> Self {
        match value {
            Some(value) => self.query(key, value),
            None => self,
        }
    }

    /// Attach a JSON body.
    pub fn json<T: Serialize>(mut self, body: &T) -> Result<Self> {
        self.body = RequestBody::Json(serde_json::to_value(body).map_err(|err| {
            SdkError::InvalidArgument {
                message: format!("could not serialise request body: {err}"),
            }
        })?);
        Ok(self)
    }

    /// Attach an already-built JSON value.
    pub fn json_value(mut self, body: serde_json::Value) -> Self {
        self.body = RequestBody::Json(body);
        self
    }

    /// Attach an empty JSON object, for endpoints that demand one.
    pub fn empty_json(mut self) -> Self {
        self.body = RequestBody::Json(serde_json::Value::Object(serde_json::Map::new()));
        self
    }

    /// Attach raw bytes.
    pub fn bytes(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = RequestBody::Bytes(body.into());
        self
    }

    /// Attach a multipart form.
    pub fn multipart(mut self, fields: Vec<(String, String)>, files: Vec<MultipartFile>) -> Self {
        self.body = RequestBody::Multipart { fields, files };
        self
    }

    /// Convenience: upload one file to `POST /attachments`, using the field name the
    /// server expects (`bots.md` §6.5).
    pub fn upload(path: impl Into<String>, file_name: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self::post(path).multipart(
            Vec::new(),
            vec![MultipartFile::new(file_name, bytes).with_field("file")],
        )
    }

    /// Validate the request before any network activity.
    pub fn validate(&self) -> Result<()> {
        const METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
        if !METHODS.contains(&self.method.as_str()) {
            return Err(SdkError::InvalidArgument {
                message: format!("unsupported HTTP method `{}`", self.method),
            });
        }
        crate::client::validate_api_path(&self.path)?;
        if let RequestBody::Multipart { files, .. } = &self.body {
            for file in files {
                if file.field.trim().is_empty() {
                    return Err(SdkError::InvalidArgument {
                        message: "multipart file part needs a field name".into(),
                    });
                }
            }
        }
        Ok(())
    }
}
