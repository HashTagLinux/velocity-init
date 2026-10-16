# velocity-init

Small supervised init I wrote to replace sysvinit on Devuan. Fast, plain, no fuss (v0.1.0).

## What's here

- `velocity-init` (`src/main.rs`) — runs as PID 1.
- `velocityctl` (`src/bin/velocityctl.rs`) — talks to it over the control socket.
- Library `velocity_init` (`src/lib.rs`).

## How it boots

Two ways, picked automatically:

- No `/etc/inittab`? It walks `/etc/init.d` and boots the LSB graph (`--init-dir`, `--mode native` — see `src/config.rs:149`).
- Got a non-empty `/etc/inittab`? It goes SysV on you (`rcS` / `rc N`, see `src/models.rs`). `telinit N`, runlevels, the lot.

## Build

```sh
cargo build
cargo build --release   # glibc dynamic. Build it where it'll run — docker/Dockerfile.freia works (needs glibc 2.43+)
```

Standalone build — one static binary, no libc on the target, boots anywhere:

```sh
rustup target add x86_64-unknown-linux-musl   # (aarch64-unknown-linux-musl works too)
cargo build --release --target x86_64-unknown-linux-musl   # ~1.2 MB, static-pie
```

Either build writes the same `/var/run/utmp` records: the on-disk layout is spelled out in `src/utmp.rs` rather than inherited from whichever libc compiled it.

## Test

```sh
cargo test
```

## More docs

- `bench/README.md` — how I benchmarked it against OpenRC and Dinit.
- `pkg/README.md` — how the `.deb` gets built.
