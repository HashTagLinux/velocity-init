#!/bin/sh
# OpenRC inside a container: openrc-init as PID 1.
#
# openrc-init is OpenRC's minimal PID 1 (designed for containers): it
# mounts the basics, runs the sysinit/boot runlevels and then the
# default runlevel (where bench-svc lives), then idles as PID 1. A
# SIGTERM from `docker stop` makes it run the shutdown runlevel
# (bench-svc's stop function) and exit — the container then exits
# cleanly.
#
# Path varies by distro: Debian installs it as /usr/sbin/openrc-init,
# Alpine-style builds as /sbin/openrc-init.
if [ -x /usr/sbin/openrc-init ]; then
  exec /usr/sbin/openrc-init
elif [ -x /sbin/openrc-init ]; then
  exec /sbin/openrc-init
elif [ -x /sbin/init ]; then
  # Last resort: sysvinit (runs the OpenRC runlevels via init.d/rc).
  exec /sbin/init
else
  echo "openrc-init not found; cannot boot" >&2
  sleep 3600
fi
