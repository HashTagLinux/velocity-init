//! Supervisor::spawn (split from `supervisor.rs` — verbatim move, no logic changes).
//!
//! Child post-fork paths do only async-signal-safe calls + _exit(127); signals are blocked before signalfd; fds 0/1/2 are claimed; journal/log/connection caps stay bounded.
use super::*;

impl super::Supervisor {
    /// Run a one-shot external command (a sysvinit action line like
    /// `ctrlaltdel` / `powerfail`) without supervision: fork, exec with
    /// `/bin/sh -c`, and return immediately. The child is reaped by the
    /// generic SIGCHLD path (it is *not* registered in `pid_to_service`,
    /// so `handle_child_exit` logs-and-ignores it).
    ///
    /// Uses `nix::fork` (same as [`Supervisor::spawn`]) so both fork
    /// sites share a single, consistent child setup pattern instead of
    /// one using raw `libc::fork`.
    pub fn spawn_one_shot(&self, cmd: &str) -> SupResult<Pid> {
        // `c"…" literals are `&'static CStr`: no allocation, no `Result`,
        // no panic path in PID 1 (unlike `CString::new(…).expect()`).
        let sh = c"/bin/sh".to_owned();
        let cflag = c"-c".to_owned();
        let body = CString::new(cmd).map_err(|_| SupervisorError::BadCommand)?;
        // `vars_os` so a non-UTF-8 inherited value cannot panic PID 1,
        // and a map so the PATH we add below cannot end up duplicated.
        let mut merged: HashMap<OsString, OsString> = std::env::vars_os().collect();
        // A bare kernel launch gives init an almost empty env; actions
        // still need to find `reboot`/`halt` by bare name. Never clobber
        // an explicit operator PATH — only fill the gap.
        ensure_default_path(&mut merged);
        // Shared with `spawn` (see `cstrings_from_env_map`): one loop,
        // one NUL-skipping policy, no drift between fork sites.
        // The `CString`s must outlive `envp` — it only borrows their
        // bytes, and both have to stay alive until `execve` in the child.
        let env_c = cstrings_from_env_map(merged, "one-shot action");
        let mut envp: Vec<*const libc::c_char> = env_c.iter().map(|c| c.as_ptr()).collect();
        envp.push(std::ptr::null());
        let argv = [sh.as_ptr(), cflag.as_ptr(), body.as_ptr(), std::ptr::null()];

        match unsafe { fork() }? {
            ForkResult::Parent { child } => {
                debug!("one-shot action spawned (pid {})", child);
                Ok(child)
            }
            ForkResult::Child => {
                // Child: own process group so a later sweep can signal it,
                // and clear PID 1's blocked signal mask so the action can
                // receive signals normally.
                let _ = setpgid(Pid::from_raw(0), Pid::from_raw(0));
                let empty = SigSet::empty();
                let _ = sigprocmask(SigmaskHow::SIG_SETMASK, Some(&empty), None);
                // Rust's runtime sets SIGPIPE to SIG_IGN at startup, and
                // an ignored disposition survives fork *and* exec — so
                // without this every service would be born with SIGPIPE
                // ignored, unlike under sysvinit/systemd. `signal()` is
                // on the async-signal-safe list, so it is legal here.
                unsafe {
                    libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                }
                // Replace our image; on failure exit with a distinctive
                // code (`_exit` avoids atexit destructors).
                unsafe {
                    libc::execve(sh.as_ptr(), argv.as_ptr(), envp.as_ptr());
                    libc::_exit(127)
                }
            }
        }
    }

    /// `fork` + `execve` the service.
    ///
    /// Built-in services (e.g. `getty@console`) exec an explicit
    /// `service.cmd`; everything else runs `<script> start`.
    ///
    /// The child environment is built in the *parent* (before `fork`)
    /// with this precedence (low → high):
    ///
    /// 1. the inherited environment (defaults);
    /// 2. `# X-Velocity-Env:` files (`/etc/default/…`), relative paths
    ///    resolved against the init script's directory;
    /// 3. `extra_env` — protocol entries like `NOTIFY_SOCKET`,
    ///    `LISTEN_FDS`, `LISTEN_PID`. These are never shadowed.
    ///
    /// Activation fds listed in `dup_fds` are `dup2`ed onto `3..3+n`
    /// (clearing `CLOEXEC`) so the service inherits them exactly where
    /// `sd_listen_fds` expects them.
    ///
    /// # Safety
    ///
    /// `fork` is the one unavoidable `unsafe` syscall here: we must clone
    /// the entire process image. Only async-signal-safe operations happen
    /// in the child (`setpgid`, `sigprocmask`, `dup2`, a pointer rewrite
    /// in pre-allocated memory, `execve`), and on exec failure the child
    /// exits via `_exit(127)` without touching the Rust heap.
    pub(crate) fn spawn(
        &self,
        service: &Service,
        extra_env: &[(String, String)],
        dup_fds: &[(i32, i32)],
    ) -> SupResult<(Pid, OwnedFd)> {
        let script_c = CString::new(service.script_path.as_os_str().as_bytes()).map_err(|e| {
            SupervisorError::BadPath {
                service: service.name.clone(),
                source: e,
            }
        })?;

        // Executable + argv: explicit cmd for built-ins, else script.
        // `Service::cmd` is a public field, so `Some(vec![])` is
        // reachable without a panic budget: indexing `args[0]` would
        // kill PID 1. Reject it as an error instead.
        let (exec_c, args): (CString, Vec<CString>) = match &service.cmd {
            Some(cmd) => {
                if cmd.is_empty() {
                    return Err(SupervisorError::EmptyCommand(service.name.clone()));
                }
                let args: Vec<CString> = cmd
                    .iter()
                    .map(|a| {
                        CString::new(a.as_str()).map_err(|e| SupervisorError::BadPath {
                            service: service.name.clone(),
                            source: e,
                        })
                    })
                    .collect::<Result<_, _>>()?;
                // `cmd` is non-empty (checked above), so `[0]` cannot panic.
                let exec_c = args[0].clone();
                (exec_c, args)
            }
            None => (
                script_c.clone(),
                vec![script_c.clone(), c"start".to_owned()],
            ),
        };

        // Log-routing pipe: the service writes stdout/stderr into the
        // write end (fd 1/2 in the child), PID 1 drains the read end
        // in the event loop.
        //
        // ONLY the read end may be non-blocking. `pipe2(O_NONBLOCK)`
        // sets the flag on *both* ends and `dup2` shares the open file
        // description, so the service would inherit a non-blocking fd 1
        // and fd 2. That silently breaks services: stdio does not retry
        // EAGAIN, so once the 64 KiB pipe buffer fills (a chatty daemon,
        // or PID 1 busy in one dispatch batch) every write fails and the
        // output is dropped — `sh` even reports "write error: Resource
        // temporarily unavailable". sysvinit gives daemons the console,
        // i.e. a *blocking* fd, so we must too.
        let (log_read, log_write) =
            pipe2(OFlag::O_CLOEXEC).map_err(SupervisorError::LogPipe)?;
        let log_read_raw = log_read.as_raw_fd();
        let log_write_raw = log_write.as_raw_fd();
        // The read end must never block PID 1's event loop, so it — and
        // only it — gets O_NONBLOCK, set here rather than at pipe2().
        set_nonblocking(log_read_raw)?;

        // Build the child environment *before* fork, layering
        // (see the doc comment): inherited defaults, then env files
        // (they override defaults), then extra_env (always wins).
        // A merged map guarantees no duplicate `KEY=` entries.
        //
        // `vars_os` rather than `vars`: a non-UTF-8 value in the
        // environment (legal on Linux, and inherited by init from the
        // kernel) must not panic a PID 1 — `vars()` unwraps into
        // `String` and dies on exactly that.
        let mut merged: HashMap<OsString, OsString> = std::env::vars_os().collect();
        if let Some(header) = &service.header {
            let base = service
                .script_path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::path::PathBuf::from("/"));
            for file in &header.env_files {
                let path = if Path::new(file).is_absolute() {
                    std::path::PathBuf::from(file)
                } else {
                    base.join(file)
                };
                match parse_env_file(&path) {
                    Ok(pairs) => {
                        for (k, v) in pairs {
                            merged.insert(k.into(), v.into());
                        }
                    }
                    Err(e) => warn!(
                        "could not read env file {} for {}: {}",
                        path.display(),
                        service.name,
                        e
                    ),
                }
            }
        }
        for (k, v) in extra_env {
            merged.insert(k.clone().into(), v.clone().into());
        }
        // A bare kernel launch gives init an almost empty env; without
        // this a service that calls helpers by bare name fails. Env
        // files and `extra_env` above already won over the inherited
        // defaults, and an explicit PATH among them is kept — this only
        // fills the gap when nobody set one.
        ensure_default_path(&mut merged);

        // One shared conversion (see `cstrings_from_env_map`): no
        // duplicate `NAME=VALUE` entries, NUL bytes skipped with a
        // warning instead of failing the whole spawn.
        let env = cstrings_from_env_map(merged, &service.name);

        let mut argv: Vec<*const libc::c_char> = args.iter().map(|c| c.as_ptr()).collect();
        argv.push(std::ptr::null());
        let mut envp: Vec<*const libc::c_char> = env.iter().map(|c| c.as_ptr()).collect();
        envp.push(std::ptr::null());
        let exec_ptr = exec_c.as_ptr();

        // Messages the child may need *after* fork(), formatted now.
        // Between `fork()` and `execve` only async-signal-safe calls are
        // legal: `format!` and `log!` both allocate, and the allocator
        // (or the logger's mutex) may be held by a thread that existed
        // at fork time — in the child that is a permanent deadlock, not
        // a slow path. A service that cannot report why it failed to
        // start, and hangs doing it, is the worst possible outcome.
        let exec_fail_msg = CString::new(format!(
            "velocity-init: exec failed for {}: {}\n",
            service.name,
            exec_c.to_string_lossy()
        ))
        // The fallback has no NUL by construction (`c"…"`), so this
        // cannot panic in PID 1 — unlike `.expect("static")` on a
        // `CString::new` result.
        .unwrap_or_else(|_| c"velocity-init: exec failed\n".to_owned());
        // Static bytes: no allocation, ever.
        const SIGMASK_FAIL_MSG: &[u8] =
            b"velocity-init: child could not clear its signal mask\n";

        match unsafe { fork() }? {
            ForkResult::Parent { child } => {
                // The child calls setpgid itself; doing it here too just
                // closes the race window. The parent's write-end copy is
                // dropped below; the read end is returned to the caller.
                let _ = setpgid(child, child);
                drop(log_write);
                Ok((child, log_read))
            }
            ForkResult::Child => {
                // ── child process side ─────────────────────────────
                // Own process group so SIGTERM/SIGKILL funnels cleanly.
                let _ = setpgid(Pid::from_raw(0), Pid::from_raw(0));

                // CRITICAL: PID 1 keeps SIGCHLD/SIGTERM/SIGINT blocked
                // for its signalfd. Children inherit that mask across
                // fork() *and* exec(). If we did not clear it here,
                // daemons we spawn would never receive SIGTERM.
                let empty = SigSet::empty();
                // Restore the default disposition too: the Rust runtime
                // ignores SIGPIPE process-wide, and SIG_IGN is inherited
                // across fork *and* exec. A service that writes to a
                // closed pipe must die of SIGPIPE like it would under
                // sysvinit, not collect EPIPE and spin. `signal()` is
                // async-signal-safe, so it is legal in the child.
                unsafe {
                    libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                }
                if let Err(_e) = sigprocmask(SigmaskHow::SIG_SETMASK, Some(&empty), None) {
                    // Async-signal-safe report only (see exec_fail_msg):
                    // `warn!` here would risk deadlocking the child.
                    unsafe {
                        libc::write(
                            libc::STDERR_FILENO,
                            SIGMASK_FAIL_MSG.as_ptr() as *const libc::c_void,
                            SIGMASK_FAIL_MSG.len(),
                        )
                    };
                }

                // Hand the activation sockets to the service at the
                // standard positions.
                //
                // `dup2` clears `FD_CLOEXEC` on the target — *except*
                // when `orig == target`, where Linux returns the fd
                // unchanged and keeps `CLOEXEC` set. The orig fds were
                // created with `SOCK_CLOEXEC`, so without the explicit
                // clear below a service whose socket happened to land on
                // fd 3 would lose it at `execve` (closed as cloexec).
                // `fcntl(F_SETFD)` is async-signal-safe, so it is legal
                // in the child.
                for (orig, target) in dup_fds {
                    let _ = dup2(*orig, *target);
                    unsafe {
                        libc::fcntl(*target, libc::F_SETFD, 0);
                    }
                }

                // Route stdout/stderr into the log pipe, then drop the
                // original pipe fds from the child's table. Daemons the
                // script forks inherit fds 1/2, so their output keeps
                // flowing to PID 1 for the whole lifetime of the service.
                let _ = dup2(log_write_raw, libc::STDOUT_FILENO);
                let _ = dup2(log_write_raw, libc::STDERR_FILENO);
                // fd 0 is deliberately NOT inherited: `claim_stdio` opens
                // /dev/console without O_CLOEXEC so that PID 1 itself
                // keeps a console, and every service would otherwise
                // start with a live read/write fd on it. That is a
                // privilege problem (a console fd opened as root is not
                // subject to DAC after the daemon drops privileges, so a
                // setuid daemon could read the login prompt, passwords
                // included) and it is also not what sysvinit does: there,
                // only the getty is on the console. `/dev/null` is what
                // sysvinit, systemd and OpenRC all give services.
                //
                // open() + dup2() are async-signal-safe, so this is legal
                // in the child.
                let devnull = unsafe {
                    libc::open(b"/dev/null\0".as_ptr() as *const libc::c_char, libc::O_RDONLY)
                };
                if devnull >= 0 {
                    if devnull != libc::STDIN_FILENO {
                        let _ = dup2(devnull, libc::STDIN_FILENO);
                        let _ = unsafe { libc::close(devnull) };
                    }
                }
                let _ = close(log_read_raw);
                let _ = close(log_write_raw);

                // Rewrite the LISTEN_PID placeholder to our real PID
                // (the buffer lives in this frame until execve).
                let mut pid_buf = [0u8; 40];
                unsafe {
                    raw_rewrite_listen_pid(&mut envp, &mut pid_buf);
                }

                // Replace our image with the init script (or built-in
                // command).
                unsafe {
                    libc::execve(exec_ptr, argv.as_ptr(), envp.as_ptr());
                    // Only reached if exec failed. Report with a raw
                    // `write` (async-signal-safe, no allocator) and exit
                    // with a distinctive code via `_exit`, which skips
                    // atexit destructors.
                    libc::write(
                        libc::STDERR_FILENO,
                        exec_fail_msg.as_ptr() as *const libc::c_void,
                        exec_fail_msg.as_bytes().len(),
                    );
                    libc::_exit(127);
                }
            }
        }
    }

    /// Start a service: create its notify / activation sockets on first
    /// start, spawn the child, update bookkeeping, and (best effort)
    /// attach it to its cgroup. On success the service is in `Starting`
    /// state.
    pub fn start(&mut self, name: &str) -> SupResult<Pid> {
        // Once shutdown has begun, nothing may start again.
        if self.system_state == SystemState::ShuttingDown {
            return Err(SupervisorError::ShuttingDown);
        }
        // Disarm any pending restart timer — a fresh start supersedes it.
        self.disarm_timer(name);

        let service = self
            .services
            .get(name)
            .ok_or_else(|| SupervisorError::UnknownService(name.to_string()))?;

        // A second spawn of a live service would orphan the child we
        // are holding: `start` overwrites `pid_to_service[old]`, so the
        // first process is never reaped, never stopped, and keeps
        // running (and holding its port). `Starting` with a live pid is
        // the same situation — still waiting for READY=1, not a crash
        // loop. `Starting` *without* a pid (or with a restart armed) is
        // the restart path, which must be allowed through.
        let busy = match service.state() {
            ServiceState::Running => true,
            ServiceState::Starting => service.pid.is_some() && !service.restart_pending,
            _ => false,
        };
        if busy {
            return Err(SupervisorError::AlreadyRunning(name.to_string()));
        }
        // A service that is still being torn down must not be spawned
        // underneath itself: the dying child's pid mapping would be
        // overwritten and it would escape the stop entirely.
        if service.state() == ServiceState::Stopping {
            return Err(SupervisorError::AlreadyStopping(name.to_string()));
        }

        // Readiness notify socket, created once and reused on restart.
        if !self.notify_sockets.contains_key(name) {
            match create_notify_socket(name) {
                Ok(fd) => {
                    self.notify_sockets.insert(name.to_string(), fd);
                }
                Err(e) => warn!("could not create notify socket for {}: {}", name, e),
            }
        }

        // Socket-activation fds, created once: they keep listening
        // across restarts — that is the point of socket activation.
        let specs: Vec<SocketKind> = service
            .header
            .as_ref()
            .map(|h| h.sockets.clone())
            .unwrap_or_default();
        if !specs.is_empty() && !self.socket_fds.contains_key(name) {
            let fds = create_socket_fds(name, &specs);
            if !fds.is_empty() {
                self.socket_fds.insert(name.to_string(), fds);
            }
        }

        // Child environment additions and fd remappings.
        let mut extra: Vec<(String, String)> = Vec::new();
        if self.notify_sockets.contains_key(name) {
            extra.push((
                "NOTIFY_SOCKET".into(),
                format!("@{}", String::from_utf8_lossy(&notify_addr_bytes(name))),
            ));
        }
        let mut dup: Vec<(i32, i32)> = Vec::new();
        if let Some(fds) = self.socket_fds.get(name) {
            if !fds.is_empty() {
                extra.push(("LISTEN_FDS".into(), fds.len().to_string()));
                extra.push(("LISTEN_FDS_START".into(), "3".to_string()));
                extra.push(("LISTEN_PID".into(), "0".to_string()));
                for (i, fd) in fds.iter().enumerate() {
                    dup.push((fd.as_raw_fd(), 3 + i as i32));
                }
            }
        }

        let (pid, log_read) = self.spawn(service, &extra, &dup)?;
        self.log_pipes.insert(name.to_string(), log_read);

        let limits: HashMap<String, String> = service
            .header
            .as_ref()
            .map(|h| h.limits.clone())
            .unwrap_or_default();

        let svc = self
            .services
            .get_mut(name)
            // `spawn` above only borrowed `&self`, so the entry checked
            // at the top of `start` cannot have vanished — but PID 1
            // never panics on a bookkeeping surprise; return an error.
            .ok_or_else(|| SupervisorError::UnknownService(name.to_string()))?;
        // Legal from every state `start` accepts (`Stopped`, `Failed`,
        // or a pid-less `Starting` restart): the guards above rejected
        // `Running` and `Stopping`, the only sources of refusal.
        if !svc.transition(ServiceState::Starting) {
            warn!("{:<20} refused Starting (state {})", name, svc.state());
        }
        svc.pid = Some(pid);
        svc.running_since = None;
        svc.restart_pending = false;
        svc.restart_after_stop = false;
        self.pid_to_service.insert(pid, name.to_string());

        // Attach to a cgroup. Failure is non-fatal: the service still
        // runs, we just can't scope its process tree.
        match cgroups::setup_cgroup(name, pid, &limits) {
            Ok(dir) => debug!("started {} (pid {}) in cgroup {}", name, pid, dir.display()),
            Err(e) => warn!("could not attach {} to cgroup: {}", name, e),
        }

        info!("{:<20} Spawned pid {}", name, pid);
        Ok(pid)
    }

    /// Mark a service as running (used by boot and the event loop).
    ///
    /// * `pid: None` → the process daemonized (exit 0); track state only.
    /// * `pid: Some(pid)` → the child is still alive; keep supervising it.
    pub fn mark_running(&mut self, name: &str, pid: Option<Pid>) {
        if let Some(svc) = self.services.get_mut(name) {
            // Callers only ever promote a `Starting` service (boot wait,
            // `READY=1`, post-restart). A rejection means a duplicate or
            // late readiness for a service that already moved on — keep
            // the newer state and just refresh the bookkeeping below.
            if !svc.transition(ServiceState::Running) {
                debug!(
                    "{:<20} late Running ignored (state {})",
                    name,
                    svc.state()
                );
            }
            svc.pid = pid;
            svc.running_since = Some(Instant::now());
            svc.restart_pending = false;
        }
        info!(
            "{:<20} Running{}",
            name,
            if pid.is_some() {
                " (foreground child supervised)"
            } else {
                " (daemonized)"
            }
        );
    }

}

#[cfg(test)]
mod tests {
    use super::super::{Supervisor, SupervisorError};
    use crate::models::{RestartPolicy, Service};

    /// A built-in service with an empty argv vector must be rejected
    /// with an error — never panic on `args[0]`. (`Service::cmd` is a
    /// public field, so `Some(vec![])` is reachable input for PID 1.)
    #[test]
    fn test_empty_cmd_vector_is_rejected_not_panicking() {
        let mut sup = Supervisor::new();
        let mut svc = Service::new("bad".into(), "/etc/init.d/bad".into());
        svc.cmd = Some(vec![]);
        svc.restart_policy = RestartPolicy::NoRestart;
        sup.register(svc);
        assert!(matches!(
            sup.start("bad"),
            Err(SupervisorError::EmptyCommand(_))
        ));
    }

    /// A one-shot action with a NUL byte cannot reach `execve` and must
    /// fail before any fork happens (no child, no side effects).
    #[test]
    fn test_one_shot_nul_command_is_rejected() {
        let sup = Supervisor::new();
        assert!(matches!(
            sup.spawn_one_shot("reboot\0now"),
            Err(SupervisorError::BadCommand)
        ));
    }
}
