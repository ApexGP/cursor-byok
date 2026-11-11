//! In-memory diagnostic bundle generation.
//!
//! The zip never touches disk: it is written into a `Cursor<Vec<u8>>`, hashed,
//! and handed to the upload client. Callers own the returned buffer and drop it
//! as soon as the upload finishes, on success or failure.

use std::io::{Cursor, Write};

use chrono::{SecondsFormat, Utc};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use zip::{
    write::{SimpleFileOptions, ZipWriter},
    CompressionMethod,
};

use crate::{
    diagnostics::{logs::LogTail, report::DiagnosticReport},
    Error, Result,
};

/// Hard cap for the sealed zip. The design trims logs first, then rejects.
pub const MAX_BUNDLE_BYTES: usize = 8 * 1024 * 1024;
/// Bundle/manifest schema version shared with the upload contract.
pub const BUNDLE_SCHEMA_VERSION: u32 = 1;

/// A sealed, in-memory diagnostic zip and its manifest.
#[derive(Debug, Clone)]
pub struct Bundle {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub manifest: serde_json::Value,
}

/// Names of the payload entries; `manifest.json` is added separately.
const PAYLOAD_ENTRIES: usize = 4;

/// Builds the diagnostic zip in memory and hashes the sealed bytes.
///
/// The manifest intentionally omits the zip SHA-256: embedding it would change
/// the zip and invalidate the hash. The hash travels with [`Bundle::sha256`]
/// and as the `X-Report-SHA256` upload header instead.
pub async fn build_bundle(report: &DiagnosticReport, log_tail: &LogTail) -> Result<Bundle> {
    // `report.json` is the full report; keep its embedded log section aligned
    // with the tail that is actually sealed into `logs/tail.log`.
    let mut embedded_report = report.clone();
    embedded_report.logs = log_tail.clone();
    let report_json = serde_json::to_vec(&embedded_report)?;
    let database_json = serde_json::to_vec(&report.database.cursor_byok)?;
    let cursor_state_json = serde_json::to_vec(&report.database.cursor_state)?;
    let log_bytes = log_tail_bytes(log_tail);

    let payload: [(&str, &[u8]); PAYLOAD_ENTRIES] = [
        ("report.json", &report_json),
        ("logs/tail.log", &log_bytes),
        ("evidence/database.json", &database_json),
        ("evidence/cursor-state.json", &cursor_state_json),
    ];
    let manifest = manifest_value(report, log_tail, &payload);
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;

    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    writer
        .start_file("manifest.json", options)
        .map_err(zip_error)?;
    writer.write_all(&manifest_bytes)?;
    for (name, bytes) in payload {
        writer.start_file(name, options).map_err(zip_error)?;
        writer.write_all(bytes)?;
    }
    let bytes = writer.finish().map_err(zip_error)?.into_inner();

    if bytes.len() > MAX_BUNDLE_BYTES {
        return Err(Error::Protocol(format!(
            "diagnostic bundle is {} bytes, exceeding the {} byte limit",
            bytes.len(),
            MAX_BUNDLE_BYTES
        )));
    }

    let sha256 = sha256_hex(&bytes);
    Ok(Bundle {
        bytes,
        sha256,
        manifest,
    })
}

/// Joins the redacted log tail and terminates non-empty output with a newline.
fn log_tail_bytes(log_tail: &LogTail) -> Vec<u8> {
    if log_tail.lines.is_empty() {
        return Vec::new();
    }
    let mut text = log_tail.lines.join("\n");
    text.push('\n');
    text.into_bytes()
}

fn manifest_value(
    report: &DiagnosticReport,
    log_tail: &LogTail,
    payload: &[(&str, &[u8])],
) -> Value {
    let files: Vec<Value> = payload
        .iter()
        .map(|(path, bytes)| {
            json!({
                "path": path,
                "bytes": bytes.len(),
                "sha256": sha256_hex(bytes),
            })
        })
        .collect();
    let mut entries = Vec::with_capacity(payload.len() + 1);
    entries.push("manifest.json");
    entries.extend(payload.iter().map(|(path, _)| *path));

    json!({
        "schemaVersion": BUNDLE_SCHEMA_VERSION,
        "reportId": &report.report_id,
        "createdAt": Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        "files": files,
        "entries": entries,
        "logsTruncated": log_tail.truncated,
        "note": "zip SHA-256 is not embedded in the manifest to avoid self-reference; it is returned with the bundle and sent as X-Report-SHA256",
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn zip_error(error: zip::result::ZipError) -> Error {
    Error::Protocol(format!("failed to build diagnostic bundle: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::code::DiagnosticCode;
    use crate::diagnostics::report::test_support::test_report;
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use std::io::{Cursor, Read};

    fn empty_log_tail() -> LogTail {
        LogTail::default()
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn read_entries(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes.to_vec())).unwrap();
        let mut entries = Vec::new();
        for index in 0..archive.len() {
            let mut file = archive.by_index(index).unwrap();
            let name = file.name().to_string();
            let mut data = Vec::new();
            file.read_to_end(&mut data).unwrap();
            entries.push((name, data));
        }
        entries
    }

    fn pseudo_random_lines(count: usize, line_bytes: usize) -> LogTail {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut lines = Vec::with_capacity(count);
        for _ in 0..count {
            let mut line = String::with_capacity(line_bytes);
            for _ in 0..line_bytes {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                line.push((b'!' + (state % 94) as u8) as char);
            }
            lines.push(line);
        }
        LogTail {
            lines,
            truncated: false,
            error: None,
        }
    }

    #[tokio::test]
    async fn bundle_has_manifest_report_and_evidence_entries() {
        let report = test_report(DiagnosticCode::Sync);
        let mut log_tail = empty_log_tail();
        log_tail.lines = vec!["line one".to_string(), "line two".to_string()];
        log_tail.truncated = true;

        let bundle = build_bundle(&report, &log_tail).await.unwrap();

        let entries = read_entries(&bundle.bytes);
        let names: Vec<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
        for expected in [
            "manifest.json",
            "report.json",
            "logs/tail.log",
            "evidence/database.json",
            "evidence/cursor-state.json",
        ] {
            assert!(names.contains(&expected), "missing {expected}: {names:?}");
        }
        assert_eq!(names.len(), 5, "unexpected entries: {names:?}");
        assert_eq!(bundle.sha256, sha256_hex(&bundle.bytes));

        let manifest: Value = serde_json::from_slice(
            &entries
                .iter()
                .find(|(name, _)| name == "manifest.json")
                .unwrap()
                .1,
        )
        .unwrap();
        assert_eq!(manifest["schemaVersion"], 1);
        assert_eq!(manifest["reportId"], report.report_id);
        assert!(manifest["createdAt"].as_str().is_some());
        assert_eq!(manifest["files"].as_array().unwrap().len(), 4);

        let report_json: Value = serde_json::from_slice(
            &entries
                .iter()
                .find(|(name, _)| name == "report.json")
                .unwrap()
                .1,
        )
        .unwrap();
        assert_eq!(report_json["reportId"], report.report_id);
        assert_eq!(report_json["logs"]["truncated"], true);

        let log_text = String::from_utf8(
            entries
                .iter()
                .find(|(name, _)| name == "logs/tail.log")
                .unwrap()
                .1
                .clone(),
        )
        .unwrap();
        assert_eq!(log_text, "line one\nline two\n");
    }

    #[tokio::test]
    async fn bundle_does_not_contain_unserialized_secret_sentinels() {
        let mut report = test_report(DiagnosticCode::Sync);
        report.summary = "sk-SENTINEL summary".to_string();
        report.context.conversation_id = Some("sk-SENTINEL-conversation".to_string());

        let bundle = build_bundle(&report, &empty_log_tail()).await.unwrap();
        for (name, data) in read_entries(&bundle.bytes) {
            let text = String::from_utf8_lossy(&data);
            assert!(
                !text.contains("sk-SENTINEL"),
                "secret sentinel leaked into {name}"
            );
        }
    }

    #[tokio::test]
    async fn bundle_rejects_payloads_over_eight_mib() {
        let report = test_report(DiagnosticCode::Sync);
        // Roughly 6 MiB of incompressible text appears twice (report + tail),
        // so the sealed zip must exceed the hard cap.
        let log_tail = pseudo_random_lines(8, 768 * 1024);

        let error = build_bundle(&report, &log_tail).await.unwrap_err();
        assert!(error.to_string().contains("8"), "unexpected error: {error}");
    }
}
