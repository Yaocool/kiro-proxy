#!/bin/sh
set -eu

if [ "$#" -gt 0 ]; then
  exec "$@"
fi

# The daemon deliberately binds loopback when no API key is configured. In a
# container, publish that loopback listener through a tiny TCP forwarder while
# Docker still restricts the host-side mapping to 127.0.0.1.
if [ -n "${KAM_FORWARD_PORT:-}" ]; then
  internal_port="${KAM_HTTP_PORT:-5580}"
  socat "TCP-LISTEN:${KAM_FORWARD_PORT},bind=0.0.0.0,reuseaddr,fork" \
    "TCP:127.0.0.1:${internal_port}" &
fi

exec /usr/local/bin/kamd
