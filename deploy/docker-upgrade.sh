#!/bin/sh
set -eu

script_name="$(basename "$0")"
script_dir="$(CDPATH= cd "$(dirname "$0")" && pwd)"
setup_script="$script_dir/docker-setup.sh"
upgrade_image="${KPROXY_UPGRADE_IMAGE:-ghcr.io/yaocool/kiro-proxy:latest}"

usage() {
  cat <<EOF
Usage: $script_name [DOCKER-SETUP-OPTIONS]

Pull and deploy the latest published stable kiro-proxy image. The existing
container keeps running during the pull, and docker-setup.sh restores the
previous image if startup or the health check fails.

Environment:
  KPROXY_UPGRADE_IMAGE  Image/channel to follow
                        (default: ghcr.io/yaocool/kiro-proxy:latest)

Examples:
  ./deploy/$script_name
  ./deploy/$script_name --timeout 120
  KPROXY_UPGRADE_IMAGE=ghcr.io/yaocool/kiro-proxy:edge ./deploy/$script_name

Safe docker-setup options such as --target, --project-name, --timeout, --force,
and --repair-volume are forwarded. Build, no-pull, and a second --image option
are rejected so this command always performs a registry upgrade.
EOF
}

fail() {
  echo "$script_name: $*" >&2
  exit 1
}

[ -x "$setup_script" ] || fail "setup script is not executable: $setup_script"
[ -n "$upgrade_image" ] || fail "KPROXY_UPGRADE_IMAGE must not be empty"

for argument in "$@"; do
  case "$argument" in
    -h|--help)
      usage
      exit 0
      ;;
    --build|--no-build|--no-pull|--image|--image=*)
      fail "$argument is not allowed; set KPROXY_UPGRADE_IMAGE to select another registry image"
      ;;
  esac
done

exec "$setup_script" --image "$upgrade_image" "$@"
