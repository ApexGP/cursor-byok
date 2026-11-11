//! Reads a time-windowed, redacted tail of the daily application log files.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, NaiveDateTime, Utc};

use crate::diagnostics::redact::Redactor;

/// Prefix/suffix used by the desktop's daily rolling log appender.
const LOG_FILE_PREFIX: &str = "cursor-byok";
const LOG_FILE_SUFFIX: &str = "log";
const FILE_DATE_FORMAT: &str = "%Y-%m-%d";
/// The design caps the log tail at 200 lines before the byte budget is applied.
const MAX_LOG_LINES: usize = 200;

/// Redacted log lines for the capture window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogTail {
    pub lines: Vec<String>,
    pub truncated: bool,
    pub error: Option<String>,
}

/// Collects log lines around `captured_at`.
///
/// Reads only the daily files for the capture day and, when the window crosses
/// midnight, the previous day. Missing directories and files are not errors.
/// Lines whose leading timestamp cannot be parsed are kept because they are
/// likely continuation lines of a nearby event.
pub async fn collect_log_tail(
    log_dir: &Path,
    captured_at: DateTime<Utc>,
    window: Duration,
    max_bytes: usize,
) -> LogTail {
    let Ok(window) = ChronoDuration::from_std(window) else {
        return LogTail {
            lines: Vec::new(),
            truncated: false,
            error: Some("log window is too large".to_string()),
        };
    };
    let start = captured_at
        .checked_sub_signed(window)
        .unwrap_or(DateTime::<Utc>::MIN_UTC);
    let end = captured_at
        .checked_add_signed(window)
        .unwrap_or(DateTime::<Utc>::MAX_UTC);
    let capture_date = captured_at.date_naive();

    let mut dates = Vec::with_capacity(2);
    let window_start_date = start.date_naive();
    if window_start_date < capture_date {
        dates.push(window_start_date);
    }
    dates.push(capture_date);

    let mut lines = Vec::new();
    for date in dates {
        let path = log_file_path(log_dir, date);
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                filter_window_lines(text.lines(), start, end, &mut lines);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return LogTail {
                    lines: Vec::new(),
                    truncated: false,
                    error: Some(format!("failed to read {}: {error}", path.display())),
                };
            }
        }
    }

    finalize(lines, max_bytes)
}

fn log_file_path(log_dir: &Path, date: NaiveDate) -> PathBuf {
    log_dir.join(format!(
        "{LOG_FILE_PREFIX}.{}.{LOG_FILE_SUFFIX}",
        date.format(FILE_DATE_FORMAT)
    ))
}

/// Keeps timestamped lines inside the window together with their continuation
/// lines, which have no leading timestamp.
///
/// A timestamp-less line belongs to the preceding timestamped block, so a
/// multi-line event is retained or dropped as a whole. When a file contains no
/// parseable timestamps at all (unknown format), every line is kept rather than
/// risking an empty tail.
fn filter_window_lines<'a>(
    lines: impl Iterator<Item = &'a str>,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    output: &mut Vec<String>,
) {
    let lines: Vec<&str> = lines.collect();
    if !lines
        .iter()
        .any(|line| parse_line_timestamp(line).is_some())
    {
        output.extend(lines.into_iter().map(str::to_string));
        return;
    }

    let mut block: Vec<&str> = Vec::new();
    let mut keep_block = false;
    let mut seen_header = false;
    for line in lines {
        match parse_line_timestamp(line) {
            Some(timestamp) => {
                if seen_header {
                    flush_block(&mut block, keep_block, output);
                }
                seen_header = true;
                keep_block = timestamp >= start && timestamp <= end;
                block.push(line);
            }
            // Lines before the first timestamped header are fragments from an
            // earlier rotation and carry no usable time, so they are dropped.
            None if seen_header => block.push(line),
            None => {}
        }
    }
    flush_block(&mut block, keep_block, output);
}

fn flush_block(block: &mut Vec<&str>, keep: bool, output: &mut Vec<String>) {
    if keep {
        output.extend(block.drain(..).map(str::to_string));
    } else {
        block.clear();
    }
}

/// Parses the leading timestamp emitted by the tracing file layer.
///
/// The default formatter writes RFC 3339 (e.g. `2026-11-11T12:00:05.123456Z`).
/// Timestamp-less formats are accepted too, assuming UTC.
fn parse_line_timestamp(line: &str) -> Option<DateTime<Utc>> {
    let token = line.split_whitespace().next()?;
    if let Ok(parsed) = DateTime::parse_from_rfc3339(token) {
        return Some(parsed.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(token, format) {
            return Some(naive.and_utc());
        }
    }
    None
}

/// Redacts retained lines, then caps line count and bytes from the end.
fn finalize(mut lines: Vec<String>, max_bytes: usize) -> LogTail {
    let redactor = Redactor::new();
    for line in &mut lines {
        *line = redactor.redact(line);
    }

    let mut truncated = false;
    if lines.len() > MAX_LOG_LINES {
        lines.drain(..lines.len() - MAX_LOG_LINES);
        truncated = true;
    }

    let mut kept = Vec::with_capacity(lines.len());
    let mut total_bytes = 0usize;
    for line in lines.iter().rev() {
        let separator = usize::from(!kept.is_empty());
        let cost = line.len() + separator;
        if total_bytes + cost > max_bytes {
            truncated = true;
            break;
        }
        total_bytes += cost;
        kept.push(line.clone());
    }
    kept.reverse();

    if kept.is_empty() && !lines.is_empty() {
        truncated = true;
        if max_bytes > 0 {
            let last = lines.last().expect("checked non-empty");
            let mut start = last.len().saturating_sub(max_bytes);
            while start > 0 && !last.is_char_boundary(start) {
                start -= 1;
            }
            kept.push(last[start..].to_string());
        }
    }

    LogTail {
        lines: kept,
        truncated,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn log_tail_reads_only_the_capture_window_and_redacts() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cursor-byok.2026-11-11.log");
        std::fs::write(
            &file,
            concat!(
                "old line\n",
                "api_key=SENTINEL at 12:00:00\n",
                "new line at 12:00:05\n",
            ),
        )
        .unwrap();
        let tail = collect_log_tail(
            dir.path(),
            parse("2026-11-11T12:00:05Z"),
            Duration::from_secs(60),
            4096,
        )
        .await;
        assert!(tail.lines.iter().any(|line| line.contains("new line")));
        assert!(!tail.lines.join("\n").contains("SENTINEL"));
        assert_eq!(tail.error, None);
    }

    #[tokio::test]
    async fn log_tail_filters_timestamped_lines_outside_the_window() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("cursor-byok.2026-11-11.log"),
            concat!(
                "2026-11-11T11:00:00.000000Z  INFO old entry\n",
                "2026-11-11T12:00:05.123456Z  INFO inside entry\n",
                "2026-11-11T13:00:00.000000Z  INFO future entry\n",
            ),
        )
        .unwrap();
        let tail = collect_log_tail(
            dir.path(),
            parse("2026-11-11T12:00:05Z"),
            Duration::from_secs(60),
            4096,
        )
        .await;
        assert!(tail.lines.iter().any(|line| line.contains("inside entry")));
        let joined = tail.lines.join("\n");
        assert!(!joined.contains("old entry"), "kept early line: {joined}");
        assert!(!joined.contains("future entry"), "kept late line: {joined}");
    }

    #[tokio::test]
    async fn log_tail_drops_timestamp_less_lines_far_from_the_window() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("cursor-byok.2026-11-11.log"),
            concat!(
                "2026-11-11T10:00:00.000000Z  INFO old header\n",
                "old continuation unrelated to the capture\n",
                "2026-11-11T12:00:05.000000Z  INFO inside header\n",
                "inside continuation\n",
            ),
        )
        .unwrap();
        let tail = collect_log_tail(
            dir.path(),
            parse("2026-11-11T12:00:05Z"),
            Duration::from_secs(60),
            4096,
        )
        .await;
        let joined = tail.lines.join("\n");
        assert!(joined.contains("inside header"), "missing window: {joined}");
        assert!(
            joined.contains("inside continuation"),
            "dropped nearby continuation: {joined}"
        );
        assert!(
            !joined.contains("old continuation"),
            "kept far continuation: {joined}"
        );
    }

    #[tokio::test]
    async fn log_tail_redacts_every_retained_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("cursor-byok.2026-11-11.log"),
            concat!(
                "2026-11-11T12:00:03.000000Z  INFO api_key=KEY-SENTINEL\n",
                "2026-11-11T12:00:04.000000Z  INFO Authorization: Bearer AUTH-SENTINEL\n",
                "2026-11-11T12:00:05.000000Z  INFO mail me@example.com path /Users/alice/x\n",
            ),
        )
        .unwrap();
        let tail = collect_log_tail(
            dir.path(),
            parse("2026-11-11T12:00:05Z"),
            Duration::from_secs(60),
            4096,
        )
        .await;
        let joined = tail.lines.join("\n");
        for secret in ["KEY-SENTINEL", "AUTH-SENTINEL", "me@example.com", "alice"] {
            assert!(!joined.contains(secret), "leaked {secret}: {joined}");
        }
        assert_eq!(tail.lines.len(), 3);
    }

    #[tokio::test]
    async fn log_tail_truncates_to_max_bytes_keeping_the_latest_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("cursor-byok.2026-11-11.log"),
            concat!(
                "2026-11-11T12:00:03.000000Z  INFO line-one-aaaaaaaaaaaaaaaa\n",
                "2026-11-11T12:00:04.000000Z  INFO line-two-bbbbbbbbbbbbbb\n",
                "2026-11-11T12:00:05.000000Z  INFO line-three-cccccccccccc\n",
            ),
        )
        .unwrap();
        let tail = collect_log_tail(
            dir.path(),
            parse("2026-11-11T12:00:05Z"),
            Duration::from_secs(60),
            100,
        )
        .await;
        let joined = tail.lines.join("\n");
        assert!(tail.truncated);
        assert!(joined.len() <= 100);
        assert!(joined.contains("line-three"));
        assert!(!joined.contains("line-one"));
    }

    #[tokio::test]
    async fn log_tail_truncates_a_single_oversized_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut line = String::from("2026-11-11T12:00:05.000000Z  INFO ");
        line.push_str(&"x".repeat(500));
        std::fs::write(
            dir.path().join("cursor-byok.2026-11-11.log"),
            format!("{line}\n"),
        )
        .unwrap();
        let tail = collect_log_tail(
            dir.path(),
            parse("2026-11-11T12:00:05Z"),
            Duration::from_secs(60),
            50,
        )
        .await;
        assert!(tail.truncated);
        assert_eq!(tail.lines.len(), 1);
        assert!(tail.lines.join("\n").len() <= 50);
    }

    #[tokio::test]
    async fn log_tail_caps_line_count() {
        let dir = tempfile::tempdir().unwrap();
        let mut content = String::new();
        for index in 0..250 {
            content.push_str(&format!("2026-11-11T12:00:05.000000Z  INFO line-{index}\n"));
        }
        std::fs::write(dir.path().join("cursor-byok.2026-11-11.log"), content).unwrap();
        let tail = collect_log_tail(
            dir.path(),
            parse("2026-11-11T12:00:05Z"),
            Duration::from_secs(60),
            1_000_000,
        )
        .await;
        assert!(tail.truncated);
        assert_eq!(tail.lines.len(), 200);
        assert!(tail.lines.last().unwrap().contains("line-249"));
    }

    #[tokio::test]
    async fn log_tail_is_empty_without_error_when_directory_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let tail = collect_log_tail(
            &missing,
            parse("2026-11-11T12:00:05Z"),
            Duration::from_secs(60),
            4096,
        )
        .await;
        assert_eq!(tail, LogTail::default());
    }

    #[tokio::test]
    async fn log_tail_is_empty_without_error_when_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let tail = collect_log_tail(
            dir.path(),
            parse("2026-11-11T12:00:05Z"),
            Duration::from_secs(60),
            4096,
        )
        .await;
        assert!(tail.lines.is_empty());
        assert_eq!(tail.error, None);
    }

    #[tokio::test]
    async fn log_tail_reads_the_previous_day_when_the_window_crosses_midnight() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("cursor-byok.2026-11-10.log"),
            "2026-11-10T23:59:50.000000Z  INFO previous-day line\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cursor-byok.2026-11-11.log"),
            "2026-11-11T00:00:10.000000Z  INFO current-day line\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("cursor-byok.2026-11-09.log"),
            "2026-11-09T23:00:00.000000Z  INFO ancient line\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("unrelated.log"),
            "2026-11-11T00:00:10.000000Z  INFO unrelated line\n",
        )
        .unwrap();

        let tail = collect_log_tail(
            dir.path(),
            parse("2026-11-11T00:00:20Z"),
            Duration::from_secs(60),
            4096,
        )
        .await;
        let joined = tail.lines.join("\n");
        assert!(
            joined.contains("previous-day line"),
            "missing previous day: {joined}"
        );
        assert!(
            joined.contains("current-day line"),
            "missing current day: {joined}"
        );
        assert!(
            !joined.contains("ancient line"),
            "read too far back: {joined}"
        );
        assert!(
            !joined.contains("unrelated line"),
            "read unrelated file: {joined}"
        );
    }

    #[tokio::test]
    async fn log_tail_reports_io_errors_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        // A directory named like the log file makes reading it fail.
        std::fs::create_dir(dir.path().join("cursor-byok.2026-11-11.log")).unwrap();
        let tail = collect_log_tail(
            dir.path(),
            parse("2026-11-11T12:00:05Z"),
            Duration::from_secs(60),
            4096,
        )
        .await;
        assert!(tail.lines.is_empty());
        assert!(tail.error.is_some());
    }

    fn parse(value: &str) -> DateTime<Utc> {
        value.parse().expect("valid RFC3339 timestamp")
    }
}
