//! Process supervision: spawn, track, and restart services.
//!
//! # How it works
//!
//! - **Spawning** — each service is `fork`ed and the child `execvp`s
//!   its `/etc/init.d/<name> start` script. The child creates its own
//!   process group (`setpgid`) so a failed service tree can be signalled
//!   reliably, and it resets the signal mask inherited from PID 1 so
//!   daemons actually receive SIGTERM later.
//! - **Tracking** — a `pid → name` map links reaped children back to
//!   their service.
//! - **Log routing** — each child gets a pipe for stdout/stderr; PID 1
//!   drains the read end in the event loop and logs the output with a
//!   `[service]` prefix, so daemons don't scribble on the console.
//! - **Exponential backoff** — a service that exits non-zero (or is
//!   killed by a signal) gets a restart scheduled on a per-service
//!   `timerfd`: 1s, 2s, 4s, 8s, 16s, capped at 30s. The counter resets
//!   after the service stays up for `STABLE_RUNNING_SECS` seconds.
//! - **SysV daemonising** — SysV scripts typically fork a daemon and
//!   exit `0` immediately. We treat a quick, clean exit `0` as
//!   "started successfully (daemonized)" and *never* restart it.

use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, CString, OsString};
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::sys::signal::{kill, sigprocmask, SigSet, SigmaskHow, Signal};
use nix::sys::socket::{
    bind, getsockname, listen, recvfrom, setsockopt, socket, sockopt, AddressFamily, Backlog,
    SockFlag, SockProtocol, SockType, SockaddrIn, SockaddrStorage, UnixAddr,
};
use nix::sys::time::{TimeSpec, TimeValLike};
use nix::sys::timerfd::{ClockId, Expiration, TimerFd, TimerFlags, TimerSetTimeFlags};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{close, dup2, fork, getpgid, pipe2, read, setpgid, ForkResult, Pid};

use crate::cgroups;
use crate::models::{RestartPolicy, Service, ServiceState, SocketKind, SystemState};

pub mod restart;
pub mod sockets;
pub mod spawn;
pub mod stop;

// ────────────────────── backoff parameters ───────────────────────────

/// First restart delay (milliseconds).
pub const BACKOFF_BASE_MS: u64 = 1000;
/// Hard cap on the restart delay (milliseconds).
pub const BACKOFF_CAP_MS: u64 = 30_000;
/// A service is considered "stable" after running this long, which
/// resets its restart counter.
pub const STABLE_RUNNING_SECS: u64 = 10;

/// Compute the exponential backoff delay for a given restart attempt.
///
/// attempt 1 → 1s, 2 → 2s, 3 → 4s, 4 → 8s, 5 → 16s, >= 6 → 30s.
/// attempt 0 cannot happen (attempts start at 1) but maps to the base
/// delay rather than panicking or underflowing.
pub fn backoff_delay_ms(attempts: u32) -> u64 {
    // 2^5 * 1000 = 32_000 already exceeds the cap, so exponents above 5
    // all saturate to BACKOFF_CAP_MS; clamping here keeps the shift
    // infallible without a checked_shl/unwrap_or dance.
    let exp = attempts.saturating_sub(1).min(5);
    BACKOFF_BASE_MS.saturating_mul(1u64 << exp).min(BACKOFF_CAP_MS)
}

// ───────────────────────── SupervisorError ──────────────────────────

/// Errors produced by the supervisor.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    /// The requested service is not registered.
    #[error("unknown service: {0}")]
    UnknownService(String),

    /// `fork` failed.
    #[error("fork failed: {0}")]
    Fork(#[from] nix::errno::Errno),

    /// A one-shot action command contains a NUL byte and cannot be
    /// passed to `execve`.
    #[error("action command contains NUL byte")]
    BadCommand,

    /// A built-in service carries an explicit argv (`Service::cmd`)
    /// but the vector is empty, so there is no executable to `execve`.
    /// `Service::cmd` is a public field, so this is operator/reachable
    /// input — PID 1 must return an error, never index `args[0]`.
    #[error("empty command vector for {0}")]
    EmptyCommand(String),

    /// Failed to convert a path to a C string for `execvp`.
    #[error("invalid script path for {service}: {source}")]
    BadPath {
        service: String,
        #[source]
        source: std::ffi::NulError,
    },

    /// `waitpid` failed.
    #[error("waitpid failed: {0}")]
    Wait(Errno),

    /// Setting or creating a restart timer failed.
    #[error("restart timer error for {service}: {source}")]
    Timer {
        service: String,
        #[source]
        source: nix::errno::Errno,
    },

    /// `fcntl` failed while preparing a service's fds.
    #[error("fcntl error: {0}")]
    Fcntl(Errno),

    /// A cgroup operation failed (usually non-fatal).
    #[error("cgroup error: {0}")]
    Cgroup(#[from] std::io::Error),

    /// Creating or binding a notify / activation socket failed.
    #[error("socket error for {service}: {source}")]
    Socket {
        service: String,
        #[source]
        source: nix::errno::Errno,
    },

    /// Creating the log-routing pipe failed.
    #[error("log pipe error: {0}")]
    LogPipe(Errno),

    /// An environment file (`# X-Velocity-Env:`) could not be read.
    #[error("env file {path} for {service}: {source}")]
    EnvFile {
        service: String,
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The system is shutting down; new starts are refused.
    #[error("system is shutting down")]
    ShuttingDown,

    /// The service is already up (or still coming up); `START` would
    /// orphan the child we already hold.
    #[error("{0} is already running")]
    AlreadyRunning(String),

    /// The service is being stopped right now; spawning it again would
    /// race the teardown and leave the dying child unsupervised.
    #[error("{0} is already stopping")]
    AlreadyStopping(String),

    /// The service has no process at all and cannot be stopped (it was
    /// never started, or its script already finished).
    #[error("{0} is not running")]
    NotRunning(String),
}

/// Result alias for the supervisor.
pub type SupResult<T> = Result<T, SupervisorError>;

// ─────────────────── environment files (.env / /etc/default) ─────────

/// Parse a SysV `/etc/default/<service>`-style environment file:
///
/// - `KEY=VALUE` lines become pairs;
/// - `#` comments and blank lines are ignored;
/// - an optional leading `export` followed by whitespace is stripped
///   (any whitespace: spaces, tabs — not just one space);
/// - `KEY` without `=` is ignored, as are keys that are not valid shell
///   names (`[A-Za-z_][A-Za-z0-9_]*`);
/// - the value is trimmed of surrounding whitespace, a matching pair of
///   surrounding single/double quotes is removed (`FOO="bar baz"` yields
///   `bar baz`, not `"bar baz"`), and an unquoted trailing ` # comment`
///   is stripped.
///
/// This is intentionally a minimal parser — no variable expansion, no
/// escape processing, no multi-line values.
pub fn parse_env_file(path: &Path) -> std::io::Result<Vec<(String, String)>> {
    let text = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `export FOO=bar`, `export\tFOO=bar`, `export  FOO=bar`…
        // but not `exportFOO=bar` (no whitespace = part of the key).
        let line = match line.strip_prefix("export") {
            Some(rest)
                if rest.starts_with(char::is_whitespace) || rest.is_empty() =>
            {
                rest.trim_start()
            }
            _ => line,
        };
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            if !is_valid_env_key(key) {
                continue;
            }
            out.push((key.to_string(), clean_env_value(value)));
        }
    }
    Ok(out)
}

/// True when `key` is a valid shell variable name.
fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Trim an env-file value: surrounding whitespace, one layer of matching
/// quotes, and an unquoted trailing comment.
fn clean_env_value(raw: &str) -> String {
    let value = raw.trim();
    // Quoted: strip one matching layer and keep everything inside
    // verbatim (including `#`, which must not start a comment there).
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return value[1..value.len() - 1].to_string();
        }
    }
    // Unquoted: a `#` preceded by whitespace starts a comment.
    let mut end = value.len();
    let bytes = value.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'#' && i > 0 && bytes[i - 1].is_ascii_whitespace() {
            end = i;
            break;
        }
    }
    value[..end].trim_end().to_string()
}

// ─────────── shared child-environment helpers (spawn sites) ─────────

/// Fallback `PATH` for supervised children when the inherited
/// environment has none (a bare kernel launch gives init an almost
/// empty env, and a script that calls `start-stop-daemon` by bare name
/// then fails with "not found").
///
/// Matches the historical sysvinit default. An explicitly configured
/// `PATH` — from the inherited environment or an env file — is never
/// overwritten; this only fills the gap.
pub(crate) const DEFAULT_SERVICE_PATH: &str = "/sbin:/usr/sbin:/bin:/usr/bin";

/// Ensure a merged child environment has a usable `PATH`.
///
/// Inserts [`DEFAULT_SERVICE_PATH`] only when no `PATH` is present, so
/// an operator's explicit `PATH` always wins. Shared by `spawn` and
/// `spawn_one_shot` so the two fork sites cannot drift apart.
pub(crate) fn ensure_default_path(env: &mut HashMap<OsString, OsString>) {
    env.entry(OsString::from("PATH"))
        .or_insert_with(|| OsString::from(DEFAULT_SERVICE_PATH));
}

/// Convert a merged `NAME -> VALUE` map into `NAME=VALUE` CStrings for
/// `execve`, skipping entries with NUL bytes (which `execve` cannot
/// represent) with a warning instead of failing the whole spawn.
///
/// Shared by `spawn` and `spawn_one_shot`: both fork sites previously
/// open-coded this loop, and any divergence (e.g. one using `vars()`
/// and panicking on non-UTF-8, the other using `vars_os`) would be a
/// PID 1 panic waiting for a hostile environment.
pub(crate) fn cstrings_from_env_map(
    map: HashMap<OsString, OsString>,
    service: &str,
) -> Vec<CString> {
    let mut out = Vec::with_capacity(map.len());
    for (k, v) in map {
        // Plain `NAME=VALUE`: no leading `=`. That prefix is the
        // hidden-entry convention for `putenv`, and `getenv` in
        // both glibc and musl *skips* such entries — a service would
        // silently see an empty environment.
        let mut entry = k.clone();
        entry.push("=");
        entry.push(&v);
        match CString::new(entry.as_os_str().as_bytes()) {
            Ok(c) => out.push(c),
            // Only a NUL byte can fail, and it cannot reach us from
            // `vars_os` — but PID 1 does not get to trust that.
            Err(_) => warn!(
                "skipping env entry with a NUL byte for {}: {:?}",
                service,
                k.to_string_lossy()
            ),
        }
    }
    out
}

// ─────────────────────────── Supervisor ─────────────────────────────

/// Tracks and supervises all discovered services.
pub struct Supervisor {
    /// Service state by name.
    services: HashMap<String, Service>,
    /// Reaped child PID → service name.
    pid_to_service: HashMap<Pid, String>,
    /// Per-service timers: one `timerfd` each, armed on failure (fire →
    /// restart) or on stop (fire → SIGKILL escalation).
    ///
    /// The same fd serves two masters — a backoff that means "respawn"
    /// and a stop escalation that means "SIGKILL". Inferring the intent
    /// from the service's state at fire time is wrong: the state can
    /// legitimately change while the timer is armed (a `STOP`ed child
    /// that exits promptly goes `Stopping` → `Stopped` *before* the
    /// escalation fires, and a stale escalation would then read as "not
    /// stopping → restart", silently undoing `velocityctl stop`). The
    /// purpose recorded at arm time (see [`SvcTimer`]) makes the
    /// fire-time decision independent of the state that happened to be
    /// current when the timer was set.
    ///
    /// Entries are never removed once created: the event loop registers
    /// the fd number in epoll, so closing it here would invalidate that
    /// mapping. Disarming only clears the expiry and the purpose.
    timers: HashMap<String, SvcTimer>,
    /// Per-service readiness notify socket (abstract Unix datagram),
    /// created once on first start and reused across restarts.
    notify_sockets: HashMap<String, OwnedFd>,
    /// Per-service socket-activation fds, created once and kept
    /// listening for the lifetime of the service.
    socket_fds: HashMap<String, Vec<OwnedFd>>,
    /// Per-service log-routing pipe read ends: the service's
    /// stdout/stderr is dup2'ed onto the write end, PID 1 drains the
    /// read end in the event loop and logs it with a `[name]` prefix.
    log_pipes: HashMap<String, OwnedFd>,
    /// Global lifecycle state; gates starts and restarts once a
    /// shutdown has begun.
    system_state: SystemState,
    /// Per-service timestamps of the most recent restarts, used to
    /// enforce the sysvinit "respawn limit": a respawn service that
    /// dies more than `RESPAWN_MAX_IN_WINDOW` times within
    /// `RESPAWN_WINDOW_SECS` is no longer restarted (crash loop guard).
    respawn_history: HashMap<String, VecDeque<Instant>>,
}

/// One service's timer: the fd plus what its current arming means.
///
/// Keeping the two together (instead of parallel `fd` / `purpose` maps)
/// makes "armed with a purpose" a single entry: it cannot claim a
/// purpose for a timer that failed to arm, nor arm a timer whose
/// purpose was lost.
struct SvcTimer {
    /// The `timerfd`. Created lazily, never removed (see `timers`).
    fd: TimerFd,
    /// What the current arming means, if the timer is armed.
    purpose: Option<TimerPurpose>,
}

/// Why a per-service timer is armed (see [`SvcTimer`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TimerPurpose {
    /// A backoff elapsed: respawn the service.
    Restart,
    /// A graceful stop outlived its grace period: SIGKILL the group.
    /// Never a respawn, whatever the service's state says by now.
    StopEscalation,
}

// ────────────────────── respawn guard parameters ──────────────────────

/// How many restarts are tolerated within the window before a respawn
/// service is considered to be crash-looping (sysvinit uses 5/minute).
pub const RESPAWN_MAX_IN_WINDOW: usize = 5;
/// The window over which respawn frequency is measured.
pub const RESPAWN_WINDOW_SECS: Duration = Duration::from_secs(60);
/// Minimum delay between `RestartPolicy::Always` restarts. Without this a
/// service that cannot `exec` busy-loops `fork()` at millions/sec and pegs
/// a CPU. A clean logout (exit 0) still waits this long before respawning.
pub const RESPAWN_MIN_DELAY_MS: u64 = 200;
/// Grace period (seconds) a service gets to exit after SIGTERM during
/// shutdown, before `stop_all` escalates to SIGKILL.
pub const STOP_TERM_TIMEOUT_SECS: u64 = 2;
/// Extra grace (seconds) after SIGKILL before giving up on a straggler
/// (which the root-cgroup sweep finishes off anyway).
pub const STOP_KILL_GRACE_SECS: u64 = 1;
/// How often the synchronous stop paths (`stop_service`, `stop_all`)
/// poll `waitpid` while waiting for a SIGTERM/SIGKILL to take effect.
/// Short enough to reap promptly, long enough to avoid a busy loop.
pub const STOP_POLL_INTERVAL_MS: u64 = 10;

impl Supervisor {
    /// Create an empty supervisor.
    pub fn new() -> Self {
        Self {
            services: HashMap::new(),
            pid_to_service: HashMap::new(),
            timers: HashMap::new(),
            notify_sockets: HashMap::new(),
            socket_fds: HashMap::new(),
            log_pipes: HashMap::new(),
            system_state: SystemState::Booting,
            respawn_history: HashMap::new(),
        }
    }

    /// True if this service is under the sysvinit-style respawn guard
    /// (i.e. its restart policy is `Always`), so the caller can decide
    /// whether to invoke [`Supervisor::record_respawn`].
    pub fn is_respawn_service(&self, name: &str) -> bool {
        self.services
            .get(name)
            .is_some_and(|s| s.restart_policy == RestartPolicy::Always)
    }

    /// Record a restart of a respawn service and answer whether a
    /// further restart is permitted. Beyond `RESPAWN_MAX_IN_WINDOW`
    /// restarts in `RESPAWN_WINDOW_SECS` the service is marked `Failed`
    /// and the answer is `false` — PID 1 stops feeding the crash loop.
    pub fn record_respawn(&mut self, name: &str) -> bool {
        let now = Instant::now();
        let log = self.respawn_history.entry(name.to_string()).or_default();
        log.push_back(now);
        // `saturating_duration_since` so a clock oddity can never panic
        // PID 1 (AGENTS.md: no panics in PID 1 code paths).
        while log.front().is_some_and(|t| {
            now.saturating_duration_since(*t) > RESPAWN_WINDOW_SECS
        }) {
            log.pop_front();
        }
        if log.len() > RESPAWN_MAX_IN_WINDOW {
            warn!(
                "{:<20} crashing — respawn limit reached, giving up ({} in {:.0}s)",
                name,
                log.len(),
                RESPAWN_WINDOW_SECS.as_secs_f64()
            );
            if let Some(svc) = self.services.get_mut(name) {
                // The guard sink accepts `Failed` from any state, so
                // this cannot be refused; warn in release too so a
                // future state-machine change that breaks that is visible.
                if !svc.transition(ServiceState::Failed) {
                    warn!("{:<20} refused Failed (state {})", name, svc.state());
                }
                svc.restart_pending = false;
            }
            return false;
        }
        true
    }

    /// Forget a service's crash-loop history.
    ///
    /// The respawn guard counts failures inside `RESPAWN_WINDOW_SECS`
    /// and, once the limit is reached, refuses every further restart —
    /// including the first restart an operator triggers by hand. An
    /// explicit `START`/`RESTART` is exactly the "I fixed it" signal
    /// the guard is waiting for, so it clears the window.
    pub fn reset_respawn_history(&mut self, name: &str) {
        if self.respawn_history.remove(name).is_some() {
            debug!("{}: respawn history cleared (operator request)", name);
        }
    }

    /// The current global lifecycle state.
    pub fn system_state(&self) -> SystemState {
        self.system_state
    }

    /// Mark boot complete: the system is now Running. No-op if already
    /// past Booting (a shutdown may have raced the boot tail).
    pub fn mark_system_running(&mut self) {
        if self.system_state == SystemState::Booting {
            self.system_state = SystemState::Running;
            info!("system state → running");
        }
    }

    /// Enter the `ShuttingDown` state.
    ///
    /// Returns `true` if this call performed the transition (i.e. the
    /// first shutdown request); `false` if shutdown had already begun,
    /// so a second request is a no-op and cannot restart the teardown.
    pub fn begin_shutdown(&mut self) -> bool {
        if self.system_state == SystemState::ShuttingDown {
            return false;
        }
        self.system_state = SystemState::ShuttingDown;
        info!("system state → shutting-down");
        true
    }

    /// Register a service for supervision.
    ///
    /// Re-registering a name replaces the previous definition (the last
    /// discovery wins) but is worth a journal line: duplicate provides
    /// usually mean two scripts claim the same facility.
    pub fn register(&mut self, service: Service) {
        if self.services.contains_key(&service.name) {
            warn!(
                "re-registering service {}: replacing previous definition",
                service.name
            );
        }
        self.services.insert(service.name.clone(), service);
    }

    /// Look up a service's current state by name.
    pub fn service(&self, name: &str) -> Option<&Service> {
        self.services.get(name)
    }

    /// The per-service timerfd for a service, if one exists.
    ///
    /// Despite the name it serves both timer purposes (backoff restart
    /// and stop escalation); see [`TimerPurpose`]. Use
    /// [`Supervisor::on_timer_fire`](crate::supervisor::Supervisor::on_timer_fire)
    /// — never the service state — to tell which one fired.
    pub fn restart_timer(&self, name: &str) -> Option<&TimerFd> {
        self.timers.get(name).map(|t| &t.fd)
    }

    /// Disarm a service's timer and forget what it was for.
    ///
    /// Used wherever a pending timer must not survive: a fresh `start`
    /// supersedes it, a stop or the teardown cancels it. The entry (and
    /// its fd number, which the event loop registered in epoll) is kept;
    /// only the expiry and the purpose are cleared.
    fn disarm_timer(&mut self, name: &str) {
        if let Some(t) = self.timers.get_mut(name) {
            let _ = disarm(&t.fd);
            t.purpose = None;
        }
    }

    /// Iterate over all registered services.
    pub fn services(&self) -> impl Iterator<Item = &Service> {
        self.services.values()
    }
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

/// Put `O_NONBLOCK` on a single fd, preserving its other flags.
///
/// Used for the log pipe's *read* end only. The write end must stay
/// blocking or every service inherits a non-blocking stdout/stderr and
/// loses output to EAGAIN (see the call site in `spawn`).
fn set_nonblocking(fd: RawFd) -> SupResult<()> {
    let flags = fcntl(fd, FcntlArg::F_GETFL).map_err(SupervisorError::Fcntl)?;
    let bits = OFlag::from_bits_truncate(flags);
    fcntl(fd, FcntlArg::F_SETFL(bits | OFlag::O_NONBLOCK))
        .map(|_| ())
        .map_err(SupervisorError::Fcntl)
}

/// Disarm a timerfd (clear any pending alarm).
fn disarm(timer: &TimerFd) -> Result<(), nix::errno::Errno> {
    timer.set(
        Expiration::OneShot(TimeSpec::seconds(0)),
        TimerSetTimeFlags::empty(),
    )
}

/// Signal a supervised child's **whole process group**.
///
/// `killpg` takes a *process-group id*, not a pid, and a group only
/// exists under that name when the child really is its group leader —
/// which is what the `setpgid` in the child (mirrored in the parent to
/// close the race) arranges. If that `setpgid` ever failed, the child's
/// group id is *not* its pid, and blindly calling `killpg(pid)` would
/// either match nothing at all (the service survives SIGTERM, the
/// operator waits out the full grace period and the SIGKILL, and gives
/// up on a process we never signalled) or, in the pathological case of a
/// recycled group id, address a group we have nothing to do with.
///
/// So: ask the kernel for the real group id and fall back to signalling
/// the single process when the child is not a group leader. Either way
/// the signal only ever reaches our own child.
pub fn signal_service(pid: Pid, sig: Signal) {
    if getpgid(Some(pid)) == Ok(pid) {
        let _ = nix::sys::signal::killpg(pid, sig);
    } else {
        debug!(
            "pid {} is not a process-group leader — signalling it directly",
            pid
        );
        let _ = kill(pid, sig);
    }
}

// ──────────────────── notify / activation sockets ─────────────────────

/// Abstract socket name used for a service's readiness notifier, as seen
/// by *this* init instance.
///
/// Abstract Unix sockets are namespaced to the network namespace and
/// vanish automatically when the owner exits — no filesystem litter,
/// no stale-socket cleanup.
pub fn notify_addr_bytes(name: &str) -> Vec<u8> {
    notify_addr_bytes_for(std::process::id(), name)
}

/// The notify address of `name` for the init instance running as `pid`.
///
/// The pid is part of the name so that two init processes on one machine
/// — a real PID 1 plus a developer's test run, or two tests whose
/// cleanup was interrupted — cannot fight over the same name. Without it
/// the second instance's `bind()` fails with `EADDRINUSE`, the service
/// is started with no `NOTIFY_SOCKET` at all, and the symptom is a
/// service stuck in `Starting` forever with only a warning to explain
/// it. The name is handed to services through `NOTIFY_SOCKET`, so it is
/// an internal detail either way.
pub fn notify_addr_bytes_for(pid: u32, name: &str) -> Vec<u8> {
    // "velocity-init/" (14) + pid (<= 10 digits for u32) + "/" (1).
    let mut v = Vec::with_capacity(25 + name.len());
    v.extend_from_slice(b"velocity-init/");
    v.extend_from_slice(pid.to_string().as_bytes());
    v.extend_from_slice(b"/");
    v.extend_from_slice(name.as_bytes());
    v
}

/// Create the per-service readiness notify socket (abstract Unix
/// datagram, non-blocking so the event loop can drain it safely).
fn create_notify_socket(name: &str) -> SupResult<OwnedFd> {
    // Computed once and reused for bind + logging (the helper
    // allocates, so calling it twice per start is pure waste).
    let addr_bytes = notify_addr_bytes(name);
    let addr =
        UnixAddr::new_abstract(&addr_bytes).map_err(|e| SupervisorError::Socket {
            service: name.to_string(),
            source: e,
        })?;
    let fd = socket(
        AddressFamily::Unix,
        SockType::Datagram,
        SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        None,
    )?;
    bind(fd.as_raw_fd(), &addr)?;
    debug!(
        "created notify socket for {} at @{}",
        name,
        String::from_utf8_lossy(&addr_bytes)
    );
    Ok(fd)
}

/// Create one activation socket for a service.
///
/// `SOCK_CLOEXEC` is set so the fd never leaks into unrelated children;
/// the service's own child re-exposes it with `dup2` (which clears
/// `FD_CLOEXEC`) at the `LISTEN_FDS_START` positions.
fn create_one_socket(service: &str, spec: &SocketKind) -> Result<OwnedFd, Errno> {
    match spec {
        SocketKind::Tcp(port) => {
            let fd = socket(
                AddressFamily::Inet,
                SockType::Stream,
                SockFlag::SOCK_CLOEXEC,
                Some(SockProtocol::Tcp),
            )?;
            let _ = setsockopt(&fd, sockopt::ReuseAddr, &true);
            let addr = SockaddrIn::new(0, 0, 0, 0, *port);
            bind(fd.as_raw_fd(), &addr)?;
            listen(&fd, Backlog::new(16)?)?;
            let actual = getsockname::<SockaddrIn>(fd.as_raw_fd())
                .ok()
                .map(|a| a.port());
            info!(
                "{:<20} listening on 0.0.0.0:{} (socket activation)",
                service,
                actual.unwrap_or(*port)
            );
            Ok(fd)
        }
        SocketKind::Unix(path) => {
            let _ = std::fs::remove_file(path);
            let addr = UnixAddr::new(path.as_path())?;
            let fd = socket(
                AddressFamily::Unix,
                SockType::Stream,
                SockFlag::SOCK_CLOEXEC,
                None,
            )?;
            bind(fd.as_raw_fd(), &addr)?;
            listen(&fd, Backlog::new(16)?)?;
            info!(
                "{:<20} listening on {} (socket activation)",
                service,
                path.display()
            );
            Ok(fd)
        }
    }
}

/// Create all activation sockets for a service; per-socket failures are
/// logged and skipped so the service still starts.
fn create_socket_fds(service: &str, specs: &[SocketKind]) -> Vec<OwnedFd> {
    let mut fds = Vec::new();
    for spec in specs {
        match create_one_socket(service, spec) {
            Ok(fd) => fds.push(fd),
            Err(e) => warn!(
                "could not create activation socket {} for {}: {}",
                spec, service, e
            ),
        }
    }
    fds
}

/// Replace a `LISTEN_PID=0` placeholder with the child's real PID.
///
/// The environment is allocated in the parent *before* `fork`, so the
/// child only mutates already-owned memory; the rewritten value lives
/// in the caller-provided stack buffer (alive until `execve`). No
/// allocation or heap access happens after `fork`.
///
/// # Safety
///
/// - `envp` must be null-terminated and point only to valid NUL-
///   terminated strings (or null), with exclusive access for the
///   duration of the call.
/// - `buf` must be a caller-owned `[u8; 40]` that outlives `envp`'s use
///   (i.e. lives until `execve`); the function writes `LISTEN_PID=<pid>`
///   plus NUL into it and points one `envp` slot at it.
/// - Call in the child: the pid is read with `getpid()` (async-
///   signal-safe), so calling in the parent would stamp the parent's
///   pid. `std::process::id()` is deliberately *not* used here — it
///   goes through `std` and is not documented as async-signal-safe,
///   which is the only contract the post-fork child may rely on.
unsafe fn raw_rewrite_listen_pid(envp: &mut [*const libc::c_char], buf: &mut [u8; 40]) {
    for slot in envp.iter_mut() {
        if slot.is_null() {
            break;
        }
        let bytes = CStr::from_ptr(*slot).to_bytes();
        if bytes != b"LISTEN_PID=0" {
            continue;
        }
        buf[..11].copy_from_slice(b"LISTEN_PID=");
        let mut v = libc::getpid() as u64;
        let mut i = 11;
        loop {
            buf[i] = b'0' + (v % 10) as u8;
            i += 1;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        buf[11..i].reverse();
        buf[i] = 0;
        *slot = buf.as_ptr() as *const libc::c_char;
        return;
    }
}

// ─────────────────────────── unit tests ──────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backoff_sequence() {
        // 1s, 2s, 4s, 8s, 16s, then capped at 30s.
        let expected = [1000, 2000, 4000, 8000, 16000, 30000, 30000];
        for (i, want) in expected.iter().enumerate() {
            assert_eq!(backoff_delay_ms(i as u32 + 1), *want, "attempt {}", i + 1);
        }
    }

    #[test]
    fn test_backoff_never_underflows() {
        // attempt 0 (shouldn't happen, but must not panic).
        assert_eq!(backoff_delay_ms(0), 1000);
        // a huge counter must saturate, not overflow.
        assert_eq!(backoff_delay_ms(u32::MAX), 30000);
    }

    #[test]
    fn test_register_and_lookup() {
        let mut sup = Supervisor::new();
        sup.register(Service::new("ssh".into(), "/etc/init.d/ssh".into()));
        let svc = sup.service("ssh").expect("registered");
        assert_eq!(svc.name, "ssh");
        assert_eq!(svc.state(), ServiceState::Stopped);
        assert_eq!(svc.restart_attempts, 0);
        assert!(sup.service("nope").is_none());
    }

    #[test]
    fn test_start_unknown_service() {
        let mut sup = Supervisor::new();
        assert!(matches!(
            sup.start("ghost"),
            Err(SupervisorError::UnknownService(_))
        ));
    }

    #[test]
    fn test_parse_env_file_basic() {
        let path = std::env::temp_dir().join(format!("velocity-env-test-{}", std::process::id()));
        std::fs::write(
            &path,
            "# comment line\n\nFOO=bar\nexport BAZ=\"qux\"\nQUX=\n=novalue\n",
        )
        .unwrap();
        let parsed = parse_env_file(&path).expect("parses");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            parsed,
            vec![
                ("FOO".into(), "bar".into()),
                // Surrounding quotes are stripped: the service must see
                // `qux`, not `"qux"`.
                ("BAZ".into(), "qux".into()),
                ("QUX".into(), "".into())
            ]
        );
    }

    #[test]
    fn test_parse_env_file_quotes_comments_keys() {
        let path = std::env::temp_dir().join(format!(
            "velocity-env-test2-{}",
            std::process::id()
        ));
        std::fs::write(
            &path,
            concat!(
                "SINGLE='hello world'\n",
                "EMPTY_QUOTED=\"\"\n",
                "GREETING=hello # a trailing comment\n",
                "HASH_INSIDE=\"bar # not a comment\"\n",
                "NOSPACE=bar#kept\n",
                "export\tTABBED=1\n",
                "export  SPACED=2\n",
                "exportFOO=kept\n",
                "FOO BAR=ignored\n",
                "9BAD=ignored\n",
                "GOOD_KEY_1=ok\n",
            ),
        )
        .unwrap();
        let parsed = parse_env_file(&path).expect("parses");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            parsed,
            vec![
                ("SINGLE".into(), "hello world".into()),
                ("EMPTY_QUOTED".into(), "".into()),
                ("GREETING".into(), "hello".into()),
                ("HASH_INSIDE".into(), "bar # not a comment".into()),
                ("NOSPACE".into(), "bar#kept".into()),
                ("TABBED".into(), "1".into()),
                ("SPACED".into(), "2".into()),
                // No whitespace after `export`, so this is an ordinary
                // assignment to `exportFOO` — a valid shell name, kept.
                ("exportFOO".into(), "kept".into()),
                ("GOOD_KEY_1".into(), "ok".into()),
            ]
        );
    }

    #[test]
    fn test_parse_env_file_missing_is_error() {
        assert!(parse_env_file(&std::path::PathBuf::from("/nonexistent/velocity/env")).is_err());
    }

    #[test]
    fn test_restart_policy_scheduling() {
        // Always: a restart must be scheduled.
        let mut sup = Supervisor::new();
        sup.register(Service::builtin_getty("/sbin/agetty"));
        sup.restart_after_failure("getty@console", RestartPolicy::Always);
        let svc = sup.service("getty@console").expect("registered");
        assert_eq!(svc.restart_attempts, 1);
        assert!(svc.restart_pending);

        // OnFailure: backoff restart must be scheduled.
        let mut sup = Supervisor::new();
        sup.register(Service::new("ssh".into(), "/etc/init.d/ssh".into()));
        sup.restart_after_failure("ssh", RestartPolicy::OnFailure);
        let svc = sup.service("ssh").expect("registered");
        assert_eq!(svc.restart_attempts, 1);
        assert!(svc.restart_pending);

        // NoRestart: nothing may be scheduled.
        let mut sup = Supervisor::new();
        sup.register(Service::new(
            "one-shot".into(),
            "/etc/init.d/one-shot".into(),
        ));
        sup.restart_after_failure("one-shot", RestartPolicy::NoRestart);
        let svc = sup.service("one-shot").expect("registered");
        assert_eq!(svc.restart_attempts, 0);
        assert!(!svc.restart_pending);
    }

    #[test]
    fn test_system_state_lifecycle() {
        let mut sup = Supervisor::new();
        assert_eq!(sup.system_state(), SystemState::Booting);

        sup.mark_system_running();
        assert_eq!(sup.system_state(), SystemState::Running);

        // The first shutdown request performs the transition…
        assert!(sup.begin_shutdown());
        assert_eq!(sup.system_state(), SystemState::ShuttingDown);

        // …and any later one is a no-op.
        assert!(!sup.begin_shutdown());
        assert_eq!(sup.system_state(), SystemState::ShuttingDown);
    }

    #[test]
    fn test_start_refused_while_shutting_down() {
        let mut sup = Supervisor::new();
        sup.register(Service::new("ssh".into(), "/etc/init.d/ssh".into()));
        sup.begin_shutdown();
        assert!(matches!(
            sup.start("ssh"),
            Err(SupervisorError::ShuttingDown)
        ));
    }

    #[test]
    fn test_restart_suppressed_while_shutting_down() {
        let mut sup = Supervisor::new();
        sup.register(Service::new("ssh".into(), "/etc/init.d/ssh".into()));
        sup.begin_shutdown();

        // A failure after shutdown began must not schedule a restart.
        sup.pid_to_service
            .insert(Pid::from_raw(4242), "ssh".to_string());
        sup.handle_child_exit(
            Pid::from_raw(4242),
            WaitStatus::Exited(Pid::from_raw(4242), 1),
        );
        let svc = sup.service("ssh").expect("registered");
        assert_eq!(svc.restart_attempts, 0);
        assert!(!svc.restart_pending);

        // A clean exit during shutdown must not trigger the Always path.
        let mut sup = Supervisor::new();
        sup.register(Service::builtin_getty("/sbin/agetty"));
        sup.begin_shutdown();
        sup.pid_to_service
            .insert(Pid::from_raw(4243), "getty@console".to_string());
        sup.handle_child_exit(
            Pid::from_raw(4243),
            WaitStatus::Exited(Pid::from_raw(4243), 0),
        );
        let svc = sup.service("getty@console").expect("registered");
        assert_eq!(svc.state(), ServiceState::Stopped);
        assert_eq!(svc.restart_attempts, 0);
    }

    #[test]
    fn test_respawn_guard_gives_up_after_limit() {
        // A getty (RestartPolicy::Always) that exits 0 in a tight loop
        // must be restarted until RESPAWN_MAX_IN_WINDOW is exceeded, then
        // left Failed and no longer restarted (crash-loop guard).
        let mut sup = Supervisor::new();
        sup.register(Service::builtin_getty("/sbin/agetty"));
        sup.mark_system_running();

        for i in 1..=RESPAWN_MAX_IN_WINDOW {
            let pid = Pid::from_raw(1000 + i as i32);
            sup.pid_to_service.insert(pid, "getty@console".to_string());
            sup.handle_child_exit(pid, WaitStatus::Exited(pid, 0));
            let svc = sup.service("getty@console").expect("registered");
            assert_eq!(svc.state(), ServiceState::Starting, "attempt {}", i);
            assert!(svc.restart_pending, "attempt {}", i);
        }

        // One more exit must trip the guard: Failed, no pending restart.
        let pid = Pid::from_raw(1000 + RESPAWN_MAX_IN_WINDOW as i32 + 1);
        sup.pid_to_service.insert(pid, "getty@console".to_string());
        sup.handle_child_exit(pid, WaitStatus::Exited(pid, 0));
        let svc = sup.service("getty@console").expect("registered");
        assert_eq!(svc.state(), ServiceState::Failed);
        assert!(!svc.restart_pending);
    }

    #[test]
    fn test_always_failure_never_marks_failed_transiently() {
        // A non-zero exit on an Always service must restart (Starting),
        // not transiently report Failed to STATUS/LIST queries.
        let mut sup = Supervisor::new();
        sup.register(Service::builtin_getty("/sbin/agetty"));
        sup.mark_system_running();
        let pid = Pid::from_raw(7777);
        sup.pid_to_service.insert(pid, "getty@console".to_string());
        sup.handle_child_exit(pid, WaitStatus::Exited(pid, 1));
        let svc = sup.service("getty@console").expect("registered");
        assert_eq!(svc.state(), ServiceState::Starting);
        assert!(svc.restart_pending);
    }

    #[test]
    fn test_norestart_one_shot_clean_exit_is_stopped_not_running() {
        // A one-shot (sysv `once:` line, policy=no-restart) that exits 0
        // has *finished*. Reporting it as Running forever would advertise
        // a healthy service that has no process behind it.
        let mut sup = Supervisor::new();
        sup.register(Service::new("one-shot".into(), "/etc/init.d/one-shot".into()));
        {
            let svc = sup.services.get_mut("one-shot").expect("registered");
            svc.restart_policy = RestartPolicy::NoRestart;
            // A reaped child implies the service was started: walk there,
            // as `start` would have.
            assert!(svc.transition(ServiceState::Starting));
        }
        let pid = Pid::from_raw(6001);
        sup.pid_to_service.insert(pid, "one-shot".to_string());
        sup.handle_child_exit(pid, WaitStatus::Exited(pid, 0));

        let svc = sup.service("one-shot").expect("registered");
        assert_eq!(svc.state(), ServiceState::Stopped);
        assert!(svc.pid.is_none());
        assert!(!svc.restart_pending);
    }

    #[test]
    fn test_norestart_failure_is_failed_not_stuck_starting() {
        // A NoRestart service that exits non-zero must be reported as
        // `Failed`, not left in `Starting` with no child and no timer —
        // otherwise `velocityctl status` shows it as "starting" forever.
        let mut sup = Supervisor::new();
        sup.register(Service::new("oneshot".into(), "/etc/init.d/oneshot".into()));
        {
            let svc = sup.services.get_mut("oneshot").expect("registered");
            svc.restart_policy = RestartPolicy::NoRestart;
            assert!(svc.transition(ServiceState::Starting));
        }
        let pid = Pid::from_raw(6008);
        sup.pid_to_service.insert(pid, "oneshot".to_string());
        sup.handle_child_exit(pid, WaitStatus::Exited(pid, 1));
        let svc = sup.service("oneshot").expect("registered");
        assert_eq!(svc.state(), ServiceState::Failed);
        assert!(svc.pid.is_none());
        assert!(!svc.restart_pending);

        // Same for a service killed by a signal.
        let mut sup = Supervisor::new();
        sup.register(Service::new("oneshot2".into(), "/etc/init.d/oneshot2".into()));
        {
            let svc = sup.services.get_mut("oneshot2").expect("registered");
            svc.restart_policy = RestartPolicy::NoRestart;
            assert!(svc.transition(ServiceState::Starting));
        }
        let pid = Pid::from_raw(6009);
        sup.pid_to_service.insert(pid, "oneshot2".to_string());
        sup.handle_child_exit(pid, WaitStatus::Signaled(pid, nix::sys::signal::Signal::SIGKILL, false));
        let svc = sup.service("oneshot2").expect("registered");
        assert_eq!(svc.state(), ServiceState::Failed);
        assert!(svc.pid.is_none());
    }

    #[test]
    fn test_onfailure_daemonized_exit_is_still_running() {
        // The counterpart of the test above: for an OnFailure script, a
        // clean exit 0 genuinely means "the script forked a daemon and
        // returned", and the service is running.
        let mut sup = Supervisor::new();
        sup.register(Service::new("ssh".into(), "/etc/init.d/ssh".into()));
        // A reaped child implies the service was started: walk there, as
        // `start` would have (`Stopped -> Running` is rejected — a
        // service is never running without a spawn first).
        assert!(sup
            .services
            .get_mut("ssh")
            .expect("registered")
            .transition(ServiceState::Starting));
        let pid = Pid::from_raw(6002);
        sup.pid_to_service.insert(pid, "ssh".to_string());
        sup.handle_child_exit(pid, WaitStatus::Exited(pid, 0));

        let svc = sup.service("ssh").expect("registered");
        assert_eq!(svc.state(), ServiceState::Running);
        assert!(svc.pid.is_none(), "the daemon is not our child");
        assert!(svc.running_since.is_some());
    }

    #[test]
    fn test_child_exit_clears_the_pid_mapping() {
        let mut sup = Supervisor::new();
        sup.register(Service::new("ssh".into(), "/etc/init.d/ssh".into()));
        let pid = Pid::from_raw(6003);
        sup.pid_to_service.insert(pid, "ssh".to_string());
        sup.handle_child_exit(pid, WaitStatus::Exited(pid, 0));
        assert!(
            !sup.pid_to_service.contains_key(&pid),
            "a reaped child must not stay mapped"
        );
    }

    #[test]
    fn test_stop_continue_reports_keep_the_child_tracked() {
        // `waitpid` without WUNTRACED/WCONTINUED never reports these,
        // but the SIGCHLD drain forwards whatever it gets: a stop or
        // continue report must not consume the pid mapping or clear the
        // service's pid, or the later exit would be mis-attributed.
        let mut sup = Supervisor::new();
        sup.register(Service::new("ssh".into(), "/etc/init.d/ssh".into()));
        {
            let svc = sup.services.get_mut("ssh").expect("registered");
            assert!(svc.transition(ServiceState::Starting));
            svc.pid = Some(Pid::from_raw(6100));
        }
        let pid = Pid::from_raw(6100);
        sup.pid_to_service.insert(pid, "ssh".to_string());

        sup.handle_child_exit(
            pid,
            WaitStatus::Stopped(pid, nix::sys::signal::Signal::SIGSTOP),
        );
        assert!(
            sup.pid_to_service.contains_key(&pid),
            "a stopped child is still alive: keep it mapped"
        );
        assert_eq!(sup.service("ssh").unwrap().pid, Some(pid));

        sup.handle_child_exit(pid, WaitStatus::Continued(pid));
        assert!(
            sup.pid_to_service.contains_key(&pid),
            "a continued child is still alive: keep it mapped"
        );
        assert_eq!(sup.service("ssh").unwrap().pid, Some(pid));

        // The terminal exit afterwards is still attributed correctly.
        sup.handle_child_exit(pid, WaitStatus::Exited(pid, 0));
        assert!(!sup.pid_to_service.contains_key(&pid));
        assert_eq!(
            sup.service("ssh").unwrap().state(),
            ServiceState::Running,
            "daemonised exit 0 promotes to Running"
        );
    }

    #[test]
    fn test_failure_during_shutdown_is_stopped_not_starting() {
        // A failure that races the teardown must land in `Stopped`
        // (nothing will be respawned), not sit in `Starting` with no
        // child and no timer until stop_all gets to it.
        for status in [
            WaitStatus::Exited(Pid::from_raw(6201), 1),
            WaitStatus::Signaled(
                Pid::from_raw(6201),
                nix::sys::signal::Signal::SIGTERM,
                false,
            ),
        ] {
            let mut sup = Supervisor::new();
            sup.register(Service::new("ssh".into(), "/etc/init.d/ssh".into()));
            {
                let svc = sup.services.get_mut("ssh").expect("registered");
                assert!(svc.transition(ServiceState::Starting));
            }
            let pid = Pid::from_raw(6201);
            sup.pid_to_service.insert(pid, "ssh".to_string());
            sup.begin_shutdown();
            sup.handle_child_exit(pid, status);
            let svc = sup.service("ssh").expect("registered");
            assert_eq!(svc.state(), ServiceState::Stopped, "status {status:?}");
            assert!(svc.pid.is_none());
            assert!(!svc.restart_pending);
            assert!(!svc.restart_after_stop);
            assert_eq!(svc.restart_attempts, 0);
        }
    }

    #[test]
    fn test_stop_service_forgets_the_child_it_reaped() {
        // stop_service reaps with its own waitpid, so handle_child_exit
        // never runs for that pid: it has to clear the mapping itself.
        let mut sup = Supervisor::new();
        sup.register(Service::new(
            "sleeper".into(),
            temp_script("sleeper", "sleep 30").into(),
        ));
        let pid = sup.start("sleeper").expect("spawned");
        assert!(sup.pid_to_service.contains_key(&pid));

        sup.stop_service("sleeper").expect("stopped");
        assert!(
            !sup.pid_to_service.contains_key(&pid),
            "stopped child left behind: {:?}",
            sup.pid_to_service
        );
        let svc = sup.service("sleeper").expect("registered");
        assert_eq!(svc.state(), ServiceState::Stopped);
        assert!(svc.pid.is_none());
    }

    #[test]
    fn test_start_refused_while_already_running() {
        // A second spawn would orphan the child we are holding: its pid
        // mapping is overwritten, so it is never reaped or stopped.
        let mut sup = Supervisor::new();
        sup.register(Service::new("ssh".into(), "/etc/init.d/ssh".into()));

        // Running (daemonized, pid unknown). Walk the legal path:
        // a service reaches Running only via Starting.
        if let Some(svc) = sup.services.get_mut("ssh") {
            assert!(svc.transition(ServiceState::Starting));
            assert!(svc.transition(ServiceState::Running));
        }
        assert!(matches!(
            sup.start("ssh"),
            Err(SupervisorError::AlreadyRunning(_))
        ));

        // Starting with a live child and no restart armed
        // (`Running -> Starting` is the failure-restart edge).
        if let Some(svc) = sup.services.get_mut("ssh") {
            assert!(svc.transition(ServiceState::Starting));
            svc.pid = Some(Pid::from_raw(6004));
            svc.restart_pending = false;
        }
        assert!(matches!(
            sup.start("ssh"),
            Err(SupervisorError::AlreadyRunning(_))
        ));

        // A crash-loop restart (no pid, timer armed) must still go
        // through, otherwise the service could never come back.
        if let Some(svc) = sup.services.get_mut("ssh") {
            svc.pid = None;
            svc.restart_pending = true;
        }
        // `ssh` has no real script here, so the spawn itself fails —
        // what matters is that it was *attempted* (no AlreadyRunning).
        assert!(!matches!(
            sup.start("ssh"),
            Err(SupervisorError::AlreadyRunning(_))
        ));
    }

    #[test]
    fn test_operator_start_resets_the_crash_loop_budget() {
        // After the guard trips, a manual restart must be possible again
        // instead of being refused forever by the stale window.
        let mut sup = Supervisor::new();
        sup.register(Service::builtin_getty("/sbin/agetty"));
        sup.mark_system_running();
        for i in 0..=RESPAWN_MAX_IN_WINDOW {
            let pid = Pid::from_raw(2000 + i as i32);
            sup.pid_to_service.insert(pid, "getty@console".to_string());
            sup.handle_child_exit(pid, WaitStatus::Exited(pid, 0));
        }
        assert_eq!(
            sup.service("getty@console").expect("registered").state(),
            ServiceState::Failed
        );
        assert!(
            !sup.record_respawn("getty@console"),
            "guard still closed without operator action"
        );

        sup.reset_respawn_history("getty@console");
        assert!(
            sup.record_respawn("getty@console"),
            "an explicit restart must reopen the budget"
        );
    }

    #[test]
    fn test_stability_resets_backoff_without_losing_uptime() {
        let mut sup = Supervisor::new();
        sup.register(Service::new("ssh".into(), "/etc/init.d/ssh".into()));
        let started = Instant::now() - Duration::from_secs(STABLE_RUNNING_SECS + 5);
        if let Some(svc) = sup.services.get_mut("ssh") {
            assert!(svc.transition(ServiceState::Starting));
            assert!(svc.transition(ServiceState::Running));
            svc.running_since = Some(started);
            svc.restart_attempts = 4;
        }
        sup.respawn_history.insert("ssh".to_string(), VecDeque::new());

        sup.check_backoff_reset(Instant::now());

        let svc = sup.service("ssh").expect("registered");
        assert_eq!(svc.restart_attempts, 0);
        assert_eq!(
            svc.running_since,
            Some(started),
            "uptime stamp must survive the backoff reset"
        );
        assert!(
            !sup.respawn_history.contains_key("ssh"),
            "a stable service has recovered: clear its crash-loop history"
        );
    }

    // ── asynchronous stop / restart ────────────────────────────────

    /// `request_stop` must acknowledge immediately and leave the actual
    /// teardown to the event loop: it may not block on the child.
    #[test]
    fn test_request_stop_does_not_block_and_defers_the_transition() {
        let mut sup = Supervisor::new();
        sup.register(Service::new(
            "sleeper".into(),
            temp_script("stop-noblock", "sleep 30"),
        ));
        let pid = sup.start("sleeper").expect("spawned");

        let t0 = Instant::now();
        sup.request_stop("sleeper").expect("stop requested");
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "request_stop must not wait for the child: {:?}",
            t0.elapsed()
        );
        // The service is *Stopping*, not yet Stopped: the transition
        // happens when the child is actually reaped.
        let svc = sup.service("sleeper").expect("registered");
        assert_eq!(svc.state(), ServiceState::Stopping);
        assert_eq!(svc.pid, Some(pid));

        // The escalation timer is armed, so a child that ignores SIGTERM
        // is still killed — by the event loop, not by a blocking wait.
        let timer = sup.restart_timer("sleeper").expect("escalation timer");
        assert!(timer.get().is_ok(), "timer must be readable");

        sup.stop_service("sleeper").expect("synchronous cleanup");
    }

    /// A second `STOP` while one is already running is refused rather
    /// than double-signalling.
    #[test]
    fn test_request_stop_twice_is_refused() {
        let mut sup = Supervisor::new();
        sup.register(Service::new(
            "sleeper".into(),
            temp_script("stop-twice", "sleep 30"),
        ));
        sup.start("sleeper").expect("spawned");
        sup.request_stop("sleeper").expect("first stop");
        assert!(matches!(
            sup.request_stop("sleeper"),
            Err(SupervisorError::AlreadyStopping(_))
        ));
        sup.stop_service("sleeper").expect("cleanup");
    }

    /// `START` while a service is still being torn down must be refused:
    /// spawning now would overwrite the pid mapping of a child that is
    /// still alive and let it escape the stop entirely.
    #[test]
    fn test_start_is_refused_while_stopping() {
        let mut sup = Supervisor::new();
        sup.register(Service::new(
            "sleeper".into(),
            temp_script("start-while-stopping", "sleep 30"),
        ));
        sup.start("sleeper").expect("spawned");
        sup.request_stop("sleeper").expect("stopping");
        assert!(matches!(
            sup.start("sleeper"),
            Err(SupervisorError::AlreadyStopping(_))
        ));
        sup.stop_service("sleeper").expect("cleanup");
    }

    /// A `RESTART` re-spawns the service once its child is reaped, with
    /// a genuinely new pid.
    #[test]
    fn test_request_restart_respawns_after_the_child_is_reaped() {
        let mut sup = Supervisor::new();
        sup.register(Service::new(
            "sleeper".into(),
            temp_script("restart-async", "sleep 30"),
        ));
        let first = sup.start("sleeper").expect("spawned");
        sup.request_restart("sleeper").expect("restart requested");
        let svc = sup.service("sleeper").expect("registered");
        assert_eq!(svc.state(), ServiceState::Stopping);
        assert!(svc.restart_after_stop);

        // The child dies of the SIGTERM we sent: reap it the way the
        // event loop would.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match waitpid(first, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Ok(status) => {
                    sup.handle_child_exit(first, status);
                    break;
                }
                Err(_) => break,
            }
        }

        let svc = sup.service("sleeper").expect("registered");
        assert_eq!(svc.state(), ServiceState::Starting);
        assert!(svc.pid.is_some(), "the service must be spawned again");
        assert_ne!(svc.pid, Some(first), "a fresh child, not the old pid");
        sup.stop_service("sleeper").expect("cleanup");
    }

    /// `RESTART` on a service with no child of ours (a daemonising
    /// script) cannot wait for anything, so it starts immediately.
    #[test]
    fn test_request_restart_without_a_child_starts_at_once() {
        let mut sup = Supervisor::new();
        sup.register(Service::new(
            "sleeper".into(),
            temp_script("restart-nochild", "sleep 30"),
        ));
        // Simulate a script that forked its daemon and exited: Running,
        // with no pid of ours.
        sup.start("sleeper").expect("spawned");
        if let Some(svc) = sup.services.get_mut("sleeper") {
            svc.pid = None;
            assert!(svc.transition(ServiceState::Running));
        }
        sup.request_restart("sleeper").expect("restart");
        let svc = sup.service("sleeper").expect("registered");
        assert_eq!(svc.state(), ServiceState::Starting);
        assert!(svc.pid.is_some());
        sup.stop_service("sleeper").expect("cleanup");
    }

    /// Shutdown wins over a pending `RESTART`: nothing may be respawned
    /// once teardown has begun.
    #[test]
    fn test_shutdown_cancels_a_pending_restart() {
        let mut sup = Supervisor::new();
        sup.register(Service::new(
            "sleeper".into(),
            temp_script("restart-cancelled", "sleep 30"),
        ));
        sup.start("sleeper").expect("spawned");
        sup.request_restart("sleeper").expect("restart requested");
        sup.begin_shutdown();
        sup.stop_all(&["sleeper".to_string()]);
        let svc = sup.service("sleeper").expect("registered");
        assert_eq!(svc.state(), ServiceState::Stopped);
        assert!(
            !svc.restart_after_stop,
            "a pending restart must not survive the teardown"
        );
    }

    /// An exit-0 that races an operator `STOP` must be recorded as
    /// `Stopped`: a daemonising script that finished successfully on its
    /// way out is not a running service.
    #[test]
    fn test_exit_zero_while_stopping_is_stopped_not_running() {
        let mut sup = Supervisor::new();
        sup.register(Service::new("oneshot".into(), "/etc/init.d/oneshot".into()));
        let pid = Pid::from_raw(6007);
        // Walk the legal path to `Stopping` (a fresh service cannot jump
        // there directly — that edge is rejected by design).
        if let Some(svc) = sup.services.get_mut("oneshot") {
            assert!(svc.transition(ServiceState::Starting));
            assert!(svc.transition(ServiceState::Stopping));
        }
        sup.pid_to_service.insert(pid, "oneshot".to_string());
        sup.handle_child_exit(pid, WaitStatus::Exited(pid, 0));
        let svc = sup.service("oneshot").expect("registered");
        assert_eq!(svc.state(), ServiceState::Stopped);
        assert!(svc.pid.is_none());
    }

    /// The stop escalation timer must SIGKILL, not restart.
    #[test]
    fn test_stop_timer_escalates_instead_of_restarting() {
        let mut sup = Supervisor::new();
        sup.register(Service::new(
            "sleeper".into(),
            temp_script("escalate", "sleep 30"),
        ));
        let pid = sup.start("sleeper").expect("spawned");
        sup.request_stop("sleeper").expect("stopping");

        // The timer fires while the child is still alive.
        sup.on_timer_fire("sleeper").expect("timer handled");
        let svc = sup.service("sleeper").expect("registered");
        assert_eq!(
            svc.state(),
            ServiceState::Stopping,
            "escalation must not turn into a restart"
        );
        assert_eq!(svc.pid, Some(pid), "the child is killed, not respawned");
        sup.stop_service("sleeper").expect("cleanup");
    }

    /// A child that exits *promptly* after `STOP` must not be brought
    /// back by the escalation timer that was armed a moment earlier.
    ///
    /// This is the ordinary `velocityctl stop <svc>` case for any
    /// well-behaved service: SIGTERM, the script returns 0 within
    /// milliseconds, the reaper marks it `Stopped` — and 2 s later the
    /// still-armed escalation timer fires into a service that is no
    /// longer `Stopping`, so `on_timer_fire` takes the restart branch and
    /// forks a fresh child. The operator was told `OK stopping <svc>`.
    #[test]
    fn test_promptly_exited_stop_does_not_respawn_at_escalation() {
        let mut sup = Supervisor::new();
        sup.register(Service::new(
            "sleeper".into(),
            temp_script("prompt-exit", "sleep 30"),
        ));
        let pid = sup.start("sleeper").expect("spawned");
        sup.request_stop("sleeper").expect("stopping");

        // The service honours SIGTERM and exits 0. The event loop reaps
        // it, exactly as it would in production.
        sup.handle_child_exit(pid, WaitStatus::Exited(pid, 0));
        let svc = sup.service("sleeper").expect("registered");
        assert_eq!(svc.state(), ServiceState::Stopped);

        // The escalation timer fires afterwards: nothing must be forked.
        sup.on_timer_fire("sleeper").expect("stale timer handled");
        let svc = sup.service("sleeper").expect("registered");
        assert_eq!(
            svc.state(),
            ServiceState::Stopped,
            "a stopped service must stay stopped when the stop escalation timer fires"
        );
        assert!(
            svc.pid.is_none(),
            "stale escalation timer respawned a stopped service"
        );
        sup.stop_service("sleeper").ok();
    }

    /// Write a throwaway init script under the target dir and return its
    /// path. Used by the tests that really spawn a child.
    fn temp_script(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("unit-scripts");
        std::fs::create_dir_all(&dir).expect("create script dir");
        let path = dir.join(format!("{name}-{}", std::process::id()));
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\n### BEGIN INIT INFO\n# Provides: {name}\n# Required-Start:\n# Required-Stop:\n# Default-Start: 2 3 4 5\n# Default-Stop: 0 1 6\n# Short-Description: unit-test service\n### END INIT INFO\ncase \"$1\" in start) {body} ;; stop) ;; esac\nexit 0\n"
            ),
        )
        .expect("write script");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");
        path
    }

    #[test]
    fn test_stop_during_restart_backoff_is_not_resurrected() {
        // A service between two restart attempts has *no child*, which
        // is exactly the state `request_stop` treats as "nothing to
        // wait for". The armed backoff timer used to survive that
        // stop, so `velocityctl stop` answered "OK stopping", the
        // service stayed down for the rest of the backoff, and then
        // came back on its own — the operator's stop silently undone.
        let mut sup = Supervisor::new();
        sup.register(Service::new(
            "flaky".into(),
            temp_script("stop-during-backoff", "exit 1"),
        ));
        sup.mark_system_running();

        // It dies once: a restart is now armed and no child is left.
        sup.start("flaky").expect("spawned");
        let pid = sup.service("flaky").unwrap().pid.expect("pid");
        sup.handle_child_exit(pid, WaitStatus::Exited(pid, 1));
        let svc = sup.service("flaky").unwrap();
        assert!(svc.restart_pending, "a backoff restart is armed");
        assert!(svc.pid.is_none());

        // The operator stops it while it is between attempts.
        sup.request_stop("flaky").expect("stop accepted");
        assert!(!sup.service("flaky").unwrap().restart_pending);
        assert!(
            sup.timers
                .get("flaky")
                .is_none_or(|t| t.purpose.is_none()),
            "the restart timer must not survive an operator stop"
        );

        // Even if that timer is delivered anyway (a stale expiry),
        // nothing may be spawned: no restart was pending.
        sup.on_timer_fire("flaky").expect("timer handled");
        let svc = sup.service("flaky").unwrap();
        assert_eq!(svc.state(), ServiceState::Stopped, "must stay stopped");
        assert!(svc.pid.is_none(), "no child may be spawned");
        assert!(
            sup.pid_to_service.is_empty(),
            "no process may have been started for it"
        );
    }

    #[test]
    fn test_runtime_restart_reaches_running_state() {
        // A respawned child that never sends `READY=1` — the ordinary
        // LSB script that execs a foreground daemon — used to sit in
        // `Starting` forever, because the promotion was gated on the
        // service *not* having a notify socket and `start` creates one
        // for everything. `Running` is also what lets
        // `check_backoff_reset` clear the crash-loop budget, so a
        // healthy service could never be forgiven.
        let mut sup = Supervisor::new();
        sup.register(Service::new(
            "quiet".into(),
            temp_script("runtime-restart-running", "sleep 30"),
        ));
        sup.mark_system_running();
        assert!(
            sup.notify_sockets.contains_key("quiet") == false,
            "precondition: no socket before the first start"
        );
        sup.start("quiet").expect("spawned");
        assert!(
            sup.notify_sockets.contains_key("quiet"),
            "precondition: start always creates one"
        );

        let pid = sup.service("quiet").unwrap().pid.expect("pid");
        sup.handle_child_exit(pid, WaitStatus::Exited(pid, 1));
        assert!(sup.service("quiet").unwrap().restart_pending);

        sup.on_timer_fire("quiet").expect("restart fired");
        let svc = sup.service("quiet").unwrap();
        assert_eq!(svc.state(), ServiceState::Running);
        assert!(svc.pid.is_some());
        assert!(svc.running_since.is_some());
        sup.stop_service("quiet").expect("cleanup");
    }

    #[test]
    fn test_ensure_default_path_fills_the_gap_only() {
        // Missing PATH gets the sysvinit-compatible default…
        let mut env: HashMap<OsString, OsString> = HashMap::new();
        ensure_default_path(&mut env);
        assert_eq!(
            env.get(&OsString::from("PATH")),
            Some(&OsString::from(DEFAULT_SERVICE_PATH))
        );
        // …but an explicit operator PATH is never clobbered.
        let mut env: HashMap<OsString, OsString> = HashMap::from([(
            OsString::from("PATH"),
            OsString::from("/custom/bin"),
        )]);
        ensure_default_path(&mut env);
        assert_eq!(
            env.get(&OsString::from("PATH")),
            Some(&OsString::from("/custom/bin"))
        );
    }

    #[test]
    fn test_cstrings_from_env_map_shape_and_nul_skip() {
        // Plain `NAME=VALUE`, no leading `=`, NUL entries skipped —
        // the policies both fork sites share.
        let map: HashMap<OsString, OsString> = HashMap::from([
            (OsString::from("FOO"), OsString::from("bar")),
            (OsString::from("BAD"), OsString::from("a\0b")),
        ]);
        let out = cstrings_from_env_map(map, "test-svc");
        let strs: Vec<String> = out.iter().map(|c| c.to_string_lossy().into_owned()).collect();
        assert!(strs.contains(&"FOO=bar".to_string()), "got {strs:?}");
        assert!(
            !strs.iter().any(|s| s.starts_with("BAD=")),
            "NUL entry must be skipped, got {strs:?}"
        );
        assert!(
            !strs.iter().any(|s| s.starts_with('=')),
            "no hidden putenv entries, got {strs:?}"
        );
    }

    #[test]
    fn test_raw_rewrite_listen_pid_stamps_the_caller_pid() {
        // Build a parent-side envp with the `LISTEN_PID=0` placeholder
        // plus a neighbour entry, rewrite it the way the child does,
        // and check only the placeholder moved.
        let placeholder = CString::new("LISTEN_PID=0").expect("static");
        let other = CString::new("FOO=bar").expect("static");
        let mut envp: Vec<*const libc::c_char> =
            vec![other.as_ptr(), placeholder.as_ptr(), std::ptr::null()];
        let mut buf = [0u8; 40];
        unsafe {
            raw_rewrite_listen_pid(&mut envp, &mut buf);
        }
        let want = format!("LISTEN_PID={}", unsafe { libc::getpid() });
        let rewritten = unsafe { CStr::from_ptr(envp[1]).to_bytes() };
        assert_eq!(rewritten, want.as_bytes());
        let untouched = unsafe { CStr::from_ptr(envp[0]).to_bytes() };
        assert_eq!(untouched, b"FOO=bar");
        // The replacement is NUL-terminated inside the caller buffer.
        assert_eq!(buf[want.len()], 0);
    }

    #[test]
    fn test_raw_rewrite_listen_pid_without_placeholder_is_noop() {
        let other = CString::new("FOO=bar").expect("static");
        let before = other.as_ptr();
        let mut envp: Vec<*const libc::c_char> = vec![before, std::ptr::null()];
        let mut buf = [0u8; 40];
        unsafe {
            raw_rewrite_listen_pid(&mut envp, &mut buf);
        }
        assert_eq!(envp[0], before, "no placeholder: no slot may move");
    }

    #[test]
    fn test_arm_timer_keeps_ms_precision() {
        // 1500ms used to come back as 1s: the old
        // `seconds(delay / 1000)` split truncated the remainder.
        let mut sup = Supervisor::new();
        sup.register(Service::new("t".into(), "/etc/init.d/t".into()));
        sup.arm_timer("t", 1500, TimerPurpose::Restart)
            .expect("arm 1500ms");
        let exp = sup
            .restart_timer("t")
            .expect("timer exists")
            .get()
            .expect("readable")
            .expect("armed");
        match exp {
            Expiration::OneShot(spec) => {
                let ms = spec.num_milliseconds();
                assert!(
                    (1450..=1550).contains(&ms),
                    "1500ms must survive arming, got {ms}ms"
                );
            }
            other => panic!("expected one-shot expiry, got {other:?}"),
        }
    }

    #[test]
    fn test_arm_timer_absurd_delay_clamps_instead_of_panicking() {
        // `TimeSpec::milliseconds` asserts on out-of-bounds input: PID 1
        // must never panic, so absurd delays clamp to a day.
        let mut sup = Supervisor::new();
        sup.register(Service::new("t".into(), "/etc/init.d/t".into()));
        sup.arm_timer("t", u64::MAX, TimerPurpose::Restart)
            .expect("absurd delay must clamp, not panic");
        let exp = sup
            .restart_timer("t")
            .expect("timer exists")
            .get()
            .expect("readable")
            .expect("armed");
        match exp {
            Expiration::OneShot(spec) => {
                assert!(
                    spec.num_milliseconds() <= 86_400_000,
                    "clamped to a day, got {}ms",
                    spec.num_milliseconds()
                );
            }
            other => panic!("expected one-shot expiry, got {other:?}"),
        }
    }

    #[test]
    fn test_request_restart_refused_while_shutting_down_without_side_effects() {
        // A `RESTART` during teardown must be refused like `start` is —
        // without clearing the crash-loop budget or stopping the service
        // first. The old code reset the history and zeroed the backoff
        // before failing, losing evidence for nothing.
        let mut sup = Supervisor::new();
        sup.register(Service::new("ssh".into(), "/etc/init.d/ssh".into()));
        if let Some(svc) = sup.services.get_mut("ssh") {
            assert!(svc.transition(ServiceState::Starting));
            assert!(svc.transition(ServiceState::Running));
            svc.restart_attempts = 3;
        }
        sup.respawn_history.insert(
            "ssh".to_string(),
            VecDeque::from([Instant::now()]),
        );
        sup.begin_shutdown();
        assert!(matches!(
            sup.request_restart("ssh"),
            Err(SupervisorError::ShuttingDown)
        ));
        let svc = sup.service("ssh").expect("registered");
        assert_eq!(svc.state(), ServiceState::Running);
        assert_eq!(svc.restart_attempts, 3, "backoff debt must survive refusal");
        assert!(
            sup.respawn_history.contains_key("ssh"),
            "crash-loop evidence must survive refusal"
        );
        assert!(!svc.restart_after_stop);
    }

    #[test]
    fn test_request_restart_unknown_is_unknown_service() {
        let mut sup = Supervisor::new();
        assert!(matches!(
            sup.request_restart("ghost"),
            Err(SupervisorError::UnknownService(_))
        ));
        assert!(
            !sup.respawn_history.contains_key("ghost"),
            "unknown names must not create history entries"
        );
    }

    #[test]
    fn test_stopping_failure_during_shutdown_settles_to_stopped() {
        // Shutdown wins over an in-flight stop: a failure that races
        // the teardown lands in `Stopped` with no restart, whichever
        // state the service was in when the child died.
        for starting in [true, false] {
            let mut sup = Supervisor::new();
            sup.register(Service::new("ssh".into(), "/etc/init.d/ssh".into()));
            {
                let svc = sup.services.get_mut("ssh").expect("registered");
                assert!(svc.transition(ServiceState::Starting));
                if !starting {
                    assert!(svc.transition(ServiceState::Stopping));
                }
            }
            let pid = Pid::from_raw(if starting { 6301 } else { 6302 });
            sup.pid_to_service.insert(pid, "ssh".to_string());
            // Enter Stopping after the child was "spawned" for the
            // stopping variant, or stay Starting otherwise — then begin
            // the shutdown before reaping.
            if starting {
                let svc = sup.services.get_mut("ssh").expect("registered");
                assert!(svc.transition(ServiceState::Stopping));
            }
            sup.begin_shutdown();
            sup.handle_child_exit(pid, WaitStatus::Exited(pid, 1));
            let svc = sup.service("ssh").expect("registered");
            assert_eq!(svc.state(), ServiceState::Stopped);
            assert!(!svc.restart_pending);
            assert!(!svc.restart_after_stop);
            assert_eq!(svc.restart_attempts, 0);
        }
    }
}
