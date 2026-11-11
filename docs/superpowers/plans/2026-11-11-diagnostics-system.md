# cursor-byok 诊断与一键反馈系统实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 自动捕获终态失败，生成结构化、脱敏的诊断报告；用户确认后自动上传诊断包、删除本地包，并打开预填的 GitHub Issue 页面。

**Architecture:** Rust 侧新增 `server/src/diagnostics/`，通过独立异步 writer 保存脱敏事件，按需查询数据库 allowlist 证据；控制面新增诊断查询/创建 Issue API；桌面端新增全局问题入口、预览弹窗和 Error Boundary。诊断包在内存生成，上传成功后释放，再由前端打开 GitHub `issues/new`。

**Tech Stack:** Rust/Axum/SQLx/SQLite、reqwest、zip、sha2、Tauri 2、React 19、TypeScript、Node `node:test`、Cloudflare Worker + R2（参考上传服务）。

**Spec:** `docs/superpowers/specs/2026-11-11-diagnostics-design.md`

## Global Constraints

- Issue 正文固定模板，程序只填槽；禁止自由生成正文。
- 机器可读区必须位于 `<!-- CURSOR_BYOK_DIAGNOSTIC_START -->` 与 `<!-- CURSOR_BYOK_DIAGNOSTIC_END -->` 之间，内容为合法 JSON。
- 标题格式 `[CB-<AREA>-<NNN>] <summary>`，标题最长 120 字符。
- Issue 正文预填最长 6000 字符；超限必须确定性裁剪并保持 JSON 合法。
- 采集、写入本地事件、生成 Issue、上传之前都必须脱敏。
- 禁止输出：`api_key`、Authorization、Cookie、Token、密码、请求/响应正文、消息正文、完整 `state.vscdb`、完整 `cursor-byok.db`。
- 数据库证据只允许设计文档第 7 节的表和字段。
- 上传只发生在用户点击“上传并创建 Issue”之后；上传 URL 未配置时不得回退到其它服务。
- 诊断包只在内存中生成；成功和失败路径都不得写本地文件。
- 上传成功后打开 GitHub `issues/new`，由用户点击 Submit；应用不得持有 GitHub Token。
- 诊断包上限 8 MiB，服务端保留 30 天。
- 采集不得阻塞 Agent Run 热路径；诊断模块自身失败不得递归上报。

## Review Focus

- 超大/畸形错误消息与日志：必须截断为合法 JSON，不能破坏 Issue 解析。
- 不常见密钥格式（JWT、Bearer、query token、自定义 Header）：sentinel 测试必须证明不会泄漏。
- Windows/数据库被锁或缺失：报告仍应生成，并明确记录 `dbUnavailable`。
- 上传超时/500：不得留下本地 zip，不得打开 Issue 页面。
- 诊断 JSON 超过正文上限：裁剪后 markers、JSON、`truncated` 必须一致。

---

### Task 1: 错误码、事件模型与脱敏

**Files:**
- Create: `server/src/diagnostics/mod.rs`
- Create: `server/src/diagnostics/code.rs`
- Create: `server/src/diagnostics/event.rs`
- Create: `server/src/diagnostics/redact.rs`
- Modify: `server/src/lib.rs`

**Interfaces:**
- Consumes: none.
- Produces:
  - `pub enum DiagnosticCode { AppStartup, AppPanic, Database, CursorProcess, CursorState, Sync, SyncVerify, Run, RunSse, ProviderRetry, ProviderIdle, ToolRound, UiBoundary, UiUnhandled, Update }`
  - `DiagnosticCode::as_str(self) -> &'static str`
  - `pub enum Severity { Warning, Error, Fatal }`
  - `pub struct DiagnosticEvent { report_id: String, code: DiagnosticCode, severity: Severity, captured_at: DateTime<Utc>, summary: String, error_kind: Option<String>, error_message: Option<String>, error_chain: Vec<String>, fingerprint: String, context: DiagnosticContext }`
  - `DiagnosticEvent::new(code: DiagnosticCode, summary: impl Into<String>) -> Self`
  - `DiagnosticEvent::with_error(self, kind: impl Into<String>, message: impl Into<String>) -> Self`
  - `DiagnosticEvent::with_context(self, context: DiagnosticContext) -> Self`
  - `pub struct DiagnosticContext { pub area: String, pub operation: String, pub run_id: Option<String>, pub request_id: Option<String>, pub conversation_id: Option<String>, pub model_hash: Option<String>, pub provider_type: Option<String>, pub http_status: Option<u16>, pub takeover_enabled: Option<bool>, pub cursor_running_at_sync: Option<bool> }`
  - `impl Default for DiagnosticContext`、`DiagnosticContext::new(area, operation)`；其余字段可由 crate 内代码直接赋值。
  - `pub fn fingerprint(code: DiagnosticCode, message: &str) -> String`
  - `pub struct Redactor; impl Redactor { pub fn new() -> Self; pub fn redact(&self, value: &str) -> String; }`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn all_codes_are_stable_and_unique() {
    let codes = [
        DiagnosticCode::AppStartup,
        DiagnosticCode::AppPanic,
        DiagnosticCode::Database,
        DiagnosticCode::CursorProcess,
        DiagnosticCode::CursorState,
        DiagnosticCode::Sync,
        DiagnosticCode::SyncVerify,
        DiagnosticCode::Run,
        DiagnosticCode::RunSse,
        DiagnosticCode::ProviderRetry,
        DiagnosticCode::ProviderIdle,
        DiagnosticCode::ToolRound,
        DiagnosticCode::UiBoundary,
        DiagnosticCode::UiUnhandled,
        DiagnosticCode::Update,
    ];
    let mut seen = std::collections::HashSet::new();
    for code in codes {
        assert!(seen.insert(code.as_str()));
    }
    assert_eq!(DiagnosticCode::Sync.as_str(), "CB-SYNC-001");
    assert_eq!(DiagnosticCode::Run.as_str(), "CB-RUN-001");
}
```

```rust
#[test]
fn redacts_common_credentials_and_user_paths() {
    let input = concat!(
        "Authorization: Bearer sk-live-SENTINEL ",
        "api_key=abc123 token=def456 ",
        "jwt=eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.signature ",
        r"path C:\Users\alice\AppData\Local ",
        "email report@example.com"
    );
    let output = Redactor::new().redact(input);
    for secret in ["SENTINEL", "abc123", "def456", "eyJhbGci", "alice", "report@example.com"] {
        assert!(!output.contains(secret), "leaked {secret}: {output}");
    }
    assert!(output.contains("[REDACTED]"));
    assert!(output.contains("%USERPROFILE%"));
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p cursor-server --lib diagnostics::`
Expected: FAIL，模块/类型不存在。

- [ ] **Step 3: 实现最小代码**

在 `code.rs` 定义固定字符串映射；在 `event.rs` 用 `uuid::Uuid::new_v4()` 生成 `r-YYYYMMDD-<32 hex>`，用 `sha2` 生成 fingerprint；在 `redact.rs` 实现正则替换，覆盖 `Bearer`、`api_key`/`token`/`password`/`secret`、JWT、`sk-`/`xai-`/`gsk_`/`AIza`、URL query、Windows/macOS 用户路径、邮箱。`mod.rs` 只导出类型；`lib.rs` 增加 `pub mod diagnostics;`。

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p cursor-server --lib diagnostics::`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add server/src/diagnostics server/src/lib.rs
git commit -m "feat(diagnostics): add error codes and redaction"
```

---

### Task 2: 事件存储、去重与忽略

**Files:**
- Create: `server/src/diagnostics/store.rs`
- Modify: `server/src/diagnostics/mod.rs`

**Interfaces:**
- Consumes: Task 1 types.
- Produces:
  - `pub struct DiagnosticsConfig { enabled: bool, upload_url: Option<String>, github_repository: String, retention_days: u32, max_events: usize, log_window: Duration, data_dir: PathBuf }`
  - `pub struct DiagnosticsHub { /* private */ }`
  - `DiagnosticsHub::new(config: DiagnosticsConfig, store: Store) -> Result<Arc<Self>>`
  - `DiagnosticsHub::record(&self, event: DiagnosticEvent)`
  - `DiagnosticsHub::install_global(hub: Arc<Self>)`
  - `pub fn global() -> Option<&'static Arc<DiagnosticsHub>>`
  - `pub fn record(event: DiagnosticEvent)`
  - `pub async fn reports(&self) -> Vec<ReportSummary>`
  - `pub async fn event(&self, report_id: &str) -> Option<DiagnosticEvent>`
  - `pub async fn dismiss(&self, report_id: &str) -> Result<()>`
  - `pub struct ReportSummary { report_id: String, code: String, severity: Severity, summary: String, occurrences: u32, first_seen: DateTime<Utc>, last_seen: DateTime<Utc>, dismissed: bool }`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn same_fingerprint_is_aggregated() {
    let temp = tempfile::tempdir().unwrap();
    let hub = DiagnosticsHub::new(test_config(&temp), test_store().await).unwrap();
    hub.record(DiagnosticEvent::new(DiagnosticCode::Sync, "database is locked"));
    hub.record(DiagnosticEvent::new(DiagnosticCode::Sync, "database is locked"));
    let reports = hub.reports().await;
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].occurrences, 2);
}
```

```rust
#[tokio::test]
async fn dismissed_report_is_hidden_until_retention_window() {
    let temp = tempfile::tempdir().unwrap();
    let hub = DiagnosticsHub::new(test_config(&temp), test_store().await).unwrap();
    hub.record(DiagnosticEvent::new(DiagnosticCode::UiBoundary, "render failed"));
    let id = hub.reports().await[0].report_id.clone();
    hub.dismiss(&id).await.unwrap();
    assert!(hub.reports().await.is_empty());
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p cursor-server --lib diagnostics::store`
Expected: FAIL。

- [ ] **Step 3: 实现存储**

使用 `tokio::sync::mpsc` + 独立 writer task；内存 `Mutex<BTreeMap<String, ReportSummary>>`；写 `events.jsonl`（0600）与 `dismissed.json`；启动时清理超过 `retention_days`、超过 `max_events` 的事件。写入前调用 `Redactor`。`record` 在 channel 满时丢弃并记录普通日志，绝不阻塞调用方。`sync_verify` 暂只保留错误码，不实现写入后回读。

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p cursor-server --lib diagnostics::`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add server/src/diagnostics
git commit -m "feat(diagnostics): store and deduplicate failure reports"
```

---

### Task 3: 日志窗口与基础证据

**Files:**
- Create: `server/src/diagnostics/logs.rs`
- Create: `server/src/diagnostics/evidence.rs`
- Modify: `server/src/diagnostics/mod.rs`

**Interfaces:**
- Consumes: Task 1 `Redactor`.
- Produces:
  - `pub struct LogTail { pub lines: Vec<String>, pub truncated: bool, pub error: Option<String> }`
  - `pub async fn collect_log_tail(log_dir: &Path, captured_at: DateTime<Utc>, window: Duration, max_bytes: usize) -> LogTail`
  - `pub struct BaseEvidence { pub app_version: String, pub platform: String, pub arch: String, pub cursor_version: Option<String> }`
  - `pub fn collect_base_evidence() -> BaseEvidence`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn log_tail_reads_only_the_capture_window_and_redacts() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("cursor-byok.2026-11-11.log");
    std::fs::write(
        &file,
        "old line\napi_key=SENTINEL at 12:00:00\nnew line at 12:00:05\n",
    ).unwrap();
    let tail = collect_log_tail(dir.path(), parse("2026-11-11T12:00:05Z"), Duration::from_secs(60), 4096).await;
    assert!(tail.lines.iter().any(|line| line.contains("new line")));
    assert!(!tail.lines.join("\n").contains("SENTINEL"));
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p cursor-server --lib diagnostics::logs`
Expected: FAIL。

- [ ] **Step 3: 实现**

读取日志目录中与 `captured_at` 同日的 `cursor-byok.*.log`（必要时前一日），按时间窗口过滤；每行先脱敏；总字节超过 `max_bytes` 时保留最后 N 行并置 `truncated`。`collect_base_evidence` 从 `CARGO_PKG_VERSION`、`std::env::consts::OS`、`std::env::consts::ARCH` 填充；Cursor 版本暂为 `None`。

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p cursor-server --lib diagnostics::logs`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add server/src/diagnostics
git commit -m "feat(diagnostics): collect redacted log evidence"
```

---

### Task 4: 数据库 allowlist 证据

**Files:**
- Create: `server/src/diagnostics/db_evidence.rs`
- Create: `server/src/diagnostics/cursor_state.rs`
- Modify: `server/src/local_app/mod.rs`
- Modify: `server/src/diagnostics/mod.rs`

**Interfaces:**
- Consumes: `Store` (`server/src/store/sqlite.rs`), `DiagnosticContext`.
- Produces:
  - `pub struct DatabaseEvidence { pub cursor_byok: CursorByokEvidence, pub cursor_state: CursorStateEvidence }`
  - `pub async fn collect_database_evidence(store: &Store, context: &DiagnosticContext, state_db: &Path) -> DatabaseEvidence`
  - `pub struct CursorByokEvidence { pub schema_version: Option<i64>, pub file: FileStats, pub settings: Value, pub run: Option<Value>, pub llm_calls: Vec<Value>, pub db_unavailable: Option<String> }`
  - `pub struct CursorStateEvidence { pub exists: bool, pub backup_age_ms: Option<i64>, pub catalog_ids: Vec<String>, pub synced_ids: Vec<String>, pub override_enabled: Vec<String>, pub override_disabled: Vec<String>, pub truncated: bool, pub error: Option<String> }`
  - `pub(crate) fn cursor_state_db_path() -> Result<PathBuf>` in `local_app/mod.rs`

- [ ] **Step 1: 写失败测试（sentinel）**

```rust
#[tokio::test]
async fn evidence_never_contains_api_keys_or_message_bodies() {
    let temp = tempfile::tempdir().unwrap();
    let (store, _db) = seeded_store_with_secrets(&temp).await;
    let evidence = collect_database_evidence(&store, &sync_context("run-1"), &temp.path().join("state.vscdb")).await;
    let json = serde_json::to_string(&evidence).unwrap();
    assert!(!json.contains("sk-SENTINEL"));
    assert!(!json.contains("MESSAGE-SENTINEL"));
    assert!(!json.contains("REQUEST-BODY-SENTINEL"));
    assert!(!evidence.cursor_byok.llm_calls.is_empty());
}
```

```rust
#[test]
fn cursor_state_extracts_only_catalog_and_visibility_keys() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state.vscdb");
    seed_cursor_state(&path, "AUTH-SENTINEL");
    let evidence = collect_cursor_state_evidence(&path).unwrap();
    let json = serde_json::to_string(&evidence).unwrap();
    assert!(json.contains("local-model"));
    assert!(!json.contains("AUTH-SENTINEL"));
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p cursor-server --lib diagnostics::`
Expected: FAIL。

- [ ] **Step 3: 实现**

`db_evidence.rs` 使用 `store.pool` 和显式列名，按设计文档第 7.1 节查询；所有字符串先 `Redactor::redact`；`run_id`/`conversation_id` 由 context 提供，缺失时按时间窗口查找最近失败 Run。`cursor_state.rs` 用只读 `SqliteConnectOptions`、`busy_timeout(1s)`，从 `applicationUser` 只取设计文档第 7.2 节的 key；读取失败返回 `error`，不写入。`local_app/mod.rs` 暴露 `cursor_state_db_path()` 给诊断模块。

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p cursor-server --lib diagnostics::`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add server/src/diagnostics server/src/local_app/mod.rs
git commit -m "feat(diagnostics): collect allowlisted database evidence"
```

---

### Task 5: 报告组装、Issue 正文与截断

**Files:**
- Create: `server/src/diagnostics/report.rs`
- Create: `server/src/diagnostics/issue.rs`
- Modify: `server/src/diagnostics/mod.rs`

**Interfaces:**
- Consumes: Task 1-4 types.
- Produces:
  - `pub struct DiagnosticReport { schema_version: u32, report_id: String, error_code: String, severity: Severity, captured_at: DateTime<Utc>, app: BaseEvidence, error: Value, context: DiagnosticContext, timeline: Vec<Value>, logs: LogTail, database: DatabaseEvidence, redaction: Value }`
  - `pub async fn build_report(hub: &DiagnosticsHub, report_id: &str) -> Result<DiagnosticReport>`
  - `pub struct IssueDraft { pub title: String, pub body: String, pub labels: Vec<String> }`
  - `pub fn render_issue(report: &DiagnosticReport, bundle: Option<&BundleLink>, repository: &str) -> Result<IssueDraft>`
  - `pub fn diagnostic_json_for_issue(report: &DiagnosticReport, max_chars: usize) -> Result<String>`
  - `pub struct BundleLink { pub url: String, pub sha256: String, pub expires_at: DateTime<Utc> }`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn issue_body_keeps_markers_and_valid_json_when_truncated() {
    let report = report_with_large_logs();
    let body = render_issue(&report, None, "ApexGP/cursor-byok").unwrap().body;
    assert!(body.len() <= 6000);
    let json = extract_between_markers(&body).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(parsed["schemaVersion"], 1);
    assert_eq!(parsed["logs"]["truncated"], true);
    assert!(body.contains("CURSOR_BYOK_DIAGNOSTIC_START"));
    assert!(body.contains("CURSOR_BYOK_DIAGNOSTIC_END"));
}
```

```rust
#[test]
fn title_contains_code_and_labels_are_stable() {
    let draft = render_issue(&report_with_code("CB-SYNC-001"), None, "ApexGP/cursor-byok").unwrap();
    assert!(draft.title.starts_with("[CB-SYNC-001] "));
    assert!(draft.labels.contains(&"diagnostics".to_string()));
    assert!(draft.labels.contains(&"auto-report".to_string()));
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p cursor-server --lib diagnostics::`
Expected: FAIL。

- [ ] **Step 3: 实现**

`build_report` 通过 `hub.event(report_id)` 取事件，合并基础证据、日志、数据库证据；`diagnostic_json_for_issue` 按优先级裁剪 `logs.tail`、`timeline`、`database.llmCalls`、`catalogIds`，每次裁剪置 `truncated`，并重新序列化检查 UTF-8/JSON 合法。`render_issue` 使用固定模板、HTML 注释 marker、`url::form_urlencoded` 编码 GitHub URL 所需的 title/body/labels；正文超限时继续裁剪，绝不截断 JSON 字符串中间。

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p cursor-server --lib diagnostics::`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add server/src/diagnostics
git commit -m "feat(diagnostics): render structured GitHub issue bodies"
```

---

### Task 6: 诊断包生成、自动上传与本地删除

**Files:**
- Create: `server/src/diagnostics/bundle.rs`
- Create: `server/src/diagnostics/upload.rs`
- Modify: `server/src/diagnostics/mod.rs`

**Interfaces:**
- Consumes: Task 2 `DiagnosticsConfig`; Task 5 `DiagnosticReport`.
- Produces:
  - `pub struct Bundle { pub bytes: Vec<u8>, pub sha256: String, pub manifest: Value }`
  - `pub async fn build_bundle(report: &DiagnosticReport, log_tail: &LogTail) -> Result<Bundle>`
  - `pub async fn upload_bundle(config: &DiagnosticsConfig, report_id: &str, bundle: &Bundle) -> Result<UploadedBundle>`
  - `pub struct UploadedBundle { pub url: String, pub expires_at: DateTime<Utc> }`
  - `pub async fn build_upload_and_link(config: &DiagnosticsConfig, report: &DiagnosticReport, log_tail: &LogTail) -> Result<BundleLink>`
  - `pub fn ensure_upload_configured(config: &DiagnosticsConfig) -> Result<&str>`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn bundle_has_manifest_report_and_no_secret_sentinels() {
    let report = report_with_secret_sentinel();
    let bundle = build_bundle(&report, &empty_log_tail()).await.unwrap();
    let entries = read_zip_names(&bundle.bytes);
    assert!(entries.contains(&"manifest.json".to_string()));
    assert!(entries.contains(&"report.json".to_string()));
    assert!(!String::from_utf8_lossy(&bundle.bytes).contains("sk-SENTINEL"));
}
```

```rust
#[tokio::test]
async fn upload_failure_leaves_no_local_bundle() {
    let temp = tempfile::tempdir().unwrap();
    let result = build_upload_and_link_for_test(
        &config_with_mock_url("http://127.0.0.1:1"),
        &report(),
        &temp,
    ).await;
    assert!(result.is_err());
    assert!(std::fs::read_dir(temp.path()).unwrap().next().is_none());
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p cursor-server --lib diagnostics::`
Expected: FAIL。

- [ ] **Step 3: 实现**

用现有 `zip` crate 写入 `std::io::Cursor<Vec<u8>>`，在内存生成 `manifest.json`、`report.json`、`logs/tail.log`、`evidence/database.json`、`evidence/cursor-state.json`；计算 zip SHA-256。上传用 reqwest `POST` 原始 zip，设置第 10.2 节 headers，30 秒超时；2xx 解析 `url`/`expiresAt`。`build_upload_and_link` 直接返回 `BundleLink`，zip buffer 在函数返回时释放；测试 helper 的 `temp` 只用于断言目录为空，生产代码不写该目录。上传 URL 未配置时返回 `Error::Config`，不允许回退到默认服务。

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p cursor-server --lib diagnostics::`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add server/src/diagnostics
git commit -m "feat(diagnostics): upload in-memory evidence bundles"
```

---

### Task 7: 配置、全局 hub 与失败采集点

**Files:**
- Modify: `server/src/config.rs`
- Modify: `server/src/app.rs`
- Modify: `server/src/diagnostics/mod.rs`
- Modify: `server/src/run/engine.rs`
- Modify: `server/src/local_app/mod.rs`
- Modify: `server/src/run/event.rs`（如需暴露映射辅助）

**Interfaces:**
- Consumes: Task 1-6.
- Produces:
  - `DiagnosticsConfig` 从 `Config` 构造；`CURSOR_DIAGNOSTICS_UPLOAD_URL` 先读运行时 env，再读 `option_env!`。
  - `pub fn capture_run_outcome(run_id: &str, conversation_id: &str, outcome: &RunOutcome)`
  - `pub fn capture_sync_error(error: &Error, takeover_enabled: bool, cursor_running: bool)`
  - `pub fn capture_process_error(operation: &str, error: &Error)`
  - `pub fn install_panic_hook()`

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn run_failures_map_to_stable_codes() {
    assert_eq!(code_for_run_failure(&RunFailure::Provider("x".into())).as_str(), "CB-PROVIDER-001");
    assert_eq!(code_for_run_failure(&RunFailure::Store("x".into())).as_str(), "CB-DB-001");
    assert_eq!(code_for_run_failure(&RunFailure::Protocol("x".into())).as_str(), "CB-RUN-001");
    assert_eq!(code_for_run_failure(&RunFailure::Client("x".into())).as_str(), "CB-RUN-001");
}
```

```rust
#[tokio::test]
async fn sync_error_is_captured_without_blocking_the_return() {
    let hub = install_test_hub().await;
    capture_sync_error(&Error::Store("database is locked".into()), true, true);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(hub.reports().await.iter().any(|r| r.code == "CB-SYNC-001"));
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p cursor-server --lib diagnostics::`
Expected: FAIL。

- [ ] **Step 3: 实现**

`App::new` 构造 `Arc<DiagnosticsHub>`，`install_global`，并安装 panic hook；`run/engine.rs` 在持久化失败状态后、发送 `RunEvent::Ended` 前调用 `capture_run_outcome`；`CursorHarness::sync_model_cache` 的 Err 分支调用 `capture_sync_error`；`restart_cursor`/`reconcile_cursor_backend` 的 stop/launch 错误调用 `capture_process_error`。所有 capture 只投递 channel，不 await 磁盘。

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p cursor-server --lib diagnostics::`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add server/src/config.rs server/src/app.rs server/src/diagnostics server/src/run/engine.rs server/src/local_app/mod.rs
git commit -m "feat(diagnostics): capture terminal failures"
```

---

### Task 8: 控制面诊断 API

**Files:**
- Create: `server/src/control/diagnostics.rs`
- Create: `server/tests/diagnostics_api.rs`
- Modify: `server/src/control/mod.rs`
- Modify: `server/src/control/service.rs`
- Modify: `server/src/app.rs`

**Interfaces:**
- Consumes: Task 2/5/6/7.
- Produces:
  - `GET /__byok-api__/api/diagnostics/reports -> Vec<ReportSummary>`
  - `GET /__byok-api__/api/diagnostics/reports/{report_id} -> ReportDetail`
  - `POST /__byok-api__/api/diagnostics/reports/{report_id}/issue -> IssuePreview`
  - `POST /__byok-api__/api/diagnostics/reports/{report_id}/dismiss -> 204`
  - `POST /__byok-api__/api/diagnostics/frontend -> 202`
  - `ReportDetail { summary: ReportSummary, issue_body: String, diagnostic_json: serde_json::Value, upload_available: bool }`
  - `IssuePreview { title, body, labels, issue_url, bundle_url, expires_at }`

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test]
async fn issue_endpoint_uploads_once_and_returns_a_prefilled_url() {
    let (router, _hub, _upload) = diagnostics_router_with_mock_upload().await;
    let id = seed_report(&router).await;
    let (status, body) = send(router, "POST", &format!("/__byok-api__/api/diagnostics/reports/{id}/issue"), json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert!(value["issueUrl"].as_str().unwrap().starts_with("https://github.com/ApexGP/cursor-byok/issues/new?"));
    assert!(value["bundleUrl"].as_str().unwrap().contains("/r/"));
}
```

```rust
#[tokio::test]
async fn issue_endpoint_does_not_open_issue_without_upload_config() {
    let (router, _hub, _upload) = diagnostics_router_without_upload().await;
    let id = seed_report(&router).await;
    let (status, _) = send(router, "POST", &format!("/__byok-api__/api/diagnostics/reports/{id}/issue"), json!({})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cargo test -p cursor-server --test diagnostics_api`
Expected: FAIL。

- [ ] **Step 3: 实现**

`ControlService` 持有 `Arc<DiagnosticsHub>`；handler 只做参数解析/错误映射。`/issue` 顺序固定：检查上传配置 → 采集证据 → 生成报告 → 在内存生成 zip → 上传 → 释放 zip → 返回预填 URL。任何一步失败都不返回 `issueUrl`。`/frontend` 只接受 `code`、`message`、`stack`，服务端脱敏后入库。

- [ ] **Step 4: 运行测试，确认通过**

Run: `cargo test -p cursor-server --test diagnostics_api`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add server/src/control server/src/app.rs server/tests
git commit -m "feat(diagnostics): expose reports and issue creation API"
```

---

### Task 9: 前端诊断 API 与状态

**Files:**
- Create: `apps/desktop/src/shared/diagnostics/types.ts`
- Create: `apps/desktop/src/shared/diagnostics/api.ts`
- Create: `apps/desktop/src/shared/diagnostics/store.ts`
- Modify: `apps/desktop/src/shared/api.ts`

**Interfaces:**
- Consumes: Task 8 response shapes.
- Produces:
  - `type DiagnosticsReportSummary`
  - `type DiagnosticsReportDetail`
  - `type DiagnosticsIssuePreview`
  - `listDiagnosticsReports(): Promise<DiagnosticsReportSummary[]>`
  - `getDiagnosticsReport(id: string): Promise<DiagnosticsReportDetail>`
  - `createDiagnosticsIssue(id: string): Promise<DiagnosticsIssuePreview>`
  - `dismissDiagnosticsReport(id: string): Promise<void>`
  - `reportFrontendError(input: { code, message, stack? }): Promise<void>`
  - `useDiagnosticsStore()` 返回 `{ reports, busy, error, refresh, dismiss, createIssue }`

- [ ] **Step 1: 写失败检查**

Run: `npm --prefix apps/desktop run typecheck`
Expected: FAIL，诊断类型/函数不存在。

- [ ] **Step 2: 实现**

复用 `shared/api.ts` 的 `request`；`store.ts` 使用 `useSyncExternalStore`，10 秒轮询，`document.visibilityState === "hidden"` 时跳过；上传成功后清空当前报告并返回 `DiagnosticsIssuePreview`。错误消息不得包含服务端返回的 Token/URL 凭据。

- [ ] **Step 3: 运行检查**

Run: `npm --prefix apps/desktop run typecheck`
Expected: PASS。

- [ ] **Step 4: 提交**

```bash
git add apps/desktop/src/shared/diagnostics apps/desktop/src/shared/api.ts
git commit -m "feat(diagnostics): add desktop diagnostics client"
```

---

### Task 10: 全局入口、预览弹窗与前端错误上报

**Files:**
- Create: `apps/desktop/src/features/diagnostics/DiagnosticsButton.tsx`
- Create: `apps/desktop/src/features/diagnostics/DiagnosticsDialog.tsx`
- Create: `apps/desktop/src/features/diagnostics/DiagnosticsDialog.module.scss`
- Modify: `apps/desktop/src/App.tsx`
- Modify: `apps/desktop/src/shell/AppHeader.tsx`
- Modify: `apps/desktop/src/demo/api.ts`

**Interfaces:**
- Consumes: Task 9 store.
- Produces:
  - `DiagnosticsButton`：有未处理报告时显示红点，点击打开弹窗。
  - `DiagnosticsDialog`：摘要、环境、错误链、Issue 正文/JSON 预览、“上传并创建 Issue”、“忽略”。
  - `DiagnosticsErrorBoundary`：捕获渲染错误并调用 `reportFrontendError`。

- [ ] **Step 1: 写失败检查**

Run: `npm --prefix apps/desktop run typecheck`
Expected: FAIL，组件不存在。

- [ ] **Step 2: 实现 UI**

在 `AppHeader` 的非拖拽 `uiLayer` 放按钮；使用现有 `Modal`；按钮文案固定为“上传并创建 Issue”。上传中禁用按钮；上传失败显示错误并保持报告；成功后调用现有 `api.openExternalUrl(preview.issueUrl)`，提示“已打开 GitHub，请检查并点击 Submit”。在 `App.tsx` 根部包 ErrorBoundary；`window.addEventListener("error"/"unhandledrejection")` 只上报 code/message/stack，并加防递归标记。demo `api.ts` 增加一条固定诊断报告，便于无后端预览。

- [ ] **Step 3: 运行检查**

Run: `npm --prefix apps/desktop run typecheck && npm --prefix apps/desktop run build`
Expected: PASS。

- [ ] **Step 4: 手动检查**

Run: `npm --prefix apps/desktop run build:demo`，打开 demo 的 Cursor 页面，确认全局按钮、弹窗、Issue 正文预览可见，上传按钮在无后端时显示明确错误。

- [ ] **Step 5: 提交**

```bash
git add apps/desktop/src/features/diagnostics apps/desktop/src/App.tsx apps/desktop/src/shell/AppHeader.tsx apps/desktop/src/demo/api.ts
git commit -m "feat(diagnostics): add global report entry and preview"
```

---

### Task 11: Issue 模板与 agent 解析脚本

**Files:**
- Create: `.github/ISSUE_TEMPLATE/diagnostic.yml`
- Create: `support/diagnostics/parse-issue.mjs`
- Create: `support/diagnostics/parse-issue.test.mjs`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: Issue body format from Task 5.
- Produces:
  - `node support/diagnostics/parse-issue.mjs --field errorCode` 从 stdin 提取 JSON 字段。
  - `node support/diagnostics/parse-issue.mjs` 输出紧凑 JSON。
  - 解析失败退出码 2，并输出 `invalid diagnostic block`。

- [ ] **Step 1: 写失败测试**

```javascript
test("extracts the diagnostic json block", () => {
  const body = "<!-- CURSOR_BYOK_DIAGNOSTIC_START -->\n```json\n{\"errorCode\":\"CB-SYNC-001\"}\n```\n<!-- CURSOR_BYOK_DIAGNOSTIC_END -->";
  assert.equal(extractDiagnostic(body).errorCode, "CB-SYNC-001");
});
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `node --test support/diagnostics/parse-issue.test.mjs`
Expected: FAIL。

- [ ] **Step 3: 实现**

用正则提取 marker 之间的内容，剥离 ```json fence，`JSON.parse` 后可选 `--field`；模板 `diagnostic.yml` 要求用户粘贴同一 marker+JSON。CI frontend job 增加 `node --test support/diagnostics/*.test.mjs`。

- [ ] **Step 4: 运行测试，确认通过**

Run: `node --test support/diagnostics/parse-issue.test.mjs`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add .github/ISSUE_TEMPLATE/diagnostic.yml support/diagnostics .github/workflows/ci.yml
git commit -m "feat(diagnostics): add issue template and agent parser"
```

---

### Task 12: 诊断上传服务参考实现

**Files:**
- Create: `support/diagnostics-relay/package.json`
- Create: `support/diagnostics-relay/wrangler.toml`
- Create: `support/diagnostics-relay/src/index.js`
- Create: `support/diagnostics-relay/src/validate.js`
- Create: `support/diagnostics-relay/test/validate.test.mjs`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Consumes: 第 10.2 节上传契约。
- Produces:
  - `POST /v1/reports` → `201 {id,url,expiresAt}`；校验 reportId、SHA-256、zip magic、8 MiB 上限。
  - `GET /r/:id` → `200 application/zip`，`X-Robots-Tag: noindex`；不存在 `404`。
  - `validateReportId(value) -> boolean`
  - `validateZip(bytes) -> boolean`
  - `objectKey(reportId) -> string`

- [ ] **Step 1: 写失败测试**

```javascript
test("rejects non-hex report ids", () => {
  assert.equal(validateReportId("r-20261111-abc"), false);
  assert.equal(validateReportId("r-20261111-0123456789abcdef0123456789abcdef"), true);
});
test("rejects non-zip bodies", () => {
  assert.equal(validateZip(new TextEncoder().encode("not a zip")), false);
  assert.equal(validateZip(new Uint8Array([0x50, 0x4b, 0x03, 0x04])), true);
});
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `node --test support/diagnostics-relay/test/validate.test.mjs`
Expected: FAIL。

- [ ] **Step 3: 实现**

Cloudflare Worker + R2 binding `DIAGNOSTICS_BUCKET`；上传只接受 `application/zip`、`X-Report-Id`、`X-Report-SHA256`，大小 ≤ 8 MiB，拒绝已存在 key（`If-None-Match`）；写入后返回 TTL 30 天。GET 只按合法 id 读取，禁止列表。部署文档写入 `wrangler.toml` 注释；CI 增加 relay 单元测试，不执行部署。

- [ ] **Step 4: 运行测试，确认通过**

Run: `node --test support/diagnostics-relay/test/validate.test.mjs`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add support/diagnostics-relay .github/workflows/ci.yml
git commit -m "feat(diagnostics): add upload relay reference"
```

---

### Task 13: 发布配置、隐私说明与端到端验证

**Files:**
- Modify: `.github/workflows/release.yml`
- Modify: `.github/workflows/ci.yml`
- Modify: `README.md`
- Modify: `README-CN.md`
- Modify: `docs/superpowers/specs/2026-11-11-diagnostics-design.md`（如实现偏离设计）

**Interfaces:**
- Consumes: Task 1-12.
- Produces:
  - Release 构建注入 `CURSOR_DIAGNOSTICS_UPLOAD_URL`（来自 `vars.CURSOR_DIAGNOSTICS_UPLOAD_URL`）。
  - CI 运行 Rust、前端、诊断 parser 和 relay 测试。
  - README 增加“诊断与隐私”短节：默认捕获、用户确认后上传、30 天 TTL、不包含 API Key/正文。

- [ ] **Step 1: 更新 workflow**

在 release 的 tauri-action step 增加：

```yaml
env:
  CURSOR_DIAGNOSTICS_UPLOAD_URL: ${{ vars.CURSOR_DIAGNOSTICS_UPLOAD_URL }}
```

在 CI frontend job 增加：

```bash
node --test support/diagnostics/*.test.mjs
node --test support/diagnostics-relay/test/*.test.mjs
```

- [ ] **Step 2: 运行完整检查**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
npm --prefix apps/desktop run check
node --test support/diagnostics/*.test.mjs
node --test support/diagnostics-relay/test/*.test.mjs
```

Expected: 全部 PASS。

- [ ] **Step 3: 端到端手工验证**

1. 用 mock 上传服务或 `wrangler dev` 配置 `CURSOR_DIAGNOSTICS_UPLOAD_URL`。
2. 注入一个 sync 失败（测试环境可临时让 `state.vscdb` 不可读）。
3. 确认全局按钮出现，预览含错误码、环境、JSON。
4. 点击“上传并创建 Issue”，确认：zip 上传成功、本地没有生成诊断包文件、浏览器打开预填页面。
5. 用 `gh issue view --json body` 或 parser 脚本提取 JSON，确认 schema 合法。
6. 检查 sentinel API Key、消息正文、`cursorAuth` 均不存在。

- [ ] **Step 4: 提交**

```bash
git add .github README.md README-CN.md docs/superpowers
git commit -m "docs(diagnostics): document privacy and release wiring"
```

---

## Self-Review

- Spec coverage：错误码、捕获边界、本地存储、去重、DB allowlist、脱敏、Issue 模板、自动上传、TTL、前端入口、agent 解析、relay、release 配置均有对应任务。
- 类型一致性：`DiagnosticsConfig`、`DiagnosticEvent`、`ReportSummary`、`IssuePreview`、`BundleLink` 在各 Task 使用同一命名。
- Review Focus：超大日志（Task 3/5）、异常密钥（Task 1）、DB 锁定/缺失（Task 4/8）、上传失败（Task 6/8）、正文超限（Task 5）均有测试。
- 未决：生产上传域名由维护者在 release 变量中配置；relay 部署不纳入自动化执行。
