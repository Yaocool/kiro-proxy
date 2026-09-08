# kiro-proxy

[English](README.md) | [简体中文](README.zh-CN.md) | [Documentation](#documentation)

`kiro-proxy` exposes Claude Messages, OpenAI Chat Completions, and OpenAI Responses
compatible APIs over Kiro. The Rust daemon `kproxyd` handles generation and account
scheduling; `kproxy` manages it through a local Unix socket.

It supports enterprise SSO credentials (AWS IAM Identity Center/IdC) and explicitly
imported Kiro headless API keys (`ksk_...`). Personal/social OAuth login is not
supported. Upstream credentials and the proxy's client API keys are separate.
The project has no GUI, MITM, or local Kiro application configuration rewriting.

> Documentation follows the current source, whose workspace version is `0.2.4`.
> It includes **unreleased changes after the `v0.2.4` tag**, including the CLI
> migration. A prebuilt `v0.2.4` image does not include those changes. See the
> [changelog](CHANGELOG.md) and [1.0.0 assessment](docs/release-readiness-1.0.0.zh-CN.md).

## Capabilities and limits

| Area | Current behavior |
| --- | --- |
| APIs | Messages, token counting, Chat Completions, Responses, model discovery; JSON and SSE generation. |
| Accounts | Weighted scheduling, per-account concurrency, cooldowns, quota protection, enterprise token refresh. |
| Upstream routing | Regional Q/CodeWhisperer/Kiro runtime, endpoint failover, isolated GovCloud routing. |
| Models and tools | Dynamic discovery, aliases and conditional mappings, tool replay, Claude Tool Search and Web Search. |
| Operations | Hot-reloaded TOML, API-key quotas, TLS, webhooks, trace logs, persisted statistics, Docker and systemd. |
| Compatibility limits | Format/strict hints do not guarantee structured output. Responses state expires and is lost on restart. Hosted tools and automatic compaction differ by protocol. |

The default client policy accepts Claude Code on Claude routes and Codex on
OpenAI generation routes; model discovery accepts both. Other clients can use a
service or API-key exemption. Authentication and service key allowlists still
apply. Read the [protocol reference](docs/protocol-compatibility.md) and
[Responses support matrix](docs/openai-responses.md) before integrating a client.

## Quick start

Run commands from a checkout of this repository. Native operation uses Unix
sockets; Linux and macOS are the intended native environments. The published
Docker workflow targets **Linux amd64** with the full SSO image.

### Docker on a Linux server

With Docker Engine and a working `docker compose` command:

```bash
./deploy/docker-setup.sh
kproxy health
```

The setup script pulls the image before replacing the container, checks daemon
health, attempts image rollback on deployment failure, and installs the matching
host CLI wrapper. State lives in the `kproxy-data` volume. The default wrapper
path is `/usr/local/bin/kproxy`; use `--target "$HOME/.local/bin/kproxy"` for a
user-owned location and include that directory in `PATH`.

Fresh setup uses `latest`; subsequent setup reuses the saved image reference.
Use `./deploy/docker-upgrade.sh` to follow `latest`, or `--image` to choose a
published tag/digest. Use `./deploy/docker-setup.sh --build` to run the current
checkout, including unreleased features. Private GHCR packages require login.

Compose uses host networking. Linux supports it directly; Docker Desktop needs
host networking enabled. A newly created service defaults to `0.0.0.0:5580`;
restrict its port or choose loopback as shown below. Deployment health alone does
not prove that generation works. See the [deployment guide](docs/startup-and-debugging.md#8-docker-compose)
for platform requirements, upgrades, backup, rollback, and uninstall behavior.

### Native build

Install rustup and a C toolchain/linker. `rust-toolchain.toml` selects Rust 1.97.1
(edition 2021). The default build includes browser SSO; native SSO login also
requires an installed Chrome/Chromium.

```bash
cp .env.example .env             # First setup only; keep an existing .env
cargo build --release --locked
./target/release/kproxyd
```

In another terminal at the repository root:

```bash
export PATH="$PWD/target/release:$PATH"
kproxy health
kproxy config path
```

The example `.env` stores development state under `.kproxy-dev`. The daemon and
CLI business commands load the nearest `.env` upward from the working directory;
existing environment variables win. CLI help, guides, completion, and version
work without `.env` or a daemon. Use an absolute `KPROXY_HOME` when invoking the
programs from different directories. See [environment and paths](docs/startup-and-debugging.md#2-environment-loading).

### Add credentials and create a service

A fresh daemon has no business listener. Choose one credential import method:

```bash
kproxy account import --stdin < /secure/accounts.json
# Alternative: reads KIRO_API_KEY from the CLI environment
kproxy account add-api-key --email ci@example.com --region us-east-1
```

Then create a service and save its printed client key:

```bash
kproxy service create --name main --host 127.0.0.1 --port 5580
kproxy account list
kproxy models list
kproxy ready
```

Do not reuse a `ksk_...` upstream key as the proxy client key. Import schemas,
SSO login and credential handling are covered in [account setup](docs/startup-and-debugging.md#4-add-or-import-accounts).
`health` checks daemon liveness; `ready` checks business prerequisites. A real
generation request additionally verifies the upstream route and consumes quota.

## Connect a client

| Endpoint | Purpose |
| --- | --- |
| `POST /v1/messages` | Claude Messages; aliases `/messages` and `/anthropic/v1/messages`. |
| `POST /v1/messages/count_tokens` | Local token estimates; the Claude aliases also support `/count_tokens`. |
| `POST /v1/chat/completions` | OpenAI Chat Completions; alias `/chat/completions`. |
| `POST /v1/responses` | OpenAI Responses; alias `/responses`. |
| `GET /v1/models` | Model discovery for both clients; alias `/models`. |
| `GET /health`, `GET /ready` | Liveness and readiness on an existing service listener. |

For Claude Code, set `ANTHROPIC_BASE_URL` to `http://127.0.0.1:5580` and
`ANTHROPIC_AUTH_TOKEN` to the service's client key. Large MCP catalogs can use
`ENABLE_TOOL_SEARCH=auto`; gateway model discovery uses
`CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1`.

For Codex, use `http://127.0.0.1:5580/v1` and `wire_api = "responses"`. The
[Codex setup guide](docs/openai-responses.md) includes the provider configuration,
state limits, unsupported controls, and stream errors. Select an available model
from `kproxy models list`; aliases do not enlarge an actual model's input window.

## Daily operations

```bash
kproxy status
kproxy stats --since 1h
kproxy logs show --tail 100
kproxy logs trace <TRACE_ID>
kproxy config show --effective
kproxy service list
kproxy help --all
kproxy help logs trace
kproxy guide balance
```

Bare command groups show help. Scripts must use explicit actions such as
`logs show`, `models list`, `tasks list`, and `diagnose all`; the last command
performs real inference on all accounts. Read the [CLI migration](docs/startup-and-debugging.md#migration-from-the-old-cli)
before updating scripts. Use [startup and troubleshooting](docs/startup-and-debugging.md)
for configuration, log retention, Docker lifecycle commands and systemd.

## Documentation

| Topic | Reference |
| --- | --- |
| Deployment, CLI migration, logs and recovery | [English](docs/startup-and-debugging.md) · [中文](docs/startup-and-debugging.zh-CN.md) |
| Protocol limits, model controls and compaction | [English](docs/protocol-compatibility.md) · [中文](docs/protocol-compatibility.zh-CN.md) |
| Responses, Codex and stateful continuation | [Integration guide (中文)](docs/openai-responses.md) |
| 1.0 readiness, remediation and release procedure | [Release plan (中文)](docs/release-readiness-1.0.0.zh-CN.md) |

## Development

The nine workspace crates separate domain/configuration, storage, IPC,
translation, upstream access, scheduling, notifications, the daemon and the CLI.
See [contributor guidance](CONTRIBUTING.md) and [architecture notes](CLAUDE.md).

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

These are required checks, not a claim that the current checkout passes them.
The [release assessment](docs/release-readiness-1.0.0.zh-CN.md) records the audited
results and remaining gates; its [release checklist](docs/release-readiness-1.0.0.zh-CN.md#发布执行清单) explains tag
and image behavior. The project is licensed under [MIT](LICENSE).
