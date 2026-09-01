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
Compose stack pulls the prebuilt full image with all features and Chromium,
runs `kproxyd` with host networking, and keeps all state in the `kproxy-data`
named volume. Run the one-step setup from the repository root. It validates the
environment, pulls the image before replacing the container, waits for health,
rolls back automatically on failure, and installs the `kproxy` command on the
host:

```bash
./deploy/docker-setup.sh
kproxy health
kproxy status
```

The default target is `/usr/local/bin/kproxy`; the script invokes `sudo` when
needed. Without sudo access, install it in a user-owned directory:

```bash
./deploy/docker-setup.sh --target "$HOME/.local/bin/kproxy"
```

The host command is a small wrapper that runs the matching CLI inside the
container. This avoids Linux-container binary incompatibility on hosts such as
macOS and keeps the administration Unix socket private. Deploy an immutable
release tag explicitly; the successful image reference is saved locally for
subsequent restarts:

```bash
./deploy/docker-setup.sh --image ghcr.io/yaocool/kiro-proxy:v0.1.3
```

To automatically follow the newest stable image published as `latest`, run:

```bash
./deploy/docker-upgrade.sh
```

It checks the registry even when an older image reference was saved by a
previous deployment, and retains the same health-check and rollback behavior.

Use `--no-pull` for an offline restart of the saved image. Use `--build` only
when a deliberate local source build is required. GHCR packages are private by
default unless their visibility is changed; private deployments must run
`docker login ghcr.io` with an account/token that can read the package.

The wrapper also manages the Docker service lifecycle from the host:

```bash
kproxy restart     # Restart and wait until the health check passes
kproxy stop        # Stop; restart remains available afterwards
kproxy uninstall   # Completely uninstall the service
kproxy uninstall --backup-dir /srv/kproxy-backups
```

`uninstall` first stops the daemon gracefully, copies all of `/var/lib/kproxy`
to the host, and verifies that `config.toml` exists before removing the
container, persistent data volume, unshared image, and installed wrapper. A
backup failure aborts the uninstall and starts the original container again.
The default backup root is `~/.kproxy/backups`; override it with `--backup-dir`
or `KPROXY_BACKUP_DIR`. Interactive use asks whether to retain the backup.
`--yes` retains it by default, and only an explicit `--delete-backup` removes it
after a successful uninstall. The source checkout is always retained.

On Linux, the script also checks the named volume before changing the container.
If Docker retains the volume metadata but its host data directory is gone, an
interactive run offers to recreate it; automation can opt in with
`--repair-volume`. Repair is allowed only when the volume belongs to this
Compose project, Docker's volume root is available, and the data path is truly
missing. A missing disk mount or unsafe symlink stops the setup instead of
hiding potentially recoverable data.

A fresh daemon exposes only its Unix administration socket. Create the first
business proxy explicitly and save the API key printed by the command:

```bash
kproxy status
kproxy service create --name main
kproxy service list
```

Import at least one supported Kiro enterprise SSO account before sending
generation requests:

```bash
kproxy account import --stdin < accounts.json
kproxy account probe --all
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
`max_loaded_tools` (default and proxy ceiling 512), deferred Tool
Search working-set definitions with `max_tool_input_tokens`, and the serialized
Kiro request with `max_upstream_payload_bytes`. Ordinary requests without Tool
Search are not subject to that 32k working-set budget: their definitions remain
part of the model's total input-token estimate and are still bounded by the
context window, tool count, and payload size. Truly oversized requests fail
locally instead of producing an opaque upstream error. HTTP
`413/request_too_large` is reserved for an actual inbound body over 50 MiB;
tool, context, and translated-payload budget errors use 400 so Claude Code does
not misreport them as a 32 MB attachment failure.

Mapping-aware context compaction for Claude Messages is enabled by default with
`context.auto_compact_on_overflow`. Before the first upstream generation, the
proxy compacts against the mapped model's safe window. If upstream still
returns `prompt is too long` or `context length exceeded`, the proxy compacts
against a conservative window and retries only once. A semantic-summary request
never receives the original oversized conversation directly: the local
tokenizer first creates a bounded checkpoint for the summary model. The same
compaction artifact may also be reapplied once if the selected account resolves
to a smaller window. OpenAI Chat
Completions and context growth after a Tool Search response has started retain
hard context-limit errors because they cannot safely return a leading Claude
`compaction` boundary. A summary timeout releases the main request immediately;
late accounting is allowed only for a bounded grace period, after which the
summary stream is canceled and any already decoded usage is settled.

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
`upstream.web_search_timeout_ms` is 60,000. Every MCP request carries Kiro's
required `x-amzn-kiro-profile-arn` header. When an imported account has no
profile ARN, the proxy discovers it through `ListAvailableProfiles`, collapses
concurrent discovery for the same token, and persists the result. Builder ID
and Social accounts use Kiro's compatible fixed-profile fallback.
Domain/location filters, code-execution callers, strict schemas, and eager
streaming are rejected when Kiro cannot provide equivalent semantics. The
proxy-generated encrypted fields
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
| `stats-history/` | `${XDG_DATA_HOME:-~/.local/share}/kproxy/` | One-minute request aggregates split into bounded UTC hourly shards. |
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
kproxy service show main
kproxy service create --name main --port 5580
kproxy service edit main --port 5581 --add-api-key ci
kproxy service disable main
kproxy service enable main
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
kproxy account rm <id|email> [<id|email> ...]

kproxy config list
kproxy config show server
kproxy config show pool --effective
kproxy config edit pool          # edits only one module; bare edit still opens the full file
kproxy config reset pool         # resets only one module and preserves every other module
kproxy config path
kproxy config validate
kproxy config reload
kproxy config reset              # resets all general settings, preserves API keys/services

kproxy pool --watch --explain
kproxy diagnose endpoints
kproxy diagnose account --all -c 4 --timeout 45s
kproxy subscriptions
kproxy models --refresh --mapped
kproxy models resolve opus5       # show model-map and final per-account Kiro model
kproxy model-map add --name low-credit --source 'claude-opus-*' --target claude-sonnet-4.6 --below-credits-percent 10
kproxy model-map edit low-credit --below-credits-percent 15
kproxy model-map delete low-credit
kproxy model-map test claude-opus-4

kproxy apikey list
kproxy apikey list --detail
kproxy apikey show ci
kproxy apikey limit ci --credits 100
kproxy apikey limit ci --clear
kproxy alert events
kproxy alert platforms
kproxy alert config
kproxy alert add --name alerts --platform dingtalk --webhook-url 'https://oapi.dingtalk.com/robot/send?access_token=replace-me' --dingtalk-sign 'SEC-replace-me' --event token-refresh-failed,account-credit-protected,account-quota-exhausted,service-quota-exhausted
kproxy alert edit --name alerts --event token-refresh-failed --event service-quota-exhausted
kproxy alert delete alerts
kproxy status --since 30m
kproxy stats --since 1h
kproxy stats --start 2026-08-27T10:00:00+08:00 --end 2026-08-27T12:00:00+08:00
kproxy stats --detail --since 1h --by endpoint
kproxy logs show --tail 100
kproxy logs follow --level warn
kproxy logs trace trace_0123456789abcdef0123456789abcdef
kproxy logs files
kproxy logs files --level error
kproxy logs path
kproxy tasks
kproxy tasks run status_check
kproxy help
```

All commands support the global `--json` option. Run `kproxy --help` or a
subcommand's `--help` for the authoritative option list.
Subscribe to `account-credit-protected` when accounts should alert after reaching
the scheduler's remaining-credit protection threshold. Same-kind account events
for one target are batched into one message while retaining per-account
once-until-recovery suppression.
Destructive commands have no `--yes` bypass and require an interactive `y` or
`yes` confirmation. Running bare `kproxy` prints the main help; `kproxy help` lists
the available topic guides.

`kproxy account list` sorts by email by default so batch imports are easy to
audit. Use `--sort credit` or `--sort id` when those views are needed. Service,
API key, alert-target, and model lists also use stable name or identifier
ordering; logs, recent requests, and pool scores retain their semantic time or
priority order.

`kproxy logs show` and `follow` read structured request records retained by the
daemon. `kproxy logs trace <TRACE_ID>` searches all retained physical severity
and date shards and orders the matching request-chain events by timestamp; add
`--level error` to restrict it to one exact severity. Physical files use exact
severity partitions: `info.log` contains only INFO events, while WARN and ERROR
events are stored in `warn.log` and `error.log`. `kproxy logs files` discovers
these shards and prints their sizes and complete paths; `kproxy logs path` prints
the active directory, base path, format, and filter. When invoked through the
Docker host wrapper, both path commands also report the named volume's real path
on the Docker host. The legacy `kproxy logs --tail ...` and `-f` forms remain
supported.

`kproxy status` reports request, success, credit, and average-latency metrics for
the current daemon session. `kproxy stats` defaults to persisted cumulative
metrics across restarts. Both commands accept `--since 1h` or an explicit
timezone-aware RFC 3339 `--start`/`--end` range. Time-series aggregates are kept
at one-minute resolution without the previous seven-day eviction. History that
was already evicted before upgrading cannot be recovered; the CLI reports the
earliest available time. The cumulative summary stays compact in `stats.json`;
minute history is stored in bounded UTC hourly files under `stats-history/`, and
range parsing/aggregation runs outside the proxy request lock. Use
`kproxy stats --detail` for recent requests and
grouped counters, and `kproxy logs` plus trace IDs for individual failures.

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

# Read explicitly from stdin for pipelines and automation:
kproxy account add-sso --batch - -c 1 < accounts.csv
```

Use `--start-url` to override the global value for one login. When a smaller
binary without browser SSO is explicitly desired, build with
`cargo build --workspace --no-default-features` or select Docker's
`runtime-slim` target.
The Docker host wrapper automatically recognizes a readable host CSV and
streams it into the container through stdin, without copying or retaining a
password file. A container path is still read normally when no host file with
the same name exists. Passwords are accepted only from stdin or a two-column
CSV file. Add
`--headful` when MFA or an upstream page change requires manual interaction.
Every login uses a dedicated incognito Chromium context and temporary profile,
which are destroyed before the next account is processed. Before saving an
account, kproxy records Kiro's stable user ID and refuses to register that same
identity under another email. Kiro display names are diagnostic only because
IAM Identity Center names do not always match login email addresses.
This flow does not add support for non-enterprise or non-SSO accounts.

## Docker and systemd

```bash
# Production: pull while the old container is still serving, then replace it.
KPROXY_IMAGE=ghcr.io/yaocool/kiro-proxy:v0.1.3 docker compose pull kproxyd
KPROXY_IMAGE=ghcr.io/yaocool/kiro-proxy:v0.1.3 docker compose up -d --no-build

# Local source build through the explicit build override:
docker compose -f docker-compose.yml -f docker-compose.build.yml up -d --build

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

Local and CI builds reuse persistent Cargo registry and target caches. Full-image
builds compile the all-features binaries only once and serialize that release
build with the initial Chromium installation. Local Cargo parallelism defaults
to one job; override it only when the build host has enough memory, for example
`CARGO_BUILD_JOBS=4 docker compose -f docker-compose.yml -f docker-compose.build.yml build`.

The `Build and publish Docker image` GitHub Actions workflow runs only when a
`v*` tag is pushed. A version tag such as `v0.1.3` publishes `v0.1.3`, `v0.1`,
and `latest`; the tag must match the Cargo workspace version. After bumping that
version and merging the release commit, publish it with
`git tag v0.1.3 && git push origin v0.1.3`. Production upgrades pull the new image
before replacing the container and keep the named volume:

```bash
./deploy/docker-setup.sh --image ghcr.io/yaocool/kiro-proxy:v0.1.3
docker compose exec kproxyd kproxy config show --effective
```

Alternatively, follow the newest stable release automatically with
`./deploy/docker-upgrade.sh`. Set `KPROXY_UPGRADE_IMAGE` to use a different
registry image.

The script retains the previous local image under a rollback tag. If container
creation or the health check fails, it recreates the service from that image.
With host networking, the old and new containers cannot bind the same proxy
ports concurrently, so the final container switch still has a short restart
window; image download and compilation no longer consume production downtime.

Existing `config.toml` files are never overwritten. A volume created by an
older version may still contain `server.host = "127.0.0.1"`; change it with
`kproxy config edit` if the new `0.0.0.0` default is desired.

Pull, start, and install the Docker-backed wrapper in one step:

```bash
./deploy/docker-setup.sh
kproxy health
kproxy service list
```

The wrapper discovers the running daemon through its Compose label and always
uses the CLI version bundled with that container. It requires permission to use
Docker. Set `KPROXY_COMPOSE_PROJECT` when multiple kiro-proxy Compose projects are
running. To reinstall only the wrapper, run
`sudo ./deploy/install-kproxy-wrapper.sh`.

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
