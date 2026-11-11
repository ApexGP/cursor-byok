# cursor-byok 内置诊断与一键反馈设计

- 状态：已确认，进入实施计划
- 日期：2026-11-11
- 适用版本：1.0.5 之后的下一个版本
- 目标读者：实现者、维护者、用 `gh` 读取 Issue 的 agent

## 1. 背景与目标

社区用户没有开发模式，开发者也不总能拿到 Windows/macOS 设备。当前排查依赖用户手动提供日志、截图和口头描述，证据不完整，错误码不统一。

本方案把一次失败转换成**结构化、可复现、机器可读**的 GitHub Issue：

1. 应用自动捕获终态失败，生成稳定错误码。
2. 全局入口提示用户，应用内预览脱敏后的错误摘要、错误链和即将提交的内容。
3. 用户点击“上传并创建 Issue”后，应用生成脱敏诊断包并**自动上传**到项目诊断服务。
4. 诊断包只在内存中生成和上传，成功或失败后立即释放，不在本地落盘；无论用户最终是否在 GitHub 页面点击 Submit。
5. 打开预填好的 GitHub `issues/new` 页面，由用户检查并点击 Submit。
6. Issue 正文固定为“人读区 + 机器读区”，agent 可直接用 `gh issue view --json` 读取。

核心原则：**结构先行、程序填槽、用户确认、默认脱敏、本地不留包**。

## 2. 目标 / 非目标 / 反目标

### 目标

- 无开发模式也能收集可定位问题的证据。
- 错误码稳定、可检索、可聚合。
- Issue 结构同时适合人和 agent 阅读。
- 诊断包自动上传，应用不依赖用户 GitHub 登录或 Token。
- 数据库证据只取相关表/字段，绝不整库上传。
- 诊断采集不拖慢 Agent Run 热路径。

### 非目标

- 不做自动创建 Issue（用户仍点 Submit）。
- 不做持续遥测、后台上报或崩溃统计平台。
- 不收集完整 prompt、模型响应、消息正文、文件内容。
- 不把 API Key、Authorization、Cursor 登录凭据写入任何报告。
- 不自动修改用户 Cursor 状态；诊断只读。

### 反目标

- 程序自由生成 Issue 正文，导致格式漂移、agent 无法解析。
- 为图省事上传整库 `cursor-byok.db` 或 `state.vscdb`。
- 每次错误都弹阻塞窗口，干扰正常使用。
- 上传失败时把诊断包留在本地目录。

## 3. 约束

- 平台：Windows、macOS、Linux；Windows 是首要排查对象。
- 客户端只使用本地 HTTP 控制面 `/__byok-api__/api/*`。
- 不引入 GitHub Token；不要求用户登录 GitHub。
- 诊断包只在用户点击“上传并创建 Issue”后生成；只在内存中存在，不写本地文件。
- 上传成功或失败后都必须释放内存中的诊断包。
- 诊断数据与主数据库解耦：不写 `cursor-byok.db` 热路径。
- 所有输出在写入本地或上传前完成脱敏。
- Issue 正文预填长度有上限，超出部分只进入诊断包。

## 4. 用户流程

```text
终态失败
  → 生成错误码 + 报告事件（内存 + 受限本地事件日志）
  → 全局按钮出现“检测到问题”
  → 用户打开诊断面板
      → 摘要 / 错误链 / 环境 / Issue 预览 / 诊断 JSON 预览
      → 显示“将上传脱敏诊断包，30 天后自动删除；本机不留副本”
  → 点击“上传并创建 Issue”
      → 在内存生成 zip
      → 上传到项目诊断服务
      → 上传结束后释放内存 zip
      → 打开 GitHub issues/new 预填页面
  → 用户检查正文、点 Submit
```

如果用户放弃 Submit：诊断包仍在服务端，按 TTL 自动删除；本地无副本。如果上传失败：不打开 Issue，不保留 zip，提示重试；用户退出诊断面板后事件仍保留（脱敏、受限）以便重新生成。

## 5. 错误码与捕获边界

错误码格式：`CB-<AREA>-<NNN>`，只新增、不复用、不改变含义。

| 错误码 | Area | 触发 | 默认级别 |
| --- | --- | --- | --- |
| `CB-APP-001` | app | 启动初始化失败 | fatal |
| `CB-APP-002` | app | panic | fatal |
| `CB-DB-001` | db | 迁移/完整性/数据库不可用 | error |
| `CB-CURSOR-001` | cursor | stop/launch/restart Cursor 失败 | error |
| `CB-CURSOR-002` | cursor | Cursor state 读取失败或锁定 | warning |
| `CB-SYNC-001` | sync | 模型目录同步失败 | error |
| `CB-SYNC-002` | sync | 同步写入后回读不一致 | error |
| `CB-RUN-001` | run | Agent Run 终态失败 | error |
| `CB-RUN-002` | run | RunSSE 首包/断线恢复耗尽 | error |
| `CB-PROVIDER-001` | provider | provider 重试耗尽 | error |
| `CB-PROVIDER-002` | provider | stream 空闲超时 | error |
| `CB-TOOL-001` | tool | 工具轮次终态失败 | error |
| `CB-UI-001` | ui | React Error Boundary | error |
| `CB-UI-002` | ui | 未处理 Promise/全局错误 | error |
| `CB-UPDATE-001` | update | 更新检查/安装失败 | warning |

捕获点只放在终态边界：Run 结束、provider 重试耗尽、sync 返回错误、进程控制失败、启动/迁移失败、panic、前端边界。不要在每次重试、每条流事件或每个 SSE 分片生成报告。

同一 `fingerprint = sha256(errorCode + normalized message + top frame/area)` 在 10 分钟内聚合，只保留一个报告并累加 `occurrences`。

## 6. 报告模型

### 6.1 Rust 侧

```rust
pub struct DiagnosticEvent {
    pub report_id: String,
    pub code: DiagnosticCode,
    pub severity: Severity,
    pub captured_at: DateTime<Utc>,
    pub summary: String,
    pub error_kind: Option<String>,
    pub error_message: Option<String>,
    pub error_chain: Vec<String>,
    pub fingerprint: String,
    pub context: DiagnosticContext,
}
```

`DiagnosticContext` 只包含 allowlist 字段：`area`、`operation`、`run_id`、`request_id`、`conversation_id`、`model_hash`、`provider_type`、`http_status`、时间戳、布尔状态。禁止把请求体、响应体、消息内容、Header 放进去。

### 6.2 Issue 中的 JSON（schemaVersion 1）

```json
{
  "schemaVersion": 1,
  "reportId": "r-20261111-0123456789abcdef0123456789abcdef",
  "errorCode": "CB-SYNC-001",
  "severity": "error",
  "capturedAt": "2026-11-11T12:34:56Z",
  "app": {
    "version": "1.0.6",
    "platform": "windows",
    "arch": "x86_64",
    "channel": "release"
  },
  "cursor": {
    "version": "0.48.6",
    "installKind": "local"
  },
  "error": {
    "kind": "model_cache_sync_failed",
    "message": "database is locked",
    "chain": ["...", "..."],
    "fingerprint": "sha256:..."
  },
  "context": {
    "area": "sync",
    "operation": "sync_model_cache",
    "runId": null,
    "requestId": null,
    "conversationIdHash": null,
    "modelHash": null,
    "takeoverEnabled": true,
    "cursorRunningAtSync": true
  },
  "timeline": [
    { "at": "2026-11-11T12:34:50Z", "event": "sync_started", "detail": "..." }
  ],
  "logs": {
    "tail": ["..."],
    "truncated": false
  },
  "bundle": {
    "url": "https://diag.example/r/r-20261111-...",
    "sha256": "...",
    "expiresAt": "2026-12-11T12:34:56Z"
  },
  "database": {
    "cursorByok": {
      "schemaVersion": 11,
      "file": {
        "bytes": 0,
        "walBytes": 0,
        "shmBytes": 0,
        "freelistPages": 0,
        "journalMode": "wal"
      },
      "settings": {},
      "run": null,
      "llmCalls": []
    },
    "cursorState": {
      "exists": true,
      "backupAgeMs": 0,
      "catalogIds": [],
      "syncedIds": [],
      "overrideEnabled": [],
      "overrideDisabled": [],
      "truncated": false
    }
  },
  "redaction": {
    "applied": ["api_key", "authorization", "bearer", "email", "user_path", "prompt_body"],
    "omitted": ["messages.content", "llm_call_requests.body", "state.vscdb.auth"],
    "notes": ["diagnostic bundle is redacted before upload"]
  }
}
```

字段规则：

- 数组默认最多 20 项；超限时截断并置 `truncated: true`。
- `logs.tail` 最多 200 行、合计 16 KiB；Issue 正文里最多 40 行。
- 所有字符串先脱敏再截断。
- `reportId` 为 `r-YYYYMMDD-<32 hex>`，客户端生成；服务端只接受该格式。

## 7. 数据库证据 allowlist

诊断模块使用只读连接和显式列名，不执行 `SELECT *`。

### 7.1 `cursor-byok.db`

允许：

- `_sqlx_migrations`：`version`、`description`、`success`。
- SQLite 文件统计：主库/WAL/SHM 字节数、`page_count`、`freelist_count`、`journal_mode`。
- `service_settings` 固定键：`cursor_takeover_enabled`、`default_model`、`llm_detailed_logging`、`commit_settings`（prompt 只保留长度与哈希）、`proxy`（只保留 mode，不含地址/用户名/密码）、`tab`（只保留 mode）。
- `model_configs`：`model_hash`、`model_type`、`request URL` 的 scheme/host/path（不含 query）、`context_window_tokens`、`max_*`、`created_at_ms`、`updated_at_ms`。**禁止 `api_key`、`custom_headers`、`extra_params`。**
- `runs`：`run_id`、`status`、`run_kind`、`subagent_kind`、`provider_call_index`、`failure_category`、脱敏后的 `failure_summary`、时间戳。
- `tool_rounds`：`round_id`、`status`、`version`、`next_completion_seq`、时间戳。
- `tool_round_calls`：`call_id`、`name`、`status`、`result_is_error`、`completed_at_ms`。**禁止 `arguments_json`、`result_content`。**
- `llm_calls`：`call_id`、`run_id`、`provider_call_index`、`model_hash`、`provider_type`、`request_type`、`model_id`、`status`、`finish_reason`、所有耗时/队列/token/字节/事件计数字段、`http_status`、`error_kind`、脱敏后的 `error_message`、`reasoning_effort`、`fast`。**禁止请求和响应正文。**
- `messages`：只按 role 计数，不取内容。
- `conversation_checkpoints`：只取数量和时间范围，不取 state/messages。

禁止：

- `llm_call_requests`、`llm_call_response_chunks`、`cursor_run_trace_artifacts` 的原始内容。
- `model_configs.api_key`、headers、extra params。
- 完整数据库文件、WAL、SHM。

### 7.2 Cursor `state.vscdb`

只读提取以下内容，不复制文件：

- `applicationUser.availableDefaultModels2[].name`
- `applicationUser.aiSettings.modelOverrideEnabled`
- `applicationUser.aiSettings.modelOverrideDisabled`
- `applicationUser.aiSettings.userAddedModels`
- `applicationUser.cursorByokModelIds`
- 备份文件 `state.vscdb.cursor-byok.bak*` 是否存在及最后修改时间

禁止：`cursorAuth/*`、`cursorCreds`、完整 `applicationUser`、完整 `state.vscdb`。

### 7.3 查询失败

数据库被锁、文件不存在、字段缺失时记录 `dbUnavailable` 和错误摘要，不阻断报告生成。DB 证据在用户打开报告或点击上传时采集，不在失败热路径执行。

## 8. 脱敏规则

在写入本地事件日志、生成 Issue、生成诊断包之前统一执行：

- `Authorization: Bearer ...`、`api_key=...`、`token=...`、`password=...`、`secret=...` 替换为 `[REDACTED]`。
- 常见 key 前缀：`sk-`、`xai-`、`gsk_`、`AIza`、JWT（`eyJ...`）整体替换。
- URL query 中的 `key`、`token`、`api_key`、`secret` 值替换。
- Windows `C:\Users\<name>` → `%USERPROFILE%`；macOS `/Users/<name>` → `~`。
- 邮箱替换为 `[email]`。
- 不记录请求体/响应体/消息正文/文件内容；必要时只记录长度和哈希。
- 预览页展示最终 Issue 正文和 JSON；用户可在 GitHub 页面上继续编辑。

## 9. 采集、存储与去重

- 目录：`~/.cursor-byok-v3/diagnostics/`，Unix 权限 0700，文件 0600。
- `events.jsonl`：只写脱敏后的事件；最大 5 MiB、最多 200 条报告、保留 7 天，启动时清理。
- 内存中保留最近 200 条事件用于聚合和实时 UI。
- `dismissed.json`：按 fingerprint 记录忽略时间；默认 24 小时内不再提示，严重级别升级时重新提示。
- 同一 fingerprint 10 分钟内合并，累加 `occurrences`，更新 `lastSeen`。
- 事件写入通过独立异步 writer，失败只记录日志，不阻塞主流程。
- panic 使用 hook 写 `crash-<timestamp>.json`；下次启动读取并转为报告。

## 10. 诊断包与自动上传

### 10.1 内容

zip 包（只在内存 buffer 中存在，不写磁盘）：

```text
manifest.json          # 文件列表、schemaVersion、reportId、生成时间、SHA-256
report.json            # 完整脱敏报告（比 Issue 正文更全）
logs/tail.log          # 时间窗口内脱敏日志，最大 1 MiB
evidence/database.json # 第 7 节 allowlist 证据
evidence/cursor-state.json
```

### 10.2 上传契约

请求：

```http
POST {CURSOR_DIAGNOSTICS_UPLOAD_URL}
Content-Type: application/zip
X-Report-Id: r-YYYYMMDD-<32 hex>
X-Report-SHA256: <zip sha256>
X-Schema-Version: 1
X-App-Version: 1.0.6
X-Platform: windows
```

响应 `201`：

```json
{
  "id": "r-20261111-...",
  "url": "https://diag.example/r/r-20261111-...",
  "expiresAt": "2026-12-11T12:34:56Z"
}
```

约束：

- 最大 8 MiB；超限先裁剪日志，再拒绝。
- 上传超时 30 秒；失败不重试超过一次，避免重复包。
- 服务端只接受 zip magic、允许的文件名、最大文件数；不解压执行。
- 服务端保存 30 天；未提交 Issue 也按 TTL 删除。
- zip 只在内存生成；上传函数返回后 buffer 立即释放，成功和失败都不写本地文件。
- 上传成功后把 `bundle.url`、`sha256`、`expiresAt` 写入 Issue JSON，再打开预填页面。

### 10.3 隐私

- 上传前必须展示预览并让用户点击“上传并创建 Issue”；错误捕获本身不上传。
- 服务端 URL 使用 128 位随机 reportId，不可枚举；禁止目录列表。
- 不在客户端嵌入 GitHub Token。
- 诊断包是脱敏后的结构化子集，不包含第 7 节禁止内容。
- v1 使用 TLS + 不可猜测 URL + TTL；后续可在不改变契约的情况下增加客户端加密。

## 11. Issue 契约

### 11.1 标题

```text
[CB-SYNC-001] Windows 模型目录同步失败
```

错误码必须来自第 5 节；摘要最多 80 字。

### 11.2 正文骨架

```markdown
<!-- cursor-byok-diagnostic:v1 -->
## 概要
{{summary}}

## 环境
| 项 | 值 |
| --- | --- |
| cursor-byok | 1.0.6 |
| Cursor | 0.48.6 |
| OS | Windows 11 23H2 |
| 模块 | sync |

## 发生了什么
{{what_happened}}

## 复现步骤
{{reproduction}}

## 诊断数据
<!-- CURSOR_BYOK_DIAGNOSTIC_START -->
```json
{{diagnostic_json}}
```
<!-- CURSOR_BYOK_DIAGNOSTIC_END -->

## 诊断包
{{bundle_url}}（{{expires_at}} 过期）

## 脱敏与缺失说明
{{redaction_notes}}
```

约束：

- 正文预填上限 1500 字节；JSON 必须保持合法，超限时先裁剪 `logs.tail`、`timeline`、数据库数组，并置 `truncated: true`。**Why**：GitHub 在预填 URL 约 7000 字符时返回 500，percent-encoding 最多把 1 个 UTF-8 字节扩成 3 个字符；完整证据仍在诊断包中。
- 标题最多 120 字符。
- labels：`diagnostics`、`auto-report`、`platform:<os>`、`area:<area>`、`severity:<level>`。缺失的 label 不影响正文解析。
- agent 用 `gh issue view <n> --json title,body,labels`，取两个 marker 之间的 JSON。

### 11.3 人工入口

`.github/ISSUE_TEMPLATE/diagnostic.yml` 提供同样的字段：概要、环境、复现步骤、诊断 JSON。人工提交也要遵守 markers 和 JSON schema。

## 12. 架构与文件

### 12.1 Server

```text
server/src/diagnostics/
├── mod.rs          # 初始化、全局 hub、record/capture 辅助
├── code.rs         # DiagnosticCode、severity、area、标题
├── event.rs        # DiagnosticEvent、DiagnosticContext、fingerprint
├── redact.rs       # 脱敏规则与测试
├── store.rs        # 内存 ring buffer、events.jsonl、dismissed、清理
├── logs.rs         # 读取时间窗口日志并脱敏
├── evidence.rs     # 环境、进程、文件、Cursor 版本等基础证据
├── db_evidence.rs  # cursor-byok.db allowlist 查询
├── cursor_state.rs # state.vscdb 只读提取
├── report.rs       # 报告组装、JSON、Issue 正文、截断
├── bundle.rs       # 内存 zip 生成、SHA-256、上传后释放
└── upload.rs       # 上传客户端与配置
```

- `App::new` 初始化 `DiagnosticHub`，提供全局 `diagnostics::record(...)`；未初始化时 no-op。
- `ControlService` 提供诊断查询/上传方法。
- 采集点：
  - `run/engine.rs`：`RunOutcome::Failed` 映射 `CB-RUN-001`/`CB-PROVIDER-001`/`CB-DB-001`。
  - `local_app/model_cache.rs` 或 `local_app/mod.rs`：sync 错误 `CB-SYNC-001`。
  - `local_app/process.rs`/`local_app/mod.rs`：stop/launch 失败 `CB-CURSOR-001`。
  - `store/migrations.rs`/启动：`CB-APP-001`/`CB-DB-001`。
  - panic hook：`CB-APP-002`。
  - 前端错误边界：`CB-UI-001`/`CB-UI-002`。

### 12.2 控制面

```text
GET  /__byok-api__/api/diagnostics/reports
GET  /__byok-api__/api/diagnostics/reports/{report_id}
POST /__byok-api__/api/diagnostics/reports/{report_id}/issue
POST /__byok-api__/api/diagnostics/reports/{report_id}/dismiss
POST /__byok-api__/api/diagnostics/frontend
```

`/issue` 执行：采集 DB 证据 → 生成 Issue 正文/JSON → 在内存生成 zip → 上传 → 释放 zip → 返回 `{title, body, labels, issueUrl, bundleUrl, expiresAt}`。如果上传 URL 未配置，返回明确配置错误，不打开 Issue。

### 12.3 Desktop

```text
apps/desktop/src/shared/diagnostics/
├── api.ts        # 控制面请求
├── store.ts      # 报告列表、轮询、忽略、上传状态
└── types.ts      # ReportSummary/IssuePreview
apps/desktop/src/features/diagnostics/
├── DiagnosticsButton.tsx
├── DiagnosticsDialog.tsx
└── DiagnosticsDialog.module.scss
```

- 全局按钮放在 `AppHeader` 右侧非拖拽层；有未读报告时显示红点。
- 轮询 10 秒；页面不可见时暂停。
- `App.tsx` 根部加 ErrorBoundary 和全局 `error`/`unhandledrejection` 上报。
- 预览弹窗复用现有 `Modal`，不阻塞主页面。

## 13. 失败处理

- 诊断模块自身失败绝不递归上报；只写普通日志。
- 上传失败：释放内存包，保留事件，提示“上传失败，可重试”；不打开 GitHub。
- DB 证据查询失败：写入 `dbUnavailable`，继续生成报告。
- 日志读取失败：`logs.tail` 为空并记录原因。
- Issue 正文超限：确定性裁剪，不产生非法 JSON。
- 服务端 TTL 清理失败：由 R2 生命周期兜底；不在客户端重试删除。

## 14. 测试与验收

必须覆盖：

- 错误码字符串稳定，不复用。
- 脱敏：API Key、Bearer、JWT、query token、邮箱、用户路径；sentinel 值绝不出现在输出。
- DB allowlist：向 `model_configs`/`messages`/`llm_call_requests` 写入 sentinel，报告与诊断包中不出现。
- `state.vscdb` 提取：只含 catalog/override/synced ids，不含 `cursorAuth`/`cursorCreds`。
- Issue 正文：markers、合法 JSON、标题/labels、长度上限、截断标志。
- 上传：mock 服务返回 201/500/超时；成功和失败后本地都不存在 zip 文件。
- 诊断包：文件清单、SHA-256、大小上限。
- 去重：同 fingerprint 聚合，dismiss 后 24 小时内不提示。
- 采集：模拟 Run 失败、sync 失败、panic、前端错误，报告能出现。
- 前端：typecheck、build、诊断弹窗在 demo/手动流程中可用。
- agent：`gh issue view --json body` 提取 JSON，JSON Schema 校验通过。

## 15. 发布与运维

- Release 配置 `CURSOR_DIAGNOSTICS_UPLOAD_URL`；未配置时隐藏/禁用上传按钮。
- 诊断服务使用独立域名与存储桶，30 天生命周期；限制 8 MiB、10 次/小时/IP。
- 诊断端点不做登录，依赖客户端脱敏、随机 URL、TTL 和限流；如滥用，升级为签名/加密方案。
- 首期默认开启捕获，但只有用户点击才上传。
- 发布说明中增加隐私说明入口。

## 16. 后续硬化（不在 v1）

- 客户端 age 公钥加密诊断包，服务端只存密文。
- 报告指纹趋势与维护者聚合脚本。
- 诊断上传的签名令牌/Proof of Work。
- 把诊断事件接入维护者本地看板。

## 17. 已由本设计决定的事项

- Issue 使用固定模板，程序只填槽；不允许自由生成正文。
- 上传发生在用户点击“上传并创建 Issue”时；未点击不上传。
- 本地不保留 zip；上传成功/失败都删除。
- 数据库证据只取 allowlist 子集，不取整库。
- 诊断服务地址通过配置注入；不在源码硬编码生产域名。
- 诊断事件本地保留 7 天/5 MiB，用户可忽略报告。

## 18. 留给实现者的决定

- 诊断服务具体部署（Cloudflare Worker + R2 参考实现）与生产域名。
- UI 是弹窗还是右侧抽屉；优先复用现有 Modal。
- 日志窗口默认 ±2 分钟/200 行，可按实测调整。
- 是否在首版就加客户端加密；当前设计允许后续无契约变更地追加。
