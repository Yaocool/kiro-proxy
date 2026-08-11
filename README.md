# kiro-proxy

[English](README.md) | [简体中文](README.zh-CN.md)

`kiro-proxy` is a headless Rust service that exposes Claude Messages and OpenAI
Chat Completions compatible APIs on top of Kiro upstream services. It includes
multi-account scheduling, automatic token refresh, endpoint failover, model
mapping, API-key quotas, TLS, webhooks, statistics, and an operations CLI.

> [!IMPORTANT]
> Account support is limited to Kiro enterprise accounts authenticated through
> the enterprise's SSO integration (AWS IAM Identity Center/IdC). All other
> account and authentication types, including personal and social-login
> accounts, are not supported.

This repository intentionally does not include a GUI, KProxy MITM support, or
local Kiro application configuration changes.

## Highlights

- Claude-compatible `/v1/messages` and `/v1/messages/count_tokens` endpoints.
- OpenAI-compatible `/v1/chat/completions` and `/v1/models` endpoints.
- Weighted multi-account scheduling with per-account concurrency limits,
  cooldowns, quota tracking, and model compatibility checks.
- Automatic enterprise IdC/SSO token refresh with per-account singleflight.
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

## Setup

### Docker Compose (recommended for a Linux server)

Docker Engine with the Compose v2 plugin is the shortest production setup. The
Compose stack builds the slim image, runs `kamd` with host networking, and keeps
all state in the `kam-data` named volume. Run these commands from the repository
root:

```bash
docker compose up -d --build
docker compose ps
docker compose logs -f kamd
```

A fresh daemon exposes only its Unix administration socket. Create the first
business proxy explicitly and save the API key printed by the command:

```bash
docker compose exec kamd kam status
docker compose exec kamd kam service create --name main
docker compose exec kamd kam service list
```

Import at least one supported Kiro enterprise SSO account before sending
generation requests:

```bash
docker compose exec -T kamd kam account import --stdin < accounts.json
docker compose exec kamd kam account probe --all
```

The default listener is `0.0.0.0:5580`. Restrict that port with the host
firewall or cloud security group; use `--host 127.0.0.1` when the proxy should
only be reachable from the Docker host. Do not run `docker compose down -v`
unless the persisted configuration, accounts, usage, and logs should be erased.

### Native build

The pinned Rust 1.97.1 toolchain is selected automatically through
`rust-toolchain.toml`.

```bash
cp .env.example .env
cargo build --release --locked

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

`kam service create` creates and starts a proxy, creates its first scoped API
key, and prints the plaintext key. You can retrieve that service's keys later
with `kam service apikeys main --show-secret`.

`kamd` and `kam` search for `.env` from the current directory upward. Existing
process environment variables take precedence over values in `.env`, so a
one-off override remains possible:

```bash
KAM_HTTP_PORT=5581 ./target/release/kamd
```

After creating `main` with the default settings, its business API binds to
`0.0.0.0:5580`; use `http://127.0.0.1:5580` from the same host:

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

## Import enterprise SSO accounts

Only credentials issued to a Kiro enterprise account through its organization
SSO may be imported. Importing a credential does not make personal, social-login,
or any other account type compatible. Import supported credentials from a JSON
file or stdin:

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
kam service create --name main --port 5580
kam service apikeys main
kam service apikeys main --show-secret
kam service delete main --yes
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

## Enterprise SSO authentication

The default slim build supports importing existing Kiro enterprise SSO
credentials and does not include Chromium. Build `kamd` with the `sso` feature
to authenticate a supported enterprise account through its IAM Identity Center
login flow:

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
This flow does not add support for non-enterprise or non-SSO accounts.

## Docker and systemd

```bash
docker compose up -d --build

# Standalone image builds, when Compose is not used:
docker build --target runtime-slim -t kamd:slim .
docker build --target runtime-full -t kamd:full .
```

Compose uses host networking so every manually created proxy service is
available on the Docker host immediately, including custom ports; no Compose
edit or container restart is needed. Services bind to `0.0.0.0` by default, so
restrict proxy ports with the host firewall or cloud security group. Use
`--host 127.0.0.1` when host-only access is desired. Docker Engine on Linux
supports host networking directly; Docker Desktop 4.34+ requires it to be
enabled in Settings. State persists in the `kam-data` volume. The full image
adds Chromium for enterprise SSO authentication.

After updating the source, rebuild and recreate the container without deleting
the named volume:

```bash
docker compose up -d --build
docker compose exec kamd kam config show --effective
```

Existing `config.toml` files are never overwritten. A volume created by an
older version may still contain `server.host = "127.0.0.1"`; change it with
`kam config edit` if the new `0.0.0.0` default is desired.

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

For complete setup, startup, deployment, logging, LLDB, and troubleshooting
guidance, see [Setup, startup, and debugging](docs/startup-and-debugging.md).

## License

MIT
