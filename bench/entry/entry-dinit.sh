#!/bin/sh
# Dinit inside a container: dinit as PID 1.
#
# The benchmark image builds dinit from source; it installs to /sbin
# (dinit-sbindir default). Prefer container mode (-o): dinit skips
# system-manager duties (console setup, shutdown command) and still
# starts the boot.d services; SIGTERM from `docker stop` stops all
# services and dinit exits. Older builds lack the flag, so probe
# --help first — that also avoids a double boot if a supported -o run
# later exits non-zero during shutdown.
if /sbin/dinit --help 2>&1 | grep -q -- "--container"; then
  exec /sbin/dinit -o
else
  exec /sbin/dinit
fi
