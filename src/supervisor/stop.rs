//! Supervisor::stop (split from `supervisor.rs` — verbatim move, no logic changes).
//!
//! Child post-fork paths do only async-signal-safe calls + _exit(127); signals are blocked before signalfd; fds 0/1/2 are claimed; journal/log/connection caps stay bounded.
use super::*;

impl super::Supervisor {
    /// Drop the pid→service mapping for a child we reaped ourselves.
    ///
    /// `stop_service`/`stop_all` reap with a direct `waitpid` instead of
    /// going through [`Supervisor::handle_child_exit`], so they must
    /// clear the mapping themselves. A stale entry is a slow leak, and
    /// once the kernel recycles that pid it becomes worse than a leak:
    /// the next unrelated child to exit would be attributed to a service
    /// that stopped long ago.
    pub fn forget_child(&mut self, pid: Pid) -> Option<String> {
        self.pid_to_service.remove(&pid)
    }

    /// Is `pid` still a child we are tracking?
    ///
    /// `waitpid` returning `ECHILD` only means "this process has no
    /// children left", not "this pid was never ours": the event loop's
    /// own `waitpid(-1)` pass may already have reaped it and applied the
    /// restart policy. Callers use this to tell those two cases apart
    /// rather than assuming the worst.
    pub fn owns_child(&self, pid: Pid) -> bool {
        self.pid_to_service.contains_key(&pid)
    }

    /// Ask for a service to stop, **without blocking**.
    ///
    /// This is the path the control socket and a runlevel switch use.
    /// PID 1 must never sit in a wait loop: while the old
    /// [`Supervisor::stop_service`] slept for up to two seconds per
    /// service, it stopped reaping `SIGCHLD`, stopped answering every
    /// other control connection, and delayed a poweroff request that
    /// arrived mid-`STOP` — one operator command wedging the whole
    /// machine for the duration.
    ///
    /// What happens instead:
    /// 1. the service goes to `Stopping` and its restart timer is
    ///    disarmed (a service being stopped must not come back);
    /// 2. its process group gets `SIGTERM`;
    /// 3. a per-service `timerfd` is armed for
    ///    [`STOP_TERM_TIMEOUT_SECS`]; when it fires,
    ///    [`Supervisor::on_timer_fire`] escalates to `SIGKILL`;
    /// 4. the actual `Stopped` transition happens in
    ///    [`Supervisor::handle_child_exit`] when the child is reaped.
    ///
    /// A service with no child of ours (a daemonising script, or one
    /// that already finished) has nothing to wait for: its cgroup is
    /// swept for stragglers and it is `Stopped` immediately.
    pub fn request_stop(&mut self, name: &str) -> SupResult<()> {
        // Scoped borrow: copy what the branches need, then release it
        // so `disarm_timer` (a `&mut self` helper) can run. The old
        // code open-coded the disarm inline because `svc` was still
        // borrowed; scoping keeps one disarm site instead of three.
        let pid = {
            let Some(svc) = self.services.get_mut(name) else {
                return Err(SupervisorError::UnknownService(name.to_string()));
            };
            if svc.state() == ServiceState::Stopping {
                return Err(SupervisorError::AlreadyStopping(name.to_string()));
            }
            // A queued `RESTART` is superseded by a plain stop.
            svc.restart_after_stop = false;
            svc.restart_pending = false;
            svc.pid
        };

        // Disarm the restart timer *before* branching on the pid. A
        // service with no child is exactly the state it is in between
        // two restart attempts, so a `Restart`-purpose timer is
        // routinely armed at this point: `velocityctl stop` would
        // answer "OK stopping", the service would sit in `Stopped` for
        // the rest of the backoff, and then `on_timer_fire` would find
        // no stop in progress and start it again — the operator's stop
        // silently undone up to 30s later.
        self.disarm_timer(name);

        match pid {
            Some(pid) => {
                // `Stopped -> Stopping` (and `Failed -> Stopping`) is
                // refused by the machine: a service with no live state
                // has nothing to tear down. A stale pid here means the
                // bookkeeping already moved on, so stopping is a no-op.
                let transitioned = self
                    .services
                    .get_mut(name)
                    .map(|svc| svc.transition(ServiceState::Stopping))
                    .unwrap_or(false);
                if !transitioned {
                    let state = self
                        .services
                        .get(name)
                        .map(|s| s.state().to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    warn!(
                        "{:<20} stop refused (state {}) — already stopped",
                        name, state
                    );
                    return Ok(());
                }
                info!("{:<20} stopping (SIGTERM to process group)", name);
                signal_service(pid, Signal::SIGTERM);
                self.arm_timer(
                    name,
                    STOP_TERM_TIMEOUT_SECS.saturating_mul(1000),
                    TimerPurpose::StopEscalation,
                )?;
                debug!(
                    "{:<20} will escalate to SIGKILL in {}s",
                    name, STOP_TERM_TIMEOUT_SECS
                );
            }
            None => {
                // Nothing of ours to wait for. Anything still alive
                // under this service (a daemon the script forked) is
                // only reachable through its cgroup.
                // Always legal (`Running`/`Starting`/`Failed`/`Stopped`
                // all reach `Stopped`).
                if let Some(svc) = self.services.get_mut(name) {
                    svc.transition(ServiceState::Stopped);
                    svc.running_since = None;
                }
                if let Err(e) = cgroups::kill_cgroup(name) {
                    warn!("could not tear down cgroup for {}: {}", name, e);
                }
                info!("{:<20} stopped", name);
            }
        }
        Ok(())
    }

    /// Ask for a service to be restarted, **without blocking**: stop it
    /// as in [`Supervisor::request_stop`] and spawn it again as soon as
    /// its child has been reaped.
    ///
    /// When there is no child to wait for (already daemonised, or
    /// already stopped) the stop is instantaneous, so the start happens
    /// straight away.
    pub fn request_restart(&mut self, name: &str) -> SupResult<()> {
        // Validate before mutating: an unknown name must not clear
        // anyone's history, and a restart during teardown must be
        // refused without side effects (consistent with `start`, which
        // returns `ShuttingDown`). The old order reset the respawn
        // budget and zeroed the backoff first, then failed later —
        // losing the crash-loop evidence for nothing.
        if !self.services.contains_key(name) {
            return Err(SupervisorError::UnknownService(name.to_string()));
        }
        if self.system_state == SystemState::ShuttingDown {
            return Err(SupervisorError::ShuttingDown);
        }
        self.reset_respawn_history(name);
        let pid = self
            .services
            .get(name)
            .ok_or_else(|| SupervisorError::UnknownService(name.to_string()))?
            .pid;
        // A fresh start carries no backoff debt from the previous life.
        if let Some(svc) = self.services.get_mut(name) {
            svc.restart_attempts = 0;
        }
        if pid.is_none() {
            if let Some(svc) = self.services.get_mut(name) {
                svc.transition(ServiceState::Stopped);
                svc.restart_after_stop = false;
            }
            if let Err(e) = cgroups::kill_cgroup(name) {
                warn!("could not tear down cgroup for {}: {}", name, e);
            }
            return self.start(name).map(|_| ());
        }
        self.request_stop(name)?;
        if let Some(svc) = self.services.get_mut(name) {
            svc.restart_after_stop = true;
        }
        info!("{:<20} will be restarted once it has exited", name);
        Ok(())
    }

    /// Gracefully stop a service: SIGTERM the process group, wait up to
    /// 2s, then force-SIGKILL via the cgroup and remove the cgroup.
    ///
    /// Blocking, so it is **not** used on the control-socket path (see
    /// [`Supervisor::request_stop`] for that): it still exists for
    /// callers that genuinely need the service gone when they return,
    /// such as the unit tests and a synchronous teardown.
    ///
    /// The wait is bounded on *both* ends: after
    /// [`STOP_TERM_TIMEOUT_SECS`] the group is `SIGKILL`ed, and
    /// [`STOP_KILL_GRACE_SECS`] later we stop waiting regardless. A
    /// process wedged in uninterruptible sleep can otherwise keep PID 1
    /// blocked forever.
    pub fn stop_service(&mut self, name: &str) -> SupResult<()> {
        let pid = {
            let Some(svc) = self.services.get_mut(name) else {
                // Teardown tolerance, not sloppiness: `stop_all` skips
                // unknown names the same way, so a shutdown over a
                // partially-discovered graph stops what exists instead
                // of failing on what does not. The operator path
                // (`request_stop`) still returns `UnknownService`.
                return Ok(());
            };
            // Synchronous teardown: the service will be `Stopped` on
            // return whatever happens below, so a refused edge (e.g. an
            // already-`Stopped` service) is not fatal — note it and
            // carry on with the sweep.
            if !svc.transition(ServiceState::Stopping) {
                debug!(
                    "{:<20} already {} — stopping anyway",
                    name,
                    svc.state()
                );
            }
            svc.restart_pending = false;
            svc.restart_after_stop = false;
            svc.pid
        };

        self.disarm_timer(name);

        // 1. Best-effort SIGTERM to the whole process group.
        info!("{:<20} stopping (SIGTERM to process group)", name);
        if let Some(pid) = pid {
            signal_service(pid, Signal::SIGTERM);
        }

        // 2. Wait up to STOP_TERM_TIMEOUT_SECS for it to go away,
        //    escalating to SIGKILL, then STOP_KILL_GRACE_SECS more for
        //    the kernel to finish. The hard deadline is what keeps an
        //    unkillable (D-state) service from blocking us forever.
        let soft = Instant::now() + Duration::from_secs(STOP_TERM_TIMEOUT_SECS);
        let hard = soft + Duration::from_secs(STOP_KILL_GRACE_SECS);
        if let Some(pid) = pid {
            let mut escalated = false;
            while Instant::now() < hard {
                match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::StillAlive) => {
                        if Instant::now() >= soft && !escalated {
                            warn!("{} did not die on SIGTERM — sending SIGKILL", name);
                            signal_service(pid, Signal::SIGKILL);
                            escalated = true;
                        }
                        std::thread::sleep(Duration::from_millis(STOP_POLL_INTERVAL_MS));
                    }
                    Ok(_) => break,              // reaped
                    Err(Errno::ECHILD) => break, // already gone
                    Err(e) => {
                        warn!("waitpid during stop of {}: {}", name, e);
                        break;
                    }
                }
            }
            if escalated && Instant::now() >= hard {
                warn!(
                    "{} survived SIGKILL — leaving it to the root-cgroup sweep",
                    name
                );
            }
            // We reaped it ourselves, so handle_child_exit will not see
            // this pid — drop the mapping here.
            self.forget_child(pid);
        }

        // 3. Clean up the cgroup (kills stragglers, removes the dir).
        if let Err(e) = cgroups::kill_cgroup(name) {
            warn!("could not tear down cgroup for {}: {}", name, e);
        }

        if let Some(svc) = self.services.get_mut(name) {
            svc.transition(ServiceState::Stopped);
            svc.pid = None;
            svc.running_since = None;
        }
        info!("{:<20} stopped", name);
        Ok(())
    }

    /// Stop every service in the given order (caller passes reverse
    /// dependency order).
    ///
    /// Unlike [`Supervisor::stop_service`] — which blocks up to 2s on a
    /// single service — shutdown teardown is *parallel*: every
    /// supervised child receives its SIGTERM up front, and one shared
    /// deadline governs the escalation to SIGKILL. N slow services cost
    /// 2s of wall time total, not N×2s, and nothing is reaped
    /// out-of-order relative to the caller's stop order (order only
    /// decides who is signalled first).
    pub fn stop_all(&mut self, order: &[String]) {
        // ── Phase 1: mark Stopping, disarm timers, SIGTERM everyone ────
        struct Dying {
            name: String,
            pid: Pid,
            escalated: bool,
        }
        let mut dying: Vec<Dying> = Vec::new();

        for name in order {
            let pid = {
                let Some(svc) = self.services.get_mut(name) else {
                    continue;
                };
                // Already-`Stopped` services refuse `Stopping`; they stay
                // `Stopped` and are finalised as such in phase 3. `debug`,
                // not `warn`: hitting this for idle services on every
                // shutdown is normal, not a bug.
                if !svc.transition(ServiceState::Stopping) {
                    debug!("{:<20} already {} at teardown", name, svc.state());
                }
                svc.restart_pending = false;
                // A `RESTART` that was still waiting for its child to die
                // must not resurrect it once teardown is under way.
                svc.restart_after_stop = false;
                svc.pid
            };
            // Scoped above so the shared helper can be used: one disarm
            // site instead of an inline copy per caller.
            self.disarm_timer(name);
            if let Some(pid) = pid {
                info!("{:<20} stopping (SIGTERM to process group)", name);
                signal_service(pid, Signal::SIGTERM);
                dying.push(Dying {
                    name: name.clone(),
                    pid,
                    escalated: false,
                });
            }
        }

        // ── Phase 2: shared-deadline reap loop ─────────────────────────
        // Up to STOP_TERM_TIMEOUT for graceful exit, then one escalation
        // pass to SIGKILL, then STOP_KILL_GRACE for the kernel to reap.
        // The hard deadline guarantees this loop terminates even if a
        // process is stuck in uninterruptible sleep (D state); anything
        // left is finished off by the root-cgroup sweep.
        let soft = Instant::now() + Duration::from_secs(STOP_TERM_TIMEOUT_SECS);
        let hard = soft + Duration::from_secs(STOP_KILL_GRACE_SECS);
        while !dying.is_empty() {
            let expired = Instant::now() >= soft;
            dying.retain_mut(|d| {
                match waitpid(d.pid, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::StillAlive) => {
                        if expired && !d.escalated {
                            warn!("{} did not die on SIGTERM — sending SIGKILL", d.name);
                            signal_service(d.pid, Signal::SIGKILL);
                            d.escalated = true;
                        }
                        true
                    }
                    Ok(_) | Err(Errno::ECHILD) => {
                        // Reaped (or already gone): the mapping is ours
                        // to clear, since handle_child_exit never saw it.
                        self.forget_child(d.pid);
                        false
                    }
                    Err(e) => {
                        warn!("waitpid during shutdown of {}: {}", d.name, e);
                        self.forget_child(d.pid);
                        false
                    }
                }
            });
            if dying.is_empty() || Instant::now() >= hard {
                break;
            }
            std::thread::sleep(Duration::from_millis(STOP_POLL_INTERVAL_MS));
        }

        // ── Phase 3: finalize state and tear down every cgroup ─────────
        for name in order {
            if let Err(e) = cgroups::kill_cgroup(name) {
                warn!("could not tear down cgroup for {}: {}", name, e);
            }
            let pid = self.services.get_mut(name).and_then(|s| {
                s.transition(ServiceState::Stopped);
                s.running_since = None;
                s.pid.take()
            });
            // Anything the shared deadline left behind (or that the
            // kernel reaped behind our back) must not keep a stale
            // pid→service entry alive.
            if let Some(pid) = pid {
                self.forget_child(pid);
            }
            debug!("{:<20} stopped", name);
        }
    }

    /// Close every log-routing pipe.
    ///
    /// Called at the very end of the teardown sequence, after all
    /// services have been stopped and their writers are gone: the
    /// pipes can no longer carry data, and leaving them open would
    /// leak fds for the rest of the shutdown. The event loop drains
    /// any buffered output first, so no log lines are lost.
    pub fn close_log_pipes(&mut self) {
        let names: Vec<String> = self.log_pipes.keys().cloned().collect();
        for name in names {
            if self.log_pipes.remove(&name).is_some() {
                debug!("closed log pipe for {}", name);
            }
        }
    }
}
