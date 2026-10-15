#!/bin/sh
# Stand-in for `agetty` in the end-to-end tests.
#
# The built-in console getty is what velocity-init injects when no
# getty-like service exists, and it is restarted on *every* exit
# (RestartPolicy::Always). Pointing the tests at a real /sbin/agetty
# would put a genuine login prompt on the host's /dev/console and
# respawn it forever; pointing them at `/bin/true` instead exits 0
# instantly and burns the crash-loop budget on every single run
# (six forks per test, then "respawn limit reached").
#
# A long-lived, idle process models the real thing: the getty is
# spawned, stays up for the whole test, and dies cleanly on SIGTERM
# during teardown — exactly the path the supervisor must handle.
#
# `exec` matters: a shell defers a SIGTERM that arrives while it is
# waiting for a foreground child, so a plain `sleep` loop would only die
# when the escalation timer SIGKILLed it — and if the test process were
# killed outright, the stand-in would survive as an orphan on the
# machine running the suite. Replacing the shell with `sleep` means the
# very signal we send reaches the sleeping process directly.
exec sleep 2147483647