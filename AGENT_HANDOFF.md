# Agent handoff — velocity-init

Scratch guide for a fresh agent session picking this work up. Written
2026-10-15. It is not part of the package — delete, move, or commit it as
you like. Re-verify anything here with the commands below before trusting
it; state moves.

## 0. Ground rules

- **PID 1 must never panic and never exit unexpectedly** ("Attempted to
  kill init" panics the kernel). Every PID-1 path keeps that invariant;
  the house style for recoverable problems is warn-and-continue.
- **The working tree mixes the user's uncommitted WIP with agent work.**
  Do not revert/overwrite the user's files:
  `Cargo.toml`, `bench/README.md`, `src/event_loop/boot.rs`,
  `src/event_loop/shutdown.rs`, `src/journal.rs`, `src/main.rs`,
  `src/supervisor/mod.rs`, `src/supervisor/restart.rs`, `tests/phase3.rs`,
  `init_10_parts.txt` (untracked).
  Agent-owned so far: `src/inittab.rs`, `src/cgroups.rs`,
  `src/lsb_parser.rs`, `src/event_loop/dispatch.rs`, `src/utmp.rs`,
  `src/bin/velocityctl.rs`, `tests/phase6.rs`, `Readme.md`; nested-run
  fix (2026-10-15, §1.3): `src/namespaces.rs` (new), one-line
  `src/lib.rs` registration, `scripts/sandbox.sh` comments.
- **Ask before editing `src/main.rs`** — it holds the user's in-flight
  panic-hold / `catch_unwind` work. The §1.3 fix touched only
  `final_reboot` there, with the user's explicit OK.
- Every fix gets a regression test. Run the full suite for **both libc
  targets** before reporting done. Do not add dependencies.
- Repo state: one commit (`Initial commit`), everything else uncommitted.
  23 entries in `git status --short` (2026-10-15, after the §1.3 fix).

## 1. What has been done (high level)

1. Full repo review (all `src/`, `build.rs`, packaging). Genuine fixes,
   each with a regression test:
   - `src/inittab.rs` — duplicated runlevel chars registered a service
     twice (`test_duplicate_runlevels_do_not_duplicate_services`).
   - `src/cgroups.rs` — LSB `x-velocity-limit` keys were joined to the
     cgroup dir unvalidated (absolute/`../` key → PID 1 writes anywhere);
     now warn-and-skip (`test_validate_limit_file_accepts_only_plain_names`,
     `test_apply_limits_never_writes_outside_the_cgroup`).
   - `src/lsb_parser.rs` — capped read turned a character straddling the
     1 MiB cap into `InvalidData`, losing the header; now lossy UTF-8
     (`test_truncation_boundary_keeps_the_header`).
   - `src/event_loop/dispatch.rs` — `unreachable!()` in `reap_children`
     replaced with `break`.
   - `tests/phase6.rs` — unused `mut`; `src/utmp.rs` test hermeticity
     (pid-unique scratch paths).
2. **Standalone (musl) support** — the big recent work:
   - `src/utmp.rs` now defines the on-disk record itself (`UtmpRecord`)
     instead of `libc::utmpx`. Reason (verified with a probe crate
     against libc 0.2.189): glibc x86_64 reads/writes **384-byte**
     records (32-bit-compat tail); musl models a **400-byte** time64
     record, so a musl build using `libc::utmpx` silently wrote a utmp
     database Devuan's tools misparse. The env knob
     `CARGO_CFG_LIBC_UNSTABLE_MUSL_V1_2_3` does **not** fix it.
   - Layout is chosen per arch: `i32` tail (384 B) on `x86`/`x86_64`;
     `i64` tail (400 B) on `aarch64`/`arm` (armhf since its time64
     transition). Unlisted arches hit a `compile_error!` — add an arch
     only after checking glibc's `bits/utmp.h`.
   - Pinned by `record_layout_matches_the_glibc_utmp_file_format`
     (offsets + sizes) and a `const` assert (size 384 or 400).
   - `src/bin/velocityctl.rs` — `#[allow(deprecated)]` with rationale at
     the `localtime_r` call (`libc::time_t` deprecation is forward-looking;
     width is already 8 bytes on 64-bit musl).
   - `Readme.md` — "Standalone build" section.
   - **Verified**: glibc suite 242 lib + 59 integration tests green, 0
     warnings; musl suite identical; release binary `static-pie`, 1,159,888
     bytes, runs on a glibc host; cross-libc wire check showed records
     byte-identical except a 1-second timestamp; a real PID-1 sandbox boot
     of the static binary wrote `/var/run/utmp` that the system's own
     `who -r` read as `run-level 3 … last=S`.

3. **Nested-runtime detection** (open item A — done 2026-10-15) — sandbox
   boots used to end in **exit 130**: the sandbox's private `/proc` hides
   the `NSpid` depth (a single value, and not even 1 — it is the outer
   PID), so `pid_ns_depth()` returned 1, `final_reboot` called
   `reboot(2)`, and the kernel turned it into SIGINT for the nested
   PID 1. Nothing after "teardown complete" logged.
   - New `src/namespaces.rs`: `in_mapped_user_namespace()` — first line
     of `/proc/self/uid_map` is not the initial `0 0 4294967295`
     (missing/malformed ⇒ `false`, so a real PID 1 keeps its reboot
     path); `sandbox_requested()` — `VELOCITY_SANDBOX=1`, exported by
     `scripts/sandbox.sh` and read nowhere before this.
   - `final_reboot` takes the nested path when `NSpid` depth > 1 **or**
     the uid map is non-identity **or** VELOCITY_SANDBOX=1; when the env
     knob fires with neither namespace probe, it warns (on a real
     machine, exiting 0 instead of rebooting panics the kernel).
   - 5 unit tests pin the parsers; `scripts/sandbox.sh`'s stale
     "reboot(2): refused … EPERM" comment corrected; the env export now
     has a comment saying what reads it.
   - **Verified** (2026-10-15): glibc suite 247 lib tests, 0 warnings;
     musl suite identical (247 lib); static release rebuilt
     (`static-pie`, 1,163,984 bytes); sandbox boots of both the debug
     glibc and the musl static binary exit **0**, log `nested runtime
     detected (NSpid depth 1, mapped user namespace: true,
     VELOCITY_SANDBOX: true) … exiting 0`, and the static boot's
     `/var/run/utmp` still reads back as `run-level 3 … last=S`.
   - Known gap (documented in the module docs): a root-created
     *full-identity* user namespace is indistinguishable from the
     initial namespace and escapes all three probes.

## 2. Environment

- System toolchain (glibc builds): `/usr/bin/cargo`, rustc 1.96.1.
  No rustup. `cargo clippy` is **not** installed. Network is available.
- Tests run as uid 1000; **no sudo**. `/tmp/opencode` is root-owned and
  not writable — don't use it.
- Throwaway musl toolchain (nothing touches the user's home/system):
  `/tmp` gets cleaned between sessions, so re-create it as needed:

```sh
export RUSTUP_HOME=/tmp/velocity-toolchain/rustup CARGO_HOME=/tmp/velocity-toolchain/cargo
mkdir -p /tmp/velocity-toolchain
curl -sSf https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-gnu/rustup-init \
    -o /tmp/velocity-toolchain/rustup-init && chmod +x /tmp/velocity-toolchain/rustup-init
/tmp/velocity-toolchain/rustup-init -y --no-modify-path --profile minimal \
    --default-toolchain stable --target x86_64-unknown-linux-musl
```

  If `/tmp/velocity-toolchain/cargo/bin` vanished but `cargo/registry` and
  `target-musl` survived (partial cleanup), just re-run the two commands —
  the cache gets reused.

## 3. Verification battery

```sh
# glibc: full suite + warnings (expect 0 warnings)
cargo test --no-fail-fast 2>&1 | grep -E "^(test result|error|warning)"
cargo check --all-targets 2>&1 | grep -c "^warning"       # expect: 0

# musl: full suite (needs the toolchain from §2)
export RUSTUP_HOME=/tmp/velocity-toolchain/rustup CARGO_HOME=/tmp/velocity-toolchain/cargo
CARGO_TARGET_DIR=/tmp/velocity-toolchain/target-musl \
  /tmp/velocity-toolchain/cargo/bin/cargo test --target x86_64-unknown-linux-musl --no-fail-fast \
  2>&1 | grep -E "^(test result|error|warning)"

# static release artifact (expect: "static-pie linked", ~1.16 MB)
CARGO_TARGET_DIR=/tmp/velocity-toolchain/target-musl \
  /tmp/velocity-toolchain/cargo/bin/cargo build --release \
  --target x86_64-unknown-linux-musl -p velocity-init --bin velocity-init
file /tmp/velocity-toolchain/target-musl/x86_64-unknown-linux-musl/release/velocity-init
```

Expected counts (2026-10-15, nested-run fix included): lib **247 passed**
(242 before it); the other eight targets report 0/11/18/5/10/5/10/0 —
**all ok, none failed**.

### Real-PID-1 sandbox boot (`scripts/sandbox.sh`)

Needs working user namespaces (this box: yes). Driver script (write to
`/tmp/velocity-driver.sh`, `chmod +x`) — waits for the socket, copies the
utmp PID 1 wrote out to the host, powers off:

```sh
#!/bin/bash
set -u
for _ in $(seq 1 100); do
    [ -S /run/velocity-init.sock ] && break
    sleep 0.1
done
sleep 2
cp /var/run/utmp /tmp/velocity-sandbox-utmp 2>/dev/null \
    && echo "driver: copied utmp out" >&2
/home/user/workspace/velocity-init/target/debug/velocityctl poweroff \
    || echo "driver: poweroff returned $?" >&2
```

```sh
MUSL_BIN=/tmp/velocity-toolchain/target-musl/x86_64-unknown-linux-musl/release/velocity-init
timeout 120 scripts/sandbox.sh --inittab tests/data_sysv/inittab \
    --init-dir tests/data_sysv/init.d --driver /tmp/velocity-driver.sh -- \
    "$MUSL_BIN" --mode sysv
who -r /tmp/velocity-sandbox-utmp        # expect: run-level 3 … last=S
```

Known cosmetics (not bugs): mount `WARN` spam at teardown (userns can't
unmount host mounts: EINVAL/ENOENT) and `could not attach … cgroup:
Permission denied`. The run now ends with **exit 0** — before the §1.3
fix it died of a rebooting SIGINT (exit 130) right after "teardown
complete".

### Cross-libc wire check (one-off, remembered here)

Temporarily add `tests/zz_wire_check.rs`:

```rust
use std::path::Path;

#[test]
fn zz_wire_check() {
    match (std::env::var("WIRE_WRITE").ok(), std::env::var("WIRE_READ").ok()) {
        (Some(p), _) => {
            velocity_init::utmp::write_record_to(Path::new(&p), Some(b'N'), b'3').unwrap();
            println!("wrote {p}");
        }
        (None, Some(p)) => {
            let rl = velocity_init::utmp::read_runlevel(&p).expect("readable record");
            assert_eq!(rl.previous, 'N');
            assert_eq!(rl.current, '3');
            println!("read {p} => {rl:?}");
        }
        (None, None) => panic!("set WIRE_WRITE=<path> or WIRE_READ=<path>"),
    }
}
```

Run `WIRE_WRITE=…` under gnu and musl, then `cmp -l` the two files: the
only differences must be inside the timestamp bytes (offsets 340..348,
compat layout). Cross-check reads both ways, then **delete the file**.

## 4. Open items (priority order)

### A. Nested-namespace detection — DONE 2026-10-15 (see §1.3)

Stub kept so the B–E letters below still line up. Only known gap: a
root-created *full-identity* user namespace escapes the probes
(documented in `src/namespaces.rs`).

### B. `paths.rs` runtime-dir fallback ownership (needs a product decision)

- `src/paths.rs` (`runtime_dir_for` / `is_writable_dir` / `try_create_dir`)
  accepts a pre-existing last-resort dir under `/tmp` without checking
  owner/mode; PID 1 then writes the journal there. Socket is 0600 and
  peer-cred-checked, but a foreign-owned dir is still a bad home.
- Decision needed: keep warn-and-use (a test pins the last-resort
  semantics), or refuse / pick a fresh name. Update that test if changed.

### C. CI

- No forge config in the repo. Ask where it is hosted. Proposal: a glibc
  job (`cargo test`) plus a musl job (`rustup target add … ; cargo test`),
  optionally a sandbox smoke (verify `unshare` works on the runner first).

### D. Commit the change set

- 17 modified files + 2 untracked, mixed user/agent work. Suggested
  grouping: (1) review fixes, (2) utmp standalone/musl work, (3) user's
  own WIP which they commit. Ask before committing user files.

### E. Optional polish

- Bench deltas vs dinit (`bench/README.md`): respawn backoff ~100 ms vs
  ~4 ms, shutdown ~91 ms vs ~25 ms — only if the user cares.
- `src/utmp.rs` armhf comment was corrected this session (time64 is the
  distro `_TIME_BITS=64` transition, not an upstream glibc 2.34 default).

## 5. Landmines & useful facts

- **Never reintroduce `libc::utmpx`** in `src/utmp.rs` (see §1.2). The
  utmp *file* format (384/400 bytes) is the contract; the `UtmpRecord`
  tests pin it.
- Test hermeticity: stale root-owned `/tmp/velocity-utmp-*` files exist
  and cannot be removed without sudo. Tests must use pid-unique scratch
  paths and clean up.
- **Nested-runtime probes (`src/namespaces.rs`) must fail toward "not
  nested"**: claiming nested on a real machine makes PID 1 return from
  `final_reboot`, exit 0, and panic the kernel. Only positive evidence
  counts; `VELOCITY_SANDBOX=1` is the single explicit override and it
  warns when it fires alone.
- `build.rs` enforces the glibc ≥ 2.43 floor **only** for gnu targets;
  musl builds are skipped on purpose.
- `Cargo.toml` deliberately has no `panic = "abort"` (comment explains);
  panics are caught and turned into the PID-1 panic hold.
- `src/main.rs` `panic_hold()` keeps PID 1 alive on escaped panics and
  handles SIGINT (Ctrl-Alt-Del) / SIGPWR — user's WIP; read it before
  touching anything around it.
- `python3`/`strace` may or may not be installed; don't rely on them.

## 6. First moves for the next session

1. `git status --short`; skim this file; skim the uncommitted diff for
   files you plan to touch.
2. Run the glibc suite (§3) to confirm the baseline is green.
3. If continuing the standalone story: set up the musl toolchain (§2),
   run the musl suite, rebuild the release, re-run the sandbox boot.
4. Pick an open item: B needs a product decision (paths.rs last-resort
   dir), C needs the forge location, D is commit grouping, E is optional
   polish. A is done — see §1.3.
5. Whatever you change: regression test, both suites, then report with
   the exact commands and observed results.
