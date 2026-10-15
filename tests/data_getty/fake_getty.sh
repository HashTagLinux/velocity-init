#!/bin/sh
# Fake getty for the Phase 5 tests.
#
# Real agetty spawns a login session and only exits when it ends
# (clean exit 0 = logout, non-zero = crash). This shim lets the tests
# emulate both:
#
#   GETTY_DELAY  — sleep before exiting, so the exit happens *after*
#                  the boot window and is handled by the SIGCHLD path.
#   GETTY_EXIT   — exit code (default 0).
#
# The supervisor routes stdout/stderr into `[getty@console]`-prefixed
# logs, so tests can assert on the invocation args.
echo "FAKE-GETTY invoked with: $*"
if [ -n "$GETTY_DELAY" ]; then
  sleep "$GETTY_DELAY"
fi
exit "${GETTY_EXIT:-0}"
