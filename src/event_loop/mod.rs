//! PID 1 event loop: `epoll` + `signalfd` + `timerfd`.
//!
//! # Architecture
//!
//! One `epoll` instance multiplexes exactly the file descriptors we
//! care about, so the loop can block in a single `epoll_wait` and never
//! busy-poll:
//!
//! | fd          | purpose                                        |
//! |-------------|------------------------------------------------|
//! | signalfd    | delivers SIGCHLD / SIGTERM / SIGINT / SIGALRM  |
//! | timerfd(s)  | one per service, armed by the supervisor's     |
//! |             | backoff (restart scheduling)                   |
//! | notify fd(s)| one per service: sd_notify READY=1 readiness   |
//! |             | datagrams while it is `Starting`               |
//! | log pipe(s) | one per service: its stdout/stderr, drained    |
//! |             | and re-emitted with a `[service]` prefix       |
//! | control     | the operator socket: one listener plus one fd |
//! |             | per accepted `velocityctl` connection          |
//!
//! # Why signalfd (not signal handlers)?
//!
//! Async signal handlers interrupt arbitrary code and are restricted to
//! async-signal-safe calls. `signalfd` converts signals into *readable
//! file descriptors*: we block the signals (`sigprocmask`) so their
//! default dispositions never run, then `read(2)` structured
//! `signalfd_siginfo` records — which even carry `ssi_pid` and
//! `ssi_status`, so SIGCHLD handling is race-free.
//!
//! # Why timerfd (not SIGALRM)?
//!
//! `timerfd` timers are per-service and multiplex cleanly through
//! `epoll` without any handler code; SIGALRM is blocked and ignored.
//!
//! # No panics
//!
//! Every syscall returns `Result`; a failure is logged and, at worst,
//! causes the event loop to exit with an error.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::io::AsFd;
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};
use nix::errno::Errno;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags};
use nix::sys::signal::{sigprocmask, SigSet, SigmaskHow, Signal};
use nix::sys::signalfd::SignalFd;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{getpid, read, Pid};

use crate::control_socket::{ConnAction, ControlSocket};
use crate::inittab::SysvContext;
use crate::models::{RestartPolicy, ShutdownAction};
use crate::supervisor::{signal_service, Supervisor};

pub mod boot;
pub mod dispatch;
pub mod shutdown;

/// Maximum number of events `epoll_wait` returns per iteration.
const EVENT_CAPACITY: usize = 32;
/// Tick timeout (ms): the loop wakes at least this often so that the
/// supervisor's "reset backoff after stable run" check runs.
const TICK_TIMEOUT_MS: u16 = 1000;
/// How long boot waits for a foreground service to stay alive before
/// declaring it `Running`.
const BOOT_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
/// Poll interval while boot-waiting on a service.
///
/// This is the timeout handed to `epoll_wait` by
/// [`EventLoop::wait_event_driven`], so it sets the upper bound on how
/// long a signal (a `SIGTERM` aborting the boot) or a `READY=1` can go
/// unnoticed while the boot waits on a script.
const BOOT_POLL_INTERVAL: Duration = Duration::from_millis(20);
/// How many consecutive hard `epoll_wait` failures are tolerated (each
/// backed off by 100 ms) before the event loop gives up.
const MAX_EPOLL_ERRORS: u32 = 10;
/// Upper bound on the bytes buffered for one service's unterminated log
/// line (see [`EventLoop::route_log`]). Past this the partial line is
/// emitted and dropped, so no single noisy service can exhaust PID 1's
/// memory.
const MAX_PARTIAL_LOG: usize = 8 * 1024;
/// Extra grace an `Always`-policy service (e.g. getty) gets to emit a
/// late `READY=1` before it is declared running without notifying.
/// Larger than one poll tick so a daemon that notifies within a few
/// tens of milliseconds is still caught.
const BOOT_READY_GRACE: Duration = Duration::from_millis(250);
/// How long sysv mode waits for a boot/wait/rc script to exit.
///
/// Real sysvinit waits *indefinitely* for `rcS`/`rc N`; we cap it so a
/// wedged script can only stall boot for a bounded time, but the cap
/// must comfortably exceed a legitimate slow boot (package
/// configuration, fsck). 5 minutes matches common distro practice.
const SYSV_EXIT_TIMEOUT: Duration = Duration::from_secs(300);
/// How many 4 KiB reads one log-drain pass may perform per service.
///
/// A drain loop that runs to `EAGAIN` with no bound lets one chatty
/// service starve PID 1: as fast as we read, it writes more, and the
/// loop never returns to `epoll_wait`. Sixteen iterations (64 KiB) per
/// tick is plenty for legitimate bursts; the registration is
/// level-triggered, so anything left simply fires again next tick.
pub(crate) const MAX_LOG_DRAIN_ITERS: usize = 16;
/// Minimum interval between repeated `epoll_ctl(ADD)` failure warnings
/// for the same fd (see [`EventLoop::epoll_add`]).
const EPOLL_WARN_SUPPRESS: Duration = Duration::from_secs(60);
/// How long a boot script we are abandoning gets to die after SIGKILL
/// before we stop waiting for it.
///
/// Bounded on purpose: PID 1 must not sit in a wait loop, and a process
/// wedged in uninterruptible sleep cannot be waited for at all. Anything
/// left over is finished off by the root-cgroup sweep in
/// [`EventLoop::perform_shutdown`].
const KILL_REAP_TIMEOUT: Duration = Duration::from_millis(500);

/// Errors produced by the event loop.
#[derive(Debug, thiserror::Error)]
pub enum LoopError {
    #[error("syscall error: {0}")]
    Syscall(Errno),
    #[error("supervisor error: {0}")]
    Supervisor(#[from] crate::supervisor::SupervisorError),
    /// A runlevel switch was asked for without a `/etc/inittab` to
    /// switch between. Operator error, never fatal: the caller logs it
    /// and keeps supervising.
    #[error("cannot change to runlevel {0}: this init is not running in sysv mode (no /etc/inittab)")]
    NotSysvMode(String),
    /// A runlevel that is not a single character from `0123456789SABC`
    /// (inittab(5)). Rejected rather than "switched to": no inittab
    /// line matches it, and the RUN_LVL record would claim a level the
    /// machine is not in.
    #[error("invalid runlevel {0:?}: expected a single digit, S, or on-demand a/b/c")]
    InvalidRunlevel(String),
}

impl From<Errno> for LoopError {
    fn from(e: Errno) -> Self {
        LoopError::Syscall(e)
    }
}

/// Result alias for the event loop.
pub type LoopResult<T> = Result<T, LoopError>;

/// The classic runlevel-record "previous" byte when there is none.
const PREV_RUN_NONE: u8 = b'N';

// ─────────────────────── the one boot wait ───────────────────────────

/// What a boot wait is waiting for, and what it does with the
/// supervisor when that condition is met.
///
/// One enum, three shapes of "done", because the *only* thing that ever
/// differed between the old per-call-site poll loops was the exit
/// criterion. How to wait — signalfd, log drain, `waitpid` probe — is
/// [`EventLoop::wait_event_driven`]'s job, for all of them.
pub(crate) enum WaitGoal {
    /// An LSB service is up: `READY=1`, a clean daemonising exit, or
    /// the deadline while it stayed alive (a foreground service).
    ReadyOrExit {
        pid: Pid,
        deadline: Instant,
        /// `Some(t)` when the service is long-lived by design
        /// (`RestartPolicy::Always`, i.e. a getty): alive-and-silent
        /// means running after `t`, instead of holding the boot for the
        /// full deadline on a prompt that is never coming.
        grace: Option<Instant>,
    },
    /// A one-shot boot script (`rcS`, `rc N`) ran to completion.
    Exit { pid: Pid, deadline: Instant },
    /// A script we abandoned has been SIGKILLed: just reap it.
    Reap { pid: Pid, deadline: Instant },
}

/// How [`EventLoop::wait_event_driven`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitOutcome {
    /// The goal was met.
    Done,
    /// A shutdown was requested while we were waiting. The caller
    /// decides what that means: a boot level moves on, an abandoned
    /// `rcS` gets SIGKILLed and reaped.
    Shutdown,
}

impl WaitGoal {
    /// Has the wait finished? `true` ends the loop; the supervisor has
    /// already been given whatever it needs in either case.
    ///
    /// This is the *only* place a boot wait talks to `waitpid` (the
    /// signalfd's SIGCHLD path reaps eagerly too — see the `ECHILD`
    /// arm below, which is the normal outcome for a fast-failing
    /// service, not an exotic one).
    fn poll(&self, loop_: &mut EventLoop, name: &str) -> bool {
        match self {
            WaitGoal::ReadyOrExit {
                pid,
                deadline,
                grace,
            } => poll_ready(loop_, name, *pid, *deadline, *grace),
            WaitGoal::Exit { pid, deadline } => poll_exit(loop_, name, *pid, *deadline),
            WaitGoal::Reap { pid, deadline } => poll_reap(loop_, name, *pid, *deadline),
        }
    }
}

/// Exit criterion for [`WaitGoal::ReadyOrExit`].
fn poll_ready(
    loop_: &mut EventLoop,
    name: &str,
    pid: Pid,
    deadline: Instant,
    grace: Option<Instant>,
) -> bool {
    // 0. Already up. `dispatch_boot` processed the `READY=1` datagram
    //    on the same epoll set this loop waits on, and promoting a
    //    `Starting` service to `Running` is what `mark_running` does —
    //    so this is a state check, not a re-read of the socket.
    if loop_
        .supervisor
        .service(name)
        .is_some_and(|s| s.state() == crate::models::ServiceState::Running)
    {
        info!("boot: {:<20} ready", name);
        return true;
    }

    // 1. Probe whether the child has exited.
    match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
        // exited 0: for a SysV script it daemonized, for a
        // one-shot it simply finished. Only the supervisor knows
        // which — and it is the only place that clears the
        // pid→service entry and applies the restart policy, so
        // the reaped child must go through it even here.
        Ok(WaitStatus::Exited(_, 0)) => {
            loop_.supervisor
                .handle_child_exit(pid, WaitStatus::Exited(pid, 0));
            true
        }
        // exited non-zero or signalled: hand to the supervisor;
        // it schedules a backoff restart.
        Ok(WaitStatus::Exited(_, code)) => {
            error!("{} failed to start (exit {})", name, code);
            loop_.supervisor
                .handle_child_exit(pid, WaitStatus::Exited(pid, code));
            true
        }
        Ok(WaitStatus::Signaled(_, sig, core)) => {
            error!("{} failed to start (signal {})", name, sig);
            loop_.supervisor
                .handle_child_exit(pid, WaitStatus::Signaled(pid, sig, core));
            true
        }
        // still alive: either a foreground service or a daemon
        // that hasn't forked yet.
        Ok(WaitStatus::StillAlive)
        | Ok(WaitStatus::Stopped(_, _))
        | Ok(WaitStatus::Continued(_))
        | Ok(WaitStatus::PtraceEvent(_, _, _))
        | Ok(WaitStatus::PtraceSyscall(_)) => {
            // For always-respawned services (getty): a short grace
            // period is enough to catch a late READY=1; beyond
            // that, an alive-and-silent long-lived child is
            // declared running rather than stalling boot for 2s.
            if grace.is_some_and(|g| Instant::now() >= g) {
                loop_.supervisor.mark_running(name, Some(pid));
                info!("boot: {:<20} running (always; no notify)", name);
                return true;
            }
            if Instant::now() >= deadline {
                loop_.supervisor.mark_running(name, Some(pid));
                return true;
            }
            false
        }
        Err(Errno::ECHILD) => {
            // We reap every child ourselves, and `handle_signals` above
            // already ran the generic `waitpid(-1)` loop: for a script
            // that exits quickly it reaped *this* pid a moment ago and
            // handed it to `handle_child_exit`, which logged the failure
            // and armed the restart backoff. Only then does our own
            // `waitpid` return ECHILD — so this is the *normal* path for
            // a service that failed during boot, not the exotic
            // "somebody else reaped it" case the old comment assumed.
            //
            // Marking it Running here (as the old code did) overwrote the
            // failure: the service was reported healthy, its pid was
            // dropped, the armed restart then failed with "already
            // running", and the machine kept a service that was dead and
            // reported alive. Ask the supervisor what actually happened
            // instead.
            if !loop_.supervisor.owns_child(pid) {
                // Genuinely not ours any more — the mapping was already
                // consumed by `handle_child_exit`.
                debug!(
                    "{name} was reaped by the SIGCHLD path during boot; \
                     leaving the supervisor's verdict in place"
                );
                return true;
            }
            warn!("{name} vanished during boot; assuming daemonized");
            // Nobody reaped it and it is not in the mapping any more:
            // drop the entry so a recycled pid can never be
            // mis-attributed to this service.
            loop_.supervisor.forget_child(pid);
            loop_.supervisor.mark_running(name, None);
            true
        }
        Err(e) => {
            warn!("waitpid failed for {}: {}", name, e);
            loop_.supervisor.forget_child(pid);
            loop_.supervisor.mark_running(name, None);
            true
        }
    }
}

/// Exit criterion for [`WaitGoal::Exit`].
fn poll_exit(loop_: &mut EventLoop, name: &str, pid: Pid, deadline: Instant) -> bool {
    match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::Exited(_, code)) => {
            info!("sysv {name}: exited {code}");
            loop_.supervisor
                .handle_child_exit(pid, WaitStatus::Exited(pid, code));
            true
        }
        Ok(WaitStatus::Signaled(_, sig, core)) => {
            info!("sysv {name}: killed by {sig}");
            loop_.supervisor
                .handle_child_exit(pid, WaitStatus::Signaled(pid, sig, core));
            true
        }
        Ok(_) => {
            if Instant::now() >= deadline {
                warn!("sysv {name}: did not exit within timeout — continuing");
                return true;
            }
            false
        }
        Err(Errno::ECHILD) => {
            // Reaped by the SIGCHLD path in `handle_signals`, which
            // already applied the supervisor's restart policy; our own
            // `waitpid` is simply too late.
            warn!("sysv {name}: child vanished — continuing");
            true
        }
        Err(e) => {
            warn!("sysv wait for {name} failed: {e}");
            true
        }
    }
}

/// Exit criterion for [`WaitGoal::Reap`].
///
/// The pid mapping stays with the supervisor whatever happens here: a
/// pid cannot be recycled while its process lives, so retaining it
/// cannot mis-attribute a later child, and the shutdown's
/// stop/cgroup/orphan sweeps still know the straggler is ours. The
/// `ECHILD`/error arms only clear a mapping that is already gone (the
/// SIGCHLD path reaped it first, or `waitpid` itself failed).
fn poll_reap(loop_: &mut EventLoop, name: &str, pid: Pid, deadline: Instant) -> bool {
    match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
        Ok(status @ (WaitStatus::Exited(_, _) | WaitStatus::Signaled(_, _, _))) => {
            // `handle_signals` may have reaped it first, in which case we
            // never get here; if we do, this is the real status and it
            // belongs to the supervisor.
            loop_.supervisor.handle_child_exit(pid, status);
            true
        }
        // Still alive — including stop/continue/ptrace reports, which
        // leave the child alive (and which `handle_child_exit`
        // deliberately ignores for the same reason): keep waiting until
        // the deadline, then give up and let the shutdown sweeps take it.
        Ok(_) => !reap_keep_waiting(Instant::now(), deadline),
        Err(Errno::ECHILD) => {
            loop_.supervisor.forget_child(pid);
            true
        }
        Err(e) => {
            warn!("could not reap the abandoned script {name}: {e}");
            loop_.supervisor.forget_child(pid);
            true
        }
    }
}

/// Should a `Reap` wait keep waiting? Pure predicate so the polarity
/// is unit-testable: "still alive and time remains" means keep waiting,
/// "deadline reached" means give up and let the shutdown's
/// stop/cgroup/orphan sweeps finish the straggler.
///
/// Extracted for the same reason as [`EventLoop::owns_log_fd`]: the
/// comparison once lived inline with its operands swapped (`now <
/// deadline` returned as "done"), which made `kill_and_reap` return
/// instantly instead of waiting up to `KILL_REAP_TIMEOUT`.
pub(crate) fn reap_keep_waiting(now: Instant, deadline: Instant) -> bool {
    now < deadline
}

/// The heart of PID 1: multiplexes signals and restart timers.
pub struct EventLoop {
    /// The supervisor, which owns all service state.
    supervisor: Supervisor,
    /// One epoll instance for everything.
    epoll: Epoll,
    /// fd delivering SIGCHLD / SIGTERM / SIGINT / SIGALRM.
    signal_fd: SignalFd,
    /// Restart timer fd → service name.
    timer_names: HashMap<i32, String>,
    /// Readiness notify socket fd → service name.
    notify_names: HashMap<i32, String>,
    /// Log-routing pipe fd → service name.
    log_names: HashMap<i32, String>,
    /// Service name → registered log pipe fd (to detect replacements
    /// after a restart, where the old pipe must be unregistered).
    log_pipe_names: HashMap<String, i32>,
    /// Partial, newline-less log bytes still accumulating per service
    /// (raw bytes: decoded as UTF-8 only once a full line has arrived,
    /// so a multi-byte sequence split across reads is never corrupted).
    log_partial: HashMap<String, Vec<u8>>,
    /// The operator control socket (disabled ⇒ no-op).
    control: ControlSocket,
    /// A shutdown request (signal or control command) that has been
    /// accepted but not yet acted on. Once set, the loop exits and the
    /// teardown sequence runs with this action.
    pending_shutdown: Option<ShutdownAction>,
    /// The sysvinit drop-in context, when `/etc/inittab` exists. When
    /// set the loop is in *sysv mode* (boot via `sysinit`/`wait`/
    /// `respawn` lines, runlevel switching, ctrlaltdel, powerfail).
    sysv: Option<SysvContext>,
    /// The journal to re-open once the sysv boot scripts have run, and
    /// where. A real `rcS` empties `/tmp` and `/run` (Devuan's
    /// `mountall-bootclean.sh`), which deletes the journal file we
    /// opened before the boot — the handle would keep writing to an
    /// unlinked inode forever, so `velocityctl journal` shows nothing.
    journal: Option<(&'static crate::journal::DualLogger, std::path::PathBuf)>,
    /// The parsed one-shot commands for ctrlaltdel / power actions.
    action_commands: HashMap<String, String>,
    /// A `telinit N` / `telinit q` request (or SIGHUP) accepted from the
    /// control socket; handled once the current epoll dispatch passes.
    /// A queue, not a slot: two clients can each send a `CHANGE` in the
    /// same tick, and dropping either silently is an operator-visible
    /// loss. Drained in order by the main loop.
    pending_changes: Vec<crate::control_socket::ChangeRequest>,
    /// How long a control connection may sit without completing a
    /// command line before the sweep drops it. Snapshotted from
    /// [`crate::config::RuntimeConfig`] at startup (see
    /// [`EventLoop::set_ctl_idle_timeout`]) instead of re-reading the
    /// environment on every sweep: the value is configuration, not a
    /// live knob, and env reads in the tick path are untestable
    /// without process-global mutation.
    ctl_idle_timeout: Duration,
    /// Parallel-boot concurrency cap per level (0 = unlimited).
    jobs: usize,
    /// fds whose last `epoll_ctl(ADD)` failed, with the time we last
    /// warned about them. A failed ADD is retried on the next tick (a
    /// transient `ENOMEM` must not disable a service's restart timer
    /// forever, and an fd number is routinely recycled for something
    /// else) — the timestamp only throttles the warning so one bad fd
    /// does not log on every tick. Entries for fds nobody wants any
    /// more are pruned by the registration pass.
    epoll_failed: HashMap<i32, Instant>,
    /// When the event loop's boot sequence started.
    boot_started_at: Option<std::time::Instant>,
    /// Scratch buffer for the boot waits' `epoll_wait` calls, kept on
    /// the struct so a five-minute `rcS` does not allocate one vector
    /// per 20 ms tick.
    wait_events: Vec<EpollEvent>,
}

/// Every signal this init manages, blocked for the whole process life.
///
/// Shared with [`EventLoop::new`] so the signalfd subscribes to exactly
/// the set that is blocked.
pub fn managed_signal_mask() -> SigSet {
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGCHLD);
    mask.add(Signal::SIGTERM);
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGALRM);
    // SIGHUP: sysvinit re-reads /etc/inittab on it (`telinit q`).
    mask.add(Signal::SIGHUP);
    // Sysv init also manages these (power management + runlevel):
    // SIGUSR2 = powerfail, SIGUSR1 = powerwait, SIGWINCH =
    // powerokwait, SIGPWR = powerfailnow. They are harmless to
    // block in LSB mode too (nobody sends them).
    mask.add(Signal::SIGUSR1);
    mask.add(Signal::SIGUSR2);
    mask.add(Signal::SIGWINCH);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        mask.add(Signal::SIGPWR);
    }
    mask
}

/// Block the managed signals. Call this as the *first* thing `main`
/// does — see the note there.
pub fn block_managed_signals() -> nix::Result<()> {
    sigprocmask(
        SigmaskHow::SIG_BLOCK,
        Some(&managed_signal_mask()),
        None,
    )
}

impl EventLoop {
    /// Construct the loop: block the managed signals, create the
    /// signalfd, create the epoll instance, register the signalfd.
    ///
    /// # Signal ordering
    ///
    /// Signals MUST be blocked *before* `SignalFd::new` is called, or a
    /// signal arriving in between would run its default disposition.
    pub fn new(supervisor: Supervisor, control: ControlSocket) -> LoopResult<Self> {
        // 1. Block the signals we intend to manage.
        let mask = managed_signal_mask();
        sigprocmask(SigmaskHow::SIG_BLOCK, Some(&mask), None)?;

        // 2. signalfd for the *blocked* set.
        //
        // MUST be non-blocking: `read_signal` is called in a drain loop,
        // and a blocking signalfd would block forever at the end of the
        // queue, deadlocking the whole event loop.
        let signal_fd = SignalFd::with_flags(
            &mask,
            nix::sys::signalfd::SfdFlags::SFD_NONBLOCK | nix::sys::signalfd::SfdFlags::SFD_CLOEXEC,
        )?;

        // 3. A single epoll instance.
        let epoll = Epoll::new(EpollCreateFlags::EPOLL_CLOEXEC)?;

        // 4. Register the signalfd under a token derived from its fd.
        epoll.add(
            &signal_fd,
            EpollEvent::new(EpollFlags::EPOLLIN, signal_fd.as_raw_fd() as u64),
        )?;

        // 5. Register the operator control socket listener, if any.
        if let Some(lfd) = control.listener_fd() {
            let bfd = unsafe { BorrowedFd::borrow_raw(lfd) };
            epoll.add(bfd, EpollEvent::new(EpollFlags::EPOLLIN, lfd as u64))?;
            debug!("registered control listener fd {}", lfd);
        }

        Ok(Self {
            supervisor,
            epoll,
            signal_fd,
            timer_names: HashMap::new(),
            notify_names: HashMap::new(),
            log_names: HashMap::new(),
            log_pipe_names: HashMap::new(),
            log_partial: HashMap::new(),
            control,
            pending_shutdown: None,
            sysv: None,
            journal: None,
            action_commands: HashMap::new(),
            pending_changes: Vec::new(),
            ctl_idle_timeout: crate::control_socket::conn_idle_timeout(),
            jobs: 0,
            epoll_failed: HashMap::new(),
            boot_started_at: None,
            wait_events: vec![EpollEvent::empty(); EVENT_CAPACITY],
        })
    }

    /// Bound the number of services started concurrently within one
    /// boot level (0 = unlimited, the default).
    pub fn set_jobs(&mut self, jobs: usize) {
        self.jobs = jobs;
    }

    /// Override the control-connection idle timeout with the startup
    /// snapshot from [`crate::config::RuntimeConfig`]. The constructor
    /// default already resolves the same environment, so this is a
    /// no-op unless the operator's configuration differs — it exists so
    /// the value flows explicitly from the one parsed config instead of
    /// being re-read from the environment on every sweep.
    pub fn set_ctl_idle_timeout(&mut self, timeout: Duration) {
        self.ctl_idle_timeout = timeout;
    }

    /// Switch the event loop into sysv (drop-in) mode with the given
    /// resolved runlevel context. Must be called before `run`.
    pub fn set_sysv(&mut self, ctx: SysvContext) {
        self.sysv = Some(ctx);
    }

    /// Let the loop re-open the journal once the boot scripts have run,
    /// for the same reason it re-binds the control socket.
    pub fn set_journal(&mut self, logger: &'static crate::journal::DualLogger, path: std::path::PathBuf) {
        self.journal = Some((logger, path));
    }

    /// Register the ctrlaltdel / power-fail one-shot commands (keyed by
    /// signal name). Relevant only in sysv mode.
    pub fn set_action_commands(&mut self, cmds: HashMap<String, String>) {
        self.action_commands = cmds;
    }

    /// The current runlevel (sysv mode), or `None` in LSB mode.
    pub fn runlevel(&self) -> Option<&str> {
        self.sysv.as_ref().map(|c| c.level.as_str())
    }

    /// The main loop: `epoll_wait`, dispatch, tick.
    ///
    /// Returns the [`ShutdownAction`] that ended the run — from a
    /// control command (`POWEROFF`/`REBOOT`/`HALT`), a signal
    /// (`SIGTERM`/`SIGINT` ⇒ `Halt`), or a `telinit 0`/`telinit 6`
    /// (sysv mode). The caller finalizes the teardown (filesystems,
    /// `reboot(2)`).
    pub fn run(&mut self, levels: Vec<Vec<String>>) -> LoopResult<ShutdownAction> {
        let startup_order: Vec<String> = if self.sysv.is_some() {
            // In sysv mode the service set is whatever we registered from
            // the inittab (one-shots + respawns); boot levels are empty.
            Vec::new()
        } else {
            levels.iter().flatten().cloned().collect()
        };

        if self.sysv.is_some() {
            info!(
                "sysv mode: booting from /etc/inittab runlevel {}",
                self.runlevel().unwrap_or("?")
            );
            self.sysv_boot();
        } else {
            info!(
                "--- boot: starting {} service(s) across {} concurrent level(s) ---",
                startup_order.len(),
                levels.len()
            );
            for (i, level) in levels.iter().enumerate() {
                debug!("boot level {}: {}", i, level.join(", "));
            }
            self.boot(&levels);
        }
        self.supervisor.mark_system_running();
        info!("boot complete — entering the main event loop");

        let mut events = vec![EpollEvent::empty(); EVENT_CAPACITY];
        let mut epoll_errors = 0u32;
        loop {
            // Make sure any fds the supervisor armed/created during the
            // previous iteration (restart timers, notify sockets) are
            // registered before we block on epoll.
            self.register_fds();

            let n = match self.epoll.wait(&mut events, TICK_TIMEOUT_MS) {
                Ok(n) => {
                    epoll_errors = 0;
                    n
                }
                // No async signal handlers exist, but EINTR is still
                // possible (e.g. ptrace, or signals sent to other
                // threads); treat it as a spurious wakeup.
                Err(Errno::EINTR) => 0,
                Err(e) => {
                    // A failing `epoll_wait` means our event set is
                    // unusable. Propagating it would end the loop and
                    // take every supervised service down with it, so
                    // back off and keep going; only a persistent failure
                    // gives up.
                    epoll_errors += 1;
                    warn!("epoll_wait failed ({}): {} in a row", e, epoll_errors);
                    if epoll_errors >= MAX_EPOLL_ERRORS {
                        // Give up by *shutting down*, never by returning:
                        // `run()` returning `Err` skips
                        // `perform_shutdown` entirely and `main`
                        // propagates it straight out of PID 1. That is
                        // `Attempted to kill init!` with every service
                        // still running — no stops, no cgroup sweep, no
                        // sync, no umount, no `reboot(2)` — the abrupt
                        // reset everything else here works to avoid. A
                        // halt we asked for is strictly better: the
                        // orderly path stops everything and exits 0.
                        error!(
                            "epoll_wait failed {} times in a row — shutting down rather than \
                             abandoning the supervised services",
                            epoll_errors
                        );
                        // `break`, not `continue`: `epoll_wait` is
                        // still broken, so looping back would only log
                        // the same failure while the pending shutdown
                        // waits below for the *end* of the iteration to
                        // be observed.
                        self.pending_shutdown = Some(ShutdownAction::Halt);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            for ev in events.iter().take(n) {
                // Never `?` here: `run` returning `Err` skips
                // `perform_shutdown` entirely (see the `epoll_wait`
                // failure arm above for why that is the one outcome to
                // avoid), and no dispatch failure is worth abandoning
                // every supervised service over. Log it and carry on —
                // the pending-shutdown check below still ends the loop.
                if let Err(e) = self.dispatch(ev) {
                    warn!("event dispatch failed: {e} — continuing");
                }
            }

            // Handle pending `telinit N` / `RELOAD` requests after this
            // dispatch cycle so the control connections can flush their
            // replies. Drained in order: every accepted request runs.
            for change in self.pending_changes.drain(..).collect::<Vec<_>>() {
                match change {
                    crate::control_socket::ChangeRequest::Level(level) => {
                        // A failed runlevel switch is an operator-visible
                        // problem, not a reason to abandon every service we
                        // are supervising: log it and keep the loop alive.
                        if let Err(e) = self.handle_level_change(&level) {
                            error!("runlevel change to {} failed: {}", level, e);
                        }
                    }
                    crate::control_socket::ChangeRequest::Reload => {
                        if let Err(e) = self.reload_inittab() {
                            error!("re-reading the inittab failed: {}", e);
                        }
                    }
                }
            }

            // Periodic housekeeping (at least once per second):
            // reset restart backoffs for stable services.
            self.supervisor.check_backoff_reset(Instant::now());
            // Both are a `stat` while everything is in place, and the
            // second matters in *every* mode: a service can wipe /run at
            // any time, not only from an rcS. Without this the socket
            // path can vanish mid-session with nothing noticing.
            self.reestablish_runtime_files();

            // Reclaim control-connection slots held by clients that
            // connected and then went quiet. The cap on how many can
            // exist is not a cap on how long one may live, and a socket
            // that stays unusable is how an unbootable-looking machine
            // becomes unfixable. Cheap: a map walk over at most
            // `MAX_CONNECTIONS` entries, once per tick.
            self.reap_idle_control_conns();

            if self.pending_shutdown.is_some() {
                break;
            }
        }

        let action = self.pending_shutdown.take().unwrap_or(ShutdownAction::Halt);
        self.perform_shutdown(&startup_order, action)?;
        Ok(action)
    }
}

/// Drain every complete newline-terminated line from `buf`, decoding
/// each as UTF-8; the unconsumed tail stays in `buf`.
///
/// Byte scan, not `str::find`: the tail may hold a partial multi-byte
/// sequence that is not valid UTF-8 until the rest arrives, and only
/// newline-terminated slices are ever decoded — those are complete by
/// construction, so a multi-byte character split across two `read`s is
/// reassembled rather than replaced with U+FFFD. A trailing `\r` is
/// stripped (pty-style `CRLF` output).
fn take_complete_lines(buf: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    let mut consumed = 0usize;
    while let Some(rel) = buf[consumed..].iter().position(|&b| b == b'\n') {
        let end = consumed + rel;
        let mut line: &[u8] = &buf[consumed..end];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        lines.push(String::from_utf8_lossy(line).into_owned());
        consumed = end + 1;
    }
    if consumed > 0 {
        buf.drain(..consumed);
    }
    lines
}

// ─────────────────────────── unit tests ──────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_boot_order_logic_precondition() {
        // Sanity: our tick and boot constants are sane durations.
        const _: () = assert!(TICK_TIMEOUT_MS >= 1000);
        assert!(BOOT_WAIT_TIMEOUT.as_secs() >= 1);
        assert!(BOOT_POLL_INTERVAL.as_millis() > 0);
    }

    /// The pure predicate behind `register_fds`' log-pipe bookkeeping.
    fn owns(log_names: &HashMap<i32, String>, fd: i32, name: &str) -> bool {
        log_names.get(&fd).is_some_and(|n| n == name)
    }

    #[test]
    fn test_a_stale_log_pipe_number_is_never_deleted_from_epoll() {
    // `Supervisor::start` swaps a service's log pipe by inserting a
        // new one, closing the old fd on the spot. The freed number can
        // then be handed to anything — including the control listener
        // that a `/run` wipe forced us to rebind. Unregistering that
        // number because it is "the old log pipe of some service" evicts
        // a live fd from epoll, and a listener that is no longer watched
        // accepts connections nobody ever answers.
        let mut names: HashMap<i32, String> = HashMap::new();
        names.insert(10, "wipe.sh".to_string());
        assert!(owns(&names, 10, "wipe.sh"), "still ours");

        // The number was recycled: our bookkeeping still points at it,
        // but epoll now watches something else entirely.
        names.insert(10, "control listener".to_string());
        assert!(
            !owns(&names, 10, "wipe.sh"),
            "a recycled fd number must not be deleted on a stale entry"
        );
        // And a number nobody owns at all is equally unsafe to touch.
        assert!(!owns(&names, 42, "wipe.sh"));
    }

    #[test]
    fn test_orphan_sweep_never_claims_a_bystander_when_not_pid1() {
        // The regression: the teardown sweep used to SIGKILL every
        // process parented to PID 1, which is only a synonym for "our
        // orphan" when we *are* PID 1. Under `cargo test` (and in a
        // container, and in any `docker run --pid=host`) PID 1 belongs
        // to something else — the shell that started us — and sweeping
        // on that predicate killed the test harness itself.
        let self_pid = getpid().as_raw();
        assert_ne!(self_pid, 1, "this test only makes sense off PID 1");
        for pid in [2, 7, 4242] {
            assert!(
                !EventLoop::owns_orphan(pid, self_pid),
                "pid {pid} must not be claimable while we are pid {self_pid}"
            );
        }
    }

    #[test]
    fn test_orphan_sweep_never_claims_pid1_or_itself() {
        let self_pid = getpid().as_raw();
        // These hold whatever PID we are: PID 1 is the machine's, and we
        // are obviously not our own orphan to be SIGKILLed.
        assert!(!EventLoop::owns_orphan(1, self_pid));
        assert!(!EventLoop::owns_orphan(0, self_pid));
        assert!(!EventLoop::owns_orphan(-1, self_pid));
        assert!(!EventLoop::owns_orphan(self_pid, self_pid));
    }

    #[test]
    fn test_owns_orphan_is_a_pure_pid1_predicate() {
        // Pure over both pids (no `getpid` inside), so the PID 1 case
        // is testable without being PID 1: only PID 1 owns orphans.
        assert!(EventLoop::owns_orphan(2, 1));
        assert!(EventLoop::owns_orphan(4242, 1));
        assert!(!EventLoop::owns_orphan(1, 1));
        assert!(!EventLoop::owns_orphan(0, 1));
        assert!(!EventLoop::owns_orphan(1, 1));
        assert!(!EventLoop::owns_orphan(-5, 1));
        // …and nobody else owns anything, which is what keeps `cargo
        // test` (pid != 1) from sweeping the harness.
        assert!(!EventLoop::owns_orphan(2, 137));
        assert!(!EventLoop::owns_orphan(2, 7));
    }

    #[test]
    fn test_log_drain_bound_is_nonzero_and_small() {
        // The per-tick cap that keeps one chatty service from starving
        // PID 1: nonzero (progress is guaranteed) and small (a tick
        // stays short). 16 × 4 KiB = 64 KiB, exactly one pipe buffer.
        assert!(MAX_LOG_DRAIN_ITERS > 0);
        assert!(MAX_LOG_DRAIN_ITERS <= 64);
        assert!(EPOLL_WARN_SUPPRESS.as_secs() >= 10);
    }

    #[test]
    fn test_take_complete_lines_splits_and_keeps_the_tail() {
        let mut buf = b"one\ntwo\npartial".to_vec();
        assert_eq!(take_complete_lines(&mut buf), vec!["one", "two"]);
        assert_eq!(buf, b"partial");

        // The tail completes on the next chunk.
        buf.extend_from_slice(b" end\n");
        assert_eq!(take_complete_lines(&mut buf), vec!["partial end"]);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_take_complete_lines_strips_cr_and_keeps_empties_for_the_caller() {
        // CRLF (pty output) loses the CR; a bare newline is an empty
        // line the caller (`route_log`) skips, exactly as before.
        let mut buf = b"a\r\n\nb\n".to_vec();
        assert_eq!(take_complete_lines(&mut buf), vec!["a", "", "b"]);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_take_complete_lines_reassembles_a_multibyte_char_split_across_reads() {
        // "é" is 0xC3 0xA9. The old per-chunk `from_utf8_lossy` turned a
        // split character into U+FFFD; raw bytes wait for the rest.
        let mut buf = vec![b'h', 0xC3];
        assert!(take_complete_lines(&mut buf).is_empty());
        assert_eq!(buf, vec![b'h', 0xC3]);
        buf.extend_from_slice(&[0xA9, b'\n']);
        assert_eq!(take_complete_lines(&mut buf), vec!["hé"]);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_take_complete_lines_never_decodes_the_tail() {
        // Invalid UTF-8 in a *complete* line still decodes lossy (a log
        // router must not die on binary output), but an incomplete tail
        // is left byte-identical for the next chunk.
        let mut buf = vec![0xFF, b'\n', 0xFE];
        assert_eq!(take_complete_lines(&mut buf), vec!["�"]);
        assert_eq!(buf, vec![0xFE]);
    }

    #[test]
    fn test_reap_keep_waiting_holds_until_the_deadline() {
        // Locks the polarity `poll_reap` depends on: time remaining ⇒
        // keep waiting, deadline reached ⇒ done. It once lived inline
        // backwards, which made `kill_and_reap` return instantly.
        let t0 = Instant::now();
        let deadline = t0 + Duration::from_millis(500);
        assert!(reap_keep_waiting(t0, deadline));
        assert!(reap_keep_waiting(deadline - Duration::from_nanos(1), deadline));
        assert!(!reap_keep_waiting(deadline, deadline));
        assert!(!reap_keep_waiting(deadline + Duration::from_secs(1), deadline));
    }
}
