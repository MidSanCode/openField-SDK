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
use crate::models::{
    Attachment, AttachmentBurn, AttachmentList, AttachmentResponse, ChunkReceipt, ChunkSession,
    ChunkStatus, Id, JsonObject,
};

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

    // --- the chunked path ---------------------------------------------------

    /// `POST /api/v1/attachments/chunk/init` — open a resumable upload session.
    ///
    /// # Errors
    ///
    /// * `400 invalid request` — a missing `filename`, `size` or `total_chunks`;
    /// * `400 too many chunks` — above [`MAX_TOTAL_CHUNKS`];
    /// * `400 declared size exceeds chunk capacity` — `size` is unreachable with
    ///   that many parts at [`MAX_CHUNK_BYTES`] each;
    /// * `413 file too large` / `413 storage quota exceeded`;
    /// * `403 storage bucket requires higher membership level`;
    /// * `503 storage not configured`.
    pub fn start_chunked_upload(&self, request: &NewChunkedUpload) -> Result<ChunkSession> {
        request.validate()?;
        let body = serde_json::to_value(request).map_err(super::json_error)?;
        self.post_json(&format!("{API_PREFIX}/attachments/chunk/init"), &body)
    }

    /// `POST /api/v1/attachments/chunk/:upload_id/:index` — store one part.
    ///
    /// The part is sent as `multipart/form-data` under the field name `chunk`.
    /// Indexes are **1-based**.
    ///
    /// # Errors
    ///
    /// * `400 invalid upload id` / `400 invalid chunk index`;
    /// * `400 missing chunk field` — the server also accepts a filename-less
    ///   part, which Go files under the value slot, so an empty body is the only
    ///   real failure;
    /// * `413 chunk too large` — above [`MAX_CHUNK_BYTES`];
    /// * `404 upload session not found` — absent, or owned by someone else (the
    ///   two cases are deliberately indistinguishable).
    pub fn upload_chunk(&self, request: &ChunkUploadRequest) -> Result<ChunkReceipt> {
        request.validate()?;
        let file = MultipartFile::new(format!("chunk-{}", request.index), request.bytes.clone())
            .with_field(CHUNK_FIELD)
            .with_mime("application/octet-stream");
        let response = self.base.http.multipart(
            "POST",
            &format!(
                "{API_PREFIX}/attachments/chunk/{}/{index}",
                request.upload_id,
                index = request.index
            ),
            &[],
            &[file],
        )?;
        response.decode()
    }

    /// `GET /api/v1/attachments/chunk/:upload_id` — which parts already landed.
    ///
    /// The service stats each expected object rather than listing the key
    /// prefix, because the configured backend omits freshly written objects from
    /// prefix listings.
    pub fn chunk_status(&self, upload_id: &str) -> Result<ChunkStatus> {
        let upload_id = require_upload_id(upload_id)?;
        self.get_json(&format!("{API_PREFIX}/attachments/chunk/{upload_id}"))
    }

    /// `POST /api/v1/attachments/chunk/:upload_id/complete` — assemble the object.
    ///
    /// The declared layout must match the session: a different `total_chunks` is
    /// a `400 chunk count mismatch`, and a `size` above the one reserved at init
    /// is a `400 declared size exceeds session size`.
    ///
    /// # Errors
    ///
    /// * `409 missing chunks` — carries the missing indexes under `missing`, but
    ///   this returns the error rather than the body; caller-side tracking via
    ///   [`chunk_status`](Self::chunk_status) is how a resume is driven;
    /// * `400 chunk count mismatch` / `400 declared size exceeds session size`;
    /// * `413 file too large` / `413 storage quota exceeded` — checked against the
    ///   bytes actually stored, not the declaration;
    /// * `415 unsupported file type` — after resolving the MIME type, the service
    ///   refuses document/script formats on the public bucket.
    pub fn complete_chunked_upload(
        &self,
        upload_id: &str,
        request: &NewChunkedUpload,
    ) -> Result<AttachmentResponse> {
        let upload_id = require_upload_id(upload_id)?;
        request.validate()?;
        let body = serde_json::to_value(request).map_err(super::json_error)?;
        self.post_json(
            &format!("{API_PREFIX}/attachments/chunk/{upload_id}/complete"),
            &body,
        )
    }

    /// Upload a large file through the chunked path, resuming a session if one is
    /// already open.
    ///
    /// `chunk_bytes` is the part size and is clamped to `1..=MAX_CHUNK_BYTES`.
    /// The method asks for the session status first, so re-running it with the
    /// same `upload_id` re-sends only the missing parts.
    ///
    /// # Errors
    ///
    /// Propagates whatever the individual calls answer, plus
    /// [`SdkError::InvalidArgument`] for an empty payload.
    pub fn upload_chunked(
        &self,
        upload: &Upload,
        chunk_bytes: usize,
    ) -> Result<AttachmentResponse> {
        upload.validate()?;
        if upload.bytes.is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "nothing to upload".into(),
            });
        }
        let chunk_bytes = chunk_bytes.clamp(1, MAX_CHUNK_BYTES);
        let parts: Vec<&[u8]> = upload.bytes.chunks(chunk_bytes).collect();
        let request = NewChunkedUpload::new(
            upload.file_name.clone(),
            upload.bytes.len() as i64,
            parts.len() as i64,
        )?
        .with_optional_mime(upload.mime.clone());

        let session = self.start_chunked_upload(&request)?;
        let upload_id = session.upload_id.clone();
        if upload_id.is_empty() {
            return Err(SdkError::Decode {
                message: "the server answered no upload_id".into(),
                raw: String::new(),
            });
        }
        self.upload_chunked_into(&upload_id, &request, &parts)
    }

    /// Continue an already-open session. See [`upload_chunked`](Self::upload_chunked).
    pub fn upload_chunked_into(
        &self,
        upload_id: &str,
        request: &NewChunkedUpload,
        parts: &[&[u8]],
    ) -> Result<AttachmentResponse> {
        let upload_id = require_upload_id(upload_id)?;
        request.validate()?;
        let total = parts.len() as i64;
        if total == 0 {
            return Err(SdkError::InvalidArgument {
                message: "nothing to upload".into(),
            });
        }
        if total != request.total_chunks {
            return Err(SdkError::InvalidArgument {
                message: format!(
                    "session declares {} chunks but {} parts were supplied",
                    request.total_chunks, total
                ),
            });
        }

        // Resume: only the missing indexes are (re)sent.
        let already = self
            .chunk_status(upload_id)
            .map(|status| status.uploaded)
            .unwrap_or_default();
        for (offset, part) in parts.iter().enumerate() {
            let index = offset as i64 + 1;
            if already.contains(&index) {
                continue;
            }
            self.upload_chunk(&ChunkUploadRequest {
                upload_id: upload_id.to_string(),
                index,
                bytes: part.to_vec(),
            })?;
        }
        self.complete_chunked_upload(upload_id, request)
    }

    // --- reuse and burn -----------------------------------------------------

    /// `GET /api/v1/attachments/by-hash/:hash` — reuse a blob the cloud already has.
    ///
    /// `hash` is the lowercase hex SHA-256 of the bytes; the handler lowercases
    /// and trims it. Deduplication is **per user**, so this only finds blobs the
    /// caller uploaded.
    ///
    /// # Errors
    ///
    /// * `404 attachment not found`.
    pub fn attachment_by_hash(&self, hash: &str) -> Result<Attachment> {
        let hash = validate_sha256(hash)?;
        self.get_json(&format!("{API_PREFIX}/attachments/by-hash/{hash}"))
    }

    /// `POST /api/v1/attachments/:id/burn-arm` — arm the burn-after-view timer.
    ///
    /// `burn_seconds` is capped at 24 hours by the handler's binding
    /// (`gt=0,lte=86400`). Arming happens on the **first view by someone other
    /// than the uploader**, and the uploader can never trigger it by looking at
    /// their own file — in that case the handler answers `404`, exactly as it
    /// does for an unknown id, so the uploader cannot distinguish the two.
    ///
    /// # Errors
    ///
    /// * `400 invalid request` — a non-positive or >86400 `burn_seconds`;
    /// * `404 attachment not found` — absent, or the caller is the uploader, or
    ///   the attachment is not on a burn-enabled message the caller can see.
    pub fn arm_attachment_burn(
        &self,
        attachment_id: Id,
        burn_seconds: i64,
    ) -> Result<AttachmentBurn> {
        if !(1..=MAX_BURN_SECONDS).contains(&burn_seconds) {
            return Err(SdkError::InvalidArgument {
                message: format!("burn_seconds must be 1..={MAX_BURN_SECONDS}"),
            });
        }
        let body = serde_json::json!({ "burn_seconds": burn_seconds });
        self.post_json(
            &format!("{API_PREFIX}/attachments/{attachment_id}/burn-arm"),
            &body,
        )
    }
}

/// Largest single chunk the service accepts (8 MiB, `maxChunkBytes`).
pub const MAX_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Largest number of chunks per session (`maxTotalChunks`).
pub const MAX_TOTAL_CHUNKS: i64 = 10_000;

/// Longest burn-after-view countdown the handler's binding allows (24 hours).
pub const MAX_BURN_SECONDS: i64 = 86_400;

/// The multipart field name a chunk part must use.
pub const CHUNK_FIELD: &str = "chunk";

/// A chunked upload to open or to complete.
///
/// The same body is bound by `chunk/init` and `chunk/complete`, which is why one
/// type serves both.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NewChunkedUpload {
    /// Original file name; also drives MIME sniffing at completion.
    pub filename: String,
    /// Total byte length the upload is expected to have.
    pub size: i64,
    /// Number of parts, each at most [`MAX_CHUNK_BYTES`].
    pub total_chunks: i64,
    /// Optional MIME type; guessed from the extension when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// `public` (the handler's default) | `login` | `friends` | `private`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
}

impl NewChunkedUpload {
    /// Describe an upload of `size` bytes split into `total_chunks` parts.
    ///
    /// # Errors
    ///
    /// Rejects an empty file name, a non-positive size or chunk count, more than
    /// [`MAX_TOTAL_CHUNKS`] parts, and a size that the declared part count cannot
    /// reach.
    pub fn new(filename: impl Into<String>, size: i64, total_chunks: i64) -> Result<Self> {
        let request = Self {
            filename: filename.into(),
            size,
            total_chunks,
            mime_type: None,
            visibility: None,
        };
        request.validate()?;
        Ok(request)
    }

    /// Builder: declare the MIME type.
    pub fn with_mime(mut self, mime: impl Into<String>) -> Self {
        self.mime_type = Some(mime.into());
        self
    }

    /// Builder: set the MIME type when one is known.
    pub fn with_optional_mime(mut self, mime: Option<String>) -> Self {
        self.mime_type = mime;
        self
    }

    /// Builder: declare the visibility.
    pub fn with_visibility(mut self, visibility: impl Into<String>) -> Self {
        self.visibility = Some(visibility.into());
        self
    }

    /// Split `size` bytes into equal parts of at most `chunk_bytes`.
    ///
    /// # Errors
    ///
    /// As [`NewChunkedUpload::new`].
    pub fn split(filename: impl Into<String>, size: i64, chunk_bytes: usize) -> Result<Self> {
        let chunk_bytes = chunk_bytes.clamp(1, MAX_CHUNK_BYTES) as i64;
        // Ceiling division, spelled out: `i64::div_ceil` needs a newer standard
        // library than the crate's floor allows.
        let total_chunks = (size + chunk_bytes - 1) / chunk_bytes;
        Self::new(filename, size, total_chunks)
    }

    /// Check what `chunk/init` checks, before the round trip.
    ///
    /// # Errors
    ///
    /// See [`NewChunkedUpload::new`].
    pub fn validate(&self) -> Result<()> {
        if self.filename.trim().is_empty() {
            return Err(SdkError::InvalidArgument {
                message: "filename is required".into(),
            });
        }
        if self.size <= 0 {
            return Err(SdkError::InvalidArgument {
                message: "size must be greater than zero".into(),
            });
        }
        if self.total_chunks <= 0 {
            return Err(SdkError::InvalidArgument {
                message: "total_chunks must be greater than zero".into(),
            });
        }
        if self.total_chunks > MAX_TOTAL_CHUNKS {
            return Err(SdkError::InvalidArgument {
                message: format!("total_chunks must be at most {MAX_TOTAL_CHUNKS}"),
            });
        }
        // The server gate that stops a session claiming a tiny size while
        // permitting total_chunks * maxChunkBytes of real data.
        if self.size > self.total_chunks * MAX_CHUNK_BYTES as i64 {
            return Err(SdkError::InvalidArgument {
                message: format!(
                    "declared size {} exceeds what {} chunks can carry",
                    self.size, self.total_chunks
                ),
            });
        }
        Ok(())
    }
}

/// One part of a chunked upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkUploadRequest {
    /// The session id from `chunk/init`.
    pub upload_id: String,
    /// 1-based part index.
    pub index: i64,
    /// The bytes. Accepts a JSON octet array or a base64 string across a binding.
    #[serde(deserialize_with = "crate::models::de_bytes_flexible")]
    pub bytes: Vec<u8>,
}

impl ChunkUploadRequest {
    /// Check the part against the handler's own bounds.
    ///
    /// # Errors
    ///
    /// A blank or over-long `upload_id`, an index below 1, or a part above
    /// [`MAX_CHUNK_BYTES`].
    pub fn validate(&self) -> Result<()> {
        require_upload_id(&self.upload_id)?;
        if self.index < 1 {
            return Err(SdkError::InvalidArgument {
                message: "chunk index is 1-based and must be at least 1".into(),
            });
        }
        if self.bytes.len() > MAX_CHUNK_BYTES {
            return Err(SdkError::InvalidArgument {
                message: format!(
                    "chunk is {} bytes, above the {MAX_CHUNK_BYTES} byte limit",
                    self.bytes.len()
                ),
            });
        }
        Ok(())
    }
}

/// Reject an upload id the handler would reject, and never let it add a path segment.
fn require_upload_id(upload_id: &str) -> Result<&str> {
    let trimmed = upload_id.trim();
    if trimmed.is_empty() || trimmed.len() > 64 {
        // The handler's own bound: `len(uploadID) > 64` is `400 invalid upload id`.
        return Err(SdkError::InvalidArgument {
            message: "upload_id must be 1..=64 characters".into(),
        });
    }
    if !trimmed
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err(SdkError::InvalidArgument {
            message: format!("upload_id `{upload_id}` is not a valid path segment"),
        });
    }
    Ok(trimmed)
}

/// Accept a lowercase or uppercase hex SHA-256, normalised to lowercase.
fn validate_sha256(hash: &str) -> Result<String> {
    let trimmed = hash.trim();
    if trimmed.len() != 64 || !trimmed.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(SdkError::InvalidArgument {
            message: "hash must be 64 hexadecimal characters (SHA-256)".into(),
        });
    }
    Ok(trimmed.to_ascii_lowercase())
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
