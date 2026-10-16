# velocity-init vs OpenRC vs Dinit — container benchmark

I wanted real numbers, not guesses, so I booted all three as actual PID 1
in Docker on the same `debian:stable` image. That's the closest thing I had
to Devuan that still gets updates — same apt repos, same sysvinit-style scripts.

Quick packaging notes, since I tripped on these (checked against the archive):
- `openrc` (0.56) is in debian:stable. You get `/usr/sbin/openrc-init`, which is their minimal PID 1 for containers.
- `dinit` isn't in Debian at all. ITP #1029577 has sat open since 2022, so the Dockerfile builds upstream v0.19.4 from source with meson (`--build-arg DINIT_VERSION=…`) into `/sbin`. Don't bother with `apt-get install dinit`, it fails everywhere including sid.

## How to use

From the repo root:

```sh
cargo build --release                       # glibc, dynamically linked
# Build it on whatever you'll bench on (docker/Dockerfile.freia is a
# ready Devuan Freia box). A PID 1 built against newer glibc than the
# image has just won't start there.

docker build -f bench/Dockerfile.devuan -t velocity-bench:latest .

./bench/bench.sh                            # all three, one run
./bench/bench.sh -i velocity openrc         # subset
./bench/bench.sh -r 3                       # three runs (eyeball the average)
./bench/bench.sh -p 0                       # safe mode (see Safety below)
```

Each image gets one mock service:

| init          | service definition                         |
|---------------|--------------------------------------------|
| velocity-init | `/etc/init.d/bench-svc` (LSB header)       |
| OpenRC        | same LSB script, symlinked into `default`  |
| Dinit         | `/etc/dinit.d/boot` (`type = internal`, `waits-for.d = boot.d`) → `boot.d/bench-svc` (`restart = on-failure`) |

The mock is just one idle process (`/usr/libexec/bench-svc-run`). It drops
its pid in `/tmp/bench-svc.ready` on start and cleans up on SIGTERM. There's
also a dummy `getty-console` so velocity-init's built-in console getty stays
quiet — that way each init supervises exactly one thing.

## What each column means

| column     | how it is measured                                                          |
|------------|-----------------------------------------------------------------------------|
| boot(s)    | `docker run -d` → READY file appears (polled via `docker exec`, 50 ms)      |
| initRSS    | RSS of PID 1 after boot (`ps`, KiB)                                         |
| svcRSS     | summed RSS of PID 1's children                                              |
| totRSS     | summed RSS of every container process                                       |
| initCPU%   | CPU% of PID 1 after boot                                                    |
| respawn(s) | SIGKILL the mock, seconds until its pid file shows a new pid (≤5 s poll)    |
| shutdown(s)| `docker stop -t 60` (SIGTERM to PID 1) until the container exits            |
| exit       | container exit code (`?` = could not inspect)                               |
| note       | `SIGKILL` = shutdown hit the `docker stop` timeout and was force-killed     |

## Expected results

| init          | boot | supervision | shutdown                                        |
|---------------|------|-------------|-------------------------------------------------|
| velocity-init | fast | respawns after ~100 ms (OnFailure exponential backoff, first attempt 100 ms, doubling to a 30 s cap) | SIGTERM → halt teardown: services stopped in reverse order, cgroup sweep, `reboot(2)` attempt (EPERM in safe mode, logged), exit 0 |
| OpenRC        | fast | **no respawn** — OpenRC does not supervise foreground/daemon services by default | SIGTERM → openrc-init runs the shutdown runlevel (stop function kills the pidfile pid), then exits |
| Dinit         | fast | respawns quickly (no backoff by default) | SIGTERM → stops services in dependency order, exits |

On boot times: velocity-init is a tiny binary doing the bare minimum
(mounts, one service). OpenRC walks sysinit + boot + default, dinit does
its `boot.d`. They all land in the same ballpark (tens to a few hundred ms).
`docker run`/`exec` overhead is in there too, same for everyone.

## Safety

This follows the `--privileged --cgroupns=host` setup from the smoke test
(`docker/Dockerfile`) so mounts and cgroups are real. Two gotchas:

1. **reboot(2) can take down your host.** With `--privileged` the container
   has CAP_SYS_BOOT and no seccomp, so the shutdown `reboot(2)` from any
   init might actually reboot or power off your box. **Use a throwaway VM
   or a CI runner.** Shutdown always runs last per container, so even if
   that happens you won't lose earlier numbers.

2. **`--cgroupns=host` shows the host cgroup tree.** velocity-init makes
   `/sys/fs/cgroup/velocity-init` on the host root and removes it after.
   It's transient, but pass `-p 0` if you don't want the host touched at all.

`-p 0` (safe mode) swaps `--privileged` for `--cap-add SYS_ADMIN` and keeps
Docker's default seccomp, which blocks `reboot(2)`. Then velocity-init hits
its graceful EPERM path (logs, exit 0), mount failures just log and go on,
and OpenRC/dinit use their in-container shutdown. Numbers are close but not
identical — the mount phase matters less there.

## Debugging a failed boot

`bench.sh` dumps the last 20 log lines from whatever container died.
From there I usually do:

```sh
docker run -it --rm --privileged --cgroupns=host \
  --entrypoint /bin/bash velocity-bench:latest
# velocity:  /sbin/velocity-init --dry-run --init-dir /etc/init.d
# openrc:    openrc sysinit && openrc default    (watch for hanging services)
# dinit:     dinitctl list
```

Things I've had to remind myself:
- Debian's OpenRC PID 1 is `/usr/sbin/openrc-init` (built for containers).
  The entry script tries `/usr/sbin/openrc-init`, then `/sbin/openrc-init`
  (Alpine-style), then `/sbin/init` if that's all there is.
- The dinit here is source-built, so it's `dinit -o` (container mode),
  falling back to plain `dinit` if the flag's missing. Dinit restarts
  everything by default, so the mock sets `restart = on-failure` on purpose.
- Dinit service files are just `<service-name>` (no `.dinit` suffix), and it
  only starts `boot.d` stuff because the `boot` pseudo-service in
  `/etc/dinit.d/boot` exists.
- If OpenRC boots slow, look at `fsck`/`localmount` and the image's
  `/etc/fstab` (it's minimal on purpose).

## Measured in the namespace rig (2026-08-06)

Before I had Docker handy I checked everything side by side in a sandbox
with no daemon: each init as real PID 1 in a fresh user+pid+mount namespace
(`unshare -Urpmf`), chrooted into one shared rootfs — real Debian `openrc`
.deb plus deps, dinit 0.19.4 from source, velocity-init's release binary —
same mock service and fstab everywhere. Harness was
`/tmp/opencode/rig/rig.sh` (sandbox-only; `bench.sh` is still the tool
you'd actually run).

Three runs back to back (median):

| init          | boot(ms) | init RSS(KB) | svc RSS(KB) | respawn    | respawn(ms) | shutdown(ms) | exit |
|---------------|----------|--------------|-------------|------------|-------------|--------------|------|
| velocity-init | 29       | ~3000        | ~1860       | YES        | ~1025       | ~91          | 130  |
| OpenRC        | 54       | ~2200        | ~1860       | NO         | n/a         | ~48          | 130  |
| Dinit         | 29       | ~3900        | ~1860       | YES        | ~4          | ~25          | 0    |

How I read that:
- **Boot**: same order of magnitude for all three. OpenRC costs ~25 ms
  extra for its runlevel dance (sysinit/boot/default + dep cache).
  velocity-init and dinit came out even here.
- **Supervision**: velocity-init came back after its ~1 s backoff
  (1025/1027/1028 ms — that's the exponential backoff's first try;
  the base has since been lowered to 100 ms, so expect ~125 ms),
  dinit came back in ~4 ms (no backoff by default), OpenRC didn't come
  back at all (normal — it doesn't supervise LSB scripts, and honestly
  that's the column this project exists for). Clean stops
  (SIGTERM → trap → `exit 0`) restart right away, no backoff.
- **Shutdown**: dinit and OpenRC are out in under 100 ms. velocity-init
  used to take ~2.1 s because of a dumb `killpg` bug on my part — I passed
  the negated pid so only the group *leader* got it, and it sat in its
  shell trap behind a foreground `sleep` kid until the 2 s grace ran out
  and SIGKILL fired. Passing the positive pgid to nix's `killpg` (it
  negates for you) and escalating SIGKILL to the group fixed it — ~91 ms now.
- **Exit 130**: that's the sandbox, not a failure. This box's `reboot(2)`
  doesn't return EPERM, it just kills the caller/namespace (feels like
  SIGINT), so anything ending shutdown with `reboot(2)` (velocity-init and
  openrc-init both do) shows 130. Dinit never calls it, so 0. On real Docker,
  seccomp turns `reboot(2)` into EPERM: velocity-init logs it and exits 0
  (see `final_reboot` in `src/main.rs`), and openrc-init doesn't even look
  at the return value (its shutdown can hang till the `docker stop`
  timeout — see above).
- **One caveat**: the sandbox kernel says no to sysfs/devtmpfs mounts in a
  user namespace (fake `/sys`), so the rig pre-mounts proc + a `/sys` tmpfs
  + `/run` tmpfs and skips devtmpfs (velocity-init treats that one as
  best-effort). On Docker you get genuine sysfs/devtmpfs; relative order
  should hold.

## sysvinit drop-in mode (`/etc/inittab`)

Same session, I also had it replace **sysvinit** on stock Devuan. If
`/etc/inittab` exists and isn't empty it boots SysV; otherwise you get the
native LSB graph. Missing/empty inittab isn't an error, just the other path.

What the drop-in actually does:

- **Boot**: runs the `sysinit` lines (say `si::sysinit:/etc/init.d/rcS`) and
  the default runlevel's `wait` lines (say `l2:2:wait:/etc/init.d/rc 2`) in
  file order, waiting on each. The stock `rc` script walks `/etc/rcN.d/`
  itself, so `update-rc.d` keeps working like it always did.
- **Respawn**: `respawn`/`askfirst` (getty) lines become supervised services
  that restart right away, gated by runlevel plus a 5-in-60 s crash guard
  (same limit sysvinit uses). I name them `getty@ttyN`, which also quiets
  the built-in console getty.
- **Runlevels**: `telinit N` (or `velocityctl change N`) runs the new
  level's `wait` line (`/etc/init.d/rc N`), starts that level's respawn
  lines, stops the rest. `telinit 0` / `telinit 6` mean halt/reboot.
- **Shutdown**: inittab `shutdown` lines plus the target level's `rc 0` /
  `rc 6` wait line run *before* the service stop, same as sysvinit's
  teardown (that's where the stop scripts live). Then a `ctrlaltdel` line on
  SIGINT and the `powerfail` / `powerwait` / `powerokwait` / `powerfailnow`
  lines on SIGUSR1/USR2/SIGWINCH/SIGPWR run as one-shots.
- **Tooling**: I still write `RUN_LVL` utmp records so `runlevel` / `who -r`
  keep working. `velocityctl` learned `change N`, and the same binary answers
  to `telinit` / `reboot` / `halt` / `poweroff` (argv0), covering the usual
  Devuan commands.

Service names are fixed (`boot@0`, `wait@2`, `wait@0`/`wait@6`, `once@N`,
`shut@N`, `getty@tty1`, `res@…`) so `velocityctl status` stays readable.
I checked a Devuan-like inittab end to end in the same sandbox: rcS/rc wait
lines, one supervised getty (`bench-svc-run`), `telinit 3` (ran `rc 3` plus
a respawn sweep), then `telinit 0` (ran `rc 0`, wrote utmp, exited clean).
