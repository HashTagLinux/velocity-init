#!/usr/bin/env bash
#
# velocity-init vs OpenRC vs Dinit — container benchmark.
#
# Measures, for each init system running as PID 1 in a container:
#   1. boot time          — docker run -d → the mock service is ready
#   2. resource usage     — RSS of PID 1, its children, and CPU% of PID 1
#   3. supervision        — SIGKILL the mock, time until it respawns
#   4. shutdown           — SIGTERM (docker stop) → container exit
#
# Usage: ./bench.sh [-i "openrc dinit velocity"] [-r N] [-p 1|0] [-h]
#
#   -i inits   space-separated subset of: openrc dinit velocity
#   -r N       repeat the whole benchmark N times (one row per run)
#   -p 1|0     privileged mode (default 1). 1 = --privileged
#              --cgroupns=host (mount/cgroup code paths run for real);
#              0 = safe mode (--cap-add SYS_ADMIN, seccomp still
#              blocks reboot(2), mount failures are logged non-fatal).
#   -h         this help
#
# Environment overrides:
#   BENCH_IMAGE           image tag to use (default: velocity-bench:latest)
#   BENCH_SKIP_BUILD=1    do not build the image if missing
#   BENCH_BOOT_TIMEOUT    seconds to wait for READY (default 30)
#   BENCH_RESTART_TIMEOUT seconds to wait for a respawn (default 5)
#   BENCH_STOP_TIMEOUT    docker stop -t value (default 60)
#
# Safety: with -p 1 the inits may attempt reboot(2) during the
# shutdown test; on a privileged container this can power off or
# reboot the HOST. Run on a disposable VM.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/.." && pwd)"

BENCH_IMAGE="${BENCH_IMAGE:-velocity-bench:latest}"
BENCH_BOOT_TIMEOUT="${BENCH_BOOT_TIMEOUT:-30}"
BENCH_RESTART_TIMEOUT="${BENCH_RESTART_TIMEOUT:-5}"
BENCH_STOP_TIMEOUT="${BENCH_STOP_TIMEOUT:-60}"

inits="openrc dinit velocity"
runs=1
privileged=1

usage() {
  sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
  exit 0
}

while getopts "i:r:p:h" opt; do
  case "$opt" in
    i) inits="$OPTARG" ;;
    r) runs="$OPTARG" ;;
    p) privileged="$OPTARG" ;;
    h) usage ;;
    *) usage ;;
  esac
done

# ───────────────────────── helpers ────────────────────────────────────
die() { echo "bench: $*" >&2; exit 1; }
now() { date +%s.%N; }

elapsed() { awk -v a="$1" -v b="$2" 'BEGIN { printf "%.3f", b - a }'; }

deadline_passed() { # t0 now deadline -> 0 if passed, 1 otherwise
  awk -v a="$1" -v b="$2" -v d="$3" 'BEGIN { exit (b - a > d) ? 0 : 1 }'
}

# ───────────────────────── sanity checks ──────────────────────────────
command -v docker >/dev/null || die "docker is required"
command -v date >/dev/null || die "GNU date is required"
docker info >/dev/null 2>&1 || die "cannot talk to the docker daemon (need root? no sudo available?)"

if [ ! -x "$repo/target/release/velocity-init" ] || [ ! -x "$repo/target/release/velocityctl" ]; then
  die "missing $repo/target/release/{velocity-init,velocityctl} — run: cargo build --release"
fi

if [ "${BENCH_SKIP_BUILD:-0}" != "1" ] \
  && ! docker image inspect "$BENCH_IMAGE" >/dev/null 2>&1; then
  echo "== building $BENCH_IMAGE (first run only)..."
  docker build -f "$here/Dockerfile.devuan" -t "$BENCH_IMAGE" "$repo"
fi

if [ "$privileged" = "1" ]; then
  priv_flags="--privileged --cgroupns=host"
  echo "!! WARNING: privileged mode. The shutdown test may power off / reboot"
  echo "!! this HOST if reboot(2) succeeds in the container. Run on a VM."
  echo "!! (use -p 0 for safe-but-inexact mode: seccomp blocks reboot(2))"
else
  priv_flags="--cap-add SYS_ADMIN"
  echo "== safe mode: no --privileged; reboot(2) will be denied (graceful EPERM)"
fi

# ───────────────────────── per-init logic ─────────────────────────────
wait_ready() { # name deadline -> elapsed, or 1 on timeout
  local name="$1" deadline="$2" t0 t
  t0=$(now)
  while :; do
    if docker exec "$name" test -f /tmp/bench-svc.ready 2>/dev/null; then
      elapsed "$t0" "$(now)"
      return 0
    fi
    t=$(now)
    if ! deadline_passed "$t0" "$t" "$deadline"; then return 1; fi
    sleep 0.05
  done
}

read_metrics() { # name -> "init_rss svc_rss total_rss init_cpu"
  local name="$1"
  docker exec "$name" sh -c 'ps -eo pid=,ppid=,rss=,pcpu=,comm=' 2>/dev/null \
    | awk '
        { rss = $3 + 0; cpu = $4 + 0 }
        $1 == 1 { init = rss; initcpu = cpu }
        $2 == 1 { svc += rss }
        { tot += rss }
        END { printf "%d %d %d %.1f", init, svc, tot, initcpu }'
}

do_supervision() { # name oldpid -> respawn seconds, or 1 on timeout
  local name="$1" oldpid="$2" t0 t newpid
  docker exec "$name" kill -9 "$oldpid" >/dev/null 2>&1 || true
  t0=$(now)
  while :; do
    newpid=$(docker exec "$name" cat /tmp/bench-svc.pid 2>/dev/null || true)
    if [ -n "$newpid" ] && [ "$newpid" != "$oldpid" ]; then
      elapsed "$t0" "$(now)"
      return 0
    fi
    t=$(now)
    if ! deadline_passed "$t0" "$t" "$BENCH_RESTART_TIMEOUT"; then return 1; fi
    sleep 0.05
  done
}

do_shutdown() { # name -> "elapsed exit_code timeout?"
  local name="$1" t0 t1 code hit
  t0=$(now)
  docker stop -t "$BENCH_STOP_TIMEOUT" "$name" >/dev/null 2>&1 || true
  t1=$(now)
  code=$(docker inspect --format '{{.State.ExitCode}}' "$name" 2>/dev/null || echo "?")
  hit=$(awk -v e="$(elapsed "$t0" "$t1")" -v d="$BENCH_STOP_TIMEOUT" \
    'BEGIN { print (e >= d - 0.5) ? "SIGKILL" : "clean" }')
  echo "$(elapsed "$t0" "$t1") $code $hit"
}

run_case() { # init -> tab-separated result row
  local init="$1" entry="" name="bench-$init" id
  case "$init" in
    velocity) ;;
    openrc) entry="--entrypoint /usr/sbin/entry-openrc.sh" ;;
    dinit) entry="--entrypoint /usr/sbin/entry-dinit.sh" ;;
    *) die "unknown init: $init (use openrc, dinit, velocity)" ;;
  esac

  # Fresh container before and after every case.
  docker rm -f "$name" >/dev/null 2>&1 || true
  trap 'docker rm -f "$name" >/dev/null 2>&1 || true' EXIT

  echo "-- $init: starting container"

  id=$(docker run -d --name "$name" $priv_flags $entry "$BENCH_IMAGE")
  [ -n "$id" ] || die "docker run failed for $init"

  boot=$(wait_ready "$name" "$BENCH_BOOT_TIMEOUT") || boot="FAIL"
  if [ "$boot" = "FAIL" ]; then
    echo "-- $init: boot FAILED (no READY within ${BENCH_BOOT_TIMEOUT}s)"
    docker logs --tail 20 "$name" 2>&1 || true
    echo -e "$init\tFAIL\t-\t-\t-\t-\t-\t-\t-\t-"
    return 0
  fi
  echo "-- $init: booted in ${boot}s"

  metrics=$(read_metrics "$name")
  echo "-- $init: metrics $metrics"

  svc_pid=$(docker exec "$name" cat /tmp/bench-svc.pid 2>/dev/null || echo "")
  respawn="-"
  if [ -n "$svc_pid" ]; then
    echo "-- $init: SIGKILL $svc_pid (supervision test)"
    if respawn=$(do_supervision "$name" "$svc_pid"); then
      echo "-- $init: respawned after ${respawn}s"
    else
      respawn="no"
      echo "-- $init: no respawn within ${BENCH_RESTART_TIMEOUT}s"
    fi
  else
    echo "-- $init: no pidfile — supervision test skipped"
  fi

  echo "-- $init: shutdown (SIGTERM via docker stop)"
  shutdown=$(do_shutdown "$name")
  echo "-- $init: shutdown $shutdown"

  # Each field travels as one tab-separated column; inner spaces are
  # turned into tabs so print_row sees exactly 10 columns.
  row=$(printf '%s\t%s\t%s\t%s\t%s\n' "$init" "$boot" \
    "${metrics// /$'\t'}" "$respawn" "${shutdown// /$'\t'}")
  echo "$row"
  docker rm -f "$name" >/dev/null 2>&1 || true
  trap - EXIT
}

# ───────────────────────── table output ───────────────────────────────
print_header() {
  printf '%s\n' "init        boot(s)   initRSS  svcRSS   totRSS   initCPU%  respawn(s) shutdown(s)  exit  note"
  printf '%s\n' "----------- --------  -------  -------  -------  --------  ----------  -----------  ----  ----"
}

print_row() { # tab-separated: init boot iRSS sRSS tRSS iCPU respawn shutdown exit note
  local init boot iRSS sRSS tRSS iCPU respawn shutdown exitc note
  IFS=$'\t' read -r init boot iRSS sRSS tRSS iCPU respawn shutdown exitc note <<<"$1"
  printf '%-11s %-9s %-8s %-8s %-8s %-9s %-11s %-12s %-5s %s\n' \
    "$init" "$boot" "${iRSS}K" "${sRSS}K" "${tRSS}K" "${iCPU}%" "$respawn" "$shutdown" "$exitc" "$note"
}

# ───────────────────────── main ───────────────────────────────────────
for run in $(seq 1 "$runs"); do
  echo
  echo "================ benchmark run $run/$runs ($(date -Iseconds)) ================"
  print_header
  for init in $inits; do
    row=$(run_case "$init")
    if [ -n "$row" ]; then print_row "$row"; fi
  done
done

echo
echo "== done. (boot FAILED means the READY marker never appeared;"
echo "   respawn 'no' is the expected OpenRC result — it does not supervise.)"
