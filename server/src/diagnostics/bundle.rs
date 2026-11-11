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
/// Upper bound on log-trim iterations before giving up on an oversized zip.
const MAX_TRIM_ATTEMPTS: usize = 16;

struct SealedZip {
    bytes: Vec<u8>,
    manifest: Value,
}

/// Builds the diagnostic zip in memory and hashes the sealed bytes.
///
/// If the sealed zip exceeds [`MAX_BUNDLE_BYTES`], the log tail is halved
/// deterministically (newest lines retained), `report.json` and the manifest
/// are rebuilt with the trimmed tail, and the zip is resealed. This mirrors the
/// design's "trim logs first, then reject" rule. If trimming cannot get under
/// the cap, a structured error is returned.
///
/// The manifest intentionally omits the zip SHA-256: embedding it would change
/// the zip and invalidate the hash. The hash travels with [`Bundle::sha256`]
/// and as the `X-Report-SHA256` upload header instead.
pub async fn build_bundle(report: &DiagnosticReport, log_tail: &LogTail) -> Result<Bundle> {
    let mut tail = log_tail.clone();
    let mut trims = 0;
    loop {
        let sealed = seal_zip(report, &tail)?;
        if sealed.bytes.len() <= MAX_BUNDLE_BYTES {
            return Ok(Bundle {
                sha256: sha256_hex(&sealed.bytes),
                bytes: sealed.bytes,
                manifest: sealed.manifest,
            });
        }
        if trims >= MAX_TRIM_ATTEMPTS || !trim_log_tail(&mut tail) {
            return Err(Error::Protocol(format!(
                "diagnostic bundle is {} bytes after trimming logs, exceeding the {} byte limit",
                sealed.bytes.len(),
                MAX_BUNDLE_BYTES
            )));
        }
        trims += 1;
    }
}

/// Keeps the newest half of the log tail, marking it truncated.
///
/// Returns `false` when there is nothing left to trim.
fn trim_log_tail(log_tail: &mut LogTail) -> bool {
    let len = log_tail.lines.len();
    // A single line cannot shrink by dropping lines; stop instead of
    // re-sealing the same oversized payload until the attempt cap.
    if len <= 1 {
        return false;
    }
    let keep = len.div_ceil(2);
    log_tail.lines.drain(..len - keep);
    log_tail.truncated = true;
    true
}

/// Serializes the payload entries and writes one in-memory zip.
fn seal_zip(report: &DiagnosticReport, log_tail: &LogTail) -> Result<SealedZip> {
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

    Ok(SealedZip { bytes, manifest })
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

    fn pseudo_random_text(bytes: usize) -> String {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut text = String::with_capacity(bytes);
        for _ in 0..bytes {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            text.push((b'!' + (state % 94) as u8) as char);
        }
        text
    }

    fn pseudo_random_lines(count: usize, line_bytes: usize) -> LogTail {
        LogTail {
            lines: (0..count).map(|_| pseudo_random_text(line_bytes)).collect(),
            truncated: false,
            error: None,
        }
    }

    fn entry(entries: &[(String, Vec<u8>)], name: &str) -> Vec<u8> {
        entries
            .iter()
            .find(|(entry_name, _)| entry_name == name)
            .unwrap()
            .1
            .clone()
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
    async fn bundle_trims_oversized_logs_until_under_the_cap() {
        let report = test_report(DiagnosticCode::Sync);
        // Roughly 6 MiB of incompressible text appears twice (report + tail),
        // so the first seal exceeds 8 MiB and must trigger log trimming.
        let log_tail = pseudo_random_lines(8, 768 * 1024);

        let bundle = build_bundle(&report, &log_tail).await.unwrap();
        assert!(bundle.bytes.len() <= MAX_BUNDLE_BYTES);

        let entries = read_entries(&bundle.bytes);
        let manifest: Value = serde_json::from_slice(&entry(&entries, "manifest.json")).unwrap();
        assert_eq!(manifest["logsTruncated"], true);

        let tail_text = String::from_utf8(entry(&entries, "logs/tail.log")).unwrap();
        let trimmed: Vec<&str> = tail_text.lines().collect();
        assert!(
            trimmed.len() < log_tail.lines.len(),
            "expected trimming, still {} lines",
            trimmed.len()
        );
        assert_eq!(
            trimmed.last().copied(),
            Some(log_tail.lines.last().unwrap().as_str())
        );

        let report_json: Value = serde_json::from_slice(&entry(&entries, "report.json")).unwrap();
        assert_eq!(report_json["logs"]["truncated"], true);
        assert_eq!(
            report_json["logs"]["tail"],
            json!(trimmed
                .iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>())
        );
    }

    #[tokio::test]
    async fn bundle_rejects_oversized_non_log_content() {
        let mut report = test_report(DiagnosticCode::Sync);
        // Log trimming cannot shrink this payload; it must still be rejected.
        report.timeline = vec![json!({
            "at": "2026-11-11T12:34:56Z",
            "event": "captured",
            "detail": pseudo_random_text(12 * 1024 * 1024),
        })];

        let error = build_bundle(&report, &empty_log_tail()).await.unwrap_err();
        assert!(
            matches!(error, Error::Protocol(_)),
            "unexpected error: {error}"
        );
        assert!(error.to_string().contains("8"), "unexpected error: {error}");
    }
}
