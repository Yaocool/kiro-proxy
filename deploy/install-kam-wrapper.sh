#!/bin/sh
set -eu

target="/usr/local/bin/kam"
force=0

usage() {
  cat <<'EOF'
Usage: install-kam-wrapper.sh [--force] [--target PATH]

Installs the Docker-backed kam wrapper. Run with sudo when the target directory
requires administrator privileges.
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --force)
      force=1
      shift
      ;;
    --target)
      [ "$#" -ge 2 ] || {
        echo "install-kam-wrapper.sh: --target requires a path" >&2
        exit 2
      }
      target="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "install-kam-wrapper.sh: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

script_dir="$(CDPATH= cd "$(dirname "$0")" && pwd)"
source_file="$script_dir/kam-docker"

[ -f "$source_file" ] || {
  echo "install-kam-wrapper.sh: wrapper not found: $source_file" >&2
  exit 1
}

if [ -e "$target" ] && [ "$force" -ne 1 ]; then
  echo "install-kam-wrapper.sh: target already exists: $target" >&2
  echo "rerun with --force to replace it, or use --target for another path" >&2
  exit 1
fi

install -m 0755 "$source_file" "$target"
echo "installed Docker-backed kam wrapper at $target"
echo "run 'kam health' after the Compose service is running"
