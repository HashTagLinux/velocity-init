//! EventLoop::boot (split from `event_loop.rs` — verbatim move, no logic changes).
//!
//! Child post-fork paths do only async-signal-safe calls + _exit(127); signals are blocked before signalfd; fds 0/1/2 are claimed; journal/log/connection caps stay bounded.
use super::*;

/// How many services of one boot level start at once: the whole level,
/// unless `--jobs N` caps the concurrency. Never zero — `chunks(0)`
/// panics, and a panic in PID 1 is a dead machine. The empty-level
/// `continue` in [`EventLoop::boot`] already keeps this away from zero;
/// the `.max(1)` is belt and braces so no future refactor can re-learn
/// that the hard way.
pub(crate) fn boot_batch_size(level_len: usize, jobs: usize) -> usize {
    if jobs > 0 {
        jobs
    } else {
        level_len.max(1)
    }
}

impl super::EventLoop {
    /// Boot all services level by level (dependencies first).
    ///
    /// All services within a level are spawned immediately (parallel
    /// startup) — unless `--jobs N` caps the concurrency, in which case
    /// the level is started in batches of N. We then wait for each to
    /// become *ready* — READY=1 on its notify socket, a clean
    /// daemonising exit, or the boot timeout — before the next level
    /// begins. Every level's duration and the total boot time are
    /// journaled, so the boot is measurable (not just "fast").
    ///
    /// Note: during boot the main epoll loop is not running yet. Children
    /// that exit while we are blocked in `wait_ready` are reaped when the
    /// main loop starts (briefly as zombies), and restart timers armed by
    /// failures during boot fire only then. This mirrors sysvinit's
    /// sequential `/etc/init.d/rc` boot and is harmless for a few seconds
    /// of startup.
    pub(crate) fn boot(&mut self, levels: &[Vec<String>]) {
        self.boot_started_at = Some(std::time::Instant::now());
        let total: usize = levels.iter().map(|l| l.len()).sum();
        for (i, level) in levels.iter().enumerate() {
            // A shutdown signalled mid-boot must not start *more*
            // services: whatever is already up is stopped by the
            // teardown sequence.
            if self.service_boot_signals() {
                warn!(
                    "boot: aborting at level {}/{} — shutdown requested",
                    i + 1,
                    levels.len()
                );
                break;
            }
            if level.is_empty() {
                continue;
            }
            let level_t0 = std::time::Instant::now();
            // Spawn in batches of `jobs` (or the whole level at once).
            for chunk in level.chunks(boot_batch_size(level.len(), self.jobs)) {
                let mut batch: Vec<(String, Pid)> = Vec::new();
                for name in chunk {
                    match self.supervisor.start(name) {
                        Ok(pid) => batch.push((name.clone(), pid)),
                        Err(e) => error!("failed to start {}: {}", name, e),
                    }
                }
                // Wait for this batch to be ready before starting the
                // next one, so `--jobs N` really bounds concurrency.
                for (name, pid) in &batch {
                    self.wait_ready(name, *pid);
                }
            }
            info!(
                "boot: level {}/{} ready ({} services) in {:?}",
                i + 1,
                levels.len(),
                level.len(),
                level_t0.elapsed()
            );
        }
        if let Some(t0) = self.boot_started_at {
            info!(
                "boot: {} services across {} levels complete in {:?}",
                total,
                levels.len(),
                t0.elapsed()
            );
        }
    }

    /// Boot the machine the sysvinit way (only when `/etc/inittab`
    /// exists and we are in sysv mode):
    ///
    /// 1. run every `sysinit` line (e.g. `/etc/init.d/rcS`) and wait;
    /// 2. start the `boot:` lines without waiting (inittab(5): "don't wait
    ///    for completion before continuing") — waiting for one that runs a
    ///    long-lived process would stall the boot for SYSV_EXIT_TIMEOUT;
    /// 3. run the `bootwait:` lines and wait;
    /// 4. run the `wait` lines for the boot runlevel (e.g. `/etc/init.d/rc 2`)
    ///    and wait;
    /// 5. start the `once` lines without waiting;
    /// 6. start the `respawn` lines (getty) — these stay supervised.
    ///
    /// The `boot@N`/`wait@N`/`once@N` services are registered on first
    /// use so the supervisor tracks their startup (and logs their
    /// output) exactly like any other service.
    pub(crate) fn sysv_boot(&mut self) {
        self.boot_started_at = Some(std::time::Instant::now());
        let (names, async_boot): (Vec<String>, std::collections::HashSet<String>) = {
            let ctx = self.sysv.as_ref().expect("sysv mode wraps sysv_boot");
            // boot lines, then wait lines, in order
            let names = ctx
                .boot_names
                .iter()
                .chain(ctx.wait_names.iter())
                .cloned()
                .collect();
            let async_boot = ctx.boot_async_names.iter().cloned().collect();
            (names, async_boot)
        };

        // Ensure one-shot boot/wait services exist.
        self.ensure_sysv_services(&names, crate::models::RestartPolicy::NoRestart);

        for name in &names {
            if self.service_boot_signals() {
                warn!("sysv boot: aborting at {name} — shutdown requested");
                break;
            }
            info!("sysv boot: running {name}");
            match self.supervisor.start(name) {
                Ok(pid) => {
                    // `boot:` lines are fire-and-forget: spawn and move on.
                    // Everything else (sysinit/bootwait/wait) is a one-shot
                    // that must finish before the next line starts.
                    if !async_boot.contains(name) {
                        self.wait_for_exit(name, pid);
                    }
                }
                Err(e) => error!("sysv boot: failed to start {name}: {e}"),
            }
        }
        if self.pending_shutdown.is_some() {
            warn!("sysv boot: skipped the remaining boot lines — shutdown requested");
        }

        // A shutdown requested while the wait lines ran outranks
        // starting anything else: whatever is already up is stopped by
        // the teardown, and a getty spawned on the way down would only
        // have to be killed again.
        if self.pending_shutdown.is_some() {
            warn!("sysv boot: skipping once/respawn lines — shutdown requested");
        } else {
            // once lines: fire and forget.
            let once: Vec<String> = self
                .sysv
                .as_ref()
                .map(|c| c.once_names.clone())
                .unwrap_or_default();
            self.ensure_sysv_services(&once, crate::models::RestartPolicy::NoRestart);
            for name in once {
                if let Err(e) = self.supervisor.start(&name) {
                    error!("sysv boot: failed to start once-line {name}: {e}");
                }
            }

            // respawn lines (getty): fully supervised.
            let respawn: Vec<String> = self
                .sysv
                .as_ref()
                .map(|c| c.respawn_names.clone())
                .unwrap_or_default();
            self.ensure_sysv_services(&respawn, crate::models::RestartPolicy::Always);
            for name in respawn {
                if let Err(e) = self.supervisor.start(&name) {
                    error!("sysv boot: failed to start respawn {name}: {e}");
                }
            }
        }
        info!(
            "sysv boot complete (runlevel {}) in {:?}",
            self.runlevel().unwrap_or("?"),
            self.boot_started_at
                .map(|t| t.elapsed())
                .unwrap_or_default()
        );

        // The boot scripts we just ran may have deleted our socket out
        // from under us (Devuan's rcS runs mountall-bootclean.sh, which
        // wipes /tmp and /run). Re-establish it before operators can
        // notice: velocityctl/telinit/runlevel all address it by path.
        self.reestablish_runtime_files();

        // And for utmp: `rmnologin`/`utmp` handling in a real rcS can
        // replace /var/run/utmp wholesale, which drops the RUN_LVL
        // record this process wrote before the boot — so `runlevel` and
        // `who -r` go blank. Re-publish the level we booted into.
        if let Some(ctx) = self.sysv.as_ref() {
            match ctx.level.as_bytes().first().copied() {
                Some(ch) => crate::utmp::write_runlevel_record(Some(b'N'), ch),
                None => warn!("runlevel {:?} has no level byte", ctx.level),
            }
        }
    }

    /// Make sure the control socket and the journal still exist at
    /// their paths.
    ///
    /// Init publishes both *before* running the distribution's boot
    /// scripts, and those scripts are entitled to clean `/run`: Devuan's
    /// rcS runs `mountall-bootclean.sh`, which wipes `/tmp` and `/run`
    /// (and so the socket, the journal and utmp). A listening socket
    /// whose path is gone keeps accepting connections nobody can
    /// *name*, so `velocityctl`, `telinit N` and `runlevel` are dead for
    /// the rest of the session with nothing logged anywhere.
    ///
    /// Called after the sysv boot, after every runlevel switch (`rc N`
    /// is just another init script and may clean `/run` too), and once
    /// per event-loop tick — the same thing can happen at any time in
    /// LSB mode, where a service may `rm -rf /run` or run
    /// `tmpfiles --clean`, and nothing re-established them there. Both
    /// checks are a `stat` when everything is in place.
    pub(crate) fn reestablish_runtime_files(&mut self) {
        let before = self.control.listener_fd();
        match self.control.ensure_listening() {
            Err(e) => error!(
                "control socket at {} could not be (re)bound: {}",
                self.control.path().display(),
                e
            ),
            Ok(()) => {
                // A rebind closes the old fd and installs a new one, and
                // epoll is still watching the old number — re-arm, or
                // clients connect and are never answered. Comparing the
                // fd keeps us from re-adding an already-registered one
                // (which would blacklist it as a duplicate).
                if let Some(lfd) = self.control.listener_fd() {
                    if before != Some(lfd) {
                        self.epoll_add(lfd, "control listener");
                    }
                }
            }
        }

        // Same story for the journal: the segment we opened before the
        // boot was unlinked by it, and our handle now writes to an
        // inode nobody can open. Re-open, or `velocityctl journal`
        // reports nothing for the rest of the session. The check looks
        // for the *active segment* (`<base>.0`): the base path itself is
        // never a file in the segmented layout, so probing it would
        // re-open every tick and warn every tick.
        if let Some((logger, path)) = self.journal.clone() {
            let (seg0, _) = crate::journal::seg_paths(&path);
            if seg0.exists() {
                return;
            }
            if let Err(e) = logger.reopen_journal(&path.to_string_lossy()) {
                error!("journal at {} could not be re-created: {}", path.display(), e);
            } else if !seg0.exists() {
                warn!("journal {} is still absent after re-open", path.display());
            }
        }
    }

    /// Register sysv-line services (boot/wait/once/respawn) the sysv
    /// planner has not yet created. The scripts live in the inittab
    /// itself (`/bin/sh -c <process>`), so they are created as built-in
    /// services with an explicit command.
    pub(crate) fn ensure_sysv_services(&mut self, names: &[String], policy: crate::models::RestartPolicy) {
        if self.sysv.is_none() {
            return;
        }
        for name in names {
            if self.supervisor.service(name).is_some() {
                continue;
            }
            // Borrowed per name, never a clone of the whole context:
            // this runs once per boot line and per runlevel switch, and
            // the context holds every inittab set. (A borrow across the
            // `register` call below would fight `&mut self`, which is
            // why the lookup resolves to an owned command first.)
            let cmd = match self.sysv.as_ref().and_then(|ctx| ctx.name_command(name)) {
                Some(c) => c,
                None => {
                    warn!("sysv: no inittab command for {name}");
                    continue;
                }
            };
            let header = crate::models::LsbHeader {
                short_description: Some(format!("sysv line {name}")),
                ..Default::default()
            };
            let svc = crate::models::Service::builtin(name.clone(), cmd, policy, header);
            self.supervisor.register(svc);
            debug!("registered sysv service {name}");
        }
    }

    /// Kill a boot script we are abandoning (a shutdown arrived while it
    /// was still running) and reap it.
    ///
    /// The kill goes through the same event-driven wait as every other
    /// boot wait — the real `WaitStatus` still reaches the supervisor via
    /// the SIGCHLD path, never invented here, because
    /// `handle_child_exit` makes restart and state decisions from it and
    /// a fabricated "killed by SIGKILL" would look exactly like a crash
    /// and schedule a backoff restart on the way down. If it cannot be
    /// reaped within [`KILL_REAP_TIMEOUT`], the
    /// wait gives up but the pid mapping stays with the supervisor — the
    /// pid cannot be recycled while its process lives, and the
    /// shutdown's stop/cgroup/orphan sweeps still know it is ours.
    pub(crate) fn kill_and_reap(&mut self, name: &str, pid: Pid) {
        signal_service(pid, Signal::SIGKILL);
        let _ = self.wait_event_driven(
            name,
            WaitGoal::Reap {
                pid,
                deadline: Instant::now() + KILL_REAP_TIMEOUT,
            },
        );
    }

    /// Is a signal waiting on the signalfd right now?
    ///
    /// A zero-timeout `poll`: it reports the *count* of ready fds, so a
    /// quiet `Ok(0)` (nothing waiting) must not read as pending — only
    /// a positive count with `POLLIN` asserted does.
    pub(crate) fn signal_pending(&self) -> bool {
        let mut fds = [PollFd::new(self.signal_fd.as_fd(), PollFlags::POLLIN)];
        matches!(poll(&mut fds, PollTimeout::ZERO), Ok(n) if n > 0)
            && fds[0]
                .revents()
                .is_some_and(|r| r.contains(PollFlags::POLLIN))
    }

    /// Service pending signals *during* boot, and report whether a
    /// shutdown has been requested.
    ///
    /// Boot happens before the epoll loop exists, and the managed
    /// signals are blocked and delivered through the signalfd — so
    /// nothing was reading it while we waited on boot scripts. A
    /// `SIGTERM` arriving one second into a five-minute `rcS` was
    /// therefore not acted on until that script finished (or
    /// `SYSV_EXIT_TIMEOUT` expired): `shutdown` during a slow boot
    /// looked like init had wedged, and the machine sat there.
    ///
    /// Polling the signalfd from the wait loops closes that window:
    /// signals are handled within one poll interval, and a requested
    /// shutdown aborts the rest of the boot.
    pub(crate) fn service_boot_signals(&mut self) -> bool {
        while self.signal_pending() {
            if let Err(e) = self.handle_signals() {
                warn!("handling signals during boot failed: {}", e);
                break;
            }
        }
        self.pending_shutdown.is_some()
    }

    /// Wait for a one-shot boot script to run to completion, reaping
    /// its exit status through the supervisor (a boot-time wait line
    /// that ends is "done"; its exit does not trigger a restart for a
    /// `NoRestart` service).
    pub(crate) fn wait_for_exit(&mut self, name: &str, pid: Pid) {
        let goal = WaitGoal::Exit {
            pid,
            deadline: Instant::now() + SYSV_EXIT_TIMEOUT,
        };
        if let WaitOutcome::Shutdown = self.wait_event_driven(name, goal) {
            warn!(
                "sysv {name}: aborting the wait — a shutdown was requested while the \
                 script was still running"
            );
            self.kill_and_reap(name, pid);
        }
    }

    /// Wait for a one-shot process exit, used both during sysv boot and
    /// runlevel switching.
    ///
    /// "Ready" means: `READY=1` arrived on its notify socket, it exited
    /// `0` (daemonised), or `BOOT_WAIT_TIMEOUT` elapsed while it stayed
    /// alive (foreground service). While waiting we poll the notify
    /// socket ourselves, since the main epoll loop does not run yet.
    ///
    /// A `RestartPolicy::Always` service (e.g. the console getty) is
    /// long-lived by design and never notifies readiness: rather than
    /// stalling boot for the full `BOOT_WAIT_TIMEOUT` on each of them, we
    /// give a short grace period for a late `READY=1`, and if the child
    /// is still alive and silent treat it as running immediately.
    pub(crate) fn wait_ready(&mut self, name: &str, pid: Pid) {
        let always = self.is_always(name);
        let goal = WaitGoal::ReadyOrExit {
            pid,
            deadline: Instant::now() + BOOT_WAIT_TIMEOUT,
            // `Some` ⇒ treat alive-and-silent as running after the grace
            // instead of holding the boot for the full timeout.
            grace: always.then(|| Instant::now() + BOOT_READY_GRACE),
        };
        if let WaitOutcome::Shutdown = self.wait_event_driven(name, goal) {
            warn!("boot: aborting the wait for {name} — shutdown requested");
        }
    }

    /// Is `name` a `RestartPolicy::Always` service (getty), i.e. one
    /// that is long-lived by design and will never announce readiness?
    pub(crate) fn is_always(&self, name: &str) -> bool {
        self.supervisor
            .service(name)
            .is_some_and(|s| s.restart_policy == RestartPolicy::Always)
    }

    /// The one boot wait. Every boot sequence — LSB levels, sysv `rcS`,
    /// a runlevel switch's `rc N`, and reaping an abandoned script —
    /// blocks here, on the *main* `epoll` set rather than on its own
    /// `sleep`/`poll`/`waitpid` loop.
    ///
    /// They used to be three separate poll loops (`wait_ready`,
    /// `wait_for_exit`, `kill_and_reap`), each independently
    /// re-implementing the same three duties: service the signalfd,
    /// drain the child's log pipe, and probe `waitpid`. That
    /// triplication is how they drifted apart — the fix for "a chatty
    /// boot script wedges in `write(2)` on its 64 KiB log pipe" had to
    /// be remembered three times, and it was.
    ///
    /// Waiting on `epoll` instead of `poll`+`sleep` fixes the other half:
    /// the signalfd is a *real* fd in the set, so a `SIGTERM` arriving
    /// one second into a five-minute `rcS` is seen on the next tick
    /// instead of when the script happened to finish.
    ///
    /// `goal` says what "done" means and what to do with the supervisor
    /// when it is; everything about *how* to wait is here.
    pub(crate) fn wait_event_driven(&mut self, name: &str, goal: WaitGoal) -> WaitOutcome {
        loop {
            // A shutdown outranks finishing the wait. Checked before the
            // epoll call so an already-pending request costs no tick.
            if self.service_boot_signals() {
                return WaitOutcome::Shutdown;
            }
            self.epoll_wait_for_boot();
            // Belt and braces: a log pipe whose `epoll_ctl(ADD)` failed
            // (or was refused) is still drained here, so it can never be
            // the reason a child blocks in `write(2)` — and that holds
            // for *every* service, not just the one this wait is about:
            // during a level boot several services run at once, and a
            // wedged pipe on any of them stalls its child the same way.
            // (Registered pipes are already drained through `epoll` by
            // `epoll_wait_for_boot` above; this pass only costs the
            // unregistered ones an extra non-blocking read per tick.)
            let names: Vec<String> =
                self.supervisor.services().map(|s| s.name.clone()).collect();
            for n in &names {
                self.drain_service_log(n);
            }
            if goal.poll(self, name) {
                return WaitOutcome::Done;
            }
        }
    }

    /// One bounded `epoll_wait` on the main event set, dispatching only
    /// the fds a boot needs.
    ///
    /// Deliberately narrower than [`EventLoop::dispatch`]: the operator
    /// control socket and the restart timers stay unserved until the
    /// boot is over, exactly as before. Serving `telinit 3` mid-`rcS`
    /// would run a second set of init scripts on top of the boot already
    /// running, and firing a backoff timer here would restart a service
    /// whose failure the main loop is about to process anyway.
    ///
    /// A failing `epoll_wait` cannot take the boot down: the wait's own
    /// deadline still bounds it, so we back off and try again rather
    /// than propagate an error that would end the event loop — and with
    /// it every service on the machine.
    pub(crate) fn epoll_wait_for_boot(&mut self) {
        self.register_service_fds(false);
        // `epoll_wait` takes a plain millisecond timeout. Clamped so a
        // future constant above `u16::MAX` can never wrap into a very
        // short (or zero) wait and spin.
        let ms = u16::try_from(BOOT_POLL_INTERVAL.as_millis()).unwrap_or(u16::MAX);
        match self.epoll.wait(&mut self.wait_events, ms) {
            Ok(n) => {
                // Copied out of the shared buffer by value: dispatching
                // needs `&mut self`, which the borrow of `self` would
                // otherwise still be holding.
                for i in 0..n {
                    let ev = self.wait_events[i];
                    self.dispatch_boot(&ev);
                }
            }
            Err(Errno::EINTR) => {}
            Err(e) => {
                warn!("epoll_wait failed during boot: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }

    /// Route a boot-time epoll event. Same shape as [`Self::dispatch`]
    /// minus the control socket and the restart timers, and it never
    /// returns an error: a failure while booting must degrade to a
    /// warning, not to an event loop that stops supervising.
    pub(crate) fn dispatch_boot(&mut self, ev: &EpollEvent) {
        let fd = ev.data() as i32;
        // The control listener is in this epoll set from
        // `EventLoop::new` but stays unserved until the boot is over by
        // design: answering a client mid-`rcS` (a `telinit 3` runlevel
        // switch, a `STOP`) would run a second script set on top of the
        // boot already running. Not an "unknown" fd — ignore it
        // silently instead of debug-logging every tick a client retries.
        if self.control.listener_fd() == Some(fd) {
            return;
        }
        let result = if fd == self.signal_fd.as_raw_fd() {
            self.handle_signals()
        } else if let Some(name) = self.notify_names.get(&fd).cloned() {
            self.handle_notify_fd(&name)
        } else if let Some(name) = self.log_names.get(&fd).cloned() {
            self.handle_log_fd(&name)
        } else {
            debug!("epoll event for unknown fd {} during boot", fd);
            return;
        };
        if let Err(e) = result {
            warn!("handling a boot-time event failed: {}", e);
        }
    }

}

// ─────────────────────────── unit tests ──────────────────────────────

#[cfg(test)]
mod tests {
    use super::boot_batch_size;

    #[test]
    fn test_boot_batch_size_is_the_level_unless_jobs_caps_it() {
        assert_eq!(boot_batch_size(5, 0), 5);
        assert_eq!(boot_batch_size(1, 0), 1);
        assert_eq!(boot_batch_size(5, 2), 2);
        assert_eq!(boot_batch_size(5, 99), 99);
    }

    #[test]
    fn test_boot_batch_size_is_never_zero() {
        // `[T]::chunks(0)` panics — and a panic in PID 1 is a dead
        // machine. The empty-level `continue` in `boot` already avoids
        // it; this locks the belt-and-braces `.max(1)`.
        assert_ne!(boot_batch_size(0, 0), 0);
        assert_ne!(boot_batch_size(0, 3), 0);
    }
}
