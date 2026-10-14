//! The plugin store: catalog browsing, bundle download and admin management.
//!
//! The catalog is `authPublic` so the store page can render before sign-in, but
//! **downloading requires a token** because every install is counted. The admin
//! routes need `plugin.manage` and the service re-checks that permission against
//! the database on top of the gateway, so a token alone is not enough.
//!
//! [`download_plugin`](AuthenticatedClient::download_plugin) is the one method in
//! this SDK that returns raw bytes rather than JSON: the endpoint answers
//! `application/zip` with `Content-Disposition: attachment`. That is also why it
//! is a Rust-only convenience — the sidecar and the C ABI hand it across as
//! base64 inside the usual JSON envelope.
//!
//! Bundle constraints enforced by the service (`services/plugin/…/plugin.go`):
//! at most 5 MiB, a `manifest.json` at the zip root, an id matching
//! `^[a-z0-9][a-z0-9._-]{2,63}$`, and a semantic version.

use std::path::Path;

use crate::error::{Result, SdkError};
use crate::http::MultipartFile;
use crate::models::{JsonObject, Plugin, PluginList, PluginUpload};

use super::{AuthenticatedClient, BaseClient};
use crate::client::storage::Upload;
use crate::client::API_PREFIX;

/// Largest plugin bundle the service accepts (5 MiB).
pub const MAX_BUNDLE_BYTES: usize = 5 * 1024 * 1024;

/// The multipart field name the upload endpoint reads.
pub const BUNDLE_FIELD: &str = "file";

/// Reject a plugin id the server would reject, and never let one act as a path.
pub fn validate_plugin_id(id: &str) -> Result<&str> {
    let trimmed = id.trim();
    let bytes = trimmed.as_bytes();
    let shaped = (3..=64).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(byte)
        });
    if !shaped {
        return Err(SdkError::InvalidArgument {
            message: format!("plugin id `{id}` must match ^[a-z0-9][a-z0-9._-]{{2,63}}$"),
        });
    }
    // `.` and `..` satisfy the character class, so they are excluded explicitly:
    // otherwise the id would be interpolated into a URL path as a traversal.
    if trimmed == "." || trimmed == ".." {
        return Err(SdkError::InvalidArgument {
            message: format!("plugin id `{id}` is not a valid path segment"),
        });
    }
    Ok(trimmed)
}

impl BaseClient {
    /// `GET /api/v1/plugins` — the published catalog, newest update first.
    ///
    /// Public: no token required.
    pub fn plugins(&self) -> Result<PluginList> {
        let response = self
            .http
            .json_raw("GET", &format!("{API_PREFIX}/plugins"), None, &[])?;
        response.decode()
    }

    /// `GET /api/v1/plugins/:id` — one published plugin.
    ///
    /// # Errors
    ///
    /// * `404 plugin not found` — absent *or* unpublished; the handler filters on
    ///   `published = TRUE`, so an unpublished plugin is indistinguishable from a
    ///   missing one.
    pub fn plugin(&self, id: &str) -> Result<Plugin> {
        let id = validate_plugin_id(id)?;
        let response =
            self.http
                .json_raw("GET", &format!("{API_PREFIX}/plugins/{id}"), None, &[])?;
        response.decode()
    }
}

impl AuthenticatedClient {
    /// `GET /api/v1/plugins/:id/download` — the bundle, as raw zip bytes.
    ///
    /// Requires a token; the handler increments the download counter as a side
    /// effect, so calling this for a probe is not free.
    ///
    /// # Errors
    ///
    /// * `404 plugin not found` — absent or unpublished;
    /// * `404 bundle missing on server`.
    pub fn download_plugin(&self, id: &str) -> Result<Vec<u8>> {
        let id = validate_plugin_id(id)?;
        let response = self.base.http.json_raw(
            "GET",
            &format!("{API_PREFIX}/plugins/{id}/download"),
            None,
            &[],
        )?;
        Ok(response.body)
    }

    /// Download a bundle and write it to `path`.
    ///
    /// The bytes are buffered in memory first, so a partial file is never left
    /// behind by a failed transfer.
    pub fn download_plugin_to(&self, id: &str, path: impl AsRef<Path>) -> Result<usize> {
        let bytes = self.download_plugin(id)?;
        let path = path.as_ref();
        std::fs::write(path, &bytes).map_err(|err| SdkError::InvalidArgument {
            message: format!("cannot write `{}`: {err}", path.display()),
        })?;
        Ok(bytes.len())
    }

    /// `POST /api/v1/plugins/admin/upload` — upload a bundle.
    ///
    /// Requires `plugin.manage`. Pass `publish` to put the plugin on the store
    /// immediately; uploading a version that already exists replaces the row and
    /// **resets `published` to the value given here**.
    ///
    /// # Errors
    ///
    /// * `400 missing file field`;
    /// * `400 bundle too large (max 5242880 bytes)`;
    /// * `400` — the bundle carries no valid `manifest.json`, or the manifest
    ///   fails validation;
    /// * `403` — the service re-checked `plugin.manage`.
    pub fn upload_plugin(&self, bundle: &Upload, publish: bool) -> Result<PluginUpload> {
        bundle.validate()?;
        if bundle.bytes.len() > MAX_BUNDLE_BYTES {
            return Err(SdkError::InvalidArgument {
                message: format!(
                    "bundle is {} bytes, above the {MAX_BUNDLE_BYTES} byte service limit",
                    bundle.bytes.len()
                ),
            });
        }
        let files = [
            MultipartFile::new(bundle.file_name.clone(), bundle.bytes.clone())
                .with_field(BUNDLE_FIELD)
                .with_mime("application/zip"),
        ];
        let fields = [("publish".to_string(), publish.to_string())];
        let response = self.base.http.multipart(
            "POST",
            &format!("{API_PREFIX}/plugins/admin/upload"),
            &fields,
            &files,
        )?;
        response.decode()
    }

    /// Upload a bundle read from `path`.
    pub fn upload_plugin_file(
        &self,
        path: impl AsRef<Path>,
        publish: bool,
    ) -> Result<PluginUpload> {
        let bundle = Upload::from_path_limited(path, Some(MAX_BUNDLE_BYTES as u64))?;
        self.upload_plugin(&bundle, publish)
    }

    /// `PUT /api/v1/plugins/admin/:id/publish` — list the plugin on the store.
    ///
    /// # Errors
    ///
    /// * `404 plugin not found`;
    /// * `403` — the service re-checked `plugin.manage`.
    pub fn publish_plugin(&self, id: &str) -> Result<JsonObject> {
        let id = validate_plugin_id(id)?;
        self.ok_object("PUT", &format!("{API_PREFIX}/plugins/admin/{id}/publish"))
    }

    /// `PUT /api/v1/plugins/admin/:id/unpublish` — hide the plugin from the store.
    ///
    /// The bundle stays on disk and the row is kept, so this is reversible.
    pub fn unpublish_plugin(&self, id: &str) -> Result<JsonObject> {
        let id = validate_plugin_id(id)?;
        self.ok_object("PUT", &format!("{API_PREFIX}/plugins/admin/{id}/unpublish"))
    }

    /// `DELETE /api/v1/plugins/admin/:id` — remove the row and the bundle.
    ///
    /// # Errors
    ///
    /// * `404 plugin not found`.
    pub fn delete_plugin(&self, id: &str) -> Result<JsonObject> {
        let id = validate_plugin_id(id)?;
        self.ok_object("DELETE", &format!("{API_PREFIX}/plugins/admin/{id}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_formed_ids_are_accepted() {
        for id in ["abc", "web-search", "a.b.c", "plugin_v2", &"a".repeat(64)] {
            assert!(validate_plugin_id(id).is_ok(), "{id} should be valid");
        }
    }

    #[test]
    fn malformed_ids_are_refused() {
        for id in [
            "",
            "ab",
            &"a".repeat(65),
            "Web",
            "a b",
            "a/b",
            "a\\b",
            ".",
            "..",
        ] {
            assert!(validate_plugin_id(id).is_err(), "{id} should be refused");
        }
    }

    #[test]
    fn a_traversal_id_never_reaches_a_url() {
        // `.` and `..` satisfy the character class, so they are excluded by name.
        assert!(validate_plugin_id("..").is_err());
        assert!(validate_plugin_id(".").is_err());
        assert!(validate_plugin_id("../../etc/passwd").is_err());
    }

    #[test]
    fn ids_are_trimmed() {
        assert_eq!(validate_plugin_id("  abc  ").unwrap(), "abc");
    }

    #[test]
    fn the_bundle_ceiling_matches_the_service_constant() {
        assert_eq!(MAX_BUNDLE_BYTES, 5 * 1024 * 1024);
        assert_eq!(BUNDLE_FIELD, "file");
    }
}
