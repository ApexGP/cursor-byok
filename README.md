<div align="center">

# cursor-byok

**Use your own model APIs in Cursor — locally, with Agent tools intact.**

cursor-byok is an open-source local model gateway for Cursor. It connects Cursor to the OpenAI- and Anthropic-compatible services you configure, while keeping tool calling, Skills, MCP, and multi-turn Agent workflows available.

[简体中文](./README-CN.md) · [User Guide](https://docs.leokun.cn) · [Download](https://github.com/ApexGP/cursor-byok/releases/latest) · [Report an Issue](https://github.com/ApexGP/cursor-byok/issues)

[![Release](https://img.shields.io/github/v/release/ApexGP/cursor-byok?style=flat-square)](https://github.com/ApexGP/cursor-byok/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/ApexGP/cursor-byok/total?style=flat-square)](https://github.com/ApexGP/cursor-byok/releases)
[![License](https://img.shields.io/github/license/ApexGP/cursor-byok?style=flat-square)](./LICENSE)
[![Platforms](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey?style=flat-square)](https://github.com/ApexGP/cursor-byok/releases/latest)

</div>

> [!NOTE]
>
> The upstream project [leookun/cursor-byok](https://github.com/leookun/cursor-byok) is archived. This repository is continuously maintained by [ApexGP](https://github.com/ApexGP) and is no longer part of the original GitHub fork network. The maintainer is continuing to investigate and fix [issues](https://github.com/leookun/cursor-byok/issues) inherited from upstream and to evaluate [pull requests](https://github.com/leookun/cursor-byok/pulls) that were not merged upstream. Use this repository for downloads, automatic updates, issue reports, and contributions.
>
> Thanks to the [original author](https://github.com/leookun) and [all contributors](https://github.com/leookun/cursor-byok/graphs/contributors).

![Connect cursor-byok to a wide range of model APIs](./images/en-brand-1.png)

![cursor-byok dashboard](./images/en-home-1.png)

## Why cursor-byok

Cursor is powerful, but its model choices are tied to the platform. cursor-byok runs a service on your machine and routes Cursor Agent requests to the providers, models, and credits you already use.

- **Bring your own model channels.** Configure the endpoint, credentials, model ID, context window, reasoning effort, headers, and extra request parameters yourself.
- **Keep Cursor Agent capabilities.** Tool calling, Skills, MCP, and multi-turn conversations continue to work through the local gateway.
- **Stay in control.** API keys, model configuration, and application settings are stored locally. Requests are sent only to the upstream provider you configure.
- **Use it alongside your Cursor account.** Official models and your local models can be switched freely when your account has official quota.

> [!IMPORTANT]
> cursor-byok is free and open source, but the model APIs you connect may charge for usage. This is an independent project and is not affiliated with or endorsed by Cursor or its developers.

## Features

- **Multiple API protocols:** OpenAI Responses API, OpenAI Chat Completions API, and Anthropic Messages API compatible services.
- **Model management:** Add, duplicate, edit, reorder, delete, and batch-test model configurations.
- **Connection benchmarks:** Measure time to first token, generation speed, total duration, and inspect raw provider responses.
- **Session metrics:** Track token usage, cache hit rate, conversation turns, and estimated value.
- **TAB completion options:** Choose the public service, a direct official connection, or a custom TAB service.
- **Cross-platform:** macOS, Windows, and Linux.

## Quick Start

1. Download the latest build for your platform from [GitHub Releases](https://github.com/ApexGP/cursor-byok/releases/latest).
2. Launch cursor-byok and open **Cursor Configuration**. Initialize and trust the local CA when prompted.
3. Add a model in **Model Settings**: enter the service address, API key, and model name, then save and run **Test**.
4. Keep cursor-byok running after the test passes.
5. **Fully quit and restart Cursor, then open a new conversation.** Choose your configured model in the model picker. Do not choose **Auto** when you want to use a local model.

> [!TIP]
> Cursor reads its proxy and backend configuration at process startup. After the first setup or a Cursor upgrade, a full restart is required. Conversations that were already open will not pick up the new connection.

For installation, system configuration, and FAQ, see the [User Guide](https://docs.leokun.cn).

## Model Configuration

Each model configuration is an independent upstream channel. The following table is a good starting point:

| Model family | Model type | Request protocol |
| --- | --- | --- |
| Claude | **Anthropic** | Messages API |
| GPT / OpenAI | **OpenAI** | **Responses API** |
| Other models | **OpenAI** | **Chat Completions API** |

For GPT models, prefer the **Responses API**. Chat Completions may not preserve prompt caching, which can make responses slower and more expensive.

Common fields:

- **Model type:** OpenAI or Anthropic; determines the upstream wire format.
- **Request protocol:** for OpenAI models, choose Responses API or Chat Completions API.
- **Server address:** enter a base URL and let the app append the standard endpoint, or enter a full request URL and use it as-is.
- **API key:** the credential required by the upstream service. It is stored locally.
- **Model name:** the model identifier accepted by the provider. You can also use **Get models** to load the provider's model list.
- **Display name:** shown in Cursor's model picker; it does not change the upstream model ID.
- **Description:** shown in Cursor's model tooltip.

You can also configure context window tokens, maximum output tokens, reasoning or thinking effort, custom headers, and OpenAI/Anthropic extra parameters. Custom headers and extra parameters must be JSON objects; only add fields that the provider explicitly supports.

After saving a configuration, run **Test** to make sure the address, protocol, API key, model identifier, and streaming responses all work before using the model in Cursor.

![cursor-byok model settings](./images/en-model-1.png)

## TAB Completion Service

Cursor's Tab completion is handled by a separate TAB service and does not go through the model channel. In **Settings → TAB**, choose one of the following:

- **Public service (default):** Use the public service deployed by the project author. No extra configuration is required.
- **Direct:** Connect directly to the official TAB service for the current Cursor account. Suitable when your account has official quota.
- **Custom:** Deploy `cursor-tab-server` yourself, then enter the TAB service address.

After changing TAB settings, restart Cursor and open a new conversation so the new connection takes effect.

## Compatibility and Safety

- Log in to Cursor with your own account. If you previously used a fake account from an older build, sign out first and then sign in with your account.
- When your account has official quota, official models and local models can be mixed freely.
- **Auto uses official models only** and will not automatically select a local model. If your account has no official quota, choose your configured model manually.
- Plugins, codebase indexing, and other Cursor features continue to work.
- API keys, model configuration, and application settings are stored locally. Model requests are still sent to the upstream provider you choose; follow that provider's privacy and billing terms.

## Diagnostics and Privacy

- Terminal failures are captured locally and shown through **Issues detected**. Nothing is uploaded until you click **Upload and create Issue**.
- The app uploads a redacted diagnostic bundle, opens a prefilled GitHub issue, and keeps no bundle locally. If you do not submit the issue, the server deletes the bundle after 30 days.
- Reports exclude API keys, Authorization headers, message contents, request/response bodies, and full databases. Database evidence is limited to an allowlist of diagnostic fields.
- The GitHub issue body uses a fixed template with a machine-readable JSON block so maintainers and agents can parse it with `gh`.

## How It Works and Data Privacy

```text
Cursor client
    │
    │ Agent requests and tool results
    ▼
cursor-byok local service
    │
    │ OpenAI- / Anthropic-compatible requests
    ▼
Your model API
```

cursor-byok handles protocol adaptation, model request forwarding, tool-call coordination, and conversation state on your machine. API keys and application settings stay local; requests are sent to the model provider you configure.

## Project Structure

<details>
<summary>Project tree</summary>

```text
cursor-byok/
├── apps/
│   ├── desktop/
│   │   ├── src/
│   │   │   ├── features/ # dashboard, models, calls, settings
│   │   │   ├── shell/    # window frame and page layout
│   │   │   ├── shared/   # UI, virtual list, state, API, platform
│   │   │   ├── i18n/     # localization runtime and catalogs
│   │   │   └── styles/   # global theme and typography tokens
│   │   └── src-tauri/    # Tauri desktop lifecycle
├── server/
│   ├── src/
│   │   ├── cursor/    # Cursor protocol adaptation
│   │   ├── run/       # agent runtime and internal ports
│   │   ├── provider/  # model provider adapters
│   │   ├── model/     # domain model
│   │   ├── store/     # SQLite repositories
│   │   ├── control/   # management API
│   │   ├── local_app/ # local Cursor integration
│   │   └── search/    # web and Semble search
│   ├── prompt/
│   └── migrations/
├── crates/
│   └── semble-core/   # local code index and search core
├── protocols/
│   └── cursor/        # single source of truth for Cursor protocols
├── support/           # repository support tools
│   ├── cursor-protocol-extractor/ # protocol extraction and Go codegen
│   ├── cursor-capture/            # protocol capture and debugging
│   └── benchmarks/                # code search and indexing benchmarks
├── Cargo.toml         # Rust workspace configuration
└── Makefile           # common development, check, and build commands
```

</details>

## Development

### Requirements

- Rust toolchain and Cargo
- Node.js and npm
- Tauri 2 system dependencies
- Docker (only when building the Docker image)

### Install dependencies

```bash
npm --prefix apps/desktop install
```

### Start the development environment

```bash
make dev-web       # desktop frontend
make dev-desktop   # desktop application
```

### Check and build

```bash
make check          # full check
make build-web      # build the desktop frontend
make build-server   # build the local Rust service
make build-desktop  # build Tauri installers
make build-docker   # build the Docker image
```

## Community and Support

- [User Guide](https://docs.leokun.cn)
- [GitHub Issues](https://github.com/ApexGP/cursor-byok/issues)
- [Telegram community](https://t.me/cursor_byok)
- QQ groups: `1095916242`, `1094411438`, `1095918002`, `1094419321`

When reporting an issue, include your OS, cursor-byok version, model type, request protocol, redacted service address, error message, and reproduction steps. Do not publish API keys or other credentials.

## Contributing

Issues and pull requests are welcome. Before submitting code, read [AGENTS.md](./AGENTS.md) for repository development conventions and run `make check` to verify formatting, tests, and the frontend build.

## Contributors

<a href="https://github.com/ApexGP/cursor-byok/graphs/contributors">
  <img src="https://contrib.rocks/image?repo=ApexGP/cursor-byok" />
</a>

## License

This project is open source under the [MIT License](./LICENSE).
