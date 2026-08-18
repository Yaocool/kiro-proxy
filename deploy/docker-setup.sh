#!/bin/sh
set -eu

script_name="$(basename "$0")"
script_dir="$(CDPATH= cd "$(dirname "$0")" && pwd)"
repo_root="$(CDPATH= cd "$script_dir/.." && pwd)"
compose_file="$repo_root/docker-compose.yml"

target="/usr/local/bin/kproxy"
project_name=""
health_timeout=60
build=1
force=0

usage() {
  cat <<EOF
Usage: $script_name [OPTIONS]

Set up kiro-proxy with Docker Compose and install a Docker-backed kproxy command
on the host.

Options:
  --target PATH         Host command path (default: /usr/local/bin/kproxy)
  --project-name NAME   Override the Docker Compose project name
  --timeout SECONDS     Health-check timeout (default: 60)
  --no-build            Start the existing image without rebuilding it
  --force               Replace an unrelated command already at --target
  -h, --help            Show this help

Examples:
  ./deploy/$script_name
  ./deploy/$script_name --target "\$HOME/.local/bin/kproxy"
  ./deploy/$script_name --no-build
EOF
}

fail() {
  echo "$script_name: $*" >&2
  exit 1
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --target)
      [ "$#" -ge 2 ] || fail "--target requires a path"
      target="$2"
      shift 2
      ;;
    --project-name)
      [ "$#" -ge 2 ] || fail "--project-name requires a name"
      project_name="$2"
      shift 2
      ;;
    --timeout)
      [ "$#" -ge 2 ] || fail "--timeout requires a number of seconds"
      health_timeout="$2"
      shift 2
      ;;
    --no-build)
      build=0
      shift
      ;;
    --force)
      force=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      fail "unknown option: $1 (use --help for usage)"
      ;;
  esac
done

[ -n "$target" ] || fail "--target must not be empty"
case "$health_timeout" in
  ''|*[!0-9]*) fail "--timeout must be a positive integer" ;;
esac
[ "$health_timeout" -gt 0 ] || fail "--timeout must be greater than zero"
[ -f "$compose_file" ] || fail "Compose file not found: $compose_file"

command -v docker >/dev/null 2>&1 || fail "docker command not found"
docker compose version >/dev/null 2>&1 || fail "Docker Compose v2 is required (docker compose)"
docker info >/dev/null 2>&1 || fail "cannot connect to Docker; start Docker and check your permissions"

compose() {
  if [ -n "$project_name" ]; then
    docker compose \
      --project-name "$project_name" \
      --project-directory "$repo_root" \
      --file "$compose_file" \
      "$@"
  else
    docker compose \
      --project-directory "$repo_root" \
      --file "$compose_file" \
      "$@"
  fi
}

echo "==> Validating Docker Compose configuration"
compose config --quiet

installer="$script_dir/install-kproxy-wrapper.sh"
[ -x "$installer" ] || fail "wrapper installer is not executable: $installer"
target_dir="$(dirname "$target")"
if [ ! -d "$target_dir" ]; then
  echo "==> Creating host command directory $target_dir"
  if mkdir -p "$target_dir" 2>/dev/null; then
    :
  elif [ "$(id -u)" -eq 0 ]; then
    install -d -m 0755 "$target_dir"
  else
    command -v sudo >/dev/null 2>&1 || \
      fail "cannot create $target_dir and sudo is unavailable; use --target with a writable path"
    sudo install -d -m 0755 "$target_dir"
  fi
fi

set -- "$installer" --target "$target"
if [ "$force" -eq 1 ]; then
  set -- "$@" --force
fi

echo "==> Installing the host kproxy command at $target"
if [ -w "$target_dir" ]; then
  "$@"
elif [ "$(id -u)" -eq 0 ]; then
  "$@"
else
  command -v sudo >/dev/null 2>&1 || \
    fail "$target_dir is not writable and sudo is unavailable; use --target with a writable path"
  sudo "$@"
fi

if [ "$build" -eq 1 ]; then
  echo "==> Building and starting kiro-proxy"
  compose up --detach --build
else
  echo "==> Starting kiro-proxy from the existing image"
  compose up --detach --no-build
fi

echo "==> Waiting for kproxyd to become healthy"
elapsed=0
while ! compose exec --no-tty kproxyd /usr/local/bin/kproxy health >/dev/null 2>&1; do
  if [ "$elapsed" -ge "$health_timeout" ]; then
    compose logs --tail 80 kproxyd >&2 || true
    fail "kproxyd did not become healthy within ${health_timeout}s"
  fi
  sleep 1
  elapsed=$((elapsed + 1))
done

selected_project="${project_name:-${COMPOSE_PROJECT_NAME:-kiro-proxy}}"
echo "==> Verifying the host command"
KPROXY_COMPOSE_PROJECT="$selected_project" "$target" health

host_command="kproxy"
case ":${PATH}:" in
  *:"$target_dir":*) ;;
  *) host_command="$target" ;;
esac

cat <<EOF

kiro-proxy is ready.

  Host command:  $target
  Compose stack: $selected_project
  Persistent data is stored in the kproxy-data Docker volume.

Next steps:
  $host_command status
  $host_command service create --name main
  $host_command account import --stdin < accounts.json
  $host_command account probe --all

Save the API key printed by 'service create'. To inspect the service:
  $host_command service list

Follow daemon logs:
  docker compose --project-name "$selected_project" -f "$compose_file" logs -f kproxyd
EOF
