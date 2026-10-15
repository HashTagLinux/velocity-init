//! EventLoop::shutdown (split from `event_loop.rs` — verbatim move, no logic changes).
//!
//! Child post-fork paths do only async-signal-safe calls + _exit(127); signals are blocked before signalfd; fds 0/1/2 are claimed; journal/log/connection caps stay bounded.
use super::*;
use std::collections::HashSet;

impl super::EventLoop {
    /// Perform a `telinit N`-style runlevel change (sysv mode):
    ///
    /// - run the `wait` line for the target level (e.g.
    ///   `/etc/init.d/rc 3`) and wait for it;
    /// - start respawn services bound to the target level, stop those
    ///   not;
    /// - update the runlevel record.
    ///
    /// Level `0`/`6` are handled as shutdown (the shutdown sequence
    /// itself runs `/etc/init.d/rc 0` / `rc 6` first).
    pub(crate) fn handle_level_change(&mut self, level: &str) -> LoopResult<()> {
        // sysvinit uppercases the requested level (`s` → `S`).
        let level = crate::inittab::Inittab::normalize_level(level);

        // A runlevel switch mid-teardown would start new services the
        // shutdown is about to kill again (and `ensure_sysv_services`
        // + `start` during `ShuttingDown` is exactly what the
        // supervisor refuses). Refuse up front.
        if self.supervisor.system_state() == crate::models::SystemState::ShuttingDown {
            warn!("sysv: ignoring runlevel change to {level} — shutdown already in progress");
            return Ok(());
        }

        // Runlevels 0/6 are the shutdown levels (sysvinit semantics):
        // halt vs reboot. The full teardown (run `/etc/init.d/rc 0`,
        // stop supervised services, utmp) happens in perform_shutdown.
        if level == "0" || level == "6" {
            let action = if level == "6" {
                ShutdownAction::Reboot
            } else {
                ShutdownAction::Halt
            };
            info!("sysv: runlevel {level} is a shutdown level — {action}");
            if self.pending_shutdown.is_none() {
                self.pending_shutdown = Some(action);
            }
            return Ok(());
        }

        // A real runlevel switch is a sysvinit concept, and the body
        // below dereferences `self.sysv` twice (the respawn sweep and
        // the level bookkeeping at the end). In LSB mode — no
        // /etc/inittab — that is None, and the `expect` that used to
        // sit at the end of this function killed PID 1 on a plain
        // `telinit 3`: no more reaping, no supervision, no shutdown.
        // Refuse the request up front instead, before any state moves.
        //
        // The 0/6 check above deliberately stays ahead of this guard:
        // halting and rebooting are meaningful in LSB mode too.
        if self.sysv.is_none() {
            return Err(LoopError::NotSysvMode(level));
        }

        // Defence in depth: the socket validates, and so does this, so
        // a level can never reach the utmp record or the respawn sweep
        // unvalidated (this is the only other writer of `Level`).
        if !crate::inittab::Inittab::valid_level(&level) {
            return Err(LoopError::InvalidRunlevel(level));
        }

        // On-demand levels (`telinit a/b/c`) run their entries without
        // changing the current runlevel and without touching the respawn
        // set or utmp (inittab(5): "no runlevel change will occur").
        // They used to be treated as full switches, which rewrote the
        // RUN_LVL record to `a` and stopped every getty not tagged `a`.
        if crate::inittab::Inittab::is_ondemand(&level) {
            return self.handle_ondemand_level(&level);
        }

        let prev = self.runlevel().unwrap_or("?").to_string();
        if level == prev {
            info!("sysv: already at runlevel {level} — reloading inittab scripts");
        }
        info!("sysv: switching from runlevel {prev} to {level}");

        // sysvinit runs *every* wait line bound to the target level, in
        // file order — usually just `rc N`, but a level may have more
        // than one and dropping the extras would silently skip init
        // scripts the operator configured.
        let wait_names: Vec<String> = self
            .sysv
            .as_ref()
            .map(|c| c.wait_services(&level).to_vec())
            .unwrap_or_default();
        if wait_names.is_empty() {
            warn!("sysv: no wait line for runlevel {level}");
        }
        self.ensure_sysv_services(&wait_names, crate::models::RestartPolicy::NoRestart);
        for wait_name in &wait_names {
            match self.supervisor.start(wait_name) {
                Ok(pid) => self.wait_for_exit(wait_name, pid),
                Err(e) => warn!("sysv: could not start {wait_name}: {e}"),
            }
        }

        // `once` lines for the target level run without waiting
        // (inittab(5)). They used to run at boot only, so `telinit 3`
        // silently skipped the level-3 `once` lines.
        let once_names: Vec<String> = self
            .sysv
            .as_ref()
            .map(|c| c.once_services(&level).to_vec())
            .unwrap_or_default();
        self.ensure_sysv_services(&once_names, crate::models::RestartPolicy::NoRestart);
        for once_name in &once_names {
            if let Err(e) = self.supervisor.start(once_name) {
                warn!("sysv: could not start {once_name}: {e}");
            }
        }

        // Respawn sweep across *all* respawn lines (not just the boot
        // level's): stop those not active in the target level, start
        // those that are.
        let respawn: Vec<String> = match self.sysv.as_ref() {
            Some(ctx) => ctx.all_respawn_names(),
            None => Vec::new(),
        };
        self.ensure_sysv_services(&respawn, crate::models::RestartPolicy::Always);
        for name in &respawn {
            let active = match self.sysv.as_ref() {
                Some(ctx) => ctx.respawn_active(name, &level),
                None => false,
            };
            let running = self.supervisor.service(name).is_some_and(|s| {
                s.state() == crate::models::ServiceState::Running
                    || s.state() == crate::models::ServiceState::Starting
                    || s.state() == crate::models::ServiceState::Stopping
            });
            if active && !running {
                info!("sysv: starting respawn {name} for runlevel {level}");
                let _ = self.supervisor.start(name);
            } else if !active && running {
                info!("sysv: stopping respawn {name} for runlevel {level}");
                let _ = self.supervisor.stop_service(name);
            }
        }

        // Update our current-level bookkeeping and the utmp RUN_LVL
        // record (so `runlevel` / `who -r` stay correct). Updated in
        // place: the context holds every inittab set, and cloning it
        // just to change one level string allocates (and risks
        // overwriting a concurrent reload's table with a stale copy).
        if let Some(ctx) = self.sysv.as_mut() {
            ctx.level = level.clone();
        }
        crate::utmp::write_runlevel_record(
            Some(prev.bytes().next().unwrap_or(PREV_RUN_NONE)),
            level.bytes().next().unwrap_or(b'?'),
        );
        info!("sysv: now at runlevel {level}");
        // `rc N` is an ordinary init script: it may well have cleaned
        // /run on its way through, taking the control socket and the
        // journal with it.
        self.reestablish_runtime_files();
        Ok(())
    }

    /// Run the on-demand entries for `level` (`a`/`b`/`c`, either case)
    /// without changing the current runlevel and without touching the
    /// respawn set or utmp — sysvinit's `telinit a` semantics.
    ///
    /// Runs the `ondemand` lines plus any `wait`/`once` lines tagged
    /// with the level (sysvinit marks every matching entry `DEMAND`,
    /// whatever its action). Matching is case-insensitive so `telinit A`
    /// finds a line tagged `a`. `wait` lines are waited on, the rest are
    /// fire-and-forget.
    pub(crate) fn handle_ondemand_level(&mut self, level: &str) -> LoopResult<()> {
        if self.supervisor.system_state() == crate::models::SystemState::ShuttingDown {
            warn!("sysv: ignoring ondemand run for {level} — shutdown already in progress");
            return Ok(());
        }
        if self.sysv.is_none() {
            return Err(LoopError::NotSysvMode(level.to_string()));
        }
        // `wait`/`once` maps are keyed case-sensitively; an ondemand
        // request matches either case. Deduplicated with sets: the old
        // `Vec::contains` scan was quadratic. Kept as two sets (not
        // one) so a name listed under both actions keeps the old
        // behaviour of being attempted in both passes.
        let mut seen_waits: HashSet<String> = HashSet::new();
        let mut waits: Vec<String> = Vec::new();
        if let Some(ctx) = self.sysv.as_ref() {
            for key in [level.to_string(), level.to_ascii_lowercase()] {
                for n in ctx.wait_services(&key) {
                    if seen_waits.insert(n.clone()) {
                        waits.push(n.clone());
                    }
                }
            }
        }
        let mut seen_fire: HashSet<String> = HashSet::new();
        let mut fire: Vec<String> = Vec::new();
        if let Some(ctx) = self.sysv.as_ref() {
            for key in [level.to_string(), level.to_ascii_lowercase()] {
                for n in ctx.once_services(&key) {
                    if seen_fire.insert(n.clone()) {
                        fire.push(n.clone());
                    }
                }
            }
            // `ondemand_services` already lowercases internally, so one
            // lookup covers both cases.
            for n in ctx.ondemand_services(level) {
                if seen_fire.insert(n.clone()) {
                    fire.push(n.clone());
                }
            }
        }
        if waits.is_empty() && fire.is_empty() {
            warn!("sysv: no ondemand entries for runlevel {level}");
            return Ok(());
        }
        info!("sysv: running ondemand entries for {level} (runlevel unchanged)");
        self.ensure_sysv_services(&waits, crate::models::RestartPolicy::NoRestart);
        for name in &waits {
            match self.supervisor.start(name) {
                Ok(pid) => self.wait_for_exit(name, pid),
                Err(e) => warn!("sysv: could not start {name}: {e}"),
            }
        }
        self.ensure_sysv_services(&fire, crate::models::RestartPolicy::NoRestart);
        for name in &fire {
            if let Err(e) = self.supervisor.start(name) {
                warn!("sysv: could not start {name}: {e}");
            }
        }
        self.reestablish_runtime_files();
        Ok(())
    }

    /// Re-read `/etc/inittab` (sysvinit `telinit q` / SIGHUP semantics):
    ///
    /// - refresh the ctrlaltdel / power action commands;
    /// - rebuild the runlevel context at the *current* level;
    /// - sync the respawn set: start respawns that became active for
    ///   this level, stop those that were removed or deactivated.
    pub(crate) fn reload_inittab(&mut self) -> LoopResult<()> {
        // `telinit q` during teardown must not start/stop anything.
        if self.supervisor.system_state() == crate::models::SystemState::ShuttingDown {
            warn!("reload: ignoring inittab re-read — shutdown already in progress");
            return Ok(());
        }
        // SIGHUP is `telinit q`: re-read the inittab *this init was
        // booted with*. It is explicitly a no-op outside sysv mode.
        //
        // Without this guard a stray `kill -HUP 1` on an LSB-mode init
        // (no /etc/inittab at boot, or an explicit `--init-dir`) would
        // parse whatever /etc/inittab happens to be on disk and set
        // `self.sysv`, silently switching the machine into sysv mode
        // mid-flight. That changes the shutdown path — inittab
        // `shutdown:` lines and `rc 0` / `rc 6` that were never part of
        // this boot suddenly start running — and it contradicts the
        // rule that explicit operator configuration is never discarded.
        if self.sysv.is_none() {
            warn!(
                "SIGHUP ignored: this init booted in LSB mode, so there is no inittab to \
                 re-read. (telinit q is a sysv-mode operation.)"
            );
            return Ok(());
        }
        let inittab = match crate::inittab::Inittab::parse(crate::inittab::Inittab::DEFAULT_PATH) {
            Ok(it) => it,
            Err(e) => {
                warn!("reload: cannot read {}: {}", crate::inittab::Inittab::DEFAULT_PATH, e);
                return Ok(());
            }
        };
        if inittab.is_empty() {
            warn!("reload: /etc/inittab is missing or empty — keeping current configuration");
            return Ok(());
        }
        let level = self.runlevel().unwrap_or_default().to_string();
        let ctx = SysvContext::build(&inittab, &level);

        // Refresh the one-shot signal actions from the new table.
        let mut cmds = HashMap::new();
        if let Some(l) = inittab.ctrlaltdel() {
            cmds.insert("ctrlaltdel".to_string(), l.process.clone());
        }
        for (key, action) in [
            ("powerfail", crate::inittab::Action::PowerFail),
            ("powerwait", crate::inittab::Action::PowerWait),
            ("powerokwait", crate::inittab::Action::PowerOkWait),
            ("powerfailnow", crate::inittab::Action::PowerFailNow),
        ] {
            if let Some(l) = inittab.power_action(action) {
                cmds.insert(key.to_string(), l.process.clone());
            }
        }
        self.action_commands = cmds;

        // Install the new context first: ensure_sysv_services resolves
        // commands through self.sysv.
        let respawn: Vec<String> = ctx.all_respawn_names();
        self.sysv = Some(ctx);
        self.ensure_sysv_services(&respawn, crate::models::RestartPolicy::Always);

        // Sync the respawn set with the reloaded table.
        for name in respawn {
            let active = self
                .sysv
                .as_ref()
                .is_some_and(|c| c.respawn_active(&name, &level));
            let running = self.supervisor.service(&name).is_some_and(|s| {
                s.state() == crate::models::ServiceState::Running
                    || s.state() == crate::models::ServiceState::Starting
                    || s.state() == crate::models::ServiceState::Stopping
            });
            if active && !running {
                info!("reload: starting respawn {name}");
                let _ = self.supervisor.start(&name);
            } else if !active && running {
                info!("reload: stopping respawn {name}");
                let _ = self.supervisor.stop_service(&name);
            }
        }
        info!("reload: /etc/inittab re-applied (runlevel {level})");
        Ok(())
    }

    /// Run an inittab action command (one-shot), if one is configured.
    ///
    /// Never during shutdown: `spawn_one_shot` forks without
    /// supervision, so an action started on the way down outlives the
    /// service stops and must be hunted by the orphan sweep instead.
    pub(crate) fn run_action(&mut self, cmd: &str) {
        if self.supervisor.system_state() == crate::models::SystemState::ShuttingDown {
            warn!("ignoring action {cmd} — shutdown already in progress");
            return;
        }
        match self.supervisor.spawn_one_shot(cmd) {
            Ok(pid) => debug!("action spawned: {} (pid {})", cmd, pid),
            Err(e) => error!("could not spawn action {}: {}", cmd, e),
        }
    }

    /// Run the action command registered under `key` (e.g.
    /// `"powerfailnow"`), ignoring the signal when there is none.
    pub(crate) fn run_action_or(&mut self, key: &str) {
        if let Some(cmd) = self.action_commands.get(key).cloned() {
            info!("power event {key}: running {}", cmd);
            self.run_action(&cmd);
        } else {
            debug!("power event {key} — no inittab action configured, ignoring");
        }
    }

    /// The full teardown sequence, run once a shutdown has been
    /// requested (signal or control command):
    ///
    /// 1. Transition to `ShuttingDown` — no new starts or restarts.
    /// 2. Stop every service in strict reverse topological order
    ///    (`stop_service` sends SIGTERM to the process group, waits up
    ///    to 2s, escalates to SIGKILL, then kills the service cgroup).
    /// 3. Drain any buffered log output and close the log pipes.
    /// 4. Final sweep: `cgroup.kill` on the root `velocity-init`
    ///    cgroup obliterates stragglers (escaped daemons, orphaned
    ///    writers).
    ///
    /// Filesystem teardown and the `reboot(2)` syscall are the
    /// caller's job (main), because they only apply to a real PID 1.
    pub(crate) fn perform_shutdown(
        &mut self,
        startup_order: &[String],
        action: ShutdownAction,
    ) -> LoopResult<()> {
        let sysv_mode = self.sysv.is_some();
        let stop_count = if sysv_mode {
            self.supervisor.services().count()
        } else {
            startup_order.len()
        };
        let (order, mode) = if sysv_mode {
            ("reverse registration order", "sysv mode")
        } else {
            ("reverse dependency order", "LSB mode")
        };
        info!(
            "graceful shutdown ({action}): stopping {stop_count} service(s) in {order} ({mode})",
        );

        // sysv mode: run the inittab `shutdown` lines and the target
        // level's `rc` line (`/etc/init.d/rc 0` on halt/poweroff,
        // `/etc/init.d/rc 6` on reboot). These MUST run before
        // `begin_shutdown`: the supervisor refuses to start anything
        // once the global state flips to ShuttingDown, and running them
        // is exactly where sysv keeps its stop logic.
        //
        // Borrowed per name through `self.sysv`, never a clone of the
        // whole context (see `ensure_sysv_services`): the table holds
        // every inittab set, and teardown only needs two small lists.
        if self.sysv.is_some() {
            let target = match action {
                ShutdownAction::Reboot => "6",
                _ => "0",
            };
            let teardown_names: Vec<String> = {
                let ctx = self.sysv.as_ref().expect("checked above");
                ctx.shutdown_names
                    .iter()
                    .chain(ctx.wait_services(target).iter())
                    .cloned()
                    .collect()
            };
            self.ensure_sysv_services(&teardown_names, crate::models::RestartPolicy::NoRestart);
            for name in &teardown_names {
                match self.supervisor.start(name) {
                    Ok(pid) => self.wait_for_exit(name, pid),
                    Err(e) => warn!("sysv: could not run {name}: {e}"),
                }
            }
            crate::utmp::write_runlevel_record(
                Some(
                    self.sysv
                        .as_ref()
                        .and_then(|c| c.level.bytes().next())
                        .unwrap_or(PREV_RUN_NONE),
                ),
                match action {
                    ShutdownAction::Reboot => b'6',
                    _ => b'0',
                },
            );
        }

        if !self.supervisor.begin_shutdown() {
            info!("shutdown already in progress — ignoring duplicate request");
            return Ok(());
        }

        // Reverse topological order: dependents die before their
        // dependencies, mirroring the boot levels.
        let reversed: Vec<String> = if sysv_mode {
            // sysv: stop every registered inittab service (respawns and
            // any stragglers), reverse registration order.
            let mut names: Vec<String> =
                self.supervisor.services().map(|s| s.name.clone()).collect();
            names.reverse();
            names
        } else {
            startup_order.iter().rev().cloned().collect()
        };
        self.supervisor.stop_all(&reversed);

        // Drain whatever the pipes still hold (services are dead, so
        // this is the final output), then close the read ends.
        self.drain_log_pipes();
        self.supervisor.close_log_pipes();

        // Straggler sweep: nothing supervised may survive teardown.
        if let Err(e) = crate::cgroups::kill_root_cgroup() {
            warn!("root cgroup sweep failed (non-fatal): {}", e);
        } else {
            info!("final sweep complete — root cgroup killed");
        }

        // Belt and braces: the cgroup sweep above is an *optimisation*,
        // not a guarantee. It silently does nothing whenever cgroup v2
        // is unavailable (cgroup v1 hosts, CONFIG_CGROUPS=n, the cgroup2
        // mount failing, a container without delegation) and whenever a
        // daemon escaped its service cgroup by forking before we moved
        // the child into it. In those cases a service that double-forked
        // and `setsid()`d away — postgresql, sshd, anything with a real
        // daemon — keeps running, keeps its files open, and turns the
        // umount sweep into EBUSY, i.e. a database unmounted mid-write.
        //
        // As PID 1 every such orphan is our direct child (PPid == 1), so
        // it is enumerable from /proc with no heuristics. SIGKILL them,
        // then reap so they do not linger as zombies under PID 1. The
        // predicate is only *valid* when we are PID 1 — see
        // `owns_orphan`.
        self.kill_orphaned_children();

        info!("teardown complete — services stopped, system shutting down");
        Ok(())
    }

    /// May a process parented to PID 1 be assumed to be an orphan of
    /// *ours*, and therefore SIGKILLed by the teardown sweep?
    ///
    /// Only when we are PID 1 ourselves. The kernel reparents an orphan
    /// to PID 1, so "PPid == 1" identifies our own stragglers exactly
    /// when we *are* that PID 1 — the case the sweep exists for.
    ///
    /// For any other process it identifies nothing at all: our children's
    /// orphans were reparented to the *real* PID 1, not to us, while
    /// every unrelated direct child of the machine's init still shows
    /// PPid == 1. So a non-PID-1 run (a test, a rescue shell, a
    /// `docker run --pid=host` container) sweeping on that predicate
    /// kills bystanders: in a container it kills the very shell that
    /// started it, on a host it kills the init's other children. Refuse.
    ///
    /// Pure over both pids (no `getpid` inside) so the PID 1 case is
    /// unit-testable without being PID 1: pass the caller's own pid as
    /// `self_pid`.
    pub(crate) fn owns_orphan(pid: i32, self_pid: i32) -> bool {
        // PID 1 is never ours to reap, and neither are we.
        if pid <= 1 || pid == self_pid {
            return false;
        }
        self_pid == 1
    }

    /// SIGKILL every process still parented to PID 1, then reap it.
    ///
    /// Runs at the very end of shutdown, after the graceful stops and
    /// the cgroup sweep, and only when we are PID 1 — see
    /// [`Self::owns_orphan`] for why the PPid == 1 predicate is only
    /// meaningful there.
    pub(crate) fn kill_orphaned_children(&mut self) {
        let self_pid = getpid().as_raw();
        if self_pid != 1 {
            debug!(
                "pid {} is not PID 1 — skipping the PPid==1 orphan sweep: our orphans were \
                 reparented to the machine's real PID 1, so that predicate would name \
                 processes we do not own",
                self_pid
            );
            return;
        }
        let Ok(entries) = std::fs::read_dir("/proc") else {
            warn!("cannot enumerate /proc for the orphan sweep; skipping");
            return;
        };
        let mut killed = 0usize;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Ok(pid) = name.parse::<i32>() else {
                continue; // not a pid directory
            };
            if !Self::owns_orphan(pid, self_pid) {
                continue;
            }
            // /proc/<pid>/stat field 4 is ppid. The comm field (2) can
            // contain spaces and parentheses, so parse after the last
            // ')' rather than splitting the whole line.
            let ppid_of = |pid: i32| -> Option<i32> {
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
                let after = stat.rfind(')')?;
                stat[after + 1..]
                    .split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse::<i32>().ok())
            };
            // Only *our* orphans. A descendant of a surviving service is
            // not PPid 1, so this cannot touch a process we do not own.
            if ppid_of(pid) != Some(1) {
                continue;
            }
            // Re-verify just before signalling: the pid could have
            // exited and been recycled between the directory listing and
            // now. A second stat read narrows (never closes) that
            // window; anything that changed identity in between is
            // skipped rather than killed on suspicion.
            if ppid_of(pid) != Some(1) {
                continue;
            }
            // Same `killpg`-safe helper the supervisor uses for service
            // groups would be wrong here (these are not our groups);
            // plain `kill(pid, SIGKILL)` via nix. ESRCH (exited since
            // the stat) and EPERM (not ours to signal) are both
            // non-fatal at teardown: log the latter, ignore the former.
            match nix::sys::signal::kill(Pid::from_raw(pid), Signal::SIGKILL) {
                Ok(()) => killed += 1,
                Err(Errno::ESRCH) => {}
                Err(e) => debug!("orphan sweep: could not SIGKILL {pid}: {e}"),
            }
        }
        if killed > 0 {
            info!("orphan sweep: SIGKILLed {killed} process(es) still parented to PID 1");
        }
        // Reap them so nothing is left as a permanent zombie under PID 1.
        // `waitpid(-1, WNOHANG)` cannot block and cannot consume
        // anything we still care about: every service has already been
        // stopped and reaped above.
        loop {
            match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) | Err(Errno::ECHILD) => break,
                Ok(_) => {}
                Err(Errno::EINTR) => continue,
                Err(e) => {
                    debug!("orphan sweep reap failed: {e}");
                    break;
                }
            }
        }
    }

    /// Drain one service's stdout/stderr during the boot waits.
    ///
    /// Log pipes are registered with epoll by `register_fds`, which runs
    /// in the main loop — *after* the boot. Every service nevertheless
    /// gets a 64 KiB pipe at fork time, so any boot script that printed
    /// more than that blocked in `write(2)`: in sysv mode the boot line
    /// sat there until `SYSV_EXIT_TIMEOUT` (300s) gave up on it, and in
    /// LSB mode the service was declared `running` while actually
    /// wedged mid-write. Draining here costs a bounded number of
    /// non-blocking reads per poll iteration, and the lines are logged
    /// exactly as they would have been by the main loop.
    pub(crate) fn drain_service_log(&mut self, name: &str) {
        if self.supervisor.log_pipe(name).is_none() {
            return;
        }
        let mut buf = [0u8; 4096];
        for _ in 0..MAX_LOG_DRAIN_ITERS {
            match self.supervisor.read_log(name, &mut buf) {
                Ok(0) => break,
                Ok(n) => self.route_log(name, &buf[..n]),
                Err(Errno::EINTR) => continue,
                Err(Errno::EAGAIN) => break,
                Err(e) => {
                    debug!("log drain for {name} failed: {e}");
                    break;
                }
            }
        }
    }

    /// Drain every log-routing pipe and route the tail of each stream
    /// through the normal `[name]` logging.
    pub(crate) fn drain_log_pipes(&mut self) {
        let names: Vec<String> = self.supervisor.services().map(|s| s.name.clone()).collect();
        for name in names {
            if self.supervisor.log_pipe(&name).is_none() {
                continue;
            }
            let mut buf = [0u8; 4096];
            // Bounded per pipe: at teardown every writer is already dead
            // (or SIGKILLed), so the pipe holds at most 64 KiB and one
            // pass suffices; the cap only matters if a straggler is
            // still writing while we drain.
            for _ in 0..MAX_LOG_DRAIN_ITERS {
                match self.supervisor.read_log(&name, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => self.route_log(&name, &buf[..n]),
                    Err(Errno::EAGAIN) => break,
                    Err(Errno::EINTR) => continue,
                    Err(e) => {
                        warn!("log drain for {} failed: {}", name, e);
                        break;
                    }
                }
            }
        }
        // Flush trailing partial lines so nothing is lost. Same shape
        // as the EOF path (`emit_trailing_partial`): strip one trailing
        // CR, lossy UTF-8.
        for (name, partial) in self.log_partial.drain() {
            Self::emit_trailing_partial(&name, &partial);
        }
    }
}
