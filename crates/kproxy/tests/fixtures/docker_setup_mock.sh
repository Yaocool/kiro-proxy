#!/bin/sh
set -eu

# Intentionally never call a real Docker binary, even for unexpected arguments.
printf '%s | %s\n' "${KPROXY_IMAGE:-}" "$*" >> "$DOCKER_SETUP_TEST_DIR/docker-calls"
case "$1" in
  info) exit 0 ;;
  volume)
    case "$*" in
      'volume inspect kiro-proxy_kproxy-data') exit 0 ;;
      'volume inspect --format {{.Driver}} kiro-proxy_kproxy-data') echo local; exit 0 ;;
      'volume inspect --format {{.Mountpoint}} kiro-proxy_kproxy-data')
        printf '%s\n' "$DOCKER_SETUP_TEST_DIR"; exit 0 ;;
    esac
    ;;
  inspect)
    [ "$*" != 'inspect --format {{.Image}} test-container' ] || {
      echo sha256:test-old; exit 0
    }
    ;;
  image)
    [ "$*" != 'image tag sha256:test-old kiro-proxy-rollback:kiro-proxy' ] || exit 0
    ;;
  ps) echo test-container; exit 0 ;;
  exec)
    [ "$*" != 'exec -i -e KPROXY_WRAPPER_HOST_DATA_DIR test-container /usr/local/bin/kproxy health' ] || {
      echo ok; exit 0
    }
    ;;
  compose)
    shift
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --project-directory|--project-name|--file) shift 2 ;;
        *) break ;;
      esac
    done
    case "$*" in
      version) echo 'Docker Compose test stub'; exit 0 ;;
      config)
        printf 'name: kiro-proxy\nvolumes:\n  kproxy-data:\n    name: kiro-proxy_kproxy-data\n'
        exit 0 ;;
      'ps --all --quiet kproxyd') echo test-container; exit 0 ;;
      'up --detach --build'|'up --detach --no-build'|'logs --tail 80 kproxyd') exit 0 ;;
      'exec --no-tty '* )
        echo 'unknown flag: --no-tty' >&2
        exit 16 ;;
      'exec -T kproxyd /usr/local/bin/kproxy health')
        if [ "$KPROXY_IMAGE" = 'kiro-proxy-rollback:kiro-proxy' ]; then
          [ "$DOCKER_SETUP_TEST_SCENARIO" = rollback-unhealthy ] || { echo ok; exit 0; }
        else
          case "$DOCKER_SETUP_TEST_SCENARIO" in
            healthy) echo ok; exit 0 ;;
            transient)
              [ ! -f "$DOCKER_SETUP_TEST_DIR/retried" ] || { echo ok; exit 0; }
              touch "$DOCKER_SETUP_TEST_DIR/retried"
              ;;
          esac
        fi
        printf 'admin socket unavailable: %s\n' "$KPROXY_IMAGE" >&2
        exit 7 ;;
    esac
    ;;
esac
printf 'UNEXPECTED %s\n' "$*" >> "$DOCKER_SETUP_TEST_DIR/docker-calls"
printf 'Unexpected Docker arguments: %s\n' "$*" >&2
exit 99
