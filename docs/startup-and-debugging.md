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

The default build enables all features, including Chromium SSO:

```bash
cargo build --workspace --locked
```

Disable default features only when browser login is explicitly unnecessary and
a smaller binary is preferred:

```bash
cargo build --workspace --no-default-features --locked
```

## 2. Environment loading

Copy the example before starting a local daemon:

```bash
cp .env.example .env
```

Both `kproxyd` and `kproxy` load `.env` before parsing CLI arguments on every startup.
They search from the current directory upward, which lets commands launched from
a workspace subdirectory reuse the repository-level file.

Environment precedence is:

1. Variables already present in the process environment.
2. Values loaded from the nearest `.env` found while searching upward.
3. `config.toml` values for settings that have a persistent equivalent.
4. Built-in application defaults.

An existing process variable is never overwritten by `.env`. A missing `.env`
is allowed; a malformed or unreadable file fails startup with an error.

The example uses `KPROXY_HOME=.kproxy-dev` to isolate development files. The most
important process-level variables are:

| Variable | Purpose |
| --- | --- |
| `KPROXY_HOME` | Places configuration, data, logs, and the generated admin socket below one directory. |
| `KPROXY_HTTP_PORT` | Overrides a configured proxy service port that matches the `server.port` default; it does not create a service. |
| `KPROXY_DISABLE_HTTP=1` | Prevents configured proxy services from listening while leaving their configuration and the Unix administration socket intact. |
| `KPROXY_ADMIN_SOCKET` | Overrides the socket used by the `kproxy` CLI; it does not reconfigure `kproxyd`. |
| `KPROXY_CODEWHISPERER_URL` | Overrides the CodeWhisperer upstream URL for integration tests or controlled proxies. |
| `KPROXY_AMAZONQ_URL` | Overrides the Amazon Q upstream URL for integration tests or controlled proxies. |
| `RUST_LOG` | Sets tracing filters for console and application diagnostics. |
| `RUST_BACKTRACE` | Enables Rust backtraces when set to `1` or `full`. |

Use `config.toml` instead of `.env` for persistent service, pool, model, API-key,
TLS, notification, and logging configuration.

## 3. Start locally with native binaries

Run the daemon in development mode:

```bash
cargo run -p kproxyd
```

On first startup with the example `.env`, `.kproxy-dev/` contains:

- `config.toml`: daemon configuration;
- `accounts.json`: accounts and credentials, mode `0600` on Unix;
- `daily.json`: daily credit accounting;
- `stats.json`: aggregate request statistics;
- `admin.sock`: local administration socket;
- `logs/`: logs split by UTC date and severity.

Use another terminal for the CLI. It loads the same `.env` automatically:

```bash
cargo run -p kproxy -- status
cargo run -p kproxy -- health
cargo run -p kproxy -- service list
cargo run -p kproxy -- config path
cargo run -p kproxy -- config show --effective
cargo run -p kproxy -- account list
```

To run compiled binaries instead:

```bash
cargo build --release --locked
./target/release/kproxyd
./target/release/kproxy status
```

Only one daemon may own a given administration socket. A stale socket left by a
crashed process is removed automatically; a socket accepting connections is not
deleted by a second daemon.

A fresh daemon intentionally starts with no business API proxy. `kproxy health`
still returns success because daemon health is independent of account and proxy
service availability. Use `kproxy ready` (or a proxy listener's `/ready`
endpoint) when monitoring business readiness; it reports unavailable accounts,
failed listeners, metering recovery, and stale background tasks without stopping
the daemon. Create a service explicitly when it is needed:

```bash
cargo run -p kproxy -- service create --name main
```

The command creates the service's first scoped API key and prints the plaintext
key. Use `kproxy service apikeys main` to inspect key metadata without secrets, or
add `--show-secret` to retrieve plaintext keys bound only to that service. The
default listener is `0.0.0.0`; use `--host 127.0.0.1` when only local access is
required.

### Administration-only mode

Prevent all configured proxy services from listening while keeping account and
configuration administration available:

```bash
KPROXY_DISABLE_HTTP=1 cargo run -p kproxyd
```

This is useful for storage maintenance and CLI-only tests. It does not disable
`admin.sock`.

## 4. Add or import accounts

Import existing credentials from JSON:

```bash
cargo run -p kproxy -- account import --file accounts.json
cat accounts.json | cargo run -p kproxy -- account import --stdin
```

The CLI can generate missing `id`, `machine_id`, and `created_at` fields. After
importing, inspect and probe the accounts:

```bash
cargo run -p kproxy -- account list
cargo run -p kproxy -- account probe --all
cargo run -p kproxy -- models
```

The default build includes IAM Identity Center login. First set the global start
URL in `config.toml` (for example through `kproxy config edit`):

```toml
[sso]
start_url = "https://example.awsapps.com/start"
```

Manual account additions can then omit `--start-url`:

```bash
printf '%s\n' "$PASSWORD" | cargo run -p kproxy -- account add-sso \
  --email user@example.com \
  --password-stdin
```

Use `--start-url` to override the global value for one login. `kproxy` sends the
administration request to `kproxyd`, so browser login is unavailable if the daemon
was explicitly built with `--no-default-features`. Use `--headful` for MFA or
manual verification. Only organization SSO for Kiro enterprise accounts is
supported; personal, social-login, and other authentication types are not.

## 5. Create and verify a proxy service

If one has not been created yet, create a proxy and save the returned API key:

```bash
kproxy service create --name main --port 5580
kproxy service list
kproxy service apikeys main --show-secret
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

Stop and remove a service when it is no longer needed. API keys used only by
that service are deleted with it. Keys shared with another proxy service are
retained.

```bash
kproxy service delete main
```

Destructive commands such as deletion and usage reset have no `--yes` bypass;
enter `y` or `yes` in an interactive terminal to confirm them.

## 6. Configuration and hot reload

Print the active paths and validate the current configuration:

```bash
cargo run -p kproxy -- config path
cargo run -p kproxy -- config validate
```

The daemon watches `config.toml` with a short debounce. Valid changes are applied
automatically; invalid TOML or values leave the previous configuration active.
You can also trigger reload explicitly:

```bash
cargo run -p kproxy -- config reload
```

`server.host` and `server.port` are defaults used by `kproxy service create`; they
do not create a listener themselves. Proxy service additions and address changes
are reconciled at runtime. The following changes require a daemon restart:

- `admin.socket`;
- switching the shared listener mode between HTTP and HTTPS.

Log filters, formatting, output paths, pool behavior, model rules, notification
settings, and TLS certificate contents can otherwise be updated at runtime.

The alert policy, notification targets, and model mappings can be managed
directly from the CLI. Each command validates the result, writes it atomically,
and hot reloads the daemon:

```bash
kproxy alert events
kproxy alert config --low-credit-threshold-percent 10 --max-notifications 5 --suppress-window 30m
kproxy alert add --name alerts --kind dingtalk --url https://example/hook --event token-expired,quota-exhausted
kproxy alert edit alerts --event token-expired --event quota-exhausted
kproxy alert delete alerts

kproxy model-map add --name low-credit --source 'claude-opus-*' \
  --target claude-sonnet-4.6 --below-credits-percent 10
kproxy model-map edit low-credit --below-credits-percent 15
kproxy model-map test claude-opus-4.6 --remaining-credits-percent 8
kproxy model-map delete low-credit
```

`kproxy alert events` explains when each event is emitted. Alert targets can
subscribe to multiple events by repeating `--event` or by passing a
comma-separated list; `alert edit --event ...` replaces the target's complete
subscription list.

A mapping with `--below-credits-percent` is evaluated against each selected
account's remaining credits. With no schedule it is active all day: it matches
below the threshold and stops matching automatically after the monthly quota
recovers above that threshold.

Automatic model discovery is separate from explicit model mapping. It runs once
at daemon startup, is triggered again after account changes, and then follows
`models.cache_ttl_ms`. The account `status_check` task refreshes usage data only;
it does not issue a second set of model-list requests.

Bootstrap never overwrites an existing `config.toml`. A data directory created
by an older release may therefore retain `server.host = "127.0.0.1"`. Inspect
the effective value with `kproxy config show --effective` and use `kproxy config edit`
when the current default of `0.0.0.0` is desired.

`KPROXY_HTTP_PORT` is a process override. Changing `server.port` does not supersede
that environment variable until the variable is removed and the daemon is
restarted.

## 7. Logs and trace IDs

With `KPROXY_HOME=.kproxy-dev`, logs are written below `.kproxy-dev/logs/`. Files are
named like:

```text
kproxyd-2026-08-10-info.log
kproxyd-2026-08-10-warn.log
kproxyd-2026-08-10-error.1.log
```

Logs are split by severity and UTC date. The default maximum is 100 MB per shard,
and the default retention period is three days. `log.file_path` controls the
base path; an empty value uses the data directory's `logs/kproxyd.log` base.

Search by the response trace ID:

```bash
rg 'trace_f028' .kproxy-dev/logs/
```

Or follow records through the administration API:

```bash
cargo run -p kproxy -- logs -f --level warn
cargo run -p kproxy -- stats --since 1h
cargo run -p kproxy -- stats --detail --since 1h --by endpoint
```

`kproxy stats` reports aggregate request, success, token, credit, and latency
metrics. The default is a compact summary; `--detail` adds recent requests and
grouping by model/account/apikey/endpoint. Use `kproxy logs` and trace IDs for
individual failures.

For more detail, set `RUST_LOG` or `log.level` to `debug` or `trace`. Logs do not
record prompts, generated response bodies, or API-key values.

## 8. Docker Compose

Build and start the default full image with all features and Chromium SSO:

```bash
./deploy/docker-setup.sh
kproxy health
```

This command validates Compose, builds the image, starts the service, waits for
health, and installs the host `kproxy` command. It targets
`/usr/local/bin/kproxy` by default; without sudo access, use
`--target "$HOME/.local/bin/kproxy"`. The equivalent manual commands are useful
for debugging:

```bash
docker compose config --quiet
docker compose up -d --build
docker compose ps
docker compose exec kproxyd kproxy health
docker compose logs -f kproxyd
```

On Linux Docker Engine, `failed to populate volume` together with a missing
`.../volumes/kiro-proxy_kproxy-data/_data` path means Docker retained the named
volume metadata while its data directory disappeared. The setup script checks
for this before building. It asks before repairing in an interactive terminal;
CI and other non-interactive environments can opt in explicitly:

```bash
./deploy/docker-setup.sh --no-build --repair-volume
```

Repair applies only to a volume labeled for the current Compose project whose
data path is already missing. If Docker's volume root, disk mount, or a symlink
is unsafe, the script stops and requires storage recovery instead of deleting
anything.

Compose uses `network_mode: host`. A proxy listener created inside the container
therefore binds directly in the Docker host's network namespace. Arbitrary
service ports become available immediately without editing Compose or recreating
the container. This mode is supported directly by Docker Engine on Linux. On
Docker Desktop 4.34 or newer, enable host networking under Settings > Resources
> Network before starting the stack.

The image sets `KPROXY_HOME=/var/lib/kproxy`; Compose mounts the `kproxy-data` named
volume there. Do not copy the development `.env.example` into the container or
bind-mount `.kproxy-dev`. Change persistent settings through `config.toml` with
`kproxy config edit`, or add an explicit Compose environment entry when a
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
docker compose exec kproxyd kproxy version
docker compose exec kproxyd kproxy config show --effective
```

### Use `kproxy` directly on the Docker host

Docker cannot safely install files into the host's `/usr/local/bin` from a
Dockerfile or Compose service. The one-step script installs the provided wrapper
on the host and brings the Compose service to a healthy state:

```bash
./deploy/docker-setup.sh
kproxy health
kproxy status
kproxy service list
```

The wrapper locates the running daemon by the `io.kiro-proxy.role=daemon`
container label, preserves command exit codes, forwards stdin, and allocates a
TTY only for interactive use. This keeps the admin Unix socket private and
avoids host/container binary compatibility problems.

To install only the wrapper, invoke the low-level installer directly. It updates
a wrapper managed by this project but refuses to overwrite other commands by
default:

```bash
sudo ./deploy/install-kproxy-wrapper.sh
./deploy/install-kproxy-wrapper.sh --target "$HOME/.local/bin/kproxy"
# Use this only when intentionally replacing another command with the same name:
sudo ./deploy/install-kproxy-wrapper.sh --force
```

The current host user must be allowed to access Docker. When more than one
kiro-proxy stack is running, select one by Compose project or container:

```bash
export KPROXY_COMPOSE_PROJECT=kiro-proxy
# Or: export KPROXY_DOCKER_CONTAINER=<container-name-or-id>
kproxy status
```

The host wrapper also manages the Docker service lifecycle directly:

```bash
kproxy restart
kproxy stop
kproxy uninstall
kproxy uninstall --backup-dir /srv/kproxy-backups
```

`restart` waits for the container health check, and a stopped service remains
available to `restart`. `uninstall` stops the daemon and backs up
`/var/lib/kproxy` to the host before removing the container, persistent data
volume, unshared image, and installed wrapper. The default backup root is
`~/.kproxy/backups`; use `--backup-dir` or `KPROXY_BACKUP_DIR` to override it.
A failed backup aborts the uninstall and restarts the original container.
Interactive use asks whether to retain the backup; `--yes` keeps it unless
`--delete-backup` is also explicit. The source checkout is always retained.

On a fresh volume, explicitly create the proxy and save the API key printed by
the command:

```bash
docker compose exec kproxyd kproxy status
docker compose exec -T kproxyd kproxy account import --stdin < accounts.json
docker compose exec kproxyd kproxy account probe --all
docker compose exec kproxyd kproxy service create --name main
docker compose exec kproxyd kproxy service create --name secondary --port 6000
docker compose exec kproxyd kproxy service list
docker compose exec kproxyd kproxy service apikeys main --show-secret
docker compose exec kproxyd kproxy config show --effective
docker compose exec kproxyd sh -c 'ls -lh /var/lib/kproxy/logs'
curl -i http://127.0.0.1:5580/health
curl -i http://127.0.0.1:6000/health
```

Remove a proxy listener when it is no longer needed. Its exclusive API keys are
deleted with it, while keys shared with another service are retained:

```bash
docker compose exec kproxyd kproxy service delete secondary
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

The default Docker target is `runtime-full`; it installs the official Chromium
snapshot `r1566079`, enables all features, and sets the container-specific
no-sandbox flag. The snapshot is the exact revision used by the CDP definitions
in `chromiumoxide 0.9.1`; update and test both pins together instead of allowing
an OS package upgrade to change CDP independently. Change the Compose target to
`runtime-slim` and rebuild only when browser SSO is explicitly not needed.

BuildKit keeps Cargo registry and target caches between builds. The full target
builds only the all-features binaries and waits for that release build before
performing an uncached Chromium installation, which bounds peak memory and disk
pressure on smaller hosts. Compose defaults `CARGO_BUILD_JOBS` to `1`; increase
it only on a builder with sufficient memory.

## 9. systemd

Build release binaries and install them with the provided unit:

```bash
cargo build --release --locked

sudo useradd --system --home-dir /var/lib/kproxy --shell /usr/sbin/nologin kproxy
sudo install -m 0755 target/release/kproxyd target/release/kproxy /usr/local/bin/
sudo install -m 0644 deploy/kproxyd.service /etc/systemd/system/kproxyd.service
sudo systemctl daemon-reload
sudo systemctl enable --now kproxyd
sudo systemctl status kproxyd
```

If the `kproxy` user already exists, skip `useradd`. The unit uses `/etc/kproxy`,
`/var/lib/kproxy`, and `/run/kproxy` through systemd-managed directories.

```bash
sudo -u kproxy kproxy --socket /run/kproxy/admin.sock status
sudo journalctl -u kproxyd -f
sudo systemctl reload kproxyd
```

Reload sends `SIGHUP`. Restart-required settings still need
`sudo systemctl restart kproxyd`.

Install Chrome or Chromium on the host before using `kproxy account add-sso`.
The provided unit supports the default full build: it intentionally leaves user
namespaces and executable JIT memory available for Chromium, while retaining
`NoNewPrivileges`, filesystem protection, an empty capability set, and the other
hardening controls. If the host kernel disables unprivileged user namespaces,
prefer enabling them. As a last-resort compatibility override, set
`KPROXY_CHROMIUM_NO_SANDBOX=1` with `systemctl edit kproxyd`; this disables
Chromium's own sandbox and should only be used after reviewing the host's
isolation boundary.

## 10. VS Code and LLDB

Install rust-analyzer and CodeLLDB, then add a launch configuration such as:

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "type": "lldb",
      "request": "launch",
      "name": "Debug kproxyd",
      "cargo": {
        "args": ["build", "-p", "kproxyd"],
        "filter": { "name": "kproxyd", "kind": "bin" }
      },
      "cwd": "${workspaceFolder}",
      "env": {
        "KPROXY_HOME": "${workspaceFolder}/.kproxy-dev",
        "RUST_LOG": "kproxyd=debug,kproxy_kiro=debug,kproxy_pool=debug",
        "RUST_BACKTRACE": "1"
      }
    }
  ]
}
```

Command-line LLDB is also available:

```bash
cargo build -p kproxyd
rust-lldb target/debug/kproxyd
```

Inside LLDB, set process variables with
`settings set target.env-vars KPROXY_HOME=... RUST_LOG=debug`, add breakpoints with
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
cargo test -p kproxy-kiro
cargo test -p kproxyd http::tests::every_response_has_a_unique_trace_id
cargo test -p kproxy-pool refresh::tests::successful_refresh_preserves_cooling_and_exhausted_health
```

Wiremock and end-to-end tests bind temporary loopback ports. Sandboxed CI runners
must allow local port binding.

## 12. Troubleshooting

### `Address already in use`

Choose a free port when creating the service, or use the process override for a
service configured with the default port:

```bash
kproxy service create --name main --port 5581
KPROXY_HTTP_PORT=5581 cargo run -p kproxyd
```

### Cannot connect to `admin.sock`

Confirm `kproxyd` and `kproxy` load the same `KPROXY_HOME`. Use `kproxy config path` when the
daemon is reachable, or pass the socket explicitly:

```bash
kproxy --socket /path/to/admin.sock status
```

Remember that `KPROXY_ADMIN_SOCKET` changes the CLI target only. Set
`admin.socket` in `config.toml` and restart `kproxyd` to move the daemon socket.

### Configuration changes do not apply

Run `kproxy config validate`, inspect warn/error logs, and check whether the changed
field requires restart. Also check for a process-level `KPROXY_HTTP_PORT` override.

### Claude routes return access denied

Claude routes validate the client User-Agent by default. Use a supported Claude
Code-compatible User-Agent or explicitly change `server.enforce_user_agent_check`
in a trusted deployment.

### Generation returns `503`

Run `kproxy account list` and `kproxy account probe --all`. There may be no Available
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
