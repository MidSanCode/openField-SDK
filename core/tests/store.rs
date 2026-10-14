//! End-to-end coverage of chunked uploads, content-hash reuse, burn-after-view
//! arming and the plugin store, driven against the in-process mock server.
//!
//! Run with the `testkit` feature:
//!
//! ```text
//! cargo test -p openfield-bot-sdk --features testkit --test store
//! ```
#![cfg(feature = "testkit")]

use openfield_bot::testkit::MockServer;
use openfield_bot::{
    BaseClient, ChunkUploadRequest, ErrorKind, NewChunkedUpload, Upload, MAX_BUNDLE_BYTES,
    MAX_CHUNK_BYTES, MAX_TOTAL_CHUNKS,
};

fn server() -> MockServer {
    MockServer::start().expect("the mock server must start")
}

// --- chunked uploads --------------------------------------------------------

#[test]
fn an_upload_session_reports_its_chunk_count() {
    let server = server();
    let session = server
        .client()
        .start_chunked_upload(&NewChunkedUpload::new("big.bin", 300, 3).unwrap())
        .unwrap();
    assert_eq!(session.total_chunks, 3);
    assert!(!session.upload_id.is_empty());
    assert!(session.upload_id.starts_with("chunk-"));

    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["filename"], "big.bin");
    assert_eq!(body["size"], 300);
    assert_eq!(body["total_chunks"], 3);
    assert!(body.get("mime_type").is_none(), "absent fields stay out");
}

#[test]
fn chunks_can_be_sent_queried_and_assembled() {
    let server = server();
    let bot = server.client();
    let request = NewChunkedUpload::new("big.bin", 200, 2).unwrap();
    let session = bot.start_chunked_upload(&request).unwrap();
    let upload_id = session.upload_id.clone();

    let empty = bot.chunk_status(&upload_id).unwrap();
    assert!(empty.uploaded.is_empty());
    assert_eq!(empty.missing(2), vec![1, 2]);

    let receipt = bot
        .upload_chunk(&ChunkUploadRequest {
            upload_id: upload_id.clone(),
            index: 1,
            bytes: vec![7u8; 100],
        })
        .unwrap();
    assert_eq!(receipt.index, 1);
    assert_eq!(receipt.upload_id, upload_id);

    let half = bot.chunk_status(&upload_id).unwrap();
    assert!(half.has(1));
    assert!(!half.has(2));
    assert_eq!(half.missing(2), vec![2]);

    bot.upload_chunk(&ChunkUploadRequest {
        upload_id: upload_id.clone(),
        index: 2,
        bytes: vec![8u8; 100],
    })
    .unwrap();

    let attachment = bot.complete_chunked_upload(&upload_id, &request).unwrap();
    assert_eq!(attachment.attachment().original_name, "big.bin");
    assert_eq!(attachment.attachment().size_bytes, 200);
}

#[test]
fn completing_with_a_gap_reports_the_missing_indexes() {
    let server = server();
    let bot = server.client();
    let request = NewChunkedUpload::new("big.bin", 300, 3).unwrap();
    let session = bot.start_chunked_upload(&request).unwrap();
    bot.upload_chunk(&ChunkUploadRequest {
        upload_id: session.upload_id.clone(),
        index: 1,
        bytes: vec![1u8; 100],
    })
    .unwrap();

    let error = bot
        .complete_chunked_upload(&session.upload_id, &request)
        .unwrap_err();
    assert_eq!(error.status(), Some(409));
    assert_eq!(error.api_message(), Some("missing chunks"));
}

#[test]
fn the_whole_file_driver_splits_and_completes() {
    let server = server();
    let bot = server.client();
    // 250 bytes in 100-byte parts is three parts, the last one short.
    let attachment = bot
        .upload_chunked(&Upload::new("report.bin", vec![9u8; 250]), 100)
        .unwrap();
    assert_eq!(attachment.attachment().original_name, "report.bin");
    assert_eq!(attachment.attachment().size_bytes, 250);
    assert_eq!(
        attachment.attachment().mime_type,
        "application/octet-stream"
    );

    // The session was opened with a three-chunk declaration.
    let init = server
        .requests_to("/api/v1/attachments/chunk/init")
        .pop()
        .unwrap();
    assert_eq!(init.json().unwrap()["total_chunks"], 3);
}

#[test]
fn the_driver_resumes_and_only_resends_the_missing_parts() {
    let server = server();
    let bot = server.client();
    let request = NewChunkedUpload::new("resume.bin", 300, 3).unwrap();
    let session = bot.start_chunked_upload(&request).unwrap();

    // Part 1 is already stored before the driver runs.
    bot.upload_chunk(&ChunkUploadRequest {
        upload_id: session.upload_id.clone(),
        index: 1,
        bytes: vec![1u8; 100],
    })
    .unwrap();
    server.clear_requests();

    let parts: Vec<&[u8]> = vec![&[1u8; 100][..], &[2u8; 100][..], &[3u8; 100][..]];
    let attachment = bot
        .upload_chunked_into(&session.upload_id, &request, &parts)
        .unwrap();
    assert_eq!(attachment.attachment().original_name, "resume.bin");

    // Only parts 2 and 3 went out; part 1 was skipped because the status said so.
    let uploaded: Vec<String> = server
        .requests()
        .into_iter()
        .filter(|request| request.path.ends_with("/2") || request.path.ends_with("/3"))
        .map(|request| request.path)
        .collect();
    assert_eq!(uploaded.len(), 2, "got: {uploaded:?}");
    assert!(
        server.requests_to("/api/v1/attachments/chunk/").is_empty(),
        "no request should target the bare chunk path"
    );
}

#[test]
fn the_driver_rejects_a_mismatched_part_count() {
    let server = server();
    let bot = server.client();
    let request = NewChunkedUpload::new("mismatch.bin", 300, 3).unwrap();
    let parts: Vec<&[u8]> = vec![&[1u8; 100][..]];
    let error = bot
        .upload_chunked_into("chunk-1", &request, &parts)
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);
    assert!(error.to_string().contains("declares 3 chunks"));
}

#[test]
fn session_declarations_are_checked_locally() {
    // An empty payload has nothing to send.
    assert!(Upload::new("a.bin", Vec::new()).is_empty());

    // The declared size must be reachable with the declared part count, which is
    // the gate the server applies at init.
    let too_big = NewChunkedUpload::new("a.bin", MAX_CHUNK_BYTES as i64 + 1, 1);
    assert!(too_big.is_err());

    // More parts than the session cap.
    assert!(NewChunkedUpload::new("a.bin", 100, MAX_TOTAL_CHUNKS + 1).is_err());
    assert!(NewChunkedUpload::new("", 100, 1).is_err());
    assert!(NewChunkedUpload::new("a.bin", 0, 1).is_err());
    assert!(NewChunkedUpload::new("a.bin", 100, 0).is_err());
}

#[test]
fn split_computes_the_part_count() {
    let request = NewChunkedUpload::split("a.bin", 250, 100).unwrap();
    assert_eq!(request.total_chunks, 3);
    let exact = NewChunkedUpload::split("a.bin", 200, 100).unwrap();
    assert_eq!(exact.total_chunks, 2);
    let single = NewChunkedUpload::split("a.bin", 1, 100).unwrap();
    assert_eq!(single.total_chunks, 1);
    // A part size above the server's ceiling is clamped, not rejected.
    let clamped = NewChunkedUpload::split("a.bin", 1, MAX_CHUNK_BYTES * 4).unwrap();
    assert_eq!(clamped.total_chunks, 1);
}

#[test]
fn chunk_indexes_and_sizes_are_checked_locally() {
    let server = server();
    let bot = server.client();
    let session = bot
        .start_chunked_upload(&NewChunkedUpload::new("a.bin", 100, 1).unwrap())
        .unwrap();

    // Indexes are 1-based.
    let error = bot
        .upload_chunk(&ChunkUploadRequest {
            upload_id: session.upload_id.clone(),
            index: 0,
            bytes: vec![1],
        })
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);

    // A part above the 8 MiB ceiling never leaves the process.
    let error = bot
        .upload_chunk(&ChunkUploadRequest {
            upload_id: session.upload_id.clone(),
            index: 1,
            bytes: vec![0u8; MAX_CHUNK_BYTES + 1],
        })
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);

    // A blank upload id and a traversal attempt are both refused.
    assert!(bot.chunk_status("  ").is_err());
    assert!(bot.chunk_status("../wallet").is_err());
    assert!(bot.chunk_status(&"a".repeat(65)).is_err());
}

// --- reuse and burn ---------------------------------------------------------

#[test]
fn an_attachment_is_reused_by_content_hash() {
    let server = server();
    let reused = server.client().attachment_by_hash(&"A".repeat(64)).unwrap();
    assert_eq!(reused.id, 9001);
    assert_eq!(reused.original_name, "chart.png");
    // The handler lowercases the hash before looking it up.
    assert_eq!(
        server.last_request().unwrap().path,
        format!("/api/v1/attachments/by-hash/{}", "a".repeat(64))
    );
}

#[test]
fn an_unknown_hash_is_a_404_and_a_bad_hash_never_leaves() {
    let server = server();
    let bot = server.client();
    let error = bot.attachment_by_hash(&"b".repeat(64)).unwrap_err();
    assert_eq!(error.status(), Some(404));

    let error = bot.attachment_by_hash("not-a-hash").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);
    assert!(bot.attachment_by_hash(&"z".repeat(64)).is_err());
}

#[test]
fn arming_a_burn_sets_a_deadline() {
    let server = server();
    let armed = server.client().arm_attachment_burn(9001, 300).unwrap();
    assert_eq!(armed.id, 9001);
    assert!(armed.burn_at.is_some());
    let body = server.last_request().unwrap().json().unwrap();
    assert_eq!(body["burn_seconds"], 300);
}

#[test]
fn the_burn_window_is_capped_at_a_day() {
    let server = server();
    let bot = server.client();
    assert!(bot.arm_attachment_burn(9001, 0).is_err());
    assert!(bot.arm_attachment_burn(9001, -1).is_err());
    assert!(bot.arm_attachment_burn(9001, 86_401).is_err());
    // Exactly 24 hours is allowed.
    assert!(bot.arm_attachment_burn(9001, 86_400).is_ok());
}

// --- plugin store -----------------------------------------------------------

#[test]
fn the_catalog_is_readable_without_a_token() {
    // `GET /plugins` is authPublic, so the anonymous client must reach it.
    let server = server();
    let anonymous = BaseClient::new(server.base_url()).unwrap();
    let catalog = anonymous.plugins().unwrap();
    assert_eq!(catalog.plugins.len(), 1);
    let plugin = catalog.plugin("demo").unwrap();
    assert_eq!(plugin.name, "Demo");
    assert_eq!(plugin.version, "1.0.0");
    assert_eq!(plugin.permissions, vec!["posts.view".to_string()]);
    assert!(plugin.verified);
    assert!(plugin.published);
    assert_eq!(plugin.downloads, 3);
    assert!(catalog.plugin("nope").is_none());

    let detail = anonymous.plugin("demo").unwrap();
    assert_eq!(detail.id, "demo");
}

#[test]
fn plugin_ids_are_validated_before_they_become_a_path() {
    let server = server();
    let anonymous = BaseClient::new(server.base_url()).unwrap();
    for id in ["..", ".", "a/b", "AB", "", "a"] {
        let error = anonymous.plugin(id).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidArgument, "id: {id}");
    }
    // An unpublished or missing plugin is a 404, which is preserved.
    assert_eq!(anonymous.plugin("missing").unwrap_err().status(), Some(404));
}

#[test]
fn a_bundle_download_returns_raw_zip_bytes() {
    let server = server();
    let bytes = server.client().download_plugin("demo").unwrap();
    // The mock answers a deterministic stand-in starting with the zip magic.
    assert_eq!(&bytes[..4], b"PK\x03\x04");
    assert_eq!(bytes, b"PK\x03\x04demo-bundle".to_vec());

    let error = server.client().download_plugin("missing").unwrap_err();
    assert_eq!(error.status(), Some(404));
}

#[test]
fn a_bundle_can_be_written_straight_to_disk() {
    let server = server();
    let dir = std::env::temp_dir().join("openfield-sdk-plugin-download");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bundle.zip");

    let written = server.client().download_plugin_to("demo", &path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(bytes.len(), written);
    assert_eq!(bytes, b"PK\x03\x04demo-bundle".to_vec());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn uploading_publishing_and_deleting_a_bundle() {
    let server = server();
    let bot = server.client();
    let bundle = Upload::new("bundle.zip", vec![1, 2, 3]);

    let uploaded = bot.upload_plugin(&bundle, true).unwrap();
    assert_eq!(uploaded.message, "uploaded");
    let plugin = uploaded.plugin.unwrap();
    assert_eq!(plugin.id, "demo");
    assert!(plugin.published, "publish=true was sent");

    // The multipart form carried the bundle under `file` and the flag as a field.
    let request = server.last_request().unwrap();
    assert!(request.has_multipart_field("file"));
    assert!(request.has_multipart_field("publish"));
    assert!(request.body.contains("true"), "publish=true was sent");

    let updated = bot.unpublish_plugin("demo").unwrap();
    assert_eq!(updated.get_str("message"), Some("updated"));
    // Unpublished plugins vanish from the public catalog and from detail reads.
    assert!(bot.base().plugins().unwrap().plugins.is_empty());
    assert_eq!(bot.base().plugin("demo").unwrap_err().status(), Some(404));

    bot.publish_plugin("demo").unwrap();
    assert_eq!(bot.base().plugins().unwrap().plugins.len(), 1);

    let deleted = bot.delete_plugin("demo").unwrap();
    assert_eq!(deleted.get_str("message"), Some("deleted"));
    assert_eq!(bot.delete_plugin("demo").unwrap_err().status(), Some(404));
}

#[test]
fn oversize_bundles_are_refused_locally() {
    let server = server();
    let bot = server.client();
    let big = Upload::new("bundle.zip", vec![0u8; MAX_BUNDLE_BYTES + 1]);
    let error = bot.upload_plugin(&big, false).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);
    assert!(error.to_string().contains("service limit"));

    // A bundle with no file name is refused too.
    let nameless = Upload::new("  ", vec![1]);
    assert!(bot.upload_plugin(&nameless, false).is_err());
}

#[test]
fn uploading_from_a_path_reads_the_file() {
    let server = server();
    let dir = std::env::temp_dir().join("openfield-sdk-plugin-upload");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bundle.zip");
    std::fs::write(&path, b"PK\x03\x04payload").unwrap();

    let uploaded = server.client().upload_plugin_file(&path, false).unwrap();
    assert_eq!(uploaded.plugin.unwrap().id, "demo");
    assert!(server.last_request().unwrap().has_multipart_field("file"));

    let _ = std::fs::remove_dir_all(&dir);
}
