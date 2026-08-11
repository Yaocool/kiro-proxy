# kiro-proxy

[English](README.md) | [简体中文](README.zh-CN.md)

`kiro-proxy` is a headless Rust service that exposes Claude Messages and OpenAI
Chat Completions compatible APIs on top of Kiro upstream services. It includes
multi-account scheduling, automatic token refresh, endpoint failover, model
mapping, API-key quotas, TLS, webhooks, statistics, and an operations CLI.

This repository intentionally does not include a GUI, KProxy MITM support, or
local Kiro application configuration changes.

## Highlights

- Claude-compatible `/v1/messages` and `/v1/messages/count_tokens` endpoints.
- OpenAI-compatible `/v1/chat/completions` and `/v1/models` endpoints.
- Weighted multi-account scheduling with per-account concurrency limits,
  cooldowns, quota tracking, and model compatibility checks.
- Automatic IdC and social-account token refresh with per-account singleflight.
- Account-aware Amazon Q and CodeWhisperer endpoint selection with bounded,
  in-memory availability caches.
- Dynamic model discovery, model aliases, replacements, load balancing, and
  fallback rules.
- Unix-socket administration through the `kam` CLI; no browser UI is required.
- Hot-reloaded TOML configuration, structured logs, trace IDs, statistics,
  API-key limits, TLS, and webhook notifications.
- Automatic `.env` loading by both `kamd` and `kam` on every startup.

## Workspace layout

| Component | Purpose |
| --- | --- |
| `kamd` | Long-running proxy daemon and administration server. |
| `kam` | Headless administration CLI. |
| `kam-core` | Domain models, defaults, and configuration validation. |
| `kam-store` | Atomic persistence, `.env` loading, bootstrap, and hot reload. |
| `kam-ipc` | Line-delimited JSON-RPC protocol shared by daemon and CLI. |
| `kam-translate` | Claude/OpenAI/Kiro translation, validation, and token estimation. |
| `kam-kiro` | Kiro HTTP client, Event Stream decoding, endpoint state, and model discovery. |
| `kam-pool` | Account health, credit reservation, concurrency, and weighted scheduling. |
| `kam-notify` | Webhook delivery, retries, suppression, and credit alerts. |

The CLI source is located in [`crates/kam`](crates/kam), and the daemon source
is located in [`crates/kamd`](crates/kamd).

## Quick start

The pinned Rust 1.97.1 toolchain is selected automatically through
`rust-toolchain.toml`.

```bash
cargo build --release --locked

# Both binaries load this file on every startup.
cp .env.example .env

# First startup creates config.toml, accounts.json, daily.json, and stats.json.
./target/release/kamd
```

In another terminal:

```bash
./target/release/kam health
./target/release/kam status
./target/release/kam service create --name main
./target/release/kam config path
./target/release/kam account list
```

A fresh daemon starts only the Unix administration plane. It does not create or
start a business API proxy. `kam service create` creates and starts one, creates
its first scoped API key, and prints the plaintext key. You can retrieve only
that service's keys later with `kam service apikeys main --show-secret`.

`kamd` and `kam` search for `.env` from the current directory upward. Existing
process environment variables take precedence over values in `.env`, so a
one-off override remains possible:

```bash
KAM_HTTP_PORT=5581 ./target/release/kamd
```

After creating `main` with the default settings, its business API listens on
`http://127.0.0.1:5580`:

```text
POST /v1/messages
POST /v1/messages/count_tokens
POST /v1/chat/completions
GET  /v1/models
GET  /health
```

Claude aliases `/messages` and `/anthropic/v1/messages` are also available.
OpenAI aliases `/chat/completions` and `/models` are supported as well.

## Configuration and files

Use `.env` for startup-path selection and temporary process overrides. Use
`config.toml` for persistent service, pool, model, API-key, TLS, logging, and
notification settings. See [`.env.example`](.env.example) for every supported
example variable and its purpose.

Set `KAM_HOME` to place configuration, data, logs, and the administration socket
under one directory. Without `KAM_HOME`, XDG locations are used:

| File | Default location | Notes |
| --- | --- | --- |
| `config.toml` | `${XDG_CONFIG_HOME:-~/.config}/kam/` | Human-edited daemon configuration. |
| `accounts.json` | `${XDG_DATA_HOME:-~/.local/share}/kam/` | Contains credentials; created with mode `0600`. |
| `daily.json` | `${XDG_DATA_HOME:-~/.local/share}/kam/` | Daily credit accounting, reset on UTC boundaries. |
| `stats.json` | `${XDG_DATA_HOME:-~/.local/share}/kam/` | Persisted aggregate request statistics. |
| `admin.sock` | `${XDG_RUNTIME_DIR}/kam/` or `/run/kam/` | Local administration plane. |
| Logs | `${XDG_DATA_HOME:-~/.local/share}/kam/logs/` | Split by UTC date and severity. |

On first startup, missing files are created without overwriting existing data.
Valid configuration changes are hot-reloaded. Invalid TOML or validation
failures leave the last valid configuration active. `server.host` and
`server.port` are defaults for newly created proxy services. Changes to
`admin.socket` or the shared HTTP/HTTPS listening mode require a daemon restart;
most other fields, including the proxy service list, apply without one.

External account-file changes are also reloaded. Corrupt account data never
replaces the valid in-memory snapshot. Large account stores can use a gzip
envelope plus incremental sidecar updates according to the storage settings.

## Import accounts

Import existing credentials from a JSON file or stdin:

```bash
kam account import --file accounts.json
cat accounts.json | kam account import --stdin
```

`id`, `machine_id`, and `created_at` may be omitted; the CLI generates them.

```json
[
  {
    "email": "user@example.com",
    "credentials": {
      "access_token": "...",
      "refresh_token": "...",
      "client_id": "...",
      "client_secret": "...",
      "region": "us-east-1",
      "expires_at": 1767225600,
      "auth_method": "idc"
    }
  }
]
```

Account exports contain credentials by default. Use `--redact` before sharing
diagnostic output:

```bash
kam --json account export --redact
```

## Common CLI commands

```bash
kam status
kam health
kam service list
kam service create --name main --host 127.0.0.1 --port 5580
kam service apikeys main
kam service apikeys main --show-secret
kam account list
kam account show <id|email>
kam account tag <id|email> --add prod
kam account disable <id|email>
kam account refresh <id|email>
kam account refresh --all
kam account probe --all
kam account regen-machine-id <id|email>
kam account rm <id|email> --yes

kam config show --effective
kam config path
kam config validate
kam config reload

kam pool --watch --explain
kam diagnose endpoints
kam diagnose account --all -c 4 --timeout 45s
kam subscriptions
kam models --mapped
kam model-map test claude-opus-4

kam apikey list
kam webhook test --all
kam stats --since 1h --by endpoint
kam logs -f --level warn
kam tasks
kam tasks run status_check
```

All commands support the global `--json` option. Run `kam --help` or a
subcommand's `--help` for the authoritative option list.

## SSO build

The default slim build supports credential import and does not include
Chromium. Build `kamd` with the `sso` feature to enable IAM Identity Center
browser automation:

```bash
cargo build --release --locked -p kamd --features sso

printf '%s\n' "$PASSWORD" | kam account add-sso \
  --email user@example.com \
  --start-url https://example.awsapps.com/start \
  --password-stdin

kam account add-sso --batch accounts.csv \
  --start-url https://example.awsapps.com/start -c 1
```

Passwords are accepted only from stdin or a two-column CSV file. Add
`--headful` when MFA or an upstream page change requires manual interaction.

## Docker and systemd

```bash
docker build --target runtime-slim -t kamd:slim .
docker build --target runtime-full -t kamd:full .
docker compose up -d
```

Compose uses host networking so every manually created proxy service is
available on the Docker host immediately, including custom ports; no Compose
edit or container restart is needed. Services bind to `127.0.0.1` by default.
Use `--host 0.0.0.0` only when remote access is intentional. Docker Engine on
Linux supports this directly; Docker Desktop 4.34+ requires host networking to
be enabled in Settings. State persists in the `kam-data` volume. The full image
adds Chromium for SSO.

Install the Docker-backed host wrapper once to use `kam` directly without
typing `docker compose exec`:

```bash
sudo ./deploy/install-kam-wrapper.sh
kam health
kam service list
```

The wrapper discovers the running daemon through its Compose label and always
uses the CLI version bundled with that container. It requires permission to use
Docker. Set `KAM_COMPOSE_PROJECT` when multiple kiro-proxy Compose projects are
running.

A hardened service template is available at
[`deploy/kamd.service`](deploy/kamd.service). Install `kamd` and `kam` under
`/usr/local/bin`, create the `kam` system user and group, install the unit, and
then enable the service.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
```

The project uses Rust edition 2021 with MSRV 1.97.1.

For complete startup, deployment, logging, LLDB, and troubleshooting guidance,
see [Startup and debugging](docs/startup-and-debugging.md).

## License

MIT
