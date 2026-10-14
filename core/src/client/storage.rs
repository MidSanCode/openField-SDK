//! Storage endpoints (`bots.md` §6.5).
//!
//! `POST /attachments` is `multipart/form-data` with the file in a field literally
//! named `file` — the server reads it with `c.Request.FormFile("file")`, and a
//! missing part answers `400 {"error":"missing file field"}`. Uploading returns an
//! attachment id, which is then referenced from a post's or message's
//! `attachment_ids` (maximum 9, `bots.md` §7.1).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, SdkError};
use crate::http::MultipartFile;
use crate::models::{Attachment, AttachmentList, AttachmentResponse, Id, JsonObject};

use super::AuthenticatedClient;
use crate::client::API_PREFIX;

/// The multipart field name the attachment endpoint requires.
pub const ATTACHMENT_FIELD: &str = "file";

/// A file ready to upload.
///
/// Serde-derived so the language bindings can hand one across as JSON:
/// `{"file_name":"a.png","bytes":[1,2,3]}`. A `path` is **not** a field here; the
/// bindings read the file themselves (the FFI and sidecar both accept a `path` in
/// their own payload and load it before constructing this).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Upload {
    /// Original file name, reported to the server.
    pub file_name: String,
    /// Optional MIME type; guessed from the extension when absent.
    pub mime: Option<String>,
    /// File contents. Accepts a JSON array of octets or a base64 string, because a
    /// binding should not have to turn a 2 MB file into two million JSON numbers.
    #[serde(deserialize_with = "crate::models::de_bytes_flexible")]
    pub bytes: Vec<u8>,
}

impl Upload {
    /// Build an upload from bytes.
    pub fn new(file_name: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            file_name: file_name.into(),
            mime: None,
            bytes,
        }
    }

    /// Read a file from disk.
    ///
    /// The whole file is buffered, because the transport is synchronous and the API
    /// has no resumable upload; use [`Upload::max_bytes`] to bound what is accepted.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_path_limited(path, None)
    }

    /// Read a file from disk, refusing anything above `max_bytes`.
    pub fn from_path_limited(path: impl AsRef<Path>, max_bytes: Option<u64>) -> Result<Self> {
        let path = path.as_ref();
        let metadata = std::fs::metadata(path).map_err(|err| SdkError::InvalidArgument {
            message: format!("cannot read `{}`: {err}", path.display()),
        })?;
        if !metadata.is_file() {
            return Err(SdkError::InvalidArgument {
                message: format!("`{}` is not a regular file", path.display()),
            });
        }
        if let Some(max) = max_bytes {
            if metadata.len() > max {
                return Err(SdkError::InvalidArgument {
                    message: format!(
                        "`{}` is {} bytes, above the configured {} byte ceiling",
                        path.display(),
                        metadata.len(),
                        max
                    ),
                });
            }
        }
        let bytes = std::fs::read(path).map_err(|err| SdkError::InvalidArgument {
            message: format!("cannot read `{}`: {err}", path.display()),
        })?;
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "upload.bin".to_string());
        Ok(Self::new(file_name, bytes))
    }

    /// Builder: override the MIME type.
    pub fn with_mime(mut self, mime: impl Into<String>) -> Self {
        self.mime = Some(mime.into());
        self
    }

    /// The size of the payload in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether there is nothing to upload.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Reject an upload the server or transport would refuse.
    pub fn validate(&self) -> Result<()> {
        if self.file_name.trim().is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "upload needs a file name".into(),
            });
        }
        Ok(())
    }

    /// Convert into the transport's file part, always using the `file` field name.
    pub fn into_multipart(self) -> Result<MultipartFile> {
        self.validate()?;
        let mut file = MultipartFile::new(self.file_name, self.bytes).with_field(ATTACHMENT_FIELD);
        if let Some(mime) = self.mime {
            file = file.with_mime(mime);
        }
        Ok(file)
    }
}

/// The `POST /attachments` request body shape, for callers that need it typed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentUpload {
    /// Optional caption or description, when the server build accepts one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl AuthenticatedClient {
    /// `POST /api/v1/attachments` (`storage.upload`) — upload one file.
    ///
    /// Returns the attachment, whose `id` goes into a post's or message's
    /// `attachment_ids`.
    pub fn upload(&self, upload: Upload) -> Result<AttachmentResponse> {
        let file = upload.into_multipart()?;
        let response =
            self.base
                .http
                .multipart("POST", &format!("{API_PREFIX}/attachments"), &[], &[file])?;
        response.decode()
    }

    /// Upload the bytes of a file on disk.
    pub fn upload_file(&self, path: impl AsRef<Path>) -> Result<AttachmentResponse> {
        self.upload(Upload::from_path(path)?)
    }

    /// Upload bytes under a given name.
    pub fn upload_bytes(
        &self,
        file_name: impl Into<String>,
        bytes: Vec<u8>,
    ) -> Result<AttachmentResponse> {
        self.upload(Upload::new(file_name, bytes))
    }

    /// Upload and return just the attachment id, which is what callers usually need.
    pub fn upload_id(&self, upload: Upload) -> Result<Id> {
        Ok(self.upload(upload)?.id())
    }

    /// `GET /api/v1/attachments` (`storage.list`) — list the bot's attachments.
    pub fn attachments(&self) -> Result<AttachmentList> {
        self.get_json(&format!("{API_PREFIX}/attachments"))
    }

    /// `GET /api/v1/attachments/:id` — one attachment record.
    pub fn attachment(&self, attachment_id: Id) -> Result<Attachment> {
        self.get_json(&format!("{API_PREFIX}/attachments/{attachment_id}"))
    }

    /// `DELETE /api/v1/attachments/:id` (`storage.delete`).
    pub fn delete_attachment(&self, attachment_id: Id) -> Result<JsonObject> {
        self.ok_object(
            "DELETE",
            &format!("{API_PREFIX}/attachments/{attachment_id}"),
        )
    }

    /// Upload several files and return their ids, ready for `attachment_ids`.
    ///
    /// Enforces the documented ceiling of 9 attachments per post or message.
    pub fn upload_many(&self, uploads: Vec<Upload>) -> Result<Vec<Id>> {
        if uploads.len() > super::posts::MAX_ATTACHMENTS {
            return Err(SdkError::InvalidArgument {
                message: format!(
                    "too many attachments (max {})",
                    super::posts::MAX_ATTACHMENTS
                ),
            });
        }
        let mut ids = Vec::with_capacity(uploads.len());
        for upload in uploads {
            ids.push(self.upload(upload)?.id());
        }
        Ok(ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_uses_the_required_field_name() {
        let upload = Upload::new("chart.png", vec![1, 2, 3]);
        let part = upload.into_multipart().unwrap();
        assert_eq!(part.field, "file", "the server reads FormFile(\"file\")");
        assert_eq!(
            part.mime.as_deref(),
            Some("image/png"),
            "mime guessed from extension"
        );
    }

    #[test]
    fn upload_mime_override_wins() {
        let upload = Upload::new("data.bin", vec![1]).with_mime("application/x-custom");
        let part = upload.into_multipart().unwrap();
        assert_eq!(part.mime.as_deref(), Some("application/x-custom"));
    }

    #[test]
    fn upload_rejects_empty_file_name() {
        let upload = Upload::new("  ", vec![1]);
        assert!(upload.validate().is_err());
        assert!(upload.into_multipart().is_err());
    }

    #[test]
    fn upload_len_and_empty() {
        assert_eq!(Upload::new("a", vec![1, 2]).len(), 2);
        assert!(Upload::new("a", vec![]).is_empty());
    }

    #[test]
    fn from_path_reads_a_real_file() {
        let dir = std::env::temp_dir().join("openfield-sdk-upload-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notes.txt");
        std::fs::write(&path, b"hello").unwrap();

        let upload = Upload::from_path(&path).unwrap();
        assert_eq!(upload.bytes, b"hello");
        assert_eq!(upload.file_name, "notes.txt");

        let limited = Upload::from_path_limited(&path, Some(2));
        assert!(limited.is_err(), "2 byte ceiling must reject a 5 byte file");
        let ok = Upload::from_path_limited(&path, Some(5)).unwrap();
        assert_eq!(ok.len(), 5);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_path_reports_a_missing_file() {
        let missing = std::env::temp_dir().join("openfield-sdk-definitely-missing-file");
        assert!(Upload::from_path(&missing).is_err());
    }

    #[test]
    fn attachment_field_constant_matches_the_docs() {
        assert_eq!(ATTACHMENT_FIELD, "file");
    }
}
