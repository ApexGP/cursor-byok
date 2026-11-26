# Contributing to cursor-byok

cursor-byok is a local model gateway for Cursor. It accepts Cursor Agent
traffic on the machine, projects it into OpenAI- or Anthropic-compatible
requests, and keeps Cursor's tools, MCP servers, skills, and conversation state
working.

This document is the contributor guide: development setup, architecture,
change scope, tests, and review expectations. [AGENTS.md](./AGENTS.md) contains
the binding repository conventions and takes precedence when the two disagree.
For vulnerability reporting and the security model, read
[SECURITY.md](./SECURITY.md).

## Start here

- Read [AGENTS.md](./AGENTS.md), this file, and [SECURITY.md](./SECURITY.md)
  before changing code.
- Search existing issues and pull requests before starting. For a large change,
  open an issue or draft PR first so the approach can be agreed before the
  implementation hardens.
- Keep one capability per pull request. If a change contains independent
  pieces, open stacked pull requests instead of one large mixed branch. Each PR
  must be reviewable, testable, and revertable on its own.
- Do not mix unrelated refactors, formatting sweeps, dependency upgrades,
  version bumps, or release changes into a feature PR.
- Rebase or merge the latest `main` before the final review. Resolve conflicts
  in the branch that owns the change, not in the reviewer's checkout.
- If an agent wrote or assisted with the change, disclose it in the PR body and
  make the agent follow this document. Agent output is held to the same scope,
  test, and evidence requirements as human output.

## Development environment

### Requirements

- Rust stable toolchain with `rustfmt` and `clippy`
- Node.js 22 and npm
- Tauri 2 system dependencies
- Docker only when building the container image

CI uses `dtolnay/rust-toolchain@stable`; the repository does not pin a
toolchain. Run `rustup update stable` before claiming that formatting, Clippy,
or tests pass. A stale local toolchain can pass while CI fails on a newer
Clippy lint.

### Setup

```bash
npm --prefix apps/desktop install
```

### Commands

| Command | Purpose |
| --- | --- |
| `make dev-web` | Run the desktop frontend with Vite |
| `make dev-server` | Run the local Rust service |
| `make dev-desktop` | Run the Tauri desktop application |
| `make check` | Format, Clippy, workspace tests, frontend check, release-script tests |
| `make build-web` | Build the frontend |
| `make build-server` | Build the local service |
| `make build-desktop` | Build Tauri installers |
| `make build-docker` | Build the container image |

`make check` is the minimum local gate: formatting, workspace Clippy and tests,
the frontend check, and release-script tests. CI repeats those on Linux and
additionally runs:

- macOS and Windows: platform Clippy for the desktop crate and native SQLite
  storage tests.
- Frontend job: `support/diagnostics` parser tests and
  `support/diagnostics-relay` tests.
- Go tests for `support/cursor-capture` and
  `support/cursor-protocol-extractor` are not covered by CI. Run them locally
  when touching those tools.

CI must be green on Linux, macOS, and Windows before merge. A local run is not
evidence of CI success.

## Architecture

The directory structure is the architecture. Put new behavior in the layer that
owns it; do not start from a convenient file and add a shortcut across layers.

```text
cursor-byok/
├── apps/desktop/                  # React dashboard and Tauri shell
│   ├── src/
│   │   ├── features/              # dashboard, models, calls, settings
│   │   ├── shell/                 # window frame and page layout
│   │   ├── shared/                # UI, virtual list, state, API, diagnostics
│   │   ├── i18n/                  # localization runtime and catalogs
│   │   └── styles/                # theme and typography tokens
│   └── src-tauri/                 # desktop lifecycle, embedded server, updater
├── server/
│   ├── src/
│   │   ├── api/cursor/            # Cursor Connect/protobuf surface
│   │   ├── cursor/
│   │   │   ├── transport/         # request_id routing and ordered output
│   │   │   ├── conversation/      # conversation actor, runs, message delivery
│   │   │   ├── compile/           # action, message, context, model compilation
│   │   │   ├── checkpoint/        # durable conversation state
│   │   │   ├── tools/             # dispatch, wire codec, completion gate
│   │   │   ├── protocol/          # framing and server events
│   │   │   ├── prompting/         # prompt assets, catalog, compilation
│   │   │   └── services/          # account, analytics, catalog, usage, sync
│   │   ├── run/                   # reusable agent loop
│   │   ├── provider/              # Responses, Chat Completions, Anthropic
│   │   ├── model/                 # domain types
│   │   ├── store/                 # SQLite repositories, migrations, storage
│   │   ├── control/               # local management API
│   │   ├── diagnostics/           # capture, redaction, reports, upload
│   │   ├── local_app/             # proxy/CA, Cursor process, catalog sync
│   │   └── search/                # web and Semble search
│   ├── prompt/                    # embedded prompt assets and tool catalog
│   ├── migrations/                # forward-only SQLite migrations
│   └── tests/                     # cross-module integration tests
├── crates/semble-core/            # local code index and search core
├── protocols/cursor/              # Cursor protocol source of truth
├── support/                       # capture, extraction, diagnostics, benchmarks
├── Cargo.toml
└── Makefile
```

### Dependency direction

```text
api/cursor
  └── cursor/{transport, conversation, compile, checkpoint, tools, protocol,
              services}
        ├── run ──→ {model, provider, store}
        ├── provider ──→ model
        ├── store ──→ model
        └── model
```

`run`, `provider`, `store`, and `model` must not depend on `cursor`. The generic
agent loop must stay usable without the Cursor protocol.

There are two API surfaces:

- **Cursor surface** (`server/src/api/cursor/`): protobuf/Connect, stateful by
  `request_id` and `conversation_id`, called by the Cursor client.
- **Control plane** (`/__byok-api__/api/*`, `server/src/control/`): JSON,
  stateless protocol-wise, called only by the desktop UI.

The external `/byok/v1` API and the former plugin runtime were removed. Do not
reintroduce a second model registry or an unauthenticated compatibility proxy.

### Request lifecycle

```text
BidiAppendRequest
  → TransportRegistry (request_id)
  → OrderedInbox (append_seqno)
  → compile::action (Ignore / InsertMessages / BreakMessages)
  → ConversationRuntime (conversation_id, sole owner of current_run)
  → RunEngine (model cycle / tool round / message append / compaction)
  → Provider + ToolDispatcher
  → RunEvent
  → ConversationOutput
      ├── protocol/events → OutputHub → RunSSE
      └── checkpoint/steps → CheckpointWorker → Store
```

Only `ConversationRuntime` may cancel `current_run`, end a tool, send the
terminal event, close `OutputHub`, or remove the `request_id` route. Central
ownership is what makes “exactly one terminal state” possible.

### Message delivery

| Run phase | `Ignore` | `InsertMessages` | `BreakMessages` |
| --- | --- | --- | --- |
| Before start | Drop | Initial messages | Initial messages |
| Running | Drop | Append after the current cycle; no new run | Cancel the current cycle, append, re-enter the model cycle |
| Finalizing | Drop | Queue as pending | Queue as pending |
| Ended | Drop | Start the next run | Start the next run |

A message whose `target_run_id` is not the current run is ignored as
`StaleTarget`. This prevents old or replayed client messages from polluting a
new run.

### Run states

`RunEngine` is `Running`, `Finalizing`, or `Ended`. `Running` accepts inserts,
breaks, tool results, and cancellation. `Finalizing` stops starting new work,
queues later inserts and breaks as pending, commits the final message, waits for
the checkpoint barrier, and returns `RunClosing`. `Ended` returns `RunEnded`.
`CommandResult` distinguishes `Applied`, `Duplicate`, `RunClosing`, `RunEnded`,
and `StaleTarget` so callers know whether to retry against the next run.

### Checkpoint and prefix stability

1. Provider-visible history is append-only. Without compaction, existing
   messages are never edited, removed, or reordered.
2. Checkpoint roots may only be appended to or replaced as a whole system root.
   Never insert or rewrite roots in the middle of history.
3. Previously published step BlobIDs must remain a prefix of the new
   checkpoint.
4. History must end with a user message. If it ends with an assistant message,
   append a provider-visible `runtime:continue` that is not persisted as a user
   turn.
5. Compaction is an explicit reset of checkpoint roots, not in-place deletion of
   individual messages.

Cursor's prompt cache is prefix-based. Any rewrite makes requests slower and
more expensive; a reconnect relies on the checkpoint being a replayable
snapshot. Assistant messages and their tool results are committed as a pair.
Update prefix digests only when the prompt or checkpoint contract intentionally
changes, and explain why in the PR.

### Tools

Tool dispatch and tool results are separate modules. Dispatch sends the
instruction to the Cursor client; `tool_call_result` consumes the terminal
result exactly once. The completion gate owns call-id correlation across turns.
A tool result is truncated once at the `ToolCompletion` boundary so the model
and the UI see the same content. Interrupted tool calls still write a paired
interrupted result. Tool argument errors become tool errors, not failed runs.

MCP tools join the catalog dynamically; duplicate names are rejected. Subagents
are separate runs linked by `parent_run_id` and `parent_tool_call_id`, and a
subagent cannot nest another Task.

### Storage

SQLite writes are serialized through a single writer with `BEGIN IMMEDIATE`.
Migrations are forward-only:

1. Never modify, rename, reorder, or delete an applied migration; SQLx records
   its checksum and startup will fail.
2. Add the next numbered migration. An initial migration and a later migration
   must not add the same column twice.
3. No compatibility views, triggers, shadow columns, or fallback reads.
4. Use explicit column names; do not use `SELECT *`.
5. Represent booleans as `INTEGER CHECK (x IN (0, 1))` and add indexes only for
   real query shapes.

Migration files must keep a consistent line ending; the loader accepts the
historical LF and CRLF checksums for already-applied migrations. Do not rewrite
a published migration to normalize it.

### Desktop and updater

The desktop binds the service to loopback, embeds the frontend under
`/__byok-api__/`, and keeps a minimal Tauri capability and CSP surface. The
control plane is a local management API; it has no multi-user authentication and
must not be exposed to untrusted networks.

There are three update manifests with different formats: Tauri `latest.json`,
legacy `update.json`, and Windows portable `portable-latest.json`. Do not mix
them. Releases are owner-gated and use signed or checksummed artifacts; version
bumps and release changes are maintainer work, not part of a feature PR.

### Diagnostics

Capture only terminal failures: run outcomes, sync errors, Cursor stop/launch
errors, panics, and frontend boundaries. Do not capture every retry or stream
event. Captured text passes through the redactor before it is stored, included
in a report, or uploaded. Database evidence uses an allowlisted, read-only query
set. Bundles are generated in memory, uploaded only after the user confirms,
and deleted by the relay when no issue is submitted.

When adding a diagnostic field, update the allowlist, the redaction tests, and
the issue JSON contract together. Never add message bodies, request/response
bodies, credentials, or full database files.

## Change guidelines

### Find the owning layer

| Change | Primary location |
| --- | --- |
| Cursor wire behavior or protocol action | `server/src/api/cursor/`, `server/src/cursor/` |
| Agent loop, retries, compaction | `server/src/run/`, `server/src/provider/` |
| Model or provider configuration | `server/src/model/`, `server/src/provider/` |
| Persistence or query behavior | `server/src/store/` plus a new migration |
| Local Cursor integration | `server/src/local_app/` |
| Desktop UI or Tauri lifecycle | `apps/desktop/` |
| Prompt text, modes, or tool catalog | `server/prompt/`, `server/src/cursor/prompting/` |
| Diagnostics | `server/src/diagnostics/`, `apps/desktop/src/shared/diagnostics/` |
| Repository support tooling | `support/` |

### Tests

- Put unit tests next to the code they exercise.
- Put cross-module behavior in `server/tests/`: request lifecycle, message
  delivery, tool rounds, checkpoints, prefix stability, compaction, diagnostics,
  and protocol framing.
- Cover both the normal path and the error path. A test that asserts nothing, or
  only that a function returned `Ok`, is not a test.
- Add `#[cfg(windows)]` coverage when behavior is platform-specific.
- Update `server/tests/prefix_stability.rs` only when the prompt or checkpoint
  contract intentionally changes.
- For protocol changes, add an integration test at the transport or Connect
  layer, not just a unit test of a helper.
- For UI changes, verify the production build and describe the manual check.

### Commit and pull request expectations

- Keep commits logically scoped; each commit should compile and keep the tests
  it touches passing.
- Open one PR per capability. Split dependent work into stacked PRs with the
  base branch named in the description. “Many commits” is not itself a problem;
  “many unrelated capabilities in one PR” is.
- Include in the PR description:
  - the problem and the user-visible behavior before/after;
  - the scope and explicit non-goals;
  - the tests run, with the exact commands and result;
  - risk, rollout, and rollback;
  - screenshots or recordings for UI changes.
- Rebase or merge the latest `main` before requesting final review.
- A maintainer may ask for a PR to be split or trimmed. Red CI is not ready for
  review, regardless of local results.
- Do not include generated secrets, local databases, capture files, signing
  keys, or personal paths in commits.

### Documentation

- Update `README.md` and `README-CN.md` for user-facing behavior.
- Update this file for setup, architecture, or review-process changes.
- Update [SECURITY.md](./SECURITY.md) when the trust boundary changes.
- Keep contributor guidance in tracked files; do not rely on untracked local
  process notes.
