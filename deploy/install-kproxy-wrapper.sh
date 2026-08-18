#!/bin/sh
set -eu

target="/usr/local/bin/kproxy"
force=0

usage() {
  cat <<'EOF'
Usage: install-kproxy-wrapper.sh [--force] [--target PATH]

Installs the Docker-backed kproxy wrapper. Run with sudo when the target directory
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
        echo "install-kproxy-wrapper.sh: --target requires a path" >&2
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
      echo "install-kproxy-wrapper.sh: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

script_dir="$(CDPATH= cd "$(dirname "$0")" && pwd)"
source_file="$script_dir/kproxy-docker"

[ -f "$source_file" ] || {
  echo "install-kproxy-wrapper.sh: wrapper not found: $source_file" >&2
  exit 1
}

if [ -e "$target" ]; then
  if cmp -s "$source_file" "$target"; then
    echo "Docker-backed kproxy wrapper is already installed at $target"
    exit 0
  fi

  managed=0
  if grep -qF "Managed by kiro-proxy's install-kproxy-wrapper.sh." "$target"; then
    managed=1
  else
    # Recognize the exact wrapper shipped before the managed marker was added.
    legacy_checksum="$(cksum "$target" 2>/dev/null || true)"
    case "$legacy_checksum" in
      "3654078470 1244 "*) managed=1 ;;
    esac
  fi

  if [ "$force" -ne 1 ] && [ "$managed" -ne 1 ]; then
    echo "install-kproxy-wrapper.sh: target already exists and is not managed by this project: $target" >&2
    echo "rerun with --force to replace it, or use --target for another path" >&2
    exit 1
  fi
fi

install -m 0755 "$source_file" "$target"
echo "installed Docker-backed kproxy wrapper at $target"
echo "run 'kproxy health' after the Compose service is running"
