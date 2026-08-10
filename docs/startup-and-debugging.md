# Startup and debugging guide

[English](startup-and-debugging.md) | [简体中文](startup-and-debugging.zh-CN.md)

This guide covers local development, release binaries, Docker Compose, systemd,
logs, trace IDs, LLDB, tests, and common startup failures. Unless noted
otherwise, run commands from the repository root.

## 1. Prerequisites

The repository pins Rust 1.97.1 and requests `rustfmt`, `clippy`, and
`rust-analyzer` in `rust-toolchain.toml`. With rustup installed, entering the
repository selects the correct toolchain automatically.

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
| `KAM_HTTP_PORT` | Overrides only the business HTTP/HTTPS listening port for this daemon process. |
| `KAM_DISABLE_HTTP=1` | Disables the business HTTP plane while leaving the Unix administration socket running. |
| `KAM_ADMIN_SOCKET` | Overrides the socket used by the `kam` CLI; it does not reconfigure `kamd`. |
| `KAM_CODEWHISPERER_URL` | Overrides the CodeWhisperer upstream URL for integration tests or controlled proxies. |
| `KAM_AMAZONQ_URL` | Overrides the Amazon Q upstream URL for integration tests or controlled proxies. |
| `RUST_LOG` | Sets tracing filters for console and application diagnostics. |
| `RUST_BACKTRACE` | Enables Rust backtraces when set to `1` or `full`. |

Use `config.toml` instead of `.env` for persistent service, pool, model, API-key,
TLS, notification, and logging configuration.

## 3. Start locally

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

### Administration-only mode

Disable the public business API while keeping account and configuration
administration available:

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

## 5. Verify the service

The default business address is `http://127.0.0.1:5580`.

```bash
curl -i http://127.0.0.1:5580/health

curl -i http://127.0.0.1:5580/v1/messages/count_tokens \
  -H 'content-type: application/json' \
  -H 'user-agent: claude-cli/1.0 (external, debug)' \
  -d '{"model":"claude-sonnet-4","messages":[{"role":"user","content":"hello"}]}'
```

`GET /health` and local token counting work before an upstream account is
available. A generation request returning `503` is expected when no account is
schedulable.

When API keys are enabled, add an authorization header:

```bash
curl -i http://127.0.0.1:5580/v1/models \
  -H 'authorization: Bearer <key>'
```

Binding `server.host` to a non-loopback address is rejected unless at least one
enabled API key is configured. This validation prevents accidental unauthenticated
public exposure.

Every business HTTP response includes `x-trace-id`. Save that value when
investigating an error.

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

The following changes require a daemon restart:

- `server.host`;
- `server.port`;
- `admin.socket`;
- switching the listener between HTTP and HTTPS.

Log filters, formatting, output paths, pool behavior, model rules, notification
settings, and TLS certificate contents can otherwise be updated at runtime.

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
docker compose build --pull
docker compose up -d
docker compose ps
docker compose logs -f kamd
```

Compose publishes `127.0.0.1:5580` on the host and stores state in the
`kam-data` named volume. Inside the container, `kamd` listens on loopback port
5581 and `socat` forwards container port 5580 to it. This preserves the daemon's
loopback-only safety rule while allowing Docker's host-side loopback mapping.

```bash
docker compose exec kamd kam status
docker compose exec kamd kam config show --effective
docker compose exec kamd sh -c 'ls -lh /var/lib/kam/logs'
curl -i http://127.0.0.1:5580/health
```

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

Stop the process using port 5580 or temporarily choose another port:

```bash
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
