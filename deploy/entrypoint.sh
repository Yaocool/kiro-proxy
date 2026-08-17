#!/bin/sh
set -eu

if [ "$#" -gt 0 ]; then
  exec "$@"
fi

# Optional compatibility forwarder for bridge-network deployments. The default
# Compose setup uses host networking and does not set KPROXY_FORWARD_PORT.
if [ -n "${KPROXY_FORWARD_PORT:-}" ]; then
  internal_port="${KPROXY_HTTP_PORT:-5580}"
  socat "TCP-LISTEN:${KPROXY_FORWARD_PORT},bind=0.0.0.0,reuseaddr,fork" \
    "TCP:127.0.0.1:${internal_port}" &
fi

exec /usr/local/bin/kproxyd
