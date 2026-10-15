//! Supervisor::restart (split from `supervisor.rs` — verbatim move, no logic changes).
//!
//! Child post-fork paths do only async-signal-safe calls + _exit(127); signals are blocked before signalfd; fds 0/1/2 are claimed; journal/log/connection caps stay bounded.
use super::*;

/// Settle a child exit that arrived after shutdown began: clear any
/// pending restart state and record `Stopped`. Nothing is respawned
/// once teardown began. Free function (not a method) so callers can
/// hold the service borrow while settling without fighting `&mut self`.
fn settle_shutdown_exit(svc: &mut Service, name: &str, detail: &str) {
    info!("{:<20} {} — no restart", name, detail);
    svc.restart_pending = false;
    svc.restart_after_stop = false;
    if !svc.transition(ServiceState::Stopped) {
        warn!("{:<20} refused Stopped (state {})", name, svc.state());
    }
}

/// Complete an operator `STOP`/`RESTART` that the child exit just
/// finished: record `Stopped` and return whether a `RESTART` was
/// waiting (`restart_after_stop`). Free function for the same borrow
/// reason as [`settle_shutdown_exit`].
fn complete_operator_stop(svc: &mut Service, name: &str, detail: &str) -> bool {
    info!("{:<20} {}", name, detail);
    let restart_now = std::mem::take(&mut svc.restart_after_stop);
    if !svc.transition(ServiceState::Stopped) {
        warn!("{:<20} refused Stopped (state {})", name, svc.state());
    }
    restart_now
}

impl super::Supervisor {    /// Handle a reaped child: match the PID to a service and decide
    /// whether the exit was a success, and whether a restart is due.
    ///
    /// Restart policy (see [`RestartPolicy`]):
    /// - `Always` (getty): any exit — even a clean `exit 0` — schedules
    ///   a restart with a minimum delay. Repeated failures within the
    ///   respawn window invoke the crash-loop guard (`record_respawn`),
    ///   which gives up after `RESPAWN_MAX_IN_WINDOW`.
    /// - `OnFailure` (default): non-zero exits and signals schedule an
    ///   exponential-backoff restart; a clean `exit 0` means the script
    ///   daemonized and is not restarted.
    /// - `NoRestart`: the service transitions to `Failed` and stays;
    ///   a *clean* exit 0 means the one-shot finished and is recorded as
    ///   `Stopped` (never `Running` — there is no process left alive).
    ///
    /// Services being stopped (state `Stopping`) are never restarted.
    pub fn handle_child_exit(&mut self, pid: Pid, status: WaitStatus) {
        // Non-terminal reports (stop/continue/ptrace) leave the child
        // alive: keep the pid mapping and the service's pid so the later
        // exit is still attributed correctly. `waitpid` without
        // `WUNTRACED`/`WCONTINUED` never reports these today, but the
        // generic SIGCHLD drain forwards whatever it gets — consuming
        // the mapping here would orphan the live child and mis-attribute
        // a recycled pid later.
        if !matches!(
            status,
            WaitStatus::Exited(_, _) | WaitStatus::Signaled(_, _, _)
        ) {
            match self.pid_to_service.get(&pid) {
                Some(name) => debug!(
                    "child {} of {} reported {:?} — still alive, ignoring",
                    pid, name, status
                ),
                None => debug!("reaped unknown child {} — ignoring", pid),
            }
            return;
        }

        let Some(name) = self.pid_to_service.remove(&pid) else {
            debug!("reaped unknown child {} — ignoring", pid);
            return;
        };

        // Set when the operator asked for a `RESTART` and the child we
        // were waiting for has just been reaped. Applied after the
        // mutable borrow of the service below has ended.
        let mut restart_now = false;

        let svc = match self.services.get_mut(&name) {
            Some(s) => s,
            None => {
                warn!("reaped child {} for unknown service {}", pid, name);
                return;
            }
        };
        svc.pid = None;

        let policy = svc.restart_policy;

        match status {
            WaitStatus::Exited(_, 0) => {
                // Shutdown wins over everything: nothing is respawned
                // once teardown began. Checked first in every arm so
                // the Stopping-vs-Shutdown ordering cannot drift apart
                // again (one arm used to check them in the other order).
                if self.system_state == SystemState::ShuttingDown {
                    settle_shutdown_exit(svc, &name, "exited during shutdown");
                    return;
                }
                if svc.state() == ServiceState::Stopping {
                    // An operator `STOP`/`RESTART` won the race with a
                    // script that finished successfully on its way out:
                    // the service is stopping, so it must not be
                    // reported as a live, daemonised Running service.
                    restart_now =
                        complete_operator_stop(svc, &name, "stopped (exit 0 while stopping)");
                } else if policy == RestartPolicy::NoRestart {
                    // A one-shot that exits 0 has *finished* — it did not
                    // daemonize and it must not be restarted. Marking it
                    // `Running` (as the branch below does for an
                    // `OnFailure` script that forked a daemon) would
                    // report a dead process as healthy forever.
                    if !svc.transition(ServiceState::Stopped) {
                        warn!("{:<20} refused Stopped (state {})", name, svc.state());
                    }
                    info!("{:<20} exited 0 — one-shot complete (no-restart)", name);
                    return;
                } else if policy == RestartPolicy::Always {
                    // getty exits when the console session ends; a new
                    // login prompt must appear. The crash-loop guard caps
                    // how often we will retry so a missing binary cannot
                    // spin the scheduler.
                    info!("{:<20} exited 0 — restart (policy=always)", name);
                    if !svc.transition(ServiceState::Starting) {
                        warn!("{:<20} refused Starting (state {})", name, svc.state());
                    }
                    if self.is_respawn_service(&name) && !self.record_respawn(&name) {
                        return;
                    }
                    if let Err(e) = self.schedule_restart_delay(&name, RESPAWN_MIN_DELAY_MS) {
                        error!("{}: could not schedule a respawn: {}", name, e);
                    }
                    return;
                } else {
                    // Clean exit 0: a SysV script that forked its daemon
                    // and returned. The child is gone; the service
                    // itself is up.
                    //
                    // `Stopped -> Running` is refused (a service is never
                    // running without a spawn first): in production the
                    // service is `Starting` here, so the refusal only
                    // fires for a bookkeeping bug — and then the uptime
                    // stamp and the "Running" line must not be written
                    // either, or status would advertise a healthy service
                    // that the state machine says is stopped.
                    if svc.transition(ServiceState::Running) {
                        svc.running_since = Some(Instant::now());
                        info!(
                            "{:<20} Running (daemonized; pid {} exited 0)",
                            name, pid
                        );
                    } else {
                        warn!("{:<20} refused Running (state {})", name, svc.state());
                    }
                }
            }
            WaitStatus::Exited(_, code) => {
                if self.system_state == SystemState::ShuttingDown {
                    // A failure that races the teardown must not leave
                    // the service in `Starting` with no restart coming:
                    // nothing will be respawned once shutdown began.
                    settle_shutdown_exit(
                        svc,
                        &name,
                        &format!("exited during shutdown (code {})", code),
                    );
                } else if svc.state() == ServiceState::Stopping {
                    restart_now = complete_operator_stop(
                        svc,
                        &name,
                        &format!("stopped (exit {})", code),
                    );
                } else {
                    error!("{} exited unexpectedly with code {}", name, code);
                    if !svc.transition(ServiceState::Starting) {
                        warn!("{:<20} refused Starting (state {})", name, svc.state());
                    }
                    self.restart_after_failure(&name, policy);
                }
            }
            WaitStatus::Signaled(_, sig, _) => {
                if self.system_state == SystemState::ShuttingDown {
                    settle_shutdown_exit(
                        svc,
                        &name,
                        &format!("killed during shutdown (signal {})", sig),
                    );
                } else if svc.state() == ServiceState::Stopping {
                    restart_now = complete_operator_stop(
                        svc,
                        &name,
                        &format!("stopped (killed by {})", sig),
                    );
                } else {
                    error!("{} was killed by signal {}", name, sig);
                    if !svc.transition(ServiceState::Starting) {
                        warn!("{:<20} refused Starting (state {})", name, svc.state());
                    }
                    self.restart_after_failure(&name, policy);
                }
            }
            _ => {
                // Unreachable: non-terminal reports return early above
                // so the pid mapping and `svc.pid` survive a stop/continue.
                debug!("child {} of {} reported {:?}", pid, name, status);
            }
        }

        // A `RESTART` that was waiting for this child to go away.
        if restart_now {
            if self.system_state == SystemState::ShuttingDown {
                info!("{:<20} restart abandoned (system shutting down)", name);
            } else {
                match self.start(&name) {
                    Ok(_) => info!("{:<20} restarted after stop", name),
                    Err(e) => error!("could not restart {} after stop: {}", name, e),
                }
            }
        }
    }

    /// Decide whether a failed service should be restarted, per policy:
    /// `Always` → immediately (no backoff); `OnFailure` → exponential
    /// backoff; `NoRestart` → nothing.
    pub(crate) fn restart_after_failure(&mut self, name: &str, policy: RestartPolicy) {
        // The system is being torn down: nobody gets a restart.
        if self.system_state == SystemState::ShuttingDown {
            info!("{:<20} not restarting (system shutting down)", name);
            return;
        }
        match policy {
            RestartPolicy::Always => {
                // Crash-loop guard: feed the respawn counter and stop once
                // the service dies too often in the window. `record_respawn`
                // marks the service `Failed` and bails when the limit is hit.
                if !self.record_respawn(name) {
                    return;
                }
                if let Err(e) = self.schedule_restart_delay(name, RESPAWN_MIN_DELAY_MS) {
                    error!("{}: could not schedule a restart: {}", name, e);
                }
            }
            RestartPolicy::OnFailure => {
                // The error must not be swallowed: a service whose
                // timerfd could not be created is now in `Starting`
                // with no child and no pending restart, i.e. abandoned.
                if let Err(e) = self.schedule_restart(name) {
                    error!("{}: could not schedule a restart: {}", name, e);
                }
            }
            RestartPolicy::NoRestart => {
                info!("{:<20} not restarting (policy=no-restart)", name);
                // A NoRestart service that failed must be reported as
                // `Failed`, not left in `Starting` with no child and no
                // timer — otherwise `velocityctl status` shows it as
                // "starting" forever.
                if let Some(svc) = self.services.get_mut(name) {
                    if !svc.transition(ServiceState::Failed) {
                        warn!("{:<20} refused Failed (state {})", name, svc.state());
                    }
                }
            }
        }
    }

    // ─────────────────────── log routing ─────────────────────────

    /// Schedule (or reschedule) the exponential-backoff restart for a
    /// failed service by arming its per-service `timerfd`.
    pub fn schedule_restart(&mut self, name: &str) -> SupResult<()> {
        let attempts = self.bump_restart_attempts(name)?;
        let delay_ms = backoff_delay_ms(attempts);
        self.arm_restart_timer(name, delay_ms)
    }

    /// Schedule a restart with an explicit delay in milliseconds
    /// (`0` = immediately, used by `RestartPolicy::Always`).
    pub(crate) fn schedule_restart_delay(&mut self, name: &str, delay_ms: u64) -> SupResult<()> {
        self.bump_restart_attempts(name)?;
        self.arm_restart_timer(name, delay_ms)
    }

    /// Increment a service's restart counter and return the new count.
    /// Split out so `schedule_restart` and `schedule_restart_delay`
    /// share one increment site instead of open-coding it twice.
    fn bump_restart_attempts(&mut self, name: &str) -> SupResult<u32> {
        let svc = self
            .services
            .get_mut(name)
            .ok_or_else(|| SupervisorError::UnknownService(name.to_string()))?;
        svc.restart_attempts = svc.restart_attempts.saturating_add(1);
        Ok(svc.restart_attempts)
    }

    /// Arm (or re-arm) a service's restart timer with `delay_ms`.
    pub(crate) fn arm_restart_timer(&mut self, name: &str, delay_ms: u64) -> SupResult<()> {
        self.arm_timer(name, delay_ms, TimerPurpose::Restart)?;
        if let Some(svc) = self.services.get_mut(name) {
            svc.restart_pending = true;
        }
        let attempts = self
            .services
            .get(name)
            .map(|s| s.restart_attempts)
            .unwrap_or(0);
        if delay_ms == 0 {
            warn!(
                "{:<20} scheduled immediate restart (attempt {})",
                name, attempts
            );
        } else if delay_ms < 1000 {
            warn!(
                "{:<20} scheduled restart in {}ms (attempt {})",
                name, delay_ms, attempts
            );
        } else {
            warn!(
                "{:<20} scheduled restart in {}s (attempt {})",
                name, delay_ms / 1000,
                attempts
            );
        }
        Ok(())
    }

    /// Arm (or re-arm) the per-service `timerfd` with `delay_ms`.
    ///
    /// One fd per service serves both purposes the timer has: firing
    /// the backoff restart, and escalating a stop that outlived its
    /// grace period (see [`Supervisor::on_timer_fire`]). They can never
    /// be armed at the same time — a stop disarms the pending restart,
    /// and a restart only ever runs for a service that is not stopping.
    /// `purpose` is recorded here and honoured when the timer fires, so
    /// the decision never depends on state that may have moved on since.
    pub(crate) fn arm_timer(&mut self, name: &str, delay_ms: u64, purpose: TimerPurpose) -> SupResult<()> {
        // Create the per-service timer lazily and keep it forever: the
        // event loop registered its fd number in epoll. No `expect` on
        // the syscall: fd exhaustion or a broken kernel must degrade to
        // a failed restart, never abort PID 1.
        if !self.timers.contains_key(name) {
            let fd = TimerFd::new(
                ClockId::CLOCK_MONOTONIC,
                TimerFlags::TFD_NONBLOCK | TimerFlags::TFD_CLOEXEC,
            )
            .map_err(|e| SupervisorError::Timer {
                service: name.to_string(),
                source: e,
            })?;
            self.timers.insert(
                name.to_string(),
                SvcTimer { fd, purpose: None },
            );
        }
        let timer = self
            .timers
            .get_mut(name)
            .expect("timer was just inserted or already existed");
        let _ = disarm(&timer.fd);

        // A `timerfd` armed with a zero it_value is *disarmed*, so an
        // explicit 0 delay is expressed as a 1ns expiration. Delays keep
        // millisecond precision via `TimeSpec::milliseconds`: the old
        // `seconds(delay / 1000)` split silently truncated e.g. 1500ms
        // to 1s. Absurd delays are clamped to a day rather than
        // panicking PID 1 (`TimeSpec::milliseconds` asserts on
        // out-of-bounds input, and AGENTS.md forbids panics in PID 1
        // paths): no caller arms beyond 30s today.
        const MAX_TIMER_DELAY_MS: u64 = 86_400_000; // 24h, far above the 30s cap
        let safe_ms = delay_ms.min(MAX_TIMER_DELAY_MS);
        let spec = if safe_ms == 0 {
            TimeSpec::nanoseconds(1)
        } else {
            // `safe_ms <= 86_400_000` fits in `i64` by construction, so
            // the `as` cast cannot wrap and `milliseconds` cannot panic.
            TimeSpec::milliseconds(safe_ms as i64)
        };
        timer
            .fd
            .set(Expiration::OneShot(spec), TimerSetTimeFlags::empty())
            .map_err(|e| SupervisorError::Timer {
                service: name.to_string(),
                source: e,
            })?;
        // Record what this timer is for *after* it is armed, so the
        // entry can never claim a purpose for a timer that failed to arm.
        timer.purpose = Some(purpose);
        Ok(())
    }

    /// Called when a service's timer fires. What the timer means is
    /// decided by the purpose recorded when it was *armed*, never by the
    /// service's current state — a child that exits promptly after a
    /// `STOP` reaches `Stopped` before the escalation fires, and
    /// guessing from the state there would respawn the very service the
    /// operator just stopped.
    pub fn on_timer_fire(&mut self, name: &str) -> SupResult<()> {
        if let Some(timer) = self.timers.get(name) {
            if let Err(e) = disarm(&timer.fd) {
                warn!("could not disarm {} timer: {}", name, e);
            }
        }
        // Take the purpose, leaving the entry disarmed and idle. A timer
        // with no recorded purpose is treated as a restart (defensive:
        // arming always records one, so this only fires for a timer
        // whose bookkeeping was lost — and restarting is the safe side
        // only when `restart_pending` agrees, checked below).
        let purpose = self
            .timers
            .get_mut(name)
            .and_then(|t| t.purpose.take())
            .unwrap_or(TimerPurpose::Restart);
        // Captured before the flag is cleared below: the stale-timer
        // check needs to know whether a restart was actually pending.
        let was_pending = self
            .services
            .get(name)
            .is_some_and(|s| s.restart_pending);
        if let Some(svc) = self.services.get_mut(name) {
            svc.restart_pending = false;
        }

        // A stop that ignores SIGTERM must still die: escalate.
        let pid = self.services.get(name).and_then(|s| s.pid);
        if purpose == TimerPurpose::StopEscalation {
            if let Some(pid) = pid {
                warn!(
                    "{:<20} did not exit on SIGTERM — sending SIGKILL",
                    name
                );
                signal_service(pid, Signal::SIGKILL);
                // Kill the service cgroup too: a grandchild that escaped
                // the process group is still in there.
                if let Err(e) = cgroups::kill_cgroup(name) {
                    warn!("could not tear down cgroup for {}: {}", name, e);
                }
            } else {
                // The child already exited on SIGTERM, so this is a
                // stale escalation: settle the state, and in particular
                // do *not* fall through to the respawn below.
                if let Some(svc) = self.services.get_mut(name) {
                    if svc.state() == ServiceState::Stopping
                        && !svc.transition(ServiceState::Stopped)
                    {
                        warn!("{:<20} refused Stopped (state {})", name, svc.state());
                    }
                }
            }
            return Ok(());
        }

        // A restart timer that fires while an operator stop is in flight
        // belongs to the stop, not to the restart: `request_stop` arms
        // its own escalation, so the pending respawn is dropped here.
        if self
            .services
            .get(name)
            .is_some_and(|s| s.state() == ServiceState::Stopping)
        {
            info!("{:<20} pending restart cancelled by a stop in progress", name);
            return Ok(());
        }

        // Second line of defence for the same hazard, for the case
        // where the timer survived its cancellation: every path that
        // *arms* a restart sets `restart_pending`, and every path that
        // cancels one clears it (`request_stop`, shutdown, an operator
        // start). A `Restart`-purpose timer for a service that is not
        // awaiting a restart is stale by definition — firing it would
        // resurrect a service the operator just stopped.
        if !was_pending {
            info!("{:<20} stale restart timer ignored (no restart pending)", name);
            return Ok(());
        }

        // A timer that fires *after* shutdown began (armed moments
        // before SIGTERM arrived) must not respawn anything.
        if self.system_state == SystemState::ShuttingDown {
            info!("{:<20} restart timer ignored (system shutting down)", name);
            return Ok(());
        }
        info!("{:<20} restarting (backoff elapsed)", name);
        let pid = match self.start(name) {
            Ok(pid) => pid,
            Err(e) => {
                // `start` fails for reasons that have nothing to do
                // with the service — EMFILE on the log pipe, EAGAIN
                // from `fork`, a timerfd we cannot create. Returning
                // the error made that the *last* thing that ever
                // happened to the service: no child, no timer, no
                // pending flag, nothing to revisit it. It then sat in
                // `Starting` forever with nobody supervising it, until
                // an operator noticed or the machine rebooted. Arm
                // another attempt instead, so a transient failure
                // degrades to "slower", never to "abandoned".
                error!("{:<20} restart failed ({}); retrying", name, e);
                let _ = self.schedule_restart(name);
                return Ok(());
            }
        };

        // The respawned child is live but may never announce
        // `READY=1`: the common LSB script execs a foreground daemon
        // and says nothing. Without this the service sat in `Starting`
        // forever with a live child, so `velocityctl status` reported
        // a healthy login prompt as `starting` and — because
        // `check_backoff_reset` filters on `Running` — the crash-loop
        // counters could never be cleared again.
        //
        // This must not be conditional on the notify socket: `start`
        // creates one for *every* service (that is how it knows where
        // to send `NOTIFY_SOCKET`), so its presence says nothing about
        // whether this service will ever use it. A later `READY=1` is
        // still honoured: `handle_notify_datagram` only promotes a
        // service that is still `Starting`.
        self.mark_running(name, Some(pid));
        Ok(())
    }

    /// Reset the restart counter for any *Running* service that has been
    /// stable for `STABLE_RUNNING_SECS`. Called once per event-loop tick.
    ///
    /// Stability also restores the crash-loop budget: a service that
    /// stayed up long enough has obviously recovered, so its respawn
    /// window starts empty again. Without that, a service that crashed
    /// into the guard once could never be restarted by the operator
    /// without a reboot.
    pub fn check_backoff_reset(&mut self, now: Instant) {
        let stable_for = Duration::from_secs(STABLE_RUNNING_SECS);
        let recovered: Vec<String> = self
            .services
            .values()
            .filter(|svc| {
                svc.state() == ServiceState::Running
                    && svc.restart_attempts > 0
                    && svc.running_since.is_some_and(|since| {
                        // Saturating: a clock oddity must never panic PID 1.
                        now.saturating_duration_since(since) >= stable_for
                    })
            })
            .map(|svc| svc.name.clone())
            .collect();

        for name in recovered {
            debug!("{} has been stable — resetting backoff", name);
            if let Some(svc) = self.services.get_mut(&name) {
                svc.restart_attempts = 0;
                // `running_since` is deliberately left alone: it is the
                // uptime stamp, and clearing it here would both lose the
                // uptime and stop this service from ever being seen as
                // stable again.
            }
            self.respawn_history.remove(&name);
        }
    }
}
