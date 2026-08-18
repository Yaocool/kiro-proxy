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
repair_volume=0

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
  --repair-volume       Recreate a Compose-owned volume whose data path is missing
  -h, --help            Show this help

Examples:
  ./deploy/$script_name
  ./deploy/$script_name --target "\$HOME/.local/bin/kproxy"
  ./deploy/$script_name --no-build
  ./deploy/$script_name --no-build --repair-volume
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
    --repair-volume)
      repair_volume=1
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

host_path_state() {
  path="$1"
  parent="$(dirname "$path")"
  nearest_parent="$parent"
  while [ ! -e "$nearest_parent" ] && [ ! -L "$nearest_parent" ] && [ "$nearest_parent" != "/" ]; do
    nearest_parent="$(dirname "$nearest_parent")"
  done

  if [ -L "$path" ]; then
    echo symlink
  elif [ -d "$path" ]; then
    echo directory
  elif [ -e "$path" ]; then
    echo other
  elif [ "$(id -u)" -eq 0 ] || [ -x "$nearest_parent" ]; then
    echo missing
  elif command -v sudo >/dev/null 2>&1; then
    if sudo test -L "$path"; then
      echo symlink
    elif sudo test -d "$path"; then
      echo directory
    elif sudo test -e "$path"; then
      echo other
    elif sudo test -x "$nearest_parent"; then
      echo missing
    else
      echo unknown
    fi
  else
    echo unknown
  fi
}

create_data_volume() {
  docker volume create \
    --driver local \
    --label "com.docker.compose.project=$selected_project" \
    --label "com.docker.compose.volume=kproxy-data" \
    "$data_volume_name" >/dev/null
}

prepare_data_volume() {
  volume_was_created=0
  if ! docker volume inspect "$data_volume_name" >/dev/null 2>&1; then
    echo "==> Creating Docker data volume $data_volume_name"
    create_data_volume
    volume_was_created=1
  fi

  volume_driver="$(docker volume inspect --format '{{.Driver}}' "$data_volume_name")"
  [ "$volume_driver" = "local" ] || {
    echo "==> Using Docker data volume $data_volume_name with driver $volume_driver"
    return
  }

  # Docker Desktop keeps the daemon's volume paths inside its Linux VM, so the
  # host cannot validate them directly. The affected stale-path failure occurs
  # on a native Linux Docker Engine and is checked below.
  [ "$(uname -s)" = "Linux" ] || return

  mountpoint="$(docker volume inspect --format '{{.Mountpoint}}' "$data_volume_name")"
  [ -n "$mountpoint" ] || fail "Docker returned an empty mountpoint for volume $data_volume_name"
  mountpoint_state="$(host_path_state "$mountpoint")"
  case "$mountpoint_state" in
    directory)
      return
      ;;
    unknown)
      echo "==> Warning: cannot verify Docker volume path $mountpoint; Docker will validate it during startup" >&2
      return
      ;;
    symlink|other)
      fail "Docker volume path is not a directory: $mountpoint; inspect Docker's data-root before continuing"
      ;;
  esac

  volume_dir="$(dirname "$mountpoint")"
  volume_root="$(dirname "$volume_dir")"
  volume_root_state="$(host_path_state "$volume_root")"
  [ "$volume_root_state" = "directory" ] || \
    fail "Docker volume root is unavailable: $volume_root; restore its disk mount or data-root before continuing"

  [ "$volume_was_created" -ne 1 ] || \
    fail "Docker created $data_volume_name but did not create its data path: $mountpoint; inspect the Docker daemon storage"

  volume_dir_state="$(host_path_state "$volume_dir")"
  case "$volume_dir_state" in
    directory|missing) ;;
    *)
      fail "Docker volume directory has an unsafe state ($volume_dir_state): $volume_dir; inspect it manually"
      ;;
  esac

  volume_project="$(docker volume inspect --format '{{index .Labels "com.docker.compose.project"}}' "$data_volume_name")"
  volume_logical_name="$(docker volume inspect --format '{{index .Labels "com.docker.compose.volume"}}' "$data_volume_name")"
  if [ "$volume_project" != "$selected_project" ] || [ "$volume_logical_name" != "kproxy-data" ]; then
    fail "volume $data_volume_name has a missing data path but is not owned by this Compose project; refusing to remove it"
  fi

  echo "==> Docker volume metadata exists but its data path is missing: $mountpoint" >&2
  if [ "$repair_volume" -ne 1 ]; then
    if [ -t 0 ]; then
      printf "Recreate the empty/broken volume %s? Existing inaccessible data will not be recoverable [y/N] " "$data_volume_name" >&2
      read -r answer
      case "$answer" in
        y|Y|yes|YES|Yes) ;;
        *) fail "volume repair cancelled; restore the missing data path or rerun with --repair-volume" ;;
      esac
    else
      fail "rerun with --repair-volume to recreate this broken volume non-interactively"
    fi
  fi

  echo "==> Recreating broken Docker data volume $data_volume_name"
  compose down --remove-orphans
  volume_users="$(docker ps --all --quiet --filter "volume=$data_volume_name")"
  [ -z "$volume_users" ] || \
    fail "volume $data_volume_name is still referenced by container(s): $volume_users"
  docker volume rm "$data_volume_name" >/dev/null
  create_data_volume

  repaired_mountpoint="$(docker volume inspect --format '{{.Mountpoint}}' "$data_volume_name")"
  repaired_state="$(host_path_state "$repaired_mountpoint")"
  [ "$repaired_state" = "directory" ] || \
    fail "Docker recreated $data_volume_name but its data path is still unavailable: $repaired_mountpoint"
}

echo "==> Validating Docker Compose configuration"
resolved_config="$(compose config)"
selected_project="$(printf '%s\n' "$resolved_config" | awk '
  /^name:[[:space:]]/ {
    sub(/^name:[[:space:]]*/, "")
    print
    exit
  }
')"
data_volume_name="$(printf '%s\n' "$resolved_config" | awk '
  $0 == "volumes:" { in_volumes = 1; next }
  in_volumes && $0 == "  kproxy-data:" { in_data_volume = 1; next }
  in_data_volume && /^    name:[[:space:]]/ {
    sub(/^    name:[[:space:]]*/, "")
    print
    exit
  }
')"
[ -n "$selected_project" ] || fail "could not resolve the Docker Compose project name"
[ -n "$data_volume_name" ] || fail "could not resolve the kproxy-data Docker volume name"
prepare_data_volume

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
