//! Reads a time-windowed, redacted tail of the daily application log files.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, NaiveDateTime, Utc};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};

use crate::diagnostics::redact::Redactor;

/// Prefix/suffix used by the desktop's daily rolling log appender.
const LOG_FILE_PREFIX: &str = "cursor-byok";
const LOG_FILE_SUFFIX: &str = "log";
const FILE_DATE_FORMAT: &str = "%Y-%m-%d";
/// The design caps the log tail at 200 lines before the byte budget is applied.
const MAX_LOG_LINES: usize = 200;
/// Hard cap for a single line held while streaming; longer lines keep their tail.
const MAX_LINE_BYTES: usize = 64 * 1024;
/// Bytes read from the start of a line so timestamps survive tail trimming.
const TIMESTAMP_PROBE_BYTES: usize = 64;

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
/// midnight, the previous day. Files are streamed line by line; retained output
/// never exceeds `max_bytes`, so memory stays proportional to the budget rather
/// than to the log file size. Missing directories and files are not errors.
///
/// Timestamped lines are filtered to the window. Lines whose leading timestamp
/// cannot be parsed are treated as continuations of the preceding timestamped
/// block, so a multi-line event is kept or dropped as a whole. When a file has
/// no parseable timestamp at all, only its final [`MAX_LOG_LINES`] lines are
/// retained; the result is marked truncated when lines are dropped.
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

    let redactor = Redactor::new();
    let mut retained = BoundedLines::new(MAX_LOG_LINES, max_bytes);
    for date in dates {
        let path = log_file_path(log_dir, date);
        let file = match tokio::fs::File::open(&path).await {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return error_tail(&path, &error),
        };

        let mut reader = BufReader::new(file);
        let mut line = Vec::new();
        let mut probe = Vec::new();
        let mut has_timestamp = false;
        let mut keep_block = false;
        // Preamble lines are buffered unredacted until a timestamped header
        // proves whether the file format is parseable. They are dropped without
        // redaction when a header appears.
        let mut fallback = BoundedLines::new(MAX_LOG_LINES, max_bytes);

        loop {
            match read_bounded_line(&mut reader, &mut line, &mut probe).await {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => return error_tail(&path, &error),
            }

            let probe_text = String::from_utf8_lossy(&probe);
            let text = String::from_utf8_lossy(&line);
            match parse_line_timestamp(&probe_text) {
                Some(timestamp) => {
                    if !has_timestamp {
                        has_timestamp = true;
                        fallback.clear();
                    }
                    keep_block = timestamp >= start && timestamp <= end;
                    if keep_block {
                        retained.push(redactor.redact(&text));
                    }
                }
                None if has_timestamp => {
                    if keep_block {
                        retained.push(redactor.redact(&text));
                    }
                }
                None => fallback.push(text.into_owned()),
            }
        }

        if !has_timestamp {
            retained.truncated |= fallback.truncated;
            for line in fallback.into_vec() {
                retained.push(redactor.redact(&line));
            }
        }
    }

    let truncated = retained.truncated;
    LogTail {
        lines: retained.into_vec(),
        truncated,
        error: None,
    }
}

/// Reads one newline-free line, bounding both the retained tail and the
/// timestamp probe so a pathological line cannot exhaust memory.
async fn read_bounded_line<R>(
    reader: &mut R,
    line: &mut Vec<u8>,
    probe: &mut Vec<u8>,
) -> std::io::Result<bool>
where
    R: AsyncBufRead + Unpin,
{
    line.clear();
    probe.clear();
    let mut read_any = false;
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            if !read_any {
                return Ok(false);
            }
            trim_trailing_carriage_return(line);
            return Ok(true);
        }

        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let chunk_len = newline.unwrap_or(buffer.len());
        if chunk_len > 0 {
            read_any = true;
            append_probe(probe, &buffer[..chunk_len]);
            append_bounded(line, &buffer[..chunk_len]);
        }

        match newline {
            Some(index) => {
                reader.consume(index + 1);
                trim_trailing_carriage_return(line);
                return Ok(true);
            }
            None => reader.consume(chunk_len),
        }
    }
}

fn append_probe(probe: &mut Vec<u8>, chunk: &[u8]) {
    if probe.len() >= TIMESTAMP_PROBE_BYTES {
        return;
    }
    let take = (TIMESTAMP_PROBE_BYTES - probe.len()).min(chunk.len());
    probe.extend_from_slice(&chunk[..take]);
}

fn append_bounded(line: &mut Vec<u8>, chunk: &[u8]) {
    if line.len() + chunk.len() <= MAX_LINE_BYTES {
        line.extend_from_slice(chunk);
        return;
    }

    let overflow = line.len() + chunk.len() - MAX_LINE_BYTES;
    let drained = overflow.min(line.len());
    line.drain(..drained);
    line.extend_from_slice(&chunk[overflow - drained..]);
}

fn trim_trailing_carriage_return(line: &mut Vec<u8>) {
    if line.last() == Some(&b'\r') {
        line.pop();
    }
}

fn error_tail(path: &Path, error: &std::io::Error) -> LogTail {
    let message = format!("failed to read {}: {error}", path.display());
    LogTail {
        lines: Vec::new(),
        truncated: false,
        error: Some(Redactor::new().redact(&message)),
    }
}

fn log_file_path(log_dir: &Path, date: NaiveDate) -> PathBuf {
    log_dir.join(format!(
        "{LOG_FILE_PREFIX}.{}.{LOG_FILE_SUFFIX}",
        date.format(FILE_DATE_FORMAT)
    ))
}

/// Rolling, bounded set of the latest lines.
///
/// Line and byte budgets are enforced on every push, so the structure never
/// holds more than `max_lines` lines or `max_bytes` of joined output. A single
/// oversized line is clipped to the trailing `max_bytes` bytes.
struct BoundedLines {
    lines: VecDeque<String>,
    joined_bytes: usize,
    max_lines: usize,
    max_bytes: usize,
    truncated: bool,
}

impl BoundedLines {
    fn new(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            joined_bytes: 0,
            max_lines,
            max_bytes,
            truncated: false,
        }
    }

    fn clear(&mut self) {
        self.lines.clear();
        self.joined_bytes = 0;
    }

    fn push(&mut self, mut line: String) {
        if self.max_lines == 0 || self.max_bytes == 0 {
            self.truncated = true;
            return;
        }

        if line.len() > self.max_bytes {
            let mut start = line.len() - self.max_bytes;
            while start < line.len() && !line.is_char_boundary(start) {
                start += 1;
            }
            line.drain(..start);
            self.truncated = true;
        }

        while self.lines.len() >= self.max_lines {
            self.pop_front();
            self.truncated = true;
        }
        while !self.lines.is_empty() && self.joined_bytes + 1 + line.len() > self.max_bytes {
            self.pop_front();
            self.truncated = true;
        }

        self.joined_bytes = if self.lines.is_empty() {
            line.len()
        } else {
            self.joined_bytes + 1 + line.len()
        };
        self.lines.push_back(line);
    }

    fn pop_front(&mut self) {
        if let Some(front) = self.lines.pop_front() {
            self.joined_bytes = self.joined_bytes.saturating_sub(front.len());
            if !self.lines.is_empty() {
                self.joined_bytes = self.joined_bytes.saturating_sub(1);
            }
        }
    }

    fn into_vec(self) -> Vec<String> {
        self.lines.into_iter().collect()
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

    #[test]
    fn bounded_lines_enforce_budgets_on_every_push() {
        let mut bounded = BoundedLines::new(MAX_LOG_LINES, 64);
        for index in 0..1_000 {
            bounded.push(format!("line-{index:04}-{}", "x".repeat(20)));
            assert!(
                bounded.joined_bytes <= 64,
                "byte budget exceeded: {}",
                bounded.joined_bytes
            );
            assert!(bounded.lines.len() <= MAX_LOG_LINES);
            let expected = bounded
                .lines
                .iter()
                .map(String::len)
                .sum::<usize>()
                .saturating_add(bounded.lines.len().saturating_sub(1));
            assert_eq!(bounded.joined_bytes, expected);
        }
        assert!(bounded.truncated);
        assert!(bounded
            .lines
            .back()
            .expect("non-empty")
            .contains("line-0999"));
    }

    #[tokio::test]
    async fn log_tail_holds_only_the_byte_budget_for_a_large_day_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut content = String::new();
        for index in 0..5_000 {
            content.push_str(&format!(
                "2026-11-11T12:00:05.000000Z  INFO large-{index}-{}\n",
                "y".repeat(40)
            ));
        }
        std::fs::write(dir.path().join("cursor-byok.2026-11-11.log"), content).unwrap();

        let tail = collect_log_tail(
            dir.path(),
            parse("2026-11-11T12:00:05Z"),
            Duration::from_secs(60),
            4096,
        )
        .await;

        let joined = tail.lines.join("\n");
        assert!(joined.len() <= 4096, "budget exceeded: {}", joined.len());
        assert!(tail.lines.len() <= MAX_LOG_LINES);
        assert!(tail.truncated);
        assert!(tail.lines.last().expect("non-empty").contains("large-4999"));
    }

    #[tokio::test]
    async fn log_tail_applies_the_byte_cap_after_redaction() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cursor-byok.2026-11-11.log"), "token=S\n").unwrap();

        // The raw line fits in the budget; redaction expands it, so the cap must
        // be applied to the redacted text and the secret must not survive.
        let tail = collect_log_tail(
            dir.path(),
            parse("2026-11-11T12:00:05Z"),
            Duration::from_secs(60),
            9,
        )
        .await;

        let joined = tail.lines.join("\n");
        assert!(
            !joined.to_lowercase().contains("token"),
            "not redacted: {joined}"
        );
        assert!(joined.contains("REDACTED"), "not redacted: {joined}");
        assert!(joined.len() <= 9, "budget exceeded: {joined}");
        assert!(tail.truncated);
    }

    #[test]
    fn io_errors_are_redacted_before_being_returned() {
        let error = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let tail = error_tail(Path::new("/Users/alice/logs/cursor-byok.log"), &error);
        let message = tail.error.expect("io error");
        assert!(!message.contains("alice"), "leaked user path: {message}");
        assert!(message.contains('~'), "path not redacted: {message}");
    }

    #[tokio::test]
    async fn log_tail_caps_the_fallback_when_no_timestamps_parse() {
        let dir = tempfile::tempdir().unwrap();
        let mut content = String::new();
        for index in 0..250 {
            content.push_str(&format!("plain line {index}\n"));
        }
        std::fs::write(dir.path().join("cursor-byok.2026-11-11.log"), content).unwrap();

        let tail = collect_log_tail(
            dir.path(),
            parse("2026-11-11T12:00:05Z"),
            Duration::from_secs(60),
            1_000_000,
        )
        .await;

        assert_eq!(tail.lines.len(), MAX_LOG_LINES);
        assert!(tail.truncated);
        assert!(tail
            .lines
            .first()
            .expect("non-empty")
            .contains("plain line 50"));
        assert!(!tail.lines.iter().any(|line| line.contains("plain line 49")));
        assert!(tail
            .lines
            .last()
            .expect("non-empty")
            .contains("plain line 249"));
    }

    fn parse(value: &str) -> DateTime<Utc> {
        value.parse().expect("valid RFC3339 timestamp")
    }
}
