//! EventLoop::dispatch (split from `event_loop.rs` — verbatim move, no logic changes).
//!
//! Child post-fork paths do only async-signal-safe calls + _exit(127); signals are blocked before signalfd; fds 0/1/2 are claimed; journal/log/connection caps stay bounded.
use super::*;
use std::collections::HashSet;

impl super::EventLoop {
    /// `epoll_ctl(ADD)` for a fresh fd, tolerating failure.
    ///
    /// A failure here (EBADF because the fd vanished under us, ENOMEM
    /// because the epoll set is exhausted) used to propagate out of
    /// `dispatch`/`register_fds` and end the event loop — i.e. one
    /// unlucky fd killed PID 1 and every service on the machine. It is
    /// now a throttled warning and the rest of the loop carries on.
    ///
    /// Every call retries the ADD: a transient failure must not disable
    /// the fd forever, and fd numbers are recycled constantly — an fd N
    /// that failed a minute ago may be a different, healthy fd today.
    /// `epoll_failed` only remembers *when we last warned*, so one bad
    /// fd does not log on every tick.
    pub(crate) fn epoll_add(&mut self, fd: i32, label: &str) -> bool {
        // SAFETY: every caller passes an fd it owns and keeps alive for
        // the duration of the call.
        let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
        match self.epoll.add(bfd, EpollEvent::new(EpollFlags::EPOLLIN, fd as u64)) {
            Ok(()) => {
                debug!("registered {} fd {}", label, fd);
                self.epoll_failed.remove(&fd);
                true
            }
            Err(e) => {
                let now = Instant::now();
                let warn_again = self
                    .epoll_failed
                    .get(&fd)
                    .is_none_or(|last| now.duration_since(*last) >= EPOLL_WARN_SUPPRESS);
                if warn_again {
                    warn!("could not register {} fd {}: {}", label, fd, e);
                    self.epoll_failed.insert(fd, now);
                }
                false
            }
        }
    }

    /// Is fd `fd` still registered as `name`'s log pipe?
    ///
    /// The guard on the identity check in `register_fds`, as a function
    /// so it can be tested: `log_names` maps every watched fd to its
    /// owner, so a number that is absent (or owned by something else)
    /// is a stale bookkeeping entry and must not be deleted from epoll.
    pub(crate) fn owns_log_fd(&self, fd: i32, name: &str) -> bool {
        self.log_names.get(&fd).is_some_and(|n| n == name)
    }

    /// Drop an fd from epoll and forget every name that pointed at it.
    pub(crate) fn epoll_forget(&mut self, fd: i32) {
        let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
        let _ = self.epoll.delete(bfd);
        self.timer_names.remove(&fd);
        self.notify_names.remove(&fd);
        self.log_names.remove(&fd);
        self.epoll_failed.remove(&fd);
    }

    /// Drop stale registrations before adding new ones.
    ///
    /// The name maps only ever grew: a service that was removed (or
    /// whose timer/notify/log fd was closed and whose number was then
    /// recycled for something else) left its old number in the map.
    /// `dispatch` routes by number, so a recycled fd could be handled
    /// as the wrong kind — a control connection answered as a restart
    /// timer, a log pipe mistaken for a listener. Worse, an `epoll_del`
    /// for a stale log-pipe number could evict the live fd that now
    /// owns that number.
    ///
    /// Pruning runs on the live sets the supervisor reports right now,
    /// plus the control socket's live listener/connections. Anything
    /// not live is forgotten (epoll + every name map + the warn
    /// throttle), so a recycled number starts clean.
    pub(crate) fn prune_stale_registrations(
        &mut self,
        live_timers: &HashSet<i32>,
        live_notify: &HashSet<i32>,
        live_logs: &HashSet<i32>,
    ) {
        // Collect-then-forget on purpose: `epoll_forget` takes `&mut
        // self`, which cannot run inside a `retain` closure over one of
        // its own maps.
        let stale_timers: Vec<i32> = self
            .timer_names
            .keys()
            .copied()
            .filter(|fd| !live_timers.contains(fd))
            .collect();
        for fd in stale_timers {
            self.epoll_forget(fd);
        }
        let stale_notify: Vec<i32> = self
            .notify_names
            .keys()
            .copied()
            .filter(|fd| !live_notify.contains(fd))
            .collect();
        for fd in stale_notify {
            self.epoll_forget(fd);
        }
        let stale_logs: Vec<i32> = self
            .log_names
            .keys()
            .copied()
            .filter(|fd| !live_logs.contains(fd))
            .collect();
        for fd in stale_logs {
            self.epoll_forget(fd);
        }
        // `name -> fd` must agree with `fd -> name`: a service whose
        // pipe changed (or vanished) leaves a dangling forward entry
        // that would otherwise delete the *new* owner's number.
        // No `epoll_forget` here: the reverse map owns epoll
        // membership, and pruning it above already handled the fd.
        self.log_pipe_names.retain(|name, fd| {
            live_logs.contains(fd) && self.log_names.get(fd).is_some_and(|n| n == name)
        });
        // Drop throttle entries for fds nothing owns any more (a `/run`
        // wipe rebind closes the old listener fd; a closed connection's
        // number may be recycled next accept). The listener itself is
        // re-added by `reestablish_runtime_files`.
        let listener = self.control.listener_fd();
        let has_conn = |fd: i32| self.control.has_conn(fd);
        let owned = |fd: i32| {
            self.timer_names.contains_key(&fd)
                || self.notify_names.contains_key(&fd)
                || self.log_names.contains_key(&fd)
                || Some(fd) == listener
                || has_conn(fd)
        };
        let stale_throttle: Vec<i32> = self
            .epoll_failed
            .keys()
            .copied()
            .filter(|fd| {
                !(live_timers.contains(fd)
                    || live_notify.contains(fd)
                    || live_logs.contains(fd)
                    || owned(*fd))
            })
            .collect();
        for fd in stale_throttle {
            self.epoll_failed.remove(&fd);
        }
    }

    /// Register every restart `timerfd` and readiness notify socket with
    /// epoll, exactly once per fd.
    ///
    /// A service's `timerfd` is created once (on its first failure) and
    /// reused for every subsequent backoff, so the fd stays registered
    /// for the whole lifetime of the service — we only *arm* and *disarm*
    /// it. `epoll_ctl(ADD)` on an fd that is already registered fails
    /// with `EEXIST`, which is why this is guard-checked with the name
    /// maps.
    pub(crate) fn register_fds(&mut self) {
        self.register_service_fds(true);
    }

    /// Shared `epoll_ctl(ADD)` pass over every service's fds, recording
    /// each fd under its owner's name.
    ///
    /// `timers` selects whether the per-service restart `timerfd`s are
    /// included. It is `false` only during the boot waits, and for a
    /// concrete reason: a timer that fired there would be reported by
    /// every subsequent `epoll_wait` until someone drained it (the
    /// registration is level-triggered), and the boot wait deliberately
    /// does not run a timer handler — so including them would peg PID 1
    /// at 100% CPU for the rest of a five-minute `rcS`. The main loop
    /// registers them on its first tick, which is where they belong:
    /// restarts are the supervisor's decision, not the boot's.
    ///
    /// The notify sockets and log pipes *are* included during boot (see
    /// [`EventLoop::epoll_wait_for_boot`]) and are recorded in the very
    /// same name maps this uses, so the main loop finds them already
    /// registered and does not try to add them a second time.
    pub(crate) fn register_service_fds(&mut self, timers: bool) {
        // Two phases on purpose: collecting the fds needs an immutable
        // borrow of the supervisor, and registering them needs `&mut
        // self`. Phase 1 records what *should* be watched, phase 2 does
        // the epoll work and records the name maps.
        let wanted: Vec<(String, Option<i32>, Option<i32>, Option<i32>)> = self
            .supervisor
            .services()
            .map(|svc| {
                let name = svc.name.as_str();
                let timer_fd = if timers {
                    self.supervisor.restart_timer(name).map(|t| t.as_fd().as_raw_fd())
                } else {
                    None
                };
                (
                    name.to_string(),
                    timer_fd,
                    self.supervisor.notify_socket(name).map(|f| f.as_raw_fd()),
                    self.supervisor.log_pipe(name).map(|p| p.as_raw_fd()),
                )
            })
            .collect();

        // Forget numbers nobody owns any more *before* adding new ones,
        // so a recycled fd number can never be dispatched as its
        // previous owner (see `prune_stale_registrations`).
        {
            let live_timers: HashSet<i32> =
                wanted.iter().filter_map(|(_, t, _, _)| *t).collect();
            let live_notify: HashSet<i32> =
                wanted.iter().filter_map(|(_, _, n, _)| *n).collect();
            let live_logs: HashSet<i32> =
                wanted.iter().filter_map(|(_, _, _, l)| *l).collect();
            self.prune_stale_registrations(&live_timers, &live_notify, &live_logs);
        }

        for (name, timer_fd, notify_fd, log_fd) in wanted {
            if let Some(fd) = timer_fd {
                if !self.timer_names.contains_key(&fd) && self.epoll_add(fd, "restart timer") {
                    self.timer_names.insert(fd, name.clone());
                }
            }
            if let Some(fd) = notify_fd {
                if !self.notify_names.contains_key(&fd) && self.epoll_add(fd, "notify") {
                    self.notify_names.insert(fd, name.clone());
                }
            }
            // Log pipes: register new pipes, and when a service restarts
            // (new pipe) unregister the stale one first — otherwise the
            // old fd would stay in the epoll set pointing at a name we
            // have moved on from.
            if let Some(fd) = log_fd {
                match self.log_pipe_names.get(&name) {
                    // Only unregister the old number if it is still
                    // *ours*. `Supervisor::start` replaces a service's
                    // log pipe by inserting a new one, which closes the
                    // old fd at once; by the time we get here the freed
                    // number may already belong to something else — the
                    // rebound control listener after a `/run` wipe, a
                    // timerfd, a notify socket. Deleting it from epoll
                    // then silently un-watches a live fd: for the
                    // listener that means clients connect and are never
                    // answered again.
                    Some(&old) if old != fd && self.owns_log_fd(old, &name) => {
                        self.epoll_forget(old);
                        debug!("replaced log pipe fd {} → {} for {}", old, fd, name);
                    }
                    _ => {}
                }
                if self.log_pipe_names.get(&name) != Some(&fd) && self.epoll_add(fd, "log pipe")
                {
                    self.log_names.insert(fd, name.clone());
                    self.log_pipe_names.insert(name.clone(), fd);
                }
            }
        }
    }

    /// Route a ready fd to the right handler.
    pub(crate) fn dispatch(&mut self, ev: &EpollEvent) -> LoopResult<()> {
        let fd = ev.data() as i32;
        if fd == self.signal_fd.as_raw_fd() {
            self.handle_signals()?;
        } else if self.control.listener_fd() == Some(fd) {
            self.handle_control_accept()?;
        } else if self.control.has_conn(fd) {
            self.handle_control_conn(fd, ev)?;
        } else if let Some(name) = self.timer_names.get(&fd).cloned() {
            self.handle_restart_timer(&name)?;
        } else if let Some(name) = self.notify_names.get(&fd).cloned() {
            self.handle_notify_fd(&name)?;
        } else if let Some(name) = self.log_names.get(&fd).cloned() {
            self.handle_log_fd(&name)?;
        } else {
            debug!("epoll event for unknown fd {}", fd);
        }
        Ok(())
    }

    /// A readiness notify socket is readable: drain its datagrams.
    pub(crate) fn handle_notify_fd(&mut self, name: &str) -> LoopResult<()> {
        while let Some(data) = self.supervisor.read_notify(name) {
            self.supervisor.handle_notify_datagram(name, &data);
        }
        Ok(())
    }

    /// The control listener is readable: accept every pending client
    /// connection and register each with epoll.
    pub(crate) fn handle_control_accept(&mut self) -> LoopResult<()> {
        for raw in self.control.accept_pending() {
            // We cannot watch a connection we failed to register, and
            // leaving it open would be a leak the client never sees
            // close: drop it instead.
            if !self.epoll_add(raw, "control connection") {
                self.close_conn(raw)?;
                continue;
            }
            debug!("control: accepted connection fd {}", raw);
        }
        Ok(())
    }

    /// A control client is readable and/or writable: run the command
    /// exchange, then keep, re-arm, or close the connection.
    pub(crate) fn handle_control_conn(&mut self, fd: i32, ev: &EpollEvent) -> LoopResult<()> {
        let events = ev.events();
        let mut action: Option<ConnAction> = None;
        let mut shutdown: Option<ShutdownAction> = None;

        if events.contains(EpollFlags::EPOLLIN) {
            let (conn, requested, change) = self.control.on_readable(fd, &mut self.supervisor);
            action = Some(conn);
            if let Some(requested) = requested {
                shutdown = Some(requested);
            }
            if let Some(change) = change {
                match change {
                    crate::control_socket::ChangeRequest::Level(ref level) => {
                        info!("control command accepted — changing to runlevel {level}");
                    }
                    crate::control_socket::ChangeRequest::Reload => {
                        info!("control command accepted — re-reading /etc/inittab");
                    }
                }
                // Queued, never overwritten: every accepted request runs
                // in order on the next tick boundary.
                self.pending_changes.push(change);
            }
        }
        if events.contains(EpollFlags::EPOLLOUT) && action != Some(ConnAction::Close) {
            action = Some(self.control.on_writable(fd));
        }
        // HUP/ERR means the peer is gone. The old code only closed when
        // no other action was decided, so a connection that produced a
        // `Keep` (partial line parked, peer hung up mid-command) stayed
        // open until the idle sweep reclaimed it — holding a slot for
        // the full timeout for a client that will never finish. Close
        // once any queued reply has been flushed; the reply itself is
        // already in `out_bufs`, and `close_conn` drops it with the fd.
        if events.intersects(EpollFlags::EPOLLHUP | EpollFlags::EPOLLERR) {
            let drained = !self.control.pending_write(fd);
            if action.is_none() || (drained && action != Some(ConnAction::Close)) {
                // If a reply is still queued, give it one flush attempt
                // first: a half-closed client (`printf … | socat`) still
                // expects its answer.
                if !drained && action != Some(ConnAction::Close) {
                    action = Some(self.control.on_writable(fd));
                }
                if !self.control.pending_write(fd) {
                    action = Some(ConnAction::Close);
                }
            }
        }

        match action {
            Some(ConnAction::Close) => self.close_conn(fd)?,
            _ => {
// Re-arm: EPOLLOUT stays armed only while a reply is
                // queued (EAGAIN on write), otherwise EPOLLIN only.
                //
                // Registration stays LEVEL-triggered on purpose. The
                // 100%-CPU spin this had was not an epoll mode problem:
                // `on_readable` performed a single 4 KiB `read`, so a
                // client that stopped mid-command left its tail in the
                // socket buffer with the event permanently asserted.
                // Draining to `EAGAIN` empties that buffer (the partial
                // line is parked in the connection's input buffer), which
                // is the actual fix and needs no ET semantics. Edge
                // triggering would only add the usual hazard of a lost
                // edge stranding a reply.
                let want_out = self.control.pending_write(fd);
                if let Some(cfd) = self.control.conn_fd(fd) {
                    let flags = if want_out {
                        EpollFlags::EPOLLIN | EpollFlags::EPOLLOUT
                    } else {
                        EpollFlags::EPOLLIN
                    };
                    let mut ev = EpollEvent::new(flags, fd as u64);
                    if let Err(e) = self.epoll.modify(cfd, &mut ev) {
                        warn!("could not re-arm control fd {}: {}", fd, e);
                        self.close_conn(fd)?;
                    }
                }
            }
        }

        // A shutdown command has been accepted (its OK reply is already
        // queued for the client): kick off the teardown. The event loop
        // will exit at the end of this iteration.
        if let Some(requested) = shutdown {
            info!(
                "control command accepted — system {}",
                match requested {
                    ShutdownAction::PowerOff => "powering off",
                    ShutdownAction::Reboot => "rebooting",
                    ShutdownAction::Halt => "halting",
                }
            );
            if self.pending_shutdown.is_none() {
                self.pending_shutdown = Some(requested);
            }
        }
        Ok(())
    }

    /// Drop a control connection: unregister from epoll, close its fd.
    pub(crate) fn close_conn(&mut self, fd: i32) -> LoopResult<()> {
        if self.control.conn_fd(fd).is_none() {
            return Ok(());
        }
        // One place forgets an fd: epoll plus every name map that could
        // point at it, so a recycled fd number can never inherit the
        // identity of the connection we just closed.
        self.epoll_forget(fd);
        self.control.remove_conn(fd);
        debug!("control: closed connection fd {}", fd);
        Ok(())
    }

    /// Drop control connections idle past the configured timeout (see
    /// [`crate::event_loop::EventLoop::set_ctl_idle_timeout`]).
    ///
    /// Runs once per tick; the socket only reports candidates, epoll
    /// ownership stays here. Never fails the loop: a close that errors
    /// is a warning.
    pub(crate) fn reap_idle_control_conns(&mut self) {
        let timeout = self.ctl_idle_timeout;
        let expired = self.control.expired_conns(Instant::now(), timeout);
        for fd in expired {
            warn!(
                "control: dropping connection fd {} after {:?} without a command line",
                fd, timeout
            );
            if let Err(e) = self.close_conn(fd) {
                warn!("control: could not drop idle connection fd {}: {}", fd, e);
            }
        }
    }

    /// A service's log pipe is readable: drain and route its output.
    ///
    /// Bounded: at most `MAX_LOG_DRAIN_ITERS` reads per tick. The
    /// registration is level-triggered, so anything left simply fires
    /// again — while an unbounded drain lets one `yes`-style service
    /// starve every other fd PID 1 watches.
    pub(crate) fn handle_log_fd(&mut self, name: &str) -> LoopResult<()> {
        let mut buf = [0u8; 4096];
        for _ in 0..MAX_LOG_DRAIN_ITERS {
            match self.supervisor.read_log(name, &mut buf) {
                Ok(0) => {
                    self.close_log_pipe(name)?;
                    return Ok(());
                }
                Ok(n) => self.route_log(name, &buf[..n]),
                Err(Errno::EAGAIN) => return Ok(()),
                Err(Errno::EINTR) => continue,
                Err(e) => {
                    warn!("log read for {} failed: {}", name, e);
                    return Ok(());
                }
            }
        }
        debug!("log drain for {name} hit the per-tick cap; remainder waits for the next tick");
        Ok(())
    }

    /// Split raw log bytes into lines and emit each as
    /// `[<service>] <line>`. Partial lines accumulate until a newline
    /// (or EOF) arrives, so multi-write lines stay whole.
    ///
    /// The accumulator is raw bytes, scanned in place for `b'\n'`: only
    /// complete lines are ever UTF-8-decoded. The old shape decoded the
    /// whole buffer with `from_utf8_lossy` on every 4 KiB chunk (an
    /// allocation per chunk in PID 1's hot path) and re-allocated the
    /// remainder as a fresh `String` per line; here the only allocation
    /// is the decoded line being logged.
    pub(crate) fn route_log(&mut self, name: &str, data: &[u8]) {
        let partial = self.log_partial.entry(name.to_string()).or_default();
        partial.extend_from_slice(data);
        for line in take_complete_lines(partial) {
            if !line.is_empty() {
                info!("[{}] {}", name, line);
            }
        }
        // No newline yet. A service that writes megabytes without
        // ever flushing one (a `yes`-style loop, a binary blob, a
        // progress bar without CR) would otherwise grow this buffer
        // without bound — and it is PID 1's heap, the last place on
        // the machine where an OOM is unrecoverable. Emit what we
        // have as its own truncated line and start over.
        if partial.len() >= MAX_PARTIAL_LOG {
            let chunk: Vec<u8> = std::mem::take(partial);
            info!(
                "[{}] {} …(truncated: no newline in {} bytes)",
                name,
                String::from_utf8_lossy(&chunk).trim_end_matches('\r'),
                chunk.len()
            );
        }
    }

    /// EOF on a service's log pipe: flush the trailing partial line,
    /// unregister the fd, and hand the pipe back to the supervisor so
    /// it can be dropped (closed).
    pub(crate) fn close_log_pipe(&mut self, name: &str) -> LoopResult<()> {
        if let Some(fd) = self.supervisor.take_log_pipe(name) {
            let raw = fd.as_raw_fd();
            self.epoll_forget(raw);
            self.log_pipe_names.remove(name);
            if let Some(partial) = self.log_partial.remove(name) {
                Self::emit_trailing_partial(name, &partial);
            }
            debug!("closed log pipe fd {} for {}", raw, name);
        }
        Ok(())
    }

    /// Emit one unterminated tail as its own log line (shared by the
    /// EOF path and the shutdown drain, which must agree: both strip a
    /// single trailing CR from pty-style output and both use lossy
    /// UTF-8, since a tail may end mid-sequence).
    pub(crate) fn emit_trailing_partial(name: &str, partial: &[u8]) {
        if partial.is_empty() {
            return;
        }
        let mut line: &[u8] = partial;
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        info!("[{}] {}", name, String::from_utf8_lossy(line));
    }

    /// Drain pending signalfd records.
    pub(crate) fn handle_signals(&mut self) -> LoopResult<()> {
        loop {
            let info = match self.signal_fd.read_signal() {
                Ok(Some(info)) => info,
                Ok(None) => break,
                Err(e) => {
                    // EAGAIN simply means the queue is drained. Anything
                    // else is logged and the drain stops, because
                    // returning it would end the event loop and take
                    // every service down with a single unlucky read.
                    if e != Errno::EAGAIN {
                        warn!("reading the signalfd failed: {}", e);
                    }
                    break;
                }
            };
            let signo = info.ssi_signo as i32;
            match Signal::try_from(signo) {
                Ok(Signal::SIGCHLD) => self.reap_children(),
                Ok(Signal::SIGTERM) => {
                    // First request wins; a control POWEROFF/REBOOT/HALT
                    // already accepted is never downgraded to Halt.
                    if self.pending_shutdown.is_none() {
                        info!("received SIGTERM — initiating graceful shutdown");
                        self.pending_shutdown = Some(ShutdownAction::Halt);
                    } else {
                        info!("received SIGTERM — shutdown already in progress");
                    }
                }
                // Ctrl+Alt+Del (kernel → SIGINT to PID 1). In sysv mode
                // this runs the inittab `ctrlaltdel` line (usually
                // `/sbin/shutdown -r now`); otherwise it is a Halt.
                Ok(Signal::SIGINT) => {
                    if let Some(cmd) = self.action_commands.get("ctrlaltdel").cloned() {
                        info!("SIGINT — ctrl+alt+del, running {}", cmd);
                        self.run_action(&cmd);
                    } else if self.pending_shutdown.is_none() {
                        info!("received SIGINT — initiating graceful shutdown");
                        self.pending_shutdown = Some(ShutdownAction::Halt);
                    }
                }
                Ok(Signal::SIGALRM) => {
                    warn!("ignoring SIGALRM (restart timers use timerfd)");
                }
                // sysvinit: SIGHUP means "re-read /etc/inittab" — what
                // `telinit q` does via the control socket.
                Ok(Signal::SIGHUP) => {
                    info!("received SIGHUP — re-reading /etc/inittab");
                    if let Err(e) = self.reload_inittab() {
                        error!("re-reading the inittab failed: {}", e);
                    }
                }
                // sysvinit power management: SIGPWR / SIGUSR1 / SIGUSR2 /
                // SIGWINCH run the matching inittab power lines. When no
                // action is configured we follow sysvinit and ignore them.
                Ok(Signal::SIGPWR) => self.run_action_or("powerfailnow"),
                Ok(Signal::SIGUSR2) => self.run_action_or("powerfail"),
                Ok(Signal::SIGUSR1) => self.run_action_or("powerwait"),
                Ok(Signal::SIGWINCH) => self.run_action_or("powerokwait"),
                other => {
                    debug!("unhandled signal {:?}", other.ok());
                }
            }
        }
        Ok(())
    }

    /// Reap children with `waitpid(WNOHANG)` until none remain.
    ///
    /// Called on SIGCHLD. Because SIGCHLD can coalesce (one signal for
    /// several exits), we keep calling until `ECHILD` / `StillAlive`.
    /// Each reaped child is handed to the supervisor, which decides
    /// whether a restart is due. Daemonized services are *not* restarted.
    pub(crate) fn reap_children(&mut self) {
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => break,
                // Non-terminal reports leave the child alive (and this
                // `waitpid` never asks for them — no WUNTRACED /
                // WCONTINUED — so they cannot appear today). Skip the
                // supervisor entirely rather than logging-and-ignoring
                // per child: there is nothing to decide until it exits.
                Ok(
                    WaitStatus::Stopped(_, _)
                    | WaitStatus::Continued(_)
                    | WaitStatus::PtraceEvent(_, _, _)
                    | WaitStatus::PtraceSyscall(_),
                ) => continue,
                Ok(status) => {
                    // waitpid(-1) infers the pid; normalize it here. The
                    // outer match already broke on `StillAlive`, so it
                    // cannot appear — but PID 1 has no panicking
                    // branches: unreachable shapes just end the sweep.
                    let pid = match status {
                        WaitStatus::Exited(pid, _) => pid,
                        WaitStatus::Signaled(pid, _, _) => pid,
                        WaitStatus::Stopped(pid, _)
                        | WaitStatus::Continued(pid)
                        | WaitStatus::PtraceEvent(pid, _, _)
                        | WaitStatus::PtraceSyscall(pid) => pid,
                        WaitStatus::StillAlive => break,
                    };
                    self.supervisor.handle_child_exit(pid, status);
                }
                Err(Errno::ECHILD) => break,
                Err(e) => {
                    warn!("waitpid(-1) failed: {}", e);
                    break;
                }
            }
        }
    }

    /// A restart timer fired: drain it, then ask the supervisor to
    /// spawn the service again.
    pub(crate) fn handle_restart_timer(&mut self, name: &str) -> LoopResult<()> {
        // Drain the timer fd (8 bytes = the expiration count), which is
        // required so the timer doesn't fire again immediately.
        if let Some(timer) = self.supervisor.restart_timer(name) {
            let _ = read(timer.as_fd().as_raw_fd(), &mut [0u8; 8]);
        }
        match self.supervisor.on_timer_fire(name) {
            Ok(()) => {}
            Err(e) => error!("restart of {} failed: {}", name, e),
        }
        Ok(())
    }

}
