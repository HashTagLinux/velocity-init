#!/usr/bin/env bash
#
# sandbox.sh — run a command as a *real* PID 1, fully isolated from the
# machine it runs on.
#
# WHY THIS EXISTS
# ---------------
# velocity-init behaves very differently as PID 1: it mounts the base
# filesystems and /etc/fstab, tears every mount down at shutdown, writes
# /var/run/utmp and finally calls reboot(2). Running it on a development
# host with the host's own /etc/inittab therefore boots the machine a
# second time — real daemons started twice, the real console getty
# respawned, and RUN_LVL records appended to the host's live utmp.
#
# Docker would be the usual answer, but it needs a usable daemon socket
# (usually root:docker 0660). This wrapper needs no privileges at all:
# Linux user namespaces give us uid 0 inside a private namespace, and
# from there nothing done inside can reach the host.
#
# WHAT THE SANDBOX GUARANTEES
# ---------------------------
#   * --user/--map-root-user : root only inside the namespace
#   * --pid --fork --mount-proc: we are genuinely PID 1, with our own
#                               /proc — so the PID-1 code paths *do*
#                               run (mounts, teardown, reboot(2) attempt)
#   * --mount                : private mount table (MS_REC|MS_PRIVATE),
#                               so a stray umount cannot touch the host
#   * --net --uts --ipc      : abstract sockets (sd_notify) and hostname
#                               are invisible to and cannot collide with
#                               anything on the host
#   * tmpfs on /run          : control socket + journal are namespace-local
#   * tmpfs on /var/run      : **the host's utmp is unreachable** — the
#                              one file a non-PID-1 sysv run really does
#                              damage on a live machine
#   * cgroup writes          : still denied (the real cgroup2 fs belongs
#                              to the initial user namespace), so the
#                              cgroup paths no-op
#   * reboot(2)              : never issued — velocity-init probes the
#                              mapped uid map (plus VELOCITY_SANDBOX=1)
#                              and takes its nested-runtime path, exiting
#                              0. If it *were* called anyway, the kernel
#                              would turn it into a SIGINT for PID 1:
#                              CAP_SYS_BOOT is only held in the
#                              namespace, not in the initial user
#                              namespace
#
# USAGE
#   scripts/sandbox.sh [--inittab FILE] [--init-dir DIR] [--driver FILE]
#                      <command> [args...]
#
# --inittab/--init-dir bind-mount a fixture over /etc/inittab and
# /etc/init.d *inside the namespace only*, so sysv mode can be exercised
# end to end without the host ever being touched.
#
# --driver FILE runs FILE as a background child *before* handing PID 1
# to <command>. That is how a test drives the real init: the driver
# (waiting for the control socket, then issuing velocityctl commands)
# is a child of init, while init itself keeps the pid 1 it needs for
# the mount/teardown/reboot code paths. Running init in the background
# instead would silently test the non-PID-1 fallback.
#
# EXAMPLES
#   # Boot the LSB fixtures as a real PID 1, then power off:
#   scripts/sandbox.sh -- \
#       env VELOCITY_INIT_GETTY="$PWD/tests/data_getty/idle_getty.sh" \
#       target/debug/velocity-init --init-dir tests/data \
#           --inittab /nonexistent/inittab --log-level debug
#
#   # Full sysv (inittab drop-in) boot, with fixture rcS/rc scripts:
#   scripts/sandbox.sh --inittab tests/data_sysv/inittab \
#       --init-dir tests/data_sysv/init.d -- target/debug/velocity-init --mode sysv
#
set -euo pipefail

INITTAB=""
INITDIR=""
DRIVER=""
while [ $# -gt 0 ]; do
    case "$1" in
        --inittab) INITTAB="${2:?--inittab needs a path}"; shift 2 ;;
        --init-dir) INITDIR="${2:?--init-dir needs a path}"; shift 2 ;;
        --driver) DRIVER="${2:?--driver needs a path}"; shift 2 ;;
        --) shift; break ;;
        *) break ;;
    esac
done

if [ $# -eq 0 ]; then
    echo "usage: $0 [--inittab FILE] [--init-dir DIR] <command> [args...]" >&2
    exit 2
fi

# Absolute, so the namespace (which keeps our cwd) cannot confuse them.
abspath() {
    [ -n "$1" ] || return 0
    local dir base
    dir="$(cd "$(dirname "$1")" && pwd)"
    base="$(basename "$1")"
    printf '%s/%s' "$dir" "$base"
}
abs_inittab="$(abspath "$INITTAB")"
abs_initdir="$(abspath "$INITDIR")"
abs_driver="$(abspath "$DRIVER")"

# velocity-init reads this in final_reboot (src/namespaces.rs): it forces
# the nested-runtime path even if the uid-map probe ever misses the map.
export VELOCITY_SANDBOX=1

# The fixture paths and the command are passed as *arguments* to the
# inner shell, never interpolated into its source: this repository's own
# path contains spaces and parentheses, and quoting that into a `bash -c`
# string is a bug waiting to happen.
#   $0=sandbox  $1=inittab  $2=initdir  $3=driver  $4..=command
exec unshare --user --map-root-user --pid --fork --mount --mount-proc \
             --net --uts --ipc -- \
    /bin/bash -c '
set -eu
inittab="$1"; initdir="$2"; driver="$3"; shift 3
mount -t tmpfs -o mode=0755,nosuid,nodev tmpfs /run
mount -t tmpfs -o mode=0755,nosuid,nodev tmpfs /var/run
if [ -n "$inittab" ]; then
    [ -f "$inittab" ] || { echo "sandbox: no such inittab: $inittab" >&2; exit 2; }
    mount --bind "$inittab" /etc/inittab
fi
if [ -n "$initdir" ]; then
    [ -d "$initdir" ] || { echo "sandbox: no such init dir: $initdir" >&2; exit 2; }
    mount --bind "$initdir" /etc/init.d
fi
echo "sandbox: pid=$$ netns=$(readlink /proc/self/ns/net) /run+private /var/run+private" >&2
# The driver must be a child of the init we are about to become PID 1:
# start it first, then hand the pid over with exec.
if [ -n "$driver" ]; then
    "$driver" &
fi
exec "$@"
' sandbox "$abs_inittab" "$abs_initdir" "$abs_driver" "$@"
