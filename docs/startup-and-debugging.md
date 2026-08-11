# Setup, startup, and debugging guide

[English](startup-and-debugging.md) | [简体中文](startup-and-debugging.zh-CN.md)

This guide covers first-time setup, local development, release binaries, Docker
Compose, systemd, logs, trace IDs, LLDB, tests, upgrades, and common startup
failures. Unless noted otherwise, run commands from the repository root.

## 1. Prerequisites

The repository pins Rust 1.97.1 and requests `rustfmt`, `clippy`, and
`rust-analyzer` in `rust-toolchain.toml`. With rustup installed, entering the
repository selects the correct toolchain automatically.

Choose the prerequisites for the deployment path:

- Docker deployment: Docker Engine and the Compose v2 plugin. Host networking
  is supported directly on Linux; Docker Desktop requires version 4.34 or newer
  with host networking enabled.
- Native build or development: rustup plus a C toolchain and linker.

```bash
rustup show active-toolchain
rustc --version
cargo --version
```

Build the normal workspace:

```bash
cargo build --workspace --locked
```

The standard `kamd` binary does not include Chromium. To compile browser-based
IAM Identity Center login support:

```bash
cargo build -p kamd --features sso --locked
```

## 2. Environment loading

Copy the example before starting a local daemon:

```bash
cp .env.example .env
```

Both `kamd` and `kam` load `.env` before parsing CLI arguments on every startup.
They search from the current directory upward, which lets commands launched from
a workspace subdirectory reuse the repository-level file.

Environment precedence is:

1. Variables already present in the process environment.
2. Values loaded from the nearest `.env` found while searching upward.
3. `config.toml` values for settings that have a persistent equivalent.
4. Built-in application defaults.

An existing process variable is never overwritten by `.env`. A missing `.env`
is allowed; a malformed or unreadable file fails startup with an error.

The example uses `KAM_HOME=.kam-dev` to isolate development files. The most
important process-level variables are:

| Variable | Purpose |
| --- | --- |
| `KAM_HOME` | Places configuration, data, logs, and the generated admin socket below one directory. |
| `KAM_HTTP_PORT` | Overrides a configured proxy service port that matches the `server.port` default; it does not create a service. |
| `KAM_DISABLE_HTTP=1` | Prevents configured proxy services from listening while leaving their configuration and the Unix administration socket intact. |
| `KAM_ADMIN_SOCKET` | Overrides the socket used by the `kam` CLI; it does not reconfigure `kamd`. |
| `KAM_CODEWHISPERER_URL` | Overrides the CodeWhisperer upstream URL for integration tests or controlled proxies. |
| `KAM_AMAZONQ_URL` | Overrides the Amazon Q upstream URL for integration tests or controlled proxies. |
| `RUST_LOG` | Sets tracing filters for console and application diagnostics. |
| `RUST_BACKTRACE` | Enables Rust backtraces when set to `1` or `full`. |

Use `config.toml` instead of `.env` for persistent service, pool, model, API-key,
TLS, notification, and logging configuration.

## 3. Start locally with native binaries

Run the daemon in development mode:

```bash
cargo run -p kamd
```

On first startup with the example `.env`, `.kam-dev/` contains:

- `config.toml`: daemon configuration;
- `accounts.json`: accounts and credentials, mode `0600` on Unix;
- `daily.json`: daily credit accounting;
- `stats.json`: aggregate request statistics;
- `admin.sock`: local administration socket;
- `logs/`: logs split by UTC date and severity.

Use another terminal for the CLI. It loads the same `.env` automatically:

```bash
cargo run -p kam -- status
cargo run -p kam -- health
cargo run -p kam -- service list
cargo run -p kam -- config path
cargo run -p kam -- config show --effective
cargo run -p kam -- account list
```

To run compiled binaries instead:

```bash
cargo build --release --locked
./target/release/kamd
./target/release/kam status
```

Only one daemon may own a given administration socket. A stale socket left by a
crashed process is removed automatically; a socket accepting connections is not
deleted by a second daemon.

A fresh daemon intentionally starts with no business API proxy. `kam health`
still returns success because daemon health is independent of account and proxy
service availability. Create a service explicitly when it is needed:

```bash
cargo run -p kam -- service create --name main
```

The command creates the service's first scoped API key and prints the plaintext
key. Use `kam service apikeys main` to inspect key metadata without secrets, or
add `--show-secret` to retrieve plaintext keys bound only to that service. The
default listener is `0.0.0.0`; use `--host 127.0.0.1` when only local access is
required.

### Administration-only mode

Prevent all configured proxy services from listening while keeping account and
configuration administration available:

```bash
KAM_DISABLE_HTTP=1 cargo run -p kamd
```

This is useful for storage maintenance and CLI-only tests. It does not disable
`admin.sock`.

## 4. Add or import accounts

Import existing credentials from JSON:

```bash
cargo run -p kam -- account import --file accounts.json
cat accounts.json | cargo run -p kam -- account import --stdin
```

The CLI can generate missing `id`, `machine_id`, and `created_at` fields. After
importing, inspect and probe the accounts:

```bash
cargo run -p kam -- account list
cargo run -p kam -- account probe --all
cargo run -p kam -- models
```

For a build with the `sso` feature, IAM Identity Center login is also available:

```bash
printf '%s\n' "$PASSWORD" | cargo run -p kam -- account add-sso \
  --email user@example.com \
  --start-url https://example.awsapps.com/start \
  --password-stdin
```

`kam` sends this administration request to `kamd`; therefore the running daemon,
not only the CLI, must have been built with SSO support. Use `--headful` for MFA
or manual verification.

## 5. Create and verify a proxy service

If one has not been created yet, create a proxy and save the returned API key:

```bash
kam service create --name main --port 5580
kam service list
kam service apikeys main --show-secret
```

The service now binds to `0.0.0.0:5580`; its local business address is
`http://127.0.0.1:5580`.

```bash
curl -i http://127.0.0.1:5580/health

curl -i http://127.0.0.1:5580/v1/messages/count_tokens \
  -H 'authorization: Bearer <key>' \
  -H 'content-type: application/json' \
  -H 'user-agent: claude-cli/1.0 (external, debug)' \
  -d '{"model":"claude-sonnet-4","messages":[{"role":"user","content":"hello"}]}'
```

`GET /health` returns `status: ok` even before an upstream account is available;
account counts are diagnostics, not application-health criteria. Local token
counting still requires the service API key. A generation request returning
`503` is expected when no account is schedulable.

Every newly created service requires its generated API key:

```bash
curl -i http://127.0.0.1:5580/v1/models \
  -H 'authorization: Bearer <key>'
```

Creating or configuring a service on a non-loopback address is rejected unless
that service references at least one enabled API key. This prevents accidental
unauthenticated public exposure.

Every business HTTP response includes `x-trace-id`. Save that value when
investigating an error.

Stop and remove a service when it is no longer needed. Its API keys are kept so
they cannot be deleted accidentally; remove an unused key separately with
`kam apikey rm <id> --yes`.

```bash
kam service delete main --yes
```

## 6. Configuration and hot reload

Print the active paths and validate the current configuration:

```bash
cargo run -p kam -- config path
cargo run -p kam -- config validate
```

The daemon watches `config.toml` with a short debounce. Valid changes are applied
automatically; invalid TOML or values leave the previous configuration active.
You can also trigger reload explicitly:

```bash
cargo run -p kam -- config reload
```

`server.host` and `server.port` are defaults used by `kam service create`; they
do not create a listener themselves. Proxy service additions and address changes
are reconciled at runtime. The following changes require a daemon restart:

- `admin.socket`;
- switching the shared listener mode between HTTP and HTTPS.

Log filters, formatting, output paths, pool behavior, model rules, notification
settings, and TLS certificate contents can otherwise be updated at runtime.

Bootstrap never overwrites an existing `config.toml`. A data directory created
by an older release may therefore retain `server.host = "127.0.0.1"`. Inspect
the effective value with `kam config show --effective` and use `kam config edit`
when the current default of `0.0.0.0` is desired.

`KAM_HTTP_PORT` is a process override. Changing `server.port` does not supersede
that environment variable until the variable is removed and the daemon is
restarted.

## 7. Logs and trace IDs

With `KAM_HOME=.kam-dev`, logs are written below `.kam-dev/logs/`. Files are
named like:

```text
kamd-2026-08-10-info.log
kamd-2026-08-10-warn.log
kamd-2026-08-10-error.1.log
```

Logs are split by severity and UTC date. The default maximum is 100 MB per shard,
and the default retention period is three days. `log.file_path` controls the
base path; an empty value uses the data directory's `logs/kamd.log` base.

Search by the response trace ID:

```bash
rg 'trace_f028' .kam-dev/logs/
```

Or follow records through the administration API:

```bash
cargo run -p kam -- logs -f --level warn
cargo run -p kam -- stats --since 1h --by endpoint
```

For more detail, set `RUST_LOG` or `log.level` to `debug` or `trace`. Logs do not
record prompts, generated response bodies, or API-key values.

## 8. Docker Compose

Build and start the default slim image:

```bash
docker compose config --quiet
docker compose up -d --build
docker compose ps
docker compose exec kamd kam health
docker compose logs -f kamd
```

Compose uses `network_mode: host`. A proxy listener created inside the container
therefore binds directly in the Docker host's network namespace. Arbitrary
service ports become available immediately without editing Compose or recreating
the container. This mode is supported directly by Docker Engine on Linux. On
Docker Desktop 4.34 or newer, enable host networking under Settings > Resources
> Network before starting the stack.

The image sets `KAM_HOME=/var/lib/kam`; Compose mounts the `kam-data` named
volume there. Do not copy the development `.env.example` into the container or
bind-mount `.kam-dev`. Change persistent settings through `config.toml` with
`kam config edit`, or add an explicit Compose environment entry when a
process-level override is required.

When upgrading an existing bridge-network deployment, recreate this project
container once so the new network mode takes effect. The named data volume is
preserved:

```bash
docker compose up -d --force-recreate
```

For normal source upgrades, rebuild in place and keep the named volume:

```bash
docker compose up -d --build
docker compose exec kamd kam version
docker compose exec kamd kam config show --effective
```

### Use `kam` directly on the Docker host

Docker cannot safely install files into the host's `/usr/local/bin` from a
Dockerfile or Compose service. Install the provided wrapper once on the host:

```bash
sudo ./deploy/install-kam-wrapper.sh
kam health
kam status
kam service list
```

The wrapper locates the running daemon by the `io.kiro-proxy.role=daemon`
container label, preserves command exit codes, forwards stdin, and allocates a
TTY only for interactive use. This keeps the admin Unix socket private and
avoids host/container binary compatibility problems.

The installer refuses to overwrite an existing command by default:

```bash
sudo ./deploy/install-kam-wrapper.sh --force
./deploy/install-kam-wrapper.sh --target "$HOME/.local/bin/kam"
```

The current host user must be allowed to access Docker. When more than one
kiro-proxy stack is running, select one by Compose project or container:

```bash
export KAM_COMPOSE_PROJECT=kiro-proxy
# Or: export KAM_DOCKER_CONTAINER=<container-name-or-id>
kam status
```

On a fresh volume, explicitly create the proxy and save the API key printed by
the command:

```bash
docker compose exec kamd kam status
docker compose exec -T kamd kam account import --stdin < accounts.json
docker compose exec kamd kam account probe --all
docker compose exec kamd kam service create --name main
docker compose exec kamd kam service create --name secondary --port 6000
docker compose exec kamd kam service list
docker compose exec kamd kam service apikeys main --show-secret
docker compose exec kamd kam config show --effective
docker compose exec kamd sh -c 'ls -lh /var/lib/kam/logs'
curl -i http://127.0.0.1:5580/health
curl -i http://127.0.0.1:6000/health
```

Remove a proxy listener when it is no longer needed. The associated API keys
remain until they are removed separately:

```bash
docker compose exec kamd kam service delete secondary --yes
```

The default host is `0.0.0.0`, so these listeners are reachable through the
Docker host's network interfaces. Restrict the ports with the host firewall or
cloud security group, or create a host-only listener with `--host 127.0.0.1`.
Every created service still requires its scoped API key for business requests.
Host networking removes network isolation between the container and the host,
so use the provided bridge compatibility forwarder instead when that is
unacceptable.

`docker compose down` keeps the named volume. `docker compose down -v` deletes
configuration, accounts, statistics, and logs and should be used only when an
intentional reset is required.

The default Docker target is `runtime-slim`. Change the Compose target to
`runtime-full` and rebuild when browser SSO is needed. The full image installs
Chromium and sets the container-specific no-sandbox flag.

## 9. systemd

Build release binaries and install them with the provided unit:

```bash
cargo build --release --locked

sudo useradd --system --home-dir /var/lib/kam --shell /usr/sbin/nologin kam
sudo install -m 0755 target/release/kamd target/release/kam /usr/local/bin/
sudo install -m 0644 deploy/kamd.service /etc/systemd/system/kamd.service
sudo systemctl daemon-reload
sudo systemctl enable --now kamd
sudo systemctl status kamd
```

If the `kam` user already exists, skip `useradd`. The unit uses `/etc/kam`,
`/var/lib/kam`, and `/run/kam` through systemd-managed directories.

```bash
sudo -u kam kam --socket /run/kam/admin.sock status
sudo journalctl -u kamd -f
sudo systemctl reload kamd
```

Reload sends `SIGHUP`. Restart-required settings still need
`sudo systemctl restart kamd`.

The unit's default hardening is appropriate for the slim proxy. Browser SSO may
require a dedicated unit with carefully reviewed relaxations for Chromium,
particularly `RestrictNamespaces` and `MemoryDenyWriteExecute`.

## 10. VS Code and LLDB

Install rust-analyzer and CodeLLDB, then add a launch configuration such as:

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "type": "lldb",
      "request": "launch",
      "name": "Debug kamd",
      "cargo": {
        "args": ["build", "-p", "kamd"],
        "filter": { "name": "kamd", "kind": "bin" }
      },
      "cwd": "${workspaceFolder}",
      "env": {
        "KAM_HOME": "${workspaceFolder}/.kam-dev",
        "RUST_LOG": "kamd=debug,kam_kiro=debug,kam_pool=debug",
        "RUST_BACKTRACE": "1"
      }
    }
  ]
}
```

Command-line LLDB is also available:

```bash
cargo build -p kamd
rust-lldb target/debug/kamd
```

Inside LLDB, set process variables with
`settings set target.env-vars KAM_HOME=... RUST_LOG=debug`, add breakpoints with
`breakpoint set --name <function>`, and run the process. For asynchronous
requests, correlate breakpoints and logs with `x-trace-id` rather than thread ID.

## 11. Tests and static checks

Run the full validation set before submitting changes:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
docker compose config --quiet
```

Narrow the scope while debugging:

```bash
cargo test -p kam-kiro
cargo test -p kamd http::tests::every_response_has_a_unique_trace_id
cargo test -p kam-pool refresh::tests::successful_refresh_preserves_cooling_and_exhausted_health
```

Wiremock and end-to-end tests bind temporary loopback ports. Sandboxed CI runners
must allow local port binding.

## 12. Troubleshooting

### `Address already in use`

Choose a free port when creating the service, or use the process override for a
service configured with the default port:

```bash
kam service create --name main --port 5581
KAM_HTTP_PORT=5581 cargo run -p kamd
```

### Cannot connect to `admin.sock`

Confirm `kamd` and `kam` load the same `KAM_HOME`. Use `kam config path` when the
daemon is reachable, or pass the socket explicitly:

```bash
kam --socket /path/to/admin.sock status
```

Remember that `KAM_ADMIN_SOCKET` changes the CLI target only. Set
`admin.socket` in `config.toml` and restart `kamd` to move the daemon socket.

### Configuration changes do not apply

Run `kam config validate`, inspect warn/error logs, and check whether the changed
field requires restart. Also check for a process-level `KAM_HTTP_PORT` override.

### Claude routes return access denied

Claude routes validate the client User-Agent by default. Use a supported Claude
Code-compatible User-Agent or explicitly change `server.enforce_user_agent_check`
in a trusted deployment.

### Generation returns `503`

Run `kam account list` and `kam account probe --all`. There may be no Available
account, the requested model may be incompatible, or all accounts may be in
cooldown or out of credit.

### A stream stops unexpectedly

Search warn/error logs using `x-trace-id`. Check upstream authentication refresh,
endpoint attempts, account switching, model fallback, client disconnects, and
downstream write timeouts.

### Docker still uses an old layer

```bash
docker compose build --pull --no-cache
docker compose up -d
```

Return to the [main README](../README.md).
