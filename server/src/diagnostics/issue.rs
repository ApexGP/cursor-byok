//! Renders the fixed GitHub issue template for a diagnostic report.

use serde_json::Value;

use crate::{
    diagnostics::{
        code::Severity,
        report::{clip_bytes, BundleLink, DiagnosticReport},
    },
    Result,
};

/// Maximum issue body length accepted by the template.
pub const ISSUE_BODY_LIMIT: usize = 6000;
/// Maximum title length.
pub const ISSUE_TITLE_LIMIT: usize = 120;

/// Fill-in variables for the fixed issue template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueDraft {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
}

const TITLE_SUMMARY_CHARS: usize = 80;

/// Renders title, body and labels for a prefilled GitHub issue.
pub fn render_issue(
    report: &DiagnosticReport,
    bundle: Option<&BundleLink>,
    _repository: &str,
) -> Result<IssueDraft> {
    let title = render_title(report);
    let labels = render_labels(report);

    let summary = clip_bytes(first_line(&report.summary), 400);
    let environment = render_environment(report);
    let happened = render_what_happened(report);
    let bundle_slot = render_bundle_slot(bundle);
    let redaction = render_redaction_notes(&report.redaction);

    // Measure the template with an empty JSON slot, then render it once with
    // the fitted JSON. This avoids searching template text for a placeholder
    // that user-controlled slots could also contain.
    let overhead = render_template(
        &summary,
        &environment,
        &happened,
        &bundle_slot,
        &redaction,
        "",
    )
    .len();
    let budget = ISSUE_BODY_LIMIT.saturating_sub(overhead).max(64);
    let diagnostic_json = crate::diagnostics::report::issue_json(report, bundle, budget)?;
    let body = render_template(
        &summary,
        &environment,
        &happened,
        &bundle_slot,
        &redaction,
        &diagnostic_json,
    );

    Ok(IssueDraft {
        title,
        body,
        labels,
    })
}

/// Builds a `https://github.com/<repo>/issues/new` URL for a rendered draft.
pub fn issue_url(repository: &str, draft: &IssueDraft) -> String {
    let repository = repository.trim().trim_matches('/');
    let title = clip_chars(&draft.title, ISSUE_TITLE_LIMIT);
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("title", &title);
    serializer.append_pair("body", &draft.body);
    if !draft.labels.is_empty() {
        serializer.append_pair("labels", &draft.labels.join(","));
    }
    format!(
        "https://github.com/{repository}/issues/new?{}",
        serializer.finish()
    )
}

fn render_title(report: &DiagnosticReport) -> String {
    let summary = first_line(&report.summary);
    let fallback = report
        .error
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("diagnostic report");
    let summary = if summary.is_empty() {
        fallback
    } else {
        summary
    };
    let summary = clip_chars(summary, TITLE_SUMMARY_CHARS);
    clip_chars(
        &format!("[{}] {}", report.error_code, summary),
        ISSUE_TITLE_LIMIT,
    )
}

fn render_labels(report: &DiagnosticReport) -> Vec<String> {
    let mut labels = vec!["diagnostics".to_string(), "auto-report".to_string()];
    labels.push(format!(
        "platform:{}",
        sanitize_label(&report.app.platform, "unknown")
    ));
    labels.push(format!(
        "area:{}",
        sanitize_label(&report.context.area, "unknown")
    ));
    labels.push(format!(
        "severity:{}",
        sanitize_label(severity_label(report.severity), "error")
    ));
    labels
}

fn severity_label(severity: Severity) -> &'static str {
    match severity {
        Severity::Warning => "warning",
        Severity::Error => "error",
        Severity::Fatal => "fatal",
    }
}

fn render_environment(report: &DiagnosticReport) -> String {
    let cursor = report
        .app
        .cursor_version
        .as_deref()
        .map(|version| clip_bytes(version, 40))
        .unwrap_or_else(|| "未知".to_string());
    format!(
        "| 项 | 值 |\n| --- | --- |\n| cursor-byok | {} |\n| Cursor | {} |\n| OS | {} |\n| 模块 | {} |",
        clip_bytes(&report.app.app_version, 40),
        cursor,
        clip_bytes(
            &format!("{} {}", report.app.platform, report.app.arch),
            80
        ),
        clip_bytes(&report.context.area, 60),
    )
}

fn render_what_happened(report: &DiagnosticReport) -> String {
    let error = &report.error;
    let mut lines = Vec::new();
    if let Some(kind) = error.get("kind").and_then(Value::as_str) {
        lines.push(format!("- 类型：{}", clip_bytes(kind, 200)));
    }
    if let Some(message) = error.get("message").and_then(Value::as_str) {
        lines.push(format!("- 信息：{}", clip_bytes(message, 800)));
    }
    if let Some(chain) = error.get("chain").and_then(Value::as_array) {
        let items: Vec<&str> = chain.iter().filter_map(Value::as_str).take(5).collect();
        if !items.is_empty() {
            lines.push("- 错误链：".to_string());
            for item in items {
                lines.push(format!("  - {}", clip_bytes(item, 300)));
            }
        }
    }
    if lines.is_empty() {
        "无可用错误详情。".to_string()
    } else {
        lines.join("\n")
    }
}

fn render_bundle_slot(bundle: Option<&BundleLink>) -> String {
    match bundle {
        Some(bundle) => format!(
            "诊断包：{}（SHA-256 `{}`，{} 过期，30 天后自动删除）",
            clip_bytes(&bundle.url, 300),
            clip_bytes(&bundle.sha256, 80),
            bundle
                .expires_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        ),
        None => "诊断包将在创建 Issue 前自动上传并附加链接（30 天后自动删除）；本机不保留副本。"
            .to_string(),
    }
}

fn render_redaction_notes(redaction: &Value) -> String {
    let join = |key: &str| {
        redaction
            .get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("、")
            })
            .unwrap_or_default()
    };
    format!(
        "- 已脱敏：{}\n- 已省略：{}\n- {}",
        join("applied"),
        join("omitted"),
        redaction
            .get("notes")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(Value::as_str)
            .unwrap_or("diagnostic bundle is redacted before upload")
    )
}

#[allow(clippy::too_many_arguments)]
fn render_template(
    summary: &str,
    environment: &str,
    what_happened: &str,
    bundle: &str,
    redaction: &str,
    diagnostic_json: &str,
) -> String {
    format!(
        "<!-- cursor-byok-diagnostic:v1 -->
## 概要
{summary}

## 环境
{environment}

## 发生了什么
{what_happened}

## 复现步骤
未提供。自动报告仅包含脱敏后的运行证据，不包含用户输入或消息正文。

## 诊断数据
<!-- CURSOR_BYOK_DIAGNOSTIC_START -->
```json
{diagnostic_json}
```
<!-- CURSOR_BYOK_DIAGNOSTIC_END -->

## 诊断包
{bundle}

## 脱敏与缺失说明
{redaction}
"
    )
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

fn clip_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut clipped: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}

fn sanitize_label(raw: &str, fallback: &str) -> String {
    let cleaned: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '/') {
                character
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-');
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{
        code::DiagnosticCode,
        report::test_support::{at, test_report, test_report_with_logs},
    };
    use serde_json::Value;

    fn extract_between_markers(body: &str) -> Option<&str> {
        const START: &str = "<!-- CURSOR_BYOK_DIAGNOSTIC_START -->";
        const END: &str = "<!-- CURSOR_BYOK_DIAGNOSTIC_END -->";
        let after_start = &body[body.find(START)? + START.len()..];
        let after_start = &after_start[after_start.find("```json")? + "```json".len()..];
        let after_start = after_start.trim_start();
        let end = after_start.find(END)?;
        let block = after_start[..end].trim_end();
        let block = block.strip_suffix("```")?.trim();
        Some(block)
    }

    #[test]
    fn issue_body_keeps_markers_and_valid_json_when_truncated() {
        let report = test_report_with_logs(400);
        let body = render_issue(&report, None, "ApexGP/cursor-byok")
            .unwrap()
            .body;
        assert!(
            body.len() <= ISSUE_BODY_LIMIT,
            "body is {} bytes",
            body.len()
        );
        let json = extract_between_markers(&body).unwrap();
        let parsed: Value = serde_json::from_str(json).unwrap();
        assert_eq!(parsed["schemaVersion"], 1);
        assert_eq!(parsed["logs"]["truncated"], true);
        assert!(!body.contains("conv-sentinel"));
        assert!(body.contains("CURSOR_BYOK_DIAGNOSTIC_START"));
        assert!(body.contains("CURSOR_BYOK_DIAGNOSTIC_END"));
    }

    #[test]
    fn sentinel_in_a_slot_does_not_corrupt_the_json_block() {
        let mut report = test_report(DiagnosticCode::Sync);
        report.summary = "__CURSOR_BYOK_DIAGNOSTIC_JSON__ summary".to_string();
        let body = render_issue(&report, None, "ApexGP/cursor-byok")
            .unwrap()
            .body;
        assert!(body.contains("__CURSOR_BYOK_DIAGNOSTIC_JSON__"));
        let parsed: Value = serde_json::from_str(extract_between_markers(&body).unwrap()).unwrap();
        assert_eq!(parsed["schemaVersion"], 1);
        assert_eq!(parsed["logs"]["truncated"], false);
        assert!(body.len() <= ISSUE_BODY_LIMIT);
    }

    #[test]
    fn title_contains_code_and_labels_are_stable() {
        let draft = render_issue(
            &test_report(DiagnosticCode::Sync),
            None,
            "ApexGP/cursor-byok",
        )
        .unwrap();
        assert!(draft.title.starts_with("[CB-SYNC-001] "));
        assert!(draft.labels.contains(&"diagnostics".to_string()));
        assert!(draft.labels.contains(&"auto-report".to_string()));
        assert!(draft.labels.contains(&"platform:windows".to_string()));
        assert!(draft.labels.contains(&"area:sync".to_string()));
        assert!(draft.labels.contains(&"severity:error".to_string()));
    }

    #[test]
    fn title_is_capped_at_120_characters() {
        let mut report = test_report(DiagnosticCode::Sync);
        report.summary = "长".repeat(400);
        let draft = render_issue(&report, None, "ApexGP/cursor-byok").unwrap();
        assert!(
            draft.title.chars().count() <= ISSUE_TITLE_LIMIT,
            "title has {} chars",
            draft.title.chars().count()
        );
        assert!(draft.title.starts_with("[CB-SYNC-001] "));
    }

    #[test]
    fn missing_bundle_is_stated_instead_of_fabricating_a_url() {
        let body = render_issue(
            &test_report(DiagnosticCode::Sync),
            None,
            "ApexGP/cursor-byok",
        )
        .unwrap()
        .body;
        assert!(!body.contains("https://diag."));
        assert!(body.contains("上传"));
        let json = extract_between_markers(&body).unwrap();
        let parsed: Value = serde_json::from_str(json).unwrap();
        assert!(parsed.get("bundle").is_none());
    }

    #[test]
    fn bundle_link_is_rendered_into_body_and_json() {
        let bundle = BundleLink {
            url: "https://diag.example/r/r-20261111-0123456789abcdef0123456789abcdef".to_string(),
            sha256: "abc123".to_string(),
            expires_at: at("2026-12-11T12:34:56Z"),
        };
        let body = render_issue(
            &test_report(DiagnosticCode::Sync),
            Some(&bundle),
            "ApexGP/cursor-byok",
        )
        .unwrap()
        .body;
        assert!(body.contains(&bundle.url));
        let parsed: Value = serde_json::from_str(extract_between_markers(&body).unwrap()).unwrap();
        assert_eq!(parsed["bundle"]["url"], bundle.url);
        assert_eq!(parsed["bundle"]["sha256"], "abc123");
        assert_eq!(parsed["bundle"]["expiresAt"], "2026-12-11T12:34:56Z");
    }

    #[test]
    fn body_never_exceeds_the_limit_with_large_error_slots() {
        let mut report = test_report(DiagnosticCode::Sync);
        report.summary = "摘".repeat(2000);
        report.error = serde_json::json!({
            "kind": "sqlite",
            "message": "m".repeat(20_000),
            "chain": (0..50).map(|index| format!("chain-{index}-{}", "c".repeat(500))).collect::<Vec<_>>(),
            "fingerprint": "sha256:deadbeef",
        });
        let body = render_issue(&report, None, "ApexGP/cursor-byok")
            .unwrap()
            .body;
        assert!(
            body.len() <= ISSUE_BODY_LIMIT,
            "body is {} bytes",
            body.len()
        );
        let parsed: Value = serde_json::from_str(extract_between_markers(&body).unwrap()).unwrap();
        assert_eq!(parsed["schemaVersion"], 1);
    }

    #[test]
    fn issue_url_encodes_title_body_and_labels() {
        let draft = render_issue(
            &test_report(DiagnosticCode::Sync),
            None,
            "ApexGP/cursor-byok",
        )
        .unwrap();
        let url = issue_url("ApexGP/cursor-byok", &draft);
        assert!(url.starts_with("https://github.com/ApexGP/cursor-byok/issues/new?"));
        let parsed = url::Url::parse(&url).unwrap();
        let pairs: std::collections::HashMap<String, String> =
            parsed.query_pairs().into_owned().collect();
        assert_eq!(pairs["title"], draft.title);
        assert_eq!(pairs["body"], draft.body);
        assert_eq!(pairs["labels"], draft.labels.join(","));
    }

    #[test]
    fn issue_url_normalizes_the_repository_slug() {
        let draft = IssueDraft {
            title: "[CB-APP-001] test".to_string(),
            body: "body".to_string(),
            labels: vec!["diagnostics".to_string()],
        };
        assert_eq!(
            issue_url("/ApexGP/cursor-byok/", &draft),
            issue_url("ApexGP/cursor-byok", &draft)
        );
    }
}
