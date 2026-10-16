# Agent brief: systemd-compat foundation for velocity-init

You are an autonomous coding agent working in `/home/user/workspace/velocity-init`.
Execute the four deliverables below, in order. This prompt is self-contained: do
not assume any prior conversation. Re-verify every claim about the repository
with the tools before trusting it.

---

## 1. Mission

Build the foundation for a systemd API compatibility layer for velocity-init
(the "systemd personality", working name `velocity-systemd`):

1. Design + scope documentation.
2. A conformance harness (oracles, fixtures, golden-trace capture).
3. A unit-file parser (Rust, no dependencies, fuzz-ready).
4. A facade skeleton plus an additive control-protocol v2 on the PID-1 side.

This phase is preparatory. **No D-Bus server yet, no new dependencies, no
changes to PID-1 behavior beyond the additive protocol v2, and the facade is not
started by PID 1 yet.**

Decisions already made (do not relitigate; document them):

- The systemd personality lives in separate, restartable binaries ("facade")
  in the same repository, as its own package/crate this phase (see 3.2). It
  never runs inside PID 1.
- PID 1 stays a lean supervisor. All parsing of external data (unit files,
  D-Bus, journal datagrams, Varlink, JSON) happens outside PID 1.
- Compat target: **systemd v257**; pin the newest v257 patch. Verified
  2026-10-15: tag `v257.13`, commit
  `70b5d110be7702afc4dbce012f60d49506d513da` (re-verify with
  `git ls-remote --tags` before writing it into the docs).
- Adopt as-is, do not rewrite: standalone udev, systemd generators,
  `sd-tools` (Chimera Linux's standalone sysusers/tmpfiles fork; derived from
  systemd 255 with fixes cherry-picked — not a 257 tree). Pin each adopted
  component's own version in SCOPE.md, and state which binary/package
  "standalone udev" means. Verified at v257.13: upstream has no standalone
  udev target; `-Dstandalone-binaries=true` builds only
  `systemd-{sysusers,tmpfiles,repart,shutdown}.standalone`, so SCOPE.md must
  name the exact udev binaries and build recipe rather than assume one.
- Rewrite in Rust: manager API facade, unit parser, logind, user manager,
  journald socket protocol, small `*d` shims (hostname/timedate/locale).
- Deferred decisions: any D-Bus crate (zbus) — record it as a deferred decision
  with evaluation criteria; journald native file format; resolved/networkd/
  homed/oomd/bootctl/portable are explicit non-goals.
- Escape hatches are mandatory: logind and journald must stay swappable by
  service name; elogind remains usable as a stopgap implementation.

The strategic rationale, if you need it: systemd-compatible *servers* let stock
clients (real `systemctl`, `loginctl`, `pam_systemd`, `sd-login`, GNOME/KDE)
work unpatched. Reliability comes from blast-radius isolation: the risky code
parses untrusted data in restartable processes, never in PID 1.

## 2. Read first (context loading)

Read these before writing anything:

- `AGENT_HANDOFF.md` — repo ground rules, environment, verification battery,
  list of user WIP files. It may say it is disposable; if present, still obey
  its constraints. Do not delete it.
- `Readme.md`, `Cargo.toml` (note: no `panic = "abort"`; comment explains why).
- `src/control_socket.rs` — existing v1 protocol (Command enum at ~line 68),
  framing, limits, peer-credential checks, tests. **Full read.**
- `src/bin/velocityctl.rs` — the v1 client; it does `write_all` +
  `read_to_string`, i.e. it depends on the server closing the connection
  after the reply. **Full read** before touching connection lifetimes.
- `src/models.rs` — Service/ServiceState/DependencyType/RestartPolicy/
  ShutdownAction; the PID-1 vocabulary the v2 wire strings must expose. The
  facade mirrors it in its own types (the crates stay independent).
- `src/event_loop/dispatch.rs` — control-socket accept/read/write wiring and
  the idle reaper; the additive v2 hooks land here.
- `src/supervisor/mod.rs`, `src/supervisor/sockets.rs` — readiness, notify
  sockets, log pipes, restart timers.
- `src/cgroups.rs` — current flat cgroup layout (this is planned to change to
  systemd-style slices later; document, don't change now).
- `src/journal.rs` — the journal you have; future journald-endpoint design must
  feed or complement this, not fork it.
- `src/paths.rs` — runtime dir/socket path candidates.
- `tests/` — test style (`phase6.rs`, etc.) and `scripts/sandbox.sh`.

## 3. Hard constraints (guardrails)

1. **PID-1 invariants.** PID 1 must never panic and never exit unexpectedly.
   Recoverable problems are warn-and-continue. No `unwrap`/`expect`/`panic!` on
   any PID-1 path. Byte-level limits stay bounded (see existing
   `MAX_COMMAND_BYTES`, `MAX_CONNECTIONS`, `MAX_OUTPUT_BYTES`).
2. **Do not overwrite user WIP.** The working tree mixes the user's uncommitted
   work with agent work. Never revert/overwrite: `Cargo.toml`, `bench/README.md`,
   `src/event_loop/boot.rs`, `src/event_loop/shutdown.rs`, `src/journal.rs`,
   `src/main.rs`, `src/supervisor/mod.rs`, `src/supervisor/restart.rs`,
   `tests/phase3.rs`, `init_10_parts.txt`. **Ask before editing `src/main.rs`.**
   The root `Cargo.toml` must not be modified in this phase at all: create
   `systemd-compat/` as an independent crate with its own `Cargo.toml` and
   document future workspace integration as an open item.
3. **No new dependencies.** PID-1 crate: zero new deps. `systemd-compat/` is
   *not* in the root workspace (adding one would touch root `Cargo.toml`,
   which is forbidden this phase); it may use only crates the root crate
   already uses (`log`, `thiserror` if needed) at the versions in the root
   `Cargo.lock`, plus std. It must not path-depend on `velocity-init`: the
   crates share the wire protocol, never Rust types. No serde, no zbus, no
   async runtime in this phase. Hand-roll the bounded framing; keep parsers
   allocation-bounded.
4. **Clean-room.** Do not copy systemd source code into velocity crates.
   These may be vendored as test data, with license + provenance headers:
   interface man XML (`man/org.freedesktop.*.xml`), `.policy` files (note:
   upstream ships the systemd1 policy as `org.freedesktop.systemd1.policy.in`,
   a template, not a plain `.policy`), and unit files (all
   LGPL-2.1-or-later). Systemd C sources — D-Bus vtables (`dbus-*.c`) and the
   varlink IDL (`varlink-io.systemd.*.c`) — are implementation: read them to
   verify names, never vendor or copy them. At v257 there are no introspection
   XML files checked into `src/` (verified 2026-10-15), but machine-readable
   introspection is not live-only: the relevant D-Bus daemons (`systemd`,
   `systemd-logind`, `systemd-hostnamed`, `systemd-timedated`,
   `systemd-localed`) accept `--bus-introspect list|<interface|path>` and write
   standard introspection XML offline (no bus, daemon, or root), and
   `-Ddbus-interfaces-dir=` exports the same XML at build time (verified
   v257.13). The `man/*.xml` files are DocBook specs (interfaces embedded as
   `programlisting` blocks), not introspection XML. Implementation must be
   original.
5. **Additive protocol only.** v1 clients (`velocityctl`) must keep working.
   v2 is negotiated; a v1 client must never be misparsed. Retain peer-cred
   checks and per-connection limits. Malformed input must never panic.
6. **Every change gets tests.** Bug-fix/feature discipline from the handoff:
   regression test for everything, full suite green, 0 warnings.
7. **Do not commit.** Leave the tree dirty; list what changed in your report.
8. Use `/tmp/velocity-*` or crate-local temp dirs for scratch. Do not rely on
   `/tmp/opencode`: ownership/permissions vary with the execution environment.

## 4. Deliverables

### D1 — Design + scope docs (`docs/systemd-compat/`, root `AGENTS.md`)

Create:

- `docs/systemd-compat/SCOPE.md`
  - Goal and non-goals (non-goals: resolved, networkd, homed, oomd, bootctl,
    portable). Keep non-goals and deferred decisions in *separate* lists so
    they cannot contradict each other; deferred = zbus/D-Bus crate choice and
    the journald native file format.
  - Compat target pinned: tag `v257.13`, commit
    `70b5d110be7702afc4dbce012f60d49506d513da` (re-verify this session — the
    v257 line gets patches). Adopted components (udev, generators, sd-tools)
    are pinned separately; sd-tools is derived from systemd 255, not v257.
  - **Supported-surface table** — one row per interface/method/group you intend
    to implement. Columns: `interface/method`, `consumer (who calls it)`,
    `phase`, `status (planned|in progress|done|deferred)`, `notes`. Cover at
    least: `org.freedesktop.systemd1.Manager` core verbs + signals, Unit/Service
    properties, `org.freedesktop.login1` (Manager/Seat/Session/User/Inhibit),
    hostname1/timedate1/locale1, journald sockets, userdb Varlink (the
    `io.systemd.UserDatabase` interface — `GetUserRecord`/`GetGroupRecord`/
    `GetMemberships` — served on the `/run/systemd/userdb/io.systemd.Multiplexer`
    socket; `io.systemd.Multiplexer`, `io.systemd.NameServiceSwitch` and
    `io.systemd.DropIn` are service/socket names selected via the query's
    `"service"` field, not separate interfaces), the systemd
    user-manager name on the user bus, and owning `org.freedesktop.systemd1` on
    the system bus with the matching D-Bus policy (packaging, later phase).
    Include rows for `Manager.Version` reporting the pinned v257 patch string
    and `Manager.Features` reporting a truthful subset (never advertise a flag
    whose behavior is unimplemented).
  - **Adopt vs rewrite table** with the litmus rule ("stable external contract
    + no shared state with the manager → adopt; must integrate with velocity
    supervisor → rewrite").
  - Consumer-driven priorities (which DE feature needs which interface).
- `docs/systemd-compat/ARCHITECTURE.md`
  - Process model: PID 1 ↔ control socket v2 ↔ facade (system) / facade --user;
    crash domains; supervision/restart of the facade; who parses what.
  - PID-1 purity rule stated as an invariant with rationale.
  - Slice design for later: systemd cgroup layout (`-.slice`, `system.slice`,
    `user.slice`, `user-$UID.slice`, `session-N.scope`, `Delegate=`) as the
    planned replacement for the current flat layout; document only.
  - Escape hatches: logind and journald swappable by service name; elogind
    stopgap; boot must reach a getty if the facade never starts.
  - Threat model sketch: facade accepts unprivileged input; what PID 1 treats
    as untrusted; every v2 frame is hostile. v2 subscribers pin a connection
    slot, so SUBSCRIBE is root/owner-only, while INSPECT stays read-only
    parity with STATUS (no environment exposure).
- `docs/systemd-compat/CONFORMANCE.md`
  - Oracle tiers: **offline** (vendored specs, fixture corpus, snapshot
    tests); **tool** (systemd binaries present, no daemon: `--bus-introspect`
    XML dump and `systemd-analyze verify` over the vendored corpus with
    `SYSTEMD_UNIT_PATH` pointed at it — the authored adversarial fixtures are
    parser-only, not verify inputs); **live** (systemd container/VM: `busctl
    introspect` diff, golden CLI traces). Non-offline tiers degrade to SKIP
    with a clear message naming the missing tool. A comparison mismatch is a
    failure only when the host systemd is the pinned v257.x; any other
    present version is informational (record it) and never fails the run.
    `systemd-analyze verify` specifics (verified at v257.13): it checks that
    every `ExecStart=`/`ExecStop=` binary exists and shells out to `man` for
    `Documentation=man:` entries, so raw upstream units fail unless those
    exist. Pin the invocation in `CONFORMANCE.md`: `--man=no`, plus a staging
    `--root=` containing executable stubs for the corpus's exec paths (or a
    committed expected-output snapshot per fixture); `--recursive-errors=no`
    keeps dependency noise down. Socket fixtures must include the `Service=`
    units they reference, or stay parser-only.
  - How to capture goldens on a systemd machine; provenance rules (record
    `systemctl --version`; keep raw capture plus normalized golden with
    hostname/boot-id/timestamps/pids scrubbed).
- `docs/systemd-compat/PHASES.md`
  - P0–P4 plan with acceptance tests per phase (P0 = this phase;
    P1 generators/transient units/sd_notify completeness; P2 logind + PAM +
    cgroups + user manager; P3 journald + `*d` shims + userdb; P4 long tail).
  - Risk register (semantic fidelity, moving target, polkit/logind subtleties,
    effort) and mitigation (conformance gating, escape hatches, spec-pinned
    target).
- `AGENTS.md` (repo root, new file)
  - The hard constraints from section 3, written as enforceable rules, plus
    this phase's deliverable map, the D4a file allowlist
    (`src/control_socket.rs`, `src/event_loop/dispatch.rs`, `src/lib.rs`), and
    the verification battery (section 6).

Acceptance: every SCOPE row has a consumer and phase; non-goals explicit;
invariants actionable; docs contain no invented systemd API names (verify
against the pinned source when in doubt; if you cannot verify, mark `TODO
verify` rather than guessing).

### D2 — Conformance harness (`conformance/`)

Build the verification backbone *before* the features:

- `conformance/README.md` — what each script does, how to run, and the three
  tiers: offline (no systemd needed, always runs), tool (systemd binaries
  present, no daemon), live (running systemd). Provenance rules for vendored
  data.
- `conformance/run.sh` — single entry point; exits 0 on a machine with no
  systemd tooling, printing a `SKIP` line naming the missing tool for every
  skipped check. SKIP is only for *absent tools* — under the pinned v257.x a
  present tool whose comparison fails is a real failure, never a skip; other
  versions are informational (see CONFORMANCE.md). No network access.
- Interface specs and fixtures. Verified at v257.13 (2026-10-15): systemd has
  **no introspection XML checked into `src/`**; D-Bus interfaces live in C
  vtables. Machine-readable introspection is *not* live-only: the relevant
  D-Bus daemons (`systemd`, `systemd-logind`, `systemd-hostnamed`,
  `systemd-timedated`, `systemd-localed`) support `--bus-introspect list` and
  `--bus-introspect <interface|path>`, writing standard introspection XML
  offline, and `-Ddbus-interfaces-dir=` exports the same XML at build time. So:
  - Vendor `man/org.freedesktop.systemd1.xml`, `org.freedesktop.login1.xml`,
    `org.freedesktop.hostname1.xml`, `org.freedesktop.timedate1.xml`,
    `org.freedesktop.locale1.xml` (and `org.freedesktop.LogControl1.xml`) as
    the normative DocBook API specs under `conformance/systemd-spec/` with a
    `MANIFEST` (tag, commit, source path, sha256, license: LGPL-2.1-or-later).
  - `conformance/systemd-xml/` holds introspection XML generated from a pinned
    v257.x binary via `--bus-introspect` (interface metadata, vendored as test
    data, never hand-edited) with a `MANIFEST` recording the generating binary,
    `systemctl --version`, and sha256. If no v257.x binary is available this
    phase, leave it empty with a `MANIFEST` naming that gap and the exact
    extraction command; the harness SKIPs, it never silently passes.
  - Write the capture plan in `CONFORMANCE.md`: tool tier `--bus-introspect`
    XML from installed v257.x binaries; live tier `busctl introspect` per
    object (`/org/freedesktop/systemd1`, `/org/freedesktop/login1`,
    `/org/freedesktop/hostname1`, `/org/freedesktop/timedate1`,
    `/org/freedesktop/locale1`) and `varlinkctl introspect` on
    `/run/systemd/userdb/io.systemd.Multiplexer` for userdb (v257 has no
    `.varlink` files; the only relevant IDL is the C file
    `varlink-io.systemd.UserDatabase.c`, which must not be vendored).
  - Do **not** fabricate XML.
- `conformance/unit-corpus/`
  - `systemd/` — a small set of real `.service`/`.target`/`.socket`/`.timer`/
    `.path`/`.slice` files vendored from `units/` at the pinned tag (upstream
    has no `.scope` files). Do not author on-disk `.scope` fixtures: systemd
    refuses to load non-transient scope units from disk (`scope_load`, verified
    v257.13), so cover `.scope` name/type recognition with parser unit tests
    only. `MANIFEST` carries tag, commit, source path, sha256, and the
    LGPL-2.1-or-later license note.
  - `fixtures/` — authored fixtures covering: `#` and `;` comment lines
    (including between continuation lines), blank lines, line continuations,
    sections, `Environment=` quoting/escaping, `ExecStart=` value prefixes
    (`-`, `@`, `:`, `+`, `!`, `!!`, and combinations), drop-in merge cases
    (main file + `<name>.d/*.conf` + type-level `<type>.d/*.conf`),
    invalid-but-parseable garbage, oversized input boundary. Each fixture has
    a committed expected-output snapshot and a one-line note saying what it
    pins.
- `conformance/capture/` — `capture-systemd.sh` (runs on a systemd host:
  `systemctl --version`, `systemctl list-units --all --output=json` — valid at
  v257, `systemctl show`, `loginctl list-sessions`, `busctl introspect` /
  `varlinkctl introspect` for the target objects → timestamped directory with
  a plain-text manifest recording version/commit). It refuses to mark a
  capture authoritative unless `systemctl --version` matches the pinned
  v257.x, and it stores raw capture plus normalized goldens (hostname, boot
  id, timestamps, pids scrubbed). `run-live.sh` diffs velocity output vs
  golden when a velocity system is reachable; in P0 there is nothing to diff
  yet, so it SKIPs with that exact reason.
- `conformance/check-parser.sh` — drives the fixture corpus through a
  dedicated dump entry point in `systemd-compat/` (`unit-dump` bin or
  `--dump-unit` flag; never scrape `cargo test` output). Build once, then run
  the binary per fixture and diff snapshots. Rendering must be deterministic:
  sorted keys, no absolute paths, no HashMap iteration order.

Acceptance: `sh conformance/run.sh` exits 0 here, runs all offline checks,
prints SKIP (naming the missing tool) for each unavailable tool/live oracle,
fails loudly when a pinned-v257.x oracle mismatches (other present versions
are informational and never fail the run), and never silently passes a
missing fixture.

### D3 — Unit-file parser (in `systemd-compat/`)

Create the crate `systemd-compat/` (package name `velocity-systemd`; lib +
`velocity-systemd` bin + a tiny `unit-dump` bin for conformance snapshots).
Implement the parser first, in `systemd-compat/src/unit/`.

Scope for this milestone:

- Parse `.service`, `.target`, `.socket`, `.timer`, `.path`, `.slice` files,
  and recognize `.scope` as a unit type/name without parsing scope files from
  disk (systemd rejects non-transient scope units, verified v257.13); full
  section/comment/continuation syntax; drop-ins (`.d/*.conf`) with systemd's
  v257.13 merge semantics for the sections you model: across all drop-in
  directories exactly one file per basename survives (the highest-precedence
  directory wins; a `/dev/null` symlink masks lower-precedence files of the
  same name), and the survivors apply in basename `strcmp` order regardless
  of which directory they live in — not directory-by-directory.
- Model at minimum: `[Unit]` (Description, After, Before, Requires, Wants,
  Conflicts, PartOf, BindsTo, Upholds — omit what you cannot verify),
  `[Install]` (WantedBy, RequiredBy, Alias; `WantedBy`/`RequiredBy` are
  `[Install]`-only, never `[Unit]`), `[Service]`
  (Type, ExecStart, ExecStop, ExecStartPre/Post, ExecStopPost, Restart,
  RestartSec, Environment, EnvironmentFile, WorkingDirectory, User, Group,
  RemainAfterExit, TimeoutStartSec/StopSec), `[Socket]` (ListenStream/Datagram,
  Accept, Service, SocketMode — subset), `[Timer]` (OnCalendar, OnBootSec,
  OnUnitActiveSec, Unit, Persistent), `[Path]` (PathExists, PathChanged,
  Unit), `[Slice]` (bare). Anything you cannot verify against the pinned
  source: leave out and list as a gap.
- Preserve unknown keys (store raw) rather than erroring — that mirrors
  systemd behavior and keeps the parser useful before full coverage. The
  parser is a library and must not log: return diagnostics (`unknown key`,
  `unknown section`, …) with file+line so the caller chooses the log level.
  Do not silently drop anything.
- Errors carry file + line + key; parser never panics on any input; file-size
  cap mirrored from `src/lsb_parser.rs` (1 MiB) practices; recursion/drop-in
  count bounded.
- Drop-in discovery takes an ordered directory list from the caller (no
  hardcoded `/etc/systemd` layout), most-specific first: for the unit's own
  name, `<name>.d/` (for instances the instance `.d/` before the template
  `.d/`, then the dash-truncation prefix chain), then the same drill-down
  for each alias's `<alias>.d/`, and the generic type-level `<type>.d/`
  last (verified v257.13).
- Preserve source order and origin (main file vs drop-in, filename, line) in
  the parse result so later merge semantics (scalar last-wins, list
  accumulate) never require re-parsing.
- Snapshot format: your own stable, human-diffable debug rendering (no serde),
  exposed through the `unit-dump` entry point above. Deterministic ordering;
  `conformance/check-parser.sh` diffs it.
- Include a deterministic pseudo-fuzz test (seeded byte mutations over the
  corpus; assert no panic, bounded time/memory) plus a note that `cargo-fuzz`
  targets will follow in a later phase.

Acceptance: crate tests green; all fixtures and vendored corpus parse; snapshot
diffs stable; parser exposes a documented API usable by the future manager.

### D4 — Control-protocol v2 + facade skeleton

**D4a — PID-1 side (primarily `src/control_socket.rs`; additive hooks in
`src/event_loop/dispatch.rs` and `src/lib.rs` are allowed — they are
agent-owned. Ask before any other PID-1 file, especially user WIP):**

- Versioned handshake; v1 request handling byte-identical in behavior. Note
  the de-facto v1 contract `velocityctl` relies on: the server closes the
  connection after flushing the reply (`read_to_string`). Do not change that
  for un-negotiated connections. Every v2-only verb (`INSPECT`, `SUBSCRIBE`,
  any `JOB`/list addition) is honored only after a successful v2 handshake;
  before one, it must take exactly the v1 unknown-command path (`ERR unknown
  command: …`, then close-after-reply).
- New requests (names are yours to choose, keep them boring):
  - `INSPECT <unit>` — typed snapshot: name, kind ("service" for now), state,
    pid, exec argv, cgroup path (via `cgroups::service_cgroup_dir`), restart
    policy, monotonic uptime (`Service::running_since` is an `Instant`:
    report seconds-since-start, not wall clock). All fields come from existing
    `Supervisor::service`/`Service` accessors; do **not** modify
    `src/supervisor/mod.rs` (user WIP). Read-only; peer rules as STATUS.
  - `SUBSCRIBE` — streaming events after the reply: unit state transitions,
    job-ish results, shutdown begin. The current machinery works against
    this, so specify and test all of it:
    - `flush()` closes a connection when its out buffer drains and
      `reap_idle_control_conns` drops connections idle past the timeout; both
      would kill a subscriber. Add a per-connection subscribed mode that
      suppresses close-on-drain and exempts (or heartbeats) the idle reaper.
    - Events derive on the tick from bounded supervisor state diffs (or
      explicit publication from dispatch); no unbounded per-tick work. The
      existing once-per-tick hook is `reap_idle_control_conns` in
      `src/event_loop/dispatch.rs` (called from `event_loop/mod.rs`); as an
      `EventLoop` method it can read `self.supervisor`, so the diff/publish
      hook fits inside the D4a allowlist — do not edit `event_loop/mod.rs`
      without asking.
    - Reuse `MAX_OUTPUT_BYTES` as the queue cap; pick and document one
      overflow policy (close with a final `ERR` frame, or emit a gap event)
      and test it with a subscriber that never reads.
    - Require a privileged peer (root/owner): a subscriber pins a connection
      slot for its lifetime. INSPECT stays unprivileged, but must not expose
      environment.
    - One escaped line per event; shutdown-begin is best-effort during the
      existing drain — teardown never blocks on a subscriber.
  - `JOB`-style verbs if the existing `Start/Stop/Restart` need richer results
    (e.g. accepted/queued/completed); keep `Start/Stop/Restart` v1 semantics.
- Define the v2 frame grammar explicitly (names are yours, grammar is not):
  newline-terminated single-line frames with a documented escaping rule for
  control characters (`\n`, `\r`, `\\` or percent-encoding) so no field can
  inject a frame; frames capped at ≤ `MAX_COMMAND_BYTES` so the existing
  bounded-input overflow path stays authoritative for every connection (if you
  choose a higher cap, make it per-connection and explicitly bounded); the
  handshake reply is one line; every v1 verb stays valid on a v2 connection
  with identical payloads.
- All parsing bounded and peer-cred checked exactly as v1. Malformed input
  closes the connection, never panics. New tests mirror existing
  `control_socket` tests plus: v1 client pipelined against a v2-speaking
  server (byte-identical replies and close-after-reply), v2 handshake failure,
  v2 client against an un-negotiated server, oversized frames, frame
  injection, subscribed connection survives the idle timeout, non-reading
  subscriber hits the documented overflow policy, events observed across a
  service restart.

**D4b — Facade skeleton (`systemd-compat/`):**

- `velocity-systemd` binary with `--system` / `--user` (user mode is a stub
  that exits with a clear "not implemented" error), `--control-socket PATH`,
  `--log-level`; hand-rolled stderr logger (no new deps) so `--log-level`
  does something.
- Modules: `ipc/` (v2 client: handshake, request/response, event stream),
  `unit/` (D3 parser), `manager/` (in-memory unit registry + its own
  `UnitState` enum mapped to the wire strings — the crate is independent and
  must not use `velocity_init::models::ServiceState`; no execution yet),
  `bus/` (trait `SystemdApiBus` + no-op stub so the deferred zbus decision has
  one plug-in point), `logind/` and `journal/` as documented empty stubs so
  later phases don't restructure.
- On start: connect, handshake v2, fetch the unit list (document which verb),
  subscribe, then keep running and logging events until the control connection
  drops; exit non-zero on connect/handshake failure or unexpected drop (the
  future supervisor restarts it). **Not** supervised by PID 1 yet, **not**
  shipped in packaging.
- Integration test: the facade's ipc client against a tiny in-process test
  server that speaks v2 over a temp socket; assert handshake, inspect, and one
  received event. Do not reuse the PID-1 handler: the crates are independent
  and must not gain a path dependency.

Acceptance: both crates test green on the required targets; zero warnings;
`velocityctl` v1 flows unchanged (close-after-reply pinned by test); the v2
frame grammar, peer rules, and subscription lifetime/overflow are covered by
tests; facade connects to a sandboxed PID-1 only if the user runs it (not
part of any test that needs root/PID 1).

## 5. Execution order and checkpoints

1. Context load (section 2) + write `AGENTS.md` + `docs/systemd-compat/` D1.
2. D2 harness scaffolding (offline tier runnable; tool/live tiers SKIP).
3. D3 parser + corpus + snapshots.
4. D4a protocol v2 + tests (`control_socket.rs` plus additive
   `event_loop/dispatch.rs`/`lib.rs` hooks; ask before any other PID-1 file).
5. D4b facade skeleton + tests.
6. Full verification battery; write the report.

After each step, run the relevant tests immediately; do not batch verification
to the end.

## 6. Verification battery

```sh
# PID-1 crate: full suite + warnings (expect 0 warnings)
cargo test --no-fail-fast 2>&1 | grep -E "^(test result|error|warning)"
cargo check --all-targets 2>&1 | grep -E "^(warning|error)" || true

# New crate
cargo test --manifest-path systemd-compat/Cargo.toml --no-fail-fast
( cd systemd-compat && cargo check --all-targets 2>&1 | grep -E "^(warning|error)" || true )

# Conformance (offline tier must pass here; tool/live tiers SKIP)
sh conformance/run.sh

# Musl: the handoff's both-libc rule applies to D4a (PID-1 lib code).
# Re-create the throwaway toolchain per AGENT_HANDOFF.md §2 (lives in /tmp)
# and run the musl suite; if the toolchain cannot be created, say so
# explicitly in the report.
export RUSTUP_HOME=/tmp/velocity-toolchain/rustup CARGO_HOME=/tmp/velocity-toolchain/cargo
CARGO_TARGET_DIR=/tmp/velocity-toolchain/target-musl \
  /tmp/velocity-toolchain/cargo/bin/cargo test --target x86_64-unknown-linux-musl \
  --no-fail-fast 2>&1 | grep -E "^(test result|error|warning)"
```

Expected: existing suite counts stay green (247 lib tests ± your additions) on
both libc targets, all new tests pass, 0 warnings, conformance exits 0.

## 7. Report format (final message)

- What changed: file list with one-line purpose each.
- Open items / gaps: parser keys left out, XML fixture status, deferred
  decisions, the exact tag/commit you verified.
- Commands run + observed results (copy the actual summary lines), including
  both libc targets or the explicit reason musl was skipped.
- Questions for the user: anything you had to guess.
- Explicit statement of what you did **not** do (no commits, no deps, no PID-1
  behavior change beyond v2, no user files touched).

## 8. Stop and ask

Stop and ask the user before: editing `src/main.rs` or any file listed as user
WIP; touching a PID-1 file other than `src/control_socket.rs`,
`src/event_loop/dispatch.rs`, and `src/lib.rs`; adding any dependency;
committing; making a non-additive change to v1 protocol behavior; or
implementing anything that requires root/PID-1 execution to test. The root
`Cargo.toml` is out of bounds this phase (section 3.2), not an ask-first item.
For a systemd API you cannot verify against the pinned source, mark
`TODO verify` in the docs and continue — only stop if that blocks a
deliverable.
