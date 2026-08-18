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

This repository intentionally does not include a GUI, MITM support, or
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
- Unix-socket administration through the `kproxy` CLI; no browser UI is required.
- Hot-reloaded TOML configuration, structured logs, trace IDs, statistics,
  API-key limits, TLS, and webhook notifications.
- Automatic `.env` loading by both `kproxyd` and `kproxy` on every startup.

## Workspace layout

| Component | Purpose |
| --- | --- |
| `kproxyd` | Long-running proxy daemon and administration server. |
| `kproxy` | Headless administration CLI. |
| `kproxy-core` | Domain models, defaults, and configuration validation. |
| `kproxy-store` | Atomic persistence, `.env` loading, bootstrap, and hot reload. |
| `kproxy-ipc` | Line-delimited JSON-RPC protocol shared by daemon and CLI. |
| `kproxy-translate` | Claude/OpenAI/Kiro translation, validation, and token estimation. |
| `kproxy-kiro` | Kiro HTTP client, Event Stream decoding, endpoint state, and model discovery. |
| `kproxy-pool` | Account health, credit reservation, concurrency, and weighted scheduling. |
| `kproxy-notify` | Webhook delivery, retries, suppression, and credit alerts. |

The CLI source is located in [`crates/kproxy`](crates/kproxy), and the daemon source
is located in [`crates/kproxyd`](crates/kproxyd).

## Setup

### Docker Compose (recommended for a Linux server)

Docker Engine with the Compose v2 plugin is the shortest production setup. The
Compose stack builds the full image with all features and Chromium enabled by
default, runs `kproxyd` with host networking, and keeps all state in the `kproxy-data`
named volume. Run these commands from the repository root:

```bash
docker compose up -d --build
docker compose ps
docker compose logs -f kproxyd
```

A fresh daemon exposes only its Unix administration socket. Create the first
business proxy explicitly and save the API key printed by the command:

```bash
docker compose exec kproxyd kproxy status
docker compose exec kproxyd kproxy service create --name main
docker compose exec kproxyd kproxy service list
```

Import at least one supported Kiro enterprise SSO account before sending
generation requests:

```bash
docker compose exec -T kproxyd kproxy account import --stdin < accounts.json
docker compose exec kproxyd kproxy account probe --all
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
./target/release/kproxyd
```

In another terminal:

```bash
./target/release/kproxy health
./target/release/kproxy status
./target/release/kproxy service create --name main
./target/release/kproxy config path
./target/release/kproxy account list
```

`kproxy service create` creates and starts a proxy, creates its first scoped API
key, and prints the plaintext key. You can retrieve that service's keys later
with `kproxy service apikeys main --show-secret`.

`kproxyd` and `kproxy` search for `.env` from the current directory upward. Existing
process environment variables take precedence over values in `.env`, so a
one-off override remains possible:

```bash
KPROXY_HTTP_PORT=5581 ./target/release/kproxyd
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

### Claude Code MCP Tool Search

Claude Code loads every MCP schema up front when `ANTHROPIC_BASE_URL` points to
a third-party proxy unless Tool Search is explicitly enabled. For large MCP
catalogs, start Claude Code with:

```bash
ANTHROPIC_BASE_URL=http://127.0.0.1:5580 ENABLE_TOOL_SEARCH=auto claude
```

`kiro-proxy` accepts Anthropic `defer_loading`, regex/BM25 Tool Search, and
`tool_reference` history blocks. Because Kiro has no native Tool Search server
tool, the proxy executes the search locally and continues the same response.
The official Tool Search input contains `pattern` or `query` plus an optional
`limit` from 1 to 10,000 (default 5). `kiro-proxy` honors that requested limit
and then packs results against the remaining tool-count, tool-token, context,
and payload-byte budgets; there is no fixed five-tool working set. Deferred
definitions remain outside the Kiro context and payload until discovered. The
catalog index and searches run on blocking workers rather than HTTP runtime
threads.

The generated `[context]` configuration also bounds the loaded working set with
`max_loaded_tools` (default and upstream protocol ceiling 128), loaded tool
definitions with `max_tool_input_tokens`, and the serialized Kiro request with
`max_upstream_payload_bytes`. Oversized requests fail locally with an explicit
413 instead of an opaque upstream error.

`features.tool_search_max_rounds` defaults to 4 and is hard-clamped to 8. If
that per-request server loop is exhausted, the response uses Claude's
`pause_turn` continuation state instead of converting a valid server call into
an HTTP 5xx. `features.tool_search_max_operations` defaults to 32 (valid range
1–256) and bounds aggregate searches across resumed calls and all internal
rounds; excess calls receive an in-band `unavailable` Tool Search result.
`features.enable_tool_search=false` is a rollback switch: native Tool Search
requests are then rejected explicitly instead of expanding deferred tools into
the upstream payload. Request logs retain catalog/working-set sizes, search
limits and truncation, client/upstream status, and a stable error code. Error
responses keep the normal Claude/OpenAI body and expose diagnostics through
`request-id`, `x-kproxy-error-code`, `x-kproxy-error-stage`,
`x-kproxy-upstream-status`, and `x-kproxy-account-error` headers.

### Web Search

For Claude's native `web_search` server tool, Kiro first decides whether to
search and chooses the query. The proxy then calls Kiro's `/mcp` JSON-RPC
`web_search`, returns the real result as a tool result to the same model turn,
and lets Kiro synthesize the final answer. It does not search the first user
message eagerly or present a raw search summary as model output. Streaming and
non-streaming Claude responses use `server_tool_use` and
`web_search_tool_result`; search failures are in-band tool errors and do not
cool down or ban the account. Parallel searches are preserved. In mixed
server/client-tool turns the server call remains pending until the client tool
results are returned, matching Anthropic's continuation protocol. Results carry
proxy-owned AES-256-GCM replay content so later turns can restore snippets;
modified records are rejected before entering model context. Anthropic-owned
opaque values remain accepted but are not decrypted locally. Final text carries
a structured `web_search_result_location` citation only when it actually
includes the exact result URL. The proxy
safety limit defaults to 20 searches; an explicit `max_uses` above that
configured limit is rejected instead of being silently clamped. Claude Web
Fetch is rejected explicitly until a compatible server-side executor is
implemented.

The default MCP URL is `https://runtime.{region}.kiro.dev/mcp`. Override it with
`upstream.web_search_endpoint` (the `{region}` placeholder is supported) or the
temporary `KPROXY_MCP_URL` environment variable. The default
`upstream.web_search_timeout_ms` is 60,000. Domain/location filters,
code-execution callers, strict schemas, and eager streaming are rejected when
Kiro cannot provide equivalent semantics. The proxy-generated encrypted fields
are explicitly proxy-owned and are not claimed to be interoperable with
Anthropic's hosted-search ciphertext.

## Configuration and files

Use `.env` for startup-path selection and temporary process overrides. Use
`config.toml` for persistent service, pool, model, API-key, TLS, logging, and
notification settings. See [`.env.example`](.env.example) for every supported
example variable and its purpose.

Set `KPROXY_HOME` to place configuration, data, logs, and the administration socket
under one directory. Without `KPROXY_HOME`, XDG locations are used:

| File | Default location | Notes |
| --- | --- | --- |
| `config.toml` | `${XDG_CONFIG_HOME:-~/.config}/kproxy/` | Human-edited daemon configuration. |
| `accounts.json` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | Contains credentials; created with mode `0600`. |
| `daily.json` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | Daily credit accounting, reset on UTC boundaries. |
| `stats.json` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | Persisted aggregate request statistics. |
| `web-search-replay.key` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | AES-256-GCM replay key; created with mode `0600` and never overwritten. |
| `admin.sock` | `${XDG_RUNTIME_DIR}/kproxy/` or `/run/kproxy/` | Local administration plane. |
| Logs | `${XDG_DATA_HOME:-~/.local/share}/kproxy/logs/` | Split by UTC date and severity. |

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
kproxy account import --file accounts.json
cat accounts.json | kproxy account import --stdin
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
kproxy --json account export --redact
```

## Common CLI commands

```bash
kproxy status
kproxy health
kproxy service list
kproxy service create --name main --port 5580
kproxy service apikeys main
kproxy service apikeys main --show-secret
kproxy service delete main
kproxy account list
kproxy account show <id|email>
kproxy account tag <id|email> --add prod
kproxy account disable <id|email>
kproxy account refresh <id|email>
kproxy account refresh --all
kproxy account probe --all
kproxy account regen-machine-id <id|email>
kproxy account rm <id|email>

kproxy config show --effective
kproxy config path
kproxy config validate
kproxy config reload

kproxy pool --watch --explain
kproxy diagnose endpoints
kproxy diagnose account --all -c 4 --timeout 45s
kproxy subscriptions
kproxy models --mapped
kproxy model-map add --name low-credit --source 'claude-opus-*' --target claude-sonnet-4.6 --below-credits-percent 10
kproxy model-map edit low-credit --below-credits-percent 15
kproxy model-map delete low-credit
kproxy model-map test claude-opus-4

kproxy apikey list
kproxy apikey list --detail
kproxy webhook add --name alerts --kind dingtalk --url https://example/hook --event token-expired
kproxy webhook edit alerts --event token-expired --event quota-exhausted
kproxy webhook delete alerts
kproxy stats --since 1h
kproxy stats --detail --since 1h --by endpoint
kproxy logs -f --level warn
kproxy tasks
kproxy tasks run status_check
kproxy help
```

All commands support the global `--json` option. Run `kproxy --help` or a
subcommand's `--help` for the authoritative option list.
Destructive commands have no `--yes` bypass and require an interactive `y` or
`yes` confirmation. Running bare `kproxy` prints the main help; `kproxy help` lists
the available topic guides.

`kproxy stats` reports aggregate operational traffic, success, token, credit, and
latency metrics; it does not replace per-request logs. Its default output is a
compact summary. Add `--detail` for grouped counters and recent requests.

Dynamic model discovery runs immediately at daemon startup, again when accounts
change, and thereafter when the model-cache TTL expires. The one-minute account
status task refreshes usage only and does not duplicate model-list requests.

## Enterprise SSO authentication

The default `kproxyd` build and Docker Compose enable all features, including the
Chromium-based enterprise IAM Identity Center login. First set a global start
URL with `kproxy config edit`:

```toml
[sso]
start_url = "https://example.awsapps.com/start"
```

Manual account additions can then omit `--start-url`:

```bash
printf '%s\n' "$PASSWORD" | kproxy account add-sso \
  --email user@example.com \
  --password-stdin

kproxy account add-sso --batch accounts.csv -c 1
```

Use `--start-url` to override the global value for one login. When a smaller
binary without browser SSO is explicitly desired, build with
`cargo build --workspace --no-default-features` or select Docker's
`runtime-slim` target.
Passwords are accepted only from stdin or a two-column CSV file. Add
`--headful` when MFA or an upstream page change requires manual interaction.
This flow does not add support for non-enterprise or non-SSO accounts.

## Docker and systemd

```bash
docker compose up -d --build

# The standalone default is also the full image:
docker build -t kiro-proxy:latest .
# Select slim only when browser SSO is not needed:
docker build --target runtime-slim -t kiro-proxy:slim .
docker build --target runtime-full -t kiro-proxy:full .
```

Compose uses host networking so every manually created proxy service is
available on the Docker host immediately, including custom ports; no Compose
edit or container restart is needed. Services bind to `0.0.0.0` by default, so
restrict proxy ports with the host firewall or cloud security group. Use
`--host 127.0.0.1` when host-only access is desired. Docker Engine on Linux
supports host networking directly; Docker Desktop 4.34+ requires it to be
enabled in Settings. State persists in the `kproxy-data` volume. The full image
adds Chromium for enterprise SSO authentication. Its browser is pinned to the
official `r1566079` snapshot used by the CDP definitions in `chromiumoxide
0.9.1`, so a routine image rebuild cannot silently upgrade the protocol. Update
and test both pins together when adopting browser security updates.

Docker reuses persistent Cargo registry and target caches across source updates.
Full-image builds compile the all-features binaries only once and serialize that
release build with the initial Chromium installation to avoid exhausting smaller
hosts. Cargo parallelism defaults to one job; override it only when the builder
has enough memory, for example `CARGO_BUILD_JOBS=4 docker compose build`.

After updating the source, rebuild and recreate the container without deleting
the named volume:

```bash
docker compose up -d --build
docker compose exec kproxyd kproxy config show --effective
```

Existing `config.toml` files are never overwritten. A volume created by an
older version may still contain `server.host = "127.0.0.1"`; change it with
`kproxy config edit` if the new `0.0.0.0` default is desired.

Install the Docker-backed host wrapper once to use `kproxy` directly without
typing `docker compose exec`:

```bash
sudo ./deploy/install-kproxy-wrapper.sh
kproxy health
kproxy service list
```

The wrapper discovers the running daemon through its Compose label and always
uses the CLI version bundled with that container. It requires permission to use
Docker. Set `KPROXY_COMPOSE_PROJECT` when multiple kiro-proxy Compose projects are
running.

A hardened service template is available at
[`deploy/kproxyd.service`](deploy/kproxyd.service). Install `kproxyd` and `kproxy` under
`/usr/local/bin`, create the `kproxy` system user and group, install the unit, and
then enable the service. Browser SSO additionally requires Chrome or Chromium on
the host; the provided unit keeps user namespaces and executable JIT memory
available for Chromium while retaining the other process hardening controls.

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
