# Packaging velocity-init for Debian / Devuan

This folder turns `velocity-init` into a proper `.deb` you can install on
real hardware and boot as PID 1. It's also got the scripts and tests I use
to make sure that doesn't wreck your bootloader.

## Layout

| File | Installs to | Purpose |
|------|-------------|---------|
| `sbin/velocity-init` | `/sbin/velocity-init` | the init (the `init=` target) |
| `sbin/velocityctl` (bin target) | `/usr/bin/velocityctl` | operator control client |
| `pkg/grub-maintain.sh` | `/usr/share/velocity-init/grub-maintain.sh` | GRUB editing that won't double-apply (sourced by postinst/prerm) |
| `pkg/grub-append.awk` / `grub-remove.awk` | `/usr/share/velocity-init/` | awk bits for `GRUB_CMDLINE_LINUX_DEFAULT` |
| `debian/velocity-init.hook` | `/usr/share/initramfs-tools/hooks/velocity-init` | pulls the binary into the initramfs |
| `debian/init-bottom` | `/usr/share/initramfs-tools/scripts/init-bottom/velocity-init` | quick sanity check before handoff |
| `debian/postinst` | `DEBIAN/postinst` | makes `/run/velocity-init`, adds `init=`, rebuilds initramfs |
| `debian/prerm` | `DEBIAN/prerm` | takes the `init=` arg back out, cleanly |

`postinst` and `prerm` are safe to run twice — `pkg/test-maintainers.sh`
covers that (12 checks) with a fake GRUB config + fake `update-grub` and a
fake `$DESTDIR`.

## Building the .deb

Three ways, same bits inside.

### 1. One-shot script (any box with `cargo`)

```sh
./pkg/build-deb.sh            # writes velocity-init_0.1.0-1_<arch>.deb into ./
# or pick an output dir:
./pkg/build-deb.sh pkg/dist   # → pkg/dist/velocity-init_0.1.0-1_x86_64.deb
```

- The binary is **glibc, dynamically linked** — build it on (or in a
  container of) whatever it'll boot. `docker/Dockerfile.freia` is the
  Devuan Freia box I use, toolchain included.
- Got `dpkg-deb`? It'll use it. If not, it hand-rolls a valid `.deb`
  with `ar`/`tar`/`gzip` (handy in bare sandboxes). Either way `dpkg -i`
  accepts it.
- `VELOCITY_BUILD_ID=…` stamps `--build-id` (falls back to `<cargo
  version>-1`).

Look before you install:

```sh
ar t pkg/dist/velocity-init_0.1.0-1_*.deb      # debian-binary, control.tar.gz, data.tar.gz
dpkg-deb --contents pkg/dist/velocity-init_*.deb
dpkg-deb --info pkg/dist/velocity-init_*.deb
```

### 2. Proper `dpkg-buildpackage` (on Debian/Devuan)

You'll need `debhelper` plus the host Rust toolchain.

```sh
dpkg-buildpackage -b -us -uc   # builds from debian/rules + debian/changelog
```

`debian/rules` stamps `VELOCITY_BUILD_ID` from the changelog, builds
`--locked`, and runs `cargo test --quiet` along the way.

### 3. `cargo-deb` (if you prefer it)

```sh
cargo install cargo-deb
cargo deb --no-build   # uses cargo's profile; Cargo.toml has no deb config yet
```

Note this one ignores `debian/` (it wants `[package.metadata.deb]`) — add
that block if you'd rather bless this route.

## Installing on the target

```sh
sudo dpkg -i velocity-init_0.1.0-1_amd64.deb
```

Then `postinst`:

1. `mkdir -p /run/velocity-init`
2. tacks `init=/sbin/velocity-init` onto `GRUB_CMDLINE_LINUX_DEFAULT`
   in `/etc/default/grub` (skips it if it's already there) and runs
   `update-grub`
3. runs `update-initramfs -u` so the hook lands the binary in the
   early boot image

## Devuan build: replacing `sysvinit`

On Devuan (SysV by default) you can swap the old init out instead of
detouring through GRUB. `pkg/build-devuan.sh` makes
`velocity-init_devuan_0.1.0-devuan1_<arch>.deb`, which:

- drops in `/sbin/init` as a symlink → `/sbin/velocity-init`, so the
  kernel's plain `execve("/sbin/init")` lands straight on us — **no
  `init=` GRUB fiddling**;
- installs `telinit`, `reboot`, `halt`, `poweroff`, `shutdown` under
  `/sbin/` plus `runlevel` under `/usr/bin`, all symlinks → `velocityctl`
  (it switches on `argv[0]`, so they act like the sysvinit ones, `shutdown -r` included);
- ships a default `/usr/share/velocity-init/inittab` and copies it to
  `/etc/inittab` only when you don't already have one (never clobbers
  a conffile);
- says `Depends: sysv-rc, initramfs-tools | initramfs-tools-core` and
  `Breaks/Replaces/Conflicts: sysvinit-core` (+ `systemd`), so `dpkg -i`
  pulls the old init out. It still manages the GRUB `init=` line too, just
  in case some bootloader setup handles `/sbin/init` itself.

```sh
./pkg/build-devuan.sh pkg/dist          # → velocity-init_devuan_0.1.0-devuan1_x86_64.deb
sudo dpkg -i pkg/dist/velocity-init_devuan_0.1.0-devuan1_x86_64.deb
```

Check it first:

```sh
dpkg-deb --contents pkg/dist/velocity-init_devuan_*.deb   # symlinks + inittab
dpkg-deb --info pkg/dist/velocity-init_devuan_*.deb        # Depends/Breaks
```

Back to SysV: `sudo apt-get install sysvinit-core` takes `/sbin/init`
back. If a stray `init=` GRUB line hangs around after that, reinstall this
package and its `prerm` pulls the line out again.

## Booting and checking it worked

After a reboot, PID 1 should be us:

```sh
ps -p 1 -o comm=            # velocity-init
velocityctl status          # what it's supervising, live state per service
velocityctl journal --tail 20   # boot/service journal (--follow to tail)
runlevel                    # SysV runlevel (utmp RUN_LVL via /etc/inittab)
who -r                      # same thing, other spelling
telinit 3                   # hop runlevels (if you're on the inittab drop-in)
sudo reboot                 # should take the clean teardown path
```

The journal lives at `/run/velocity-init/journal` (`VELOCITY_JOURNAL`
overrides it) — small binary append log, everything lands there, read it
with `velocityctl journal`. Handy when syslog/kmsg aren't enough.

Can't verify a boot:

- Rip `init=/sbin/velocity-init` back out of `/etc/default/grub` (same
  line postinst added), `update-grub`, reboot — you're back on the old init.
- If the kernel panics before initramfs hands over, there's a fallback
  shell left by `init-bottom/velocity-init` at the `panic` prompt. Grab it
  from there.

## Removing

```sh
sudo dpkg -r velocity-init
```

`prerm` yanks `init=/sbin/velocity-init` from GRUB and rebuilds the menu,
so boot goes back to normal.

## Notes

- Native LSB boot and the `/etc/inittab` drop-in live side by side: if
  `/etc/inittab` exists and isn't empty, you get SysV; otherwise it builds
  the LSB DAG. Uninstalling keeps `/etc/inittab` around (it's a conffile)
  for other SysV tools.
- I journal boot timings instead of guessing (`boot: level k/n ready in …`,
  `boot: … complete in …`). LSB mode starts independent services in a level
  together (systemd-style); `--jobs N` caps that if a level gets huge. SysV
  stays in inittab order (rcS → rc N), no shortcuts there.
- `--build-id` is baked at compile time from `VELOCITY_BUILD_ID`, so ops
  can tell which package is running: `velocity-init --build-id`.
- `pkg/test-maintainers.sh` runs the GRUB/initramfs checks on any laptop
  without touching the real system. Before you tag: `cargo fmt --check`,
  `cargo clippy --all-targets`, `cargo test`.
