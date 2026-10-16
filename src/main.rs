use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use log::{error, info, warn};
use nix::unistd::{getpid, unlink};

use velocity_init::config::{CliCommand, RuntimeConfig};
use velocity_init::control_socket::{
    control_socket_path, listen_first_available, ControlSocket,
};
use velocity_init::event_loop::EventLoop;
use velocity_init::graph::DependencyGraph;
use velocity_init::inittab::{Action, Inittab, SysvContext};
use velocity_init::journal;
use velocity_init::lsb_parser;
use velocity_init::models::{add_builtin_getty_if_missing, BootMode, Service};
use velocity_init::mounts;
use velocity_init::supervisor::Supervisor;

const BUILD_ID: &str = match option_env!("VELOCITY_BUILD_ID") {
    Some(id) => id,
    None => env!("CARGO_PKG_VERSION"),
};

fn print_usage() {
    eprintln!("Usage: velocity-init [OPTIONS]");
    eprintln!();
    eprintln!("Options (both `--flag value` and `--flag=value` forms work):");
    eprintln!("  --dry-run          Simulate startup; print the parallel boot plan without executing services");
    eprintln!("  --init-dir <DIR>   Path to init scripts directory (default: /etc/init.d)");
    eprintln!("  --inittab <FILE>   Path to /etc/inittab (default: /etc/inittab)");
    eprintln!("  --mode <MODE>      Boot mode: auto (default), native (LSB graph), sysv (inittab drop-in)");
    eprintln!("                     auto = sysv when the inittab has content, else native");
    eprintln!(
        "  --jobs <N>         Parallel boot concurrency cap per level (default: 0 = unlimited)"
    );
    eprintln!("  --log-level <LVL>  Log level: error, warn, info, debug, trace (default: info)");
    eprintln!("  --boot-id <ID>     Operator-supplied boot identifier (logged at startup)");
    eprintln!("  --version          Print version and exit");
    eprintln!("  --build-id         Print the package build identifier and exit");
    eprintln!("  --info             Print version, build id, C runtime and arch, then exit");
    eprintln!("  --help             Show this help message");
    eprintln!();
    eprintln!("Environment:");
    eprintln!(
        "  RUST_LOG           Log level: error, warn, info, debug, trace (wins over --log-level)"
    );
    eprintln!("  VELOCITY_INIT_DIR  Override init directory (same as --init-dir)");
    eprintln!("  VELOCITY_INIT_SOCKET  Control socket path (default: /run/velocity-init.sock)");
    eprintln!("  VELOCITY_INIT_GETTY    Console getty executable (default: /sbin/agetty)");
    eprintln!("  VELOCITY_INIT_JOBS     Parallel boot concurrency cap (same as --jobs)");
    eprintln!("  VELOCITY_JOURNAL   Journal path (default: /run/velocity-init/journal)");
    eprintln!(
        "  VELOCITY_RUNTIME_DIR  Directory holding the socket + journal when /run is not writable"
    );
    eprintln!(
        "  VELOCITY_ALLOW_SYSV_NONPID1  Permit a sysv-mode boot when not PID 1 (rescue only)"
    );
}

const C_RUNTIME: &str = match (cfg!(target_env = "gnu"), cfg!(target_env = "musl")) {
    (true, _) if cfg!(target_feature = "crt-static") => "glibc static",
    (true, _) => "glibc dynamic",
    (_, true) if cfg!(target_feature = "crt-static") => "musl static",
    _ => "musl dynamic",
};

fn print_build_info(full: bool) {
    if full {
        println!(
            "velocity-init {} ({}) | {} PID 1 | opt-3 + lto + strip + panic-unwind | {}",
            env!("CARGO_PKG_VERSION"),
            BUILD_ID,
            C_RUNTIME,
            std::env::consts::ARCH,
        );
    } else {
        println!("{}", env!("CARGO_PKG_VERSION").split('+').next().unwrap());
    }
}

fn scan_init_dir(dir: &str) -> Result<Vec<PathBuf>> {
    let path = PathBuf::from(dir);
    let mut scripts = Vec::new();

    let entries = std::fs::read_dir(&path)
        .with_context(|| format!("Failed to read init directory: {}", path.display()))?;

    for entry in entries {
        let entry = entry?;
        let entry_path = entry.path();

        if !entry_path.is_file() {
            continue;
        }

        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();
        if velocity_init::config::should_skip_init_entry(&name_str) {
            continue;
        }

        scripts.push(entry_path);
    }

    scripts.sort();
    Ok(scripts)
}

fn discover_services(init_dir: &str) -> Result<Vec<Service>> {
    let script_paths = scan_init_dir(init_dir)?;
    info!(
        "Found {} potential init scripts in {}",
        script_paths.len(),
        init_dir
    );

    let mut services = Vec::new();
    let mut skipped = 0;

    for path in &script_paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());

        match lsb_parser::parse_header(path) {
            Ok(header) => {
                info!(
                    "Parsed: {} — provides={:?}, required-start={:?}, should-start={:?}",
                    name, header.provides, header.required_start, header.should_start
                );
                services.push(Service::with_header(name, path.clone(), header));
            }
            Err(e) => {
                warn!("Skipping {} (no LSB header or parse error): {}", name, e);
                skipped += 1;
            }
        }
    }

    info!(
        "Successfully parsed {} scripts, skipped {}",
        services.len(),
        skipped
    );
    Ok(services)
}

fn main() -> Result<()> {
    install_panic_hook();

    let is_pid1 = getpid() == nix::unistd::Pid::from_raw(1);

    if let Err(e) = velocity_init::event_loop::block_managed_signals() {

        return Err(anyhow::anyhow!(
            "cannot block init's managed signals ({}): refusing to run as PID 1",
            e
        ));
    }

    claim_stdio();
    let _ = std::fs::write("/proc/self/oom_score_adj", "-1000");

    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let mut opts = match velocity_init::config::parse_cli_args(&raw_args).map_err(|e| anyhow::anyhow!(e))? {
        CliCommand::PrintVersion => {
            print_build_info(false);
            return Ok(());
        }
        CliCommand::PrintBuildId => {
            println!("{}", BUILD_ID);
            return Ok(());
        }

        CliCommand::PrintInfo => {
            print_build_info(true);
            return Ok(());
        }
        CliCommand::PrintHelp => {
            print_usage();
            return Ok(());
        }
        CliCommand::Run(o) => o,
    };
    let log_level: Option<String> = opts.log_level.take();

    let mut config = RuntimeConfig::from_env();
    if let Some(v) = opts.init_dir.take() {
        config.init_dir = v;
    }
    if let Some(v) = opts.inittab.take() {
        config.inittab = v;
    }
    let cli_jobs = opts.jobs.take();
    if let Some(v) = cli_jobs {
        config.jobs = v;

    }

    use velocity_init::console::ConsoleLogger;
    let cli_default = log_level.unwrap_or_else(|| "info".to_string());
    let cli_filter = ConsoleLogger::resolve_with(Some(&cli_default), log::LevelFilter::Info);
    let console = ConsoleLogger::new(ConsoleLogger::resolve(cli_filter));
    let journal_candidates = config.journal_candidates.clone();
    let (journal, journal_path) =
        match journal::Journal::open_first_available(&journal_candidates) {
            Ok((j, path)) => {
                let path = path.to_string_lossy().into_owned();
                eprintln!("velocity-init: journaling to {}", path);
                (Some(journal::JournalLogger::new(j)), path)
            }
            Err(e) => {
                eprintln!(
                    "velocity-init: journal unavailable ({}); console logging only",
                    e
                );
                let fallback = journal_candidates
                    .first()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| {
                        velocity_init::paths::DEFAULT_JOURNAL_PATH.to_string()
                    });
                (None, fallback)
            }
        };

    let logger: &'static journal::DualLogger =
        Box::leak(Box::new(journal::DualLogger::new(console, journal)));
    log::set_logger(logger).context("failed to install logger")?;
    log::set_max_level(log::LevelFilter::Trace);
    if let Some(id) = &opts.boot_id {
        info!("boot-id: {} — operator-identified boot", id);
    }
    info!("velocity-init starting up (jobs: {})", config.jobs);
    if config.jobs_env_invalid && cli_jobs.is_none() {

        warn!(
            "ignoring malformed {} ({:?}): expected a non-negative integer — using 0 (unlimited)",
            velocity_init::config::JOBS_ENV,
            std::env::var(velocity_init::config::JOBS_ENV).unwrap_or_default()
        );
    }

    let inittab = match Inittab::parse(&config.inittab) {
        Ok(t) => t,
        Err(e) => {
            warn!(
                "cannot read {} ({}); booting without it — sysv scripts, gettys and \
                 runlevel handling will not be configured",
                config.inittab, e
            );
            Inittab::default()
        }
    };
    let mode = opts
        .mode
        .resolve(!inittab.is_empty(), &config.init_dir);
    let sysv_mode = mode == BootMode::Sysv;
    if mode != opts.mode {
        info!(
            "boot mode {:?} → {} (--init-dir {})",
            opts.mode, mode, config.init_dir
        );
    }
    if sysv_mode && inittab.is_empty() {
        warn!(
            "sysv mode requested but {} is missing or empty — falling back to native boot",
            config.inittab
        );
    }
    if sysv_mode && !inittab.is_empty() && !is_pid1 && !opts.dry_run && !config.allow_sysv_nonpid1 {
        anyhow::bail!(
            "refusing to run the sysv boot from {} as pid {}: it would execute this \
             machine's /etc/init.d boot scripts and respawn the real gettys, and only PID 1 \
             may do that. Use --mode native, point --inittab at a test file, or set \
             VELOCITY_ALLOW_SYSV_NONPID1=1 if this really is a rescue environment.",
            config.inittab,
            getpid()
        );
    }

    let sysv_active = sysv_mode && !inittab.is_empty();
    let mut sysv_ctx: Option<SysvContext> = None;
    let mut action_commands: HashMap<String, String> = HashMap::new();

    let (services, startup_levels) = if sysv_active {
        info!(
            "sysv mode: {} present — sysvinit drop-in boot",
            config.inittab
        );
        let level = inittab.default_level().unwrap_or_else(|| {
            warn!("no initdefault line in {} — assuming runlevel 2", config.inittab);
            "2".to_string()
        });
        let ctx = SysvContext::build(&inittab, &level);

        if let Some(l) = inittab.ctrlaltdel() {
            action_commands.insert("ctrlaltdel".to_string(), l.process.clone());
        }
        for (key, action) in [
            ("powerfail", Action::PowerFail),
            ("powerwait", Action::PowerWait),
            ("powerokwait", Action::PowerOkWait),
            ("powerfailnow", Action::PowerFailNow),
        ] {
            if let Some(l) = inittab.power_action(action) {
                action_commands.insert(key.to_string(), l.process.clone());
            }
        }
        sysv_ctx = Some(ctx);
        (Vec::new(), Vec::new())
    } else {

        let mut services = match discover_services(&config.init_dir) {
            Ok(s) => s,
            Err(e) => {

                error!(
                    "could not read init directory {}: {e} — booting with no services",
                    config.init_dir
                );
                Vec::new()
            }
        };

        let getty_exe = config.getty_exe.clone();
        if add_builtin_getty_if_missing(&mut services, &getty_exe) {
            info!(
                "no getty-like service found — injected built-in console getty ({})",
                getty_exe
            );
        } else {
            info!("getty-like service present; keeping user configuration");
        }

        if services.is_empty() {

            warn!(
                "No services with LSB headers found in {} — running with the built-in \
                 getty only",
                config.init_dir
            );
        }

        info!("Building dependency graph...");
        let headers: Vec<(String, _)> = services
            .iter()
            .filter_map(|s| s.header.as_ref().map(|h| (s.name.clone(), h.clone())))
            .collect();

        let mut dep_graph = DependencyGraph::from_headers(headers);
        info!(
            "Graph built: {} nodes, {} edges",
            dep_graph.get_all_services().len(),

            dep_graph
                .get_all_services()
                .iter()
                .filter_map(|s| dep_graph.get_dependencies(s))
                .map(|d| d.len())
                .sum::<usize>()
        );

        match dep_graph.detect_cycles() {
            Some(cycle) => {
                error!(
                    "dependency cycle detected: {} — this would make PID 1 exit and the \
                     kernel panic, so the cycle is being broken instead",
                    cycle.join(" → ")
                );
                for dropped in dep_graph.break_cycles() {
                    error!("  ignoring cyclic dependency: {dropped}");
                }
                error!("boot continues with the remaining (acyclic) dependencies");
            }
            None => info!("No circular dependencies detected ✓"),
        }

        let startup_levels = dep_graph.startup_levels().unwrap_or_else(|e| {

            error!("could not order services ({}); starting them all at once", e);
            vec![dep_graph.get_all_services()]
        });

        (services, startup_levels)
    };
    let startup_order: Vec<String> = startup_levels.iter().flatten().cloned().collect();

    if opts.dry_run {
        if sysv_active {
            let Some(ctx) = sysv_ctx.as_ref() else {
                error!("sysv dry-run has no prepared context — nothing to plan");
                return Ok(());
            };
            println!();
            println!("─── SysV inittab Plan (runlevel {}) ───", ctx.level);
            println!(
                "  boot: {}\n  wait: {}",
                ctx.boot_names.join(" "),
                ctx.wait_services(&ctx.level).join(" ")
            );
            println!("  once: {}", ctx.once_names.join(" "));
            println!("  respawn: {}", ctx.respawn_names.join(" "));
            println!();
            info!("SysV dry-run: no services were started.");
        } else {
            println!();
            println!("─── Startup Plan ({} services) ───", startup_order.len());
            for (level_no, level) in startup_levels.iter().enumerate() {
                println!("  Level {}:", level_no);
                for (n, name) in level.iter().enumerate() {
                    println!("    {:>3}. {}", n + 1, name);
                    if let Some(svc) = services.iter().find(|s| s.name == *name) {
                        if let Some(header) = &svc.header {
                            if !header.sockets.is_empty() {
                                let socks: Vec<String> =
                                    header.sockets.iter().map(|s| s.to_string()).collect();
                                println!("        sockets: {}", socks.join(" "));
                            }
                        }
                    }
                }
            }
            println!();
            info!("Dry-run mode: not starting any services.");
            println!("✓ Dry-run complete. No services were started.");
        }
        return Ok(());
    }

    info!("Running as PID {}", getpid());
    if is_pid1 {

        mounts::setup_base_filesystems()
            .context("cannot set up base filesystems (required as PID 1)")?;

        claim_stdio();

        if let Err(e) = logger.reopen_journal(&journal_path) {
            warn!("journal: could not reopen at {}: {}", journal_path, e);
        }
        match mounts::parse_fstab(mounts::DEFAULT_FSTAB) {
            Ok(entries) => mounts::mount_fstab_entries(&entries),
            Err(e) => warn!(
                "no usable {} ({}); continuing with pseudo-filesystems only",
                mounts::DEFAULT_FSTAB,
                e
            ),
        }
        info!("mount setup complete");
    } else {
        info!("Not PID 1 (or running in a container); skipping mount setup");
    }

    if let Some(ctx) = sysv_ctx.as_ref() {

        velocity_init::utmp::write_boot_time();
        match ctx.level.as_bytes().first().copied() {
            Some(ch) => velocity_init::utmp::write_runlevel_record(Some(b'N'), ch),
            None => warn!(
                "runlevel {:?} has no level byte — utmp record skipped",
                ctx.level
            ),
        }
    }

    let mut supervisor = Supervisor::new();
    for service in services {
        supervisor.register(service);
    }

    let sock_candidates = config.socket_candidates.clone();
    let (control, ctl_path) = match listen_first_available(&sock_candidates) {
            Ok((c, path)) => {
                info!("control socket listening at {}", path.display());
                (c, path)
            }
            Err(e) => {
                warn!("control socket unavailable: {}", e);
                let path = sock_candidates
                    .first()
                    .cloned()
                    .unwrap_or_else(|| control_socket_path());
                (ControlSocket::disabled(path.clone()), path)
            }
        };

    let mut loop_ =
        EventLoop::new(supervisor, control).context("failed to initialise event loop")?;
    loop_.set_jobs(config.jobs);
    loop_.set_ctl_idle_timeout(config.ctl_idle_timeout);
    if let Some(ctx) = sysv_ctx.take() {
        loop_.set_sysv(ctx);
    }

    loop_.set_journal(logger, std::path::PathBuf::from(&journal_path));
    loop_.set_action_commands(action_commands);
    // PID 1 must never abort: an unexpected panic in the event loop is
    // contained here — logged, then turned into the degraded panic hold
    // instead of "Attempted to kill init".
    let loop_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        loop_.run(startup_levels)
    }));
    let action = match loop_result {
        Ok(res) => res.context("event loop failed")?,
        Err(payload) => {
            let msg = panic_payload_message(payload.as_ref());
            error!("event loop panicked: {msg}");
            if is_pid1 {
                panic_hold();
            }
            anyhow::bail!("event loop panicked: {msg}");
        }
    };
    info!("teardown complete — finalizing ({})", action);

    let _ = unlink(&ctl_path);

    if is_pid1 {

        let ns_depth = pid_ns_depth();
        // Teardown runs after the loop has stopped services; a panic
        // here must not kill the kernel either, so it lands in the hold.
        let teardown = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            mounts::teardown_filesystems();
            final_reboot(action, ns_depth);
        }));
        if teardown.is_err() {
            error!("panic during teardown; holding the system instead of exiting");
            panic_hold();
        }
    } else {
        info!("not PID 1 — skipping filesystem teardown and reboot syscall");
    }

    info!("velocity-init exiting cleanly");
    Ok(())
}

fn claim_stdio() {
    use nix::fcntl::{fcntl, open, FcntlArg, OFlag};
    use nix::sys::stat::Mode;
    use nix::unistd::{close, dup2};

    for fd in 0..=2 {
        if fcntl(fd, FcntlArg::F_GETFD).is_ok() {
            continue;
        }

        for source in ["/dev/console", "/dev/null"] {
            let Ok(handle) = open(source, OFlag::O_RDWR, Mode::empty()) else {
                continue;
            };
            if handle != fd {
                let _ = dup2(handle, fd);
                let _ = close(handle);
            }
            eprintln!("velocity-init: reclaimed stdio fd {fd} on {source}");
            break;
        }
    }
}

fn final_reboot(action: velocity_init::models::ShutdownAction, ns_depth: usize) {
    use nix::sys::reboot::{reboot, RebootMode};
    use velocity_init::models::ShutdownAction;
    use velocity_init::namespaces;

    nix::unistd::sync();

    let depth = ns_depth;
    let userns = namespaces::in_mapped_user_namespace();
    let sandbox = namespaces::sandbox_requested();
    if depth > 1 || userns || sandbox {
        if sandbox && depth <= 1 && !userns {
            // The sandbox override is only ever set by scripts/sandbox.sh.
            // No other probe fired, so this looks like a real machine —
            // where exiting 0 instead of rebooting would panic the kernel.
            warn!(
                "{} is set but no namespace probe fired — suppressing reboot(2) \
                 anyway; unset it on production machines",
                namespaces::SANDBOX_ENV
            );
        }
        info!(
            "nested runtime detected (NSpid depth {}, mapped user namespace: {}, {}: {}): \
             the runtime — not us — decides what happens to the machine, so {} was not \
             issued; exiting 0",
            depth,
            userns,
            namespaces::SANDBOX_ENV,
            sandbox,
            action
        );
        return;
    }

    let mode = match action {
        ShutdownAction::PowerOff => RebootMode::RB_POWER_OFF,
        ShutdownAction::Reboot => RebootMode::RB_AUTOBOOT,
        ShutdownAction::Halt => RebootMode::RB_HALT_SYSTEM,
    };
    match reboot(mode) {
        Ok(never) => match never {},
        Err(e) => error!("reboot({}) failed: {} — exiting", action, e),
    }
}

/// Install the panic hook before anything else runs.
///
/// Writes straight to stderr with the error ignored: a panic may have
/// poisoned the journal mutexes, and a lock().unwrap() in the hook
/// would double-panic — which aborts even with unwinding enabled.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        use std::io::Write;
        let msg = format!("velocity-init: PANIC: {info}");
        let _ = writeln!(std::io::stderr(), "{msg}");
    }));
}

/// Pull a readable message out of a panic payload.
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Contain a panic that escaped the event loop by keeping PID 1 alive.
///
/// Exiting (or aborting) as PID 1 makes the kernel panic ("Attempted to
/// kill init"). The hold reaps children so they cannot pile up as
/// zombies, and keeps Ctrl-Alt-Del (SIGINT) and the power button
/// (SIGPWR) working. Services are deliberately *not* restarted: the
/// supervisor state can no longer be trusted. Never returns.
fn panic_hold() -> ! {
    use nix::sys::reboot::{reboot, RebootMode};
    use nix::sys::signal::{sigprocmask, SigSet, SigmaskHow, Signal};
    use nix::sys::signalfd::{SfdFlags, SignalFd};
    use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
    use nix::unistd::Pid as NixPid;

    error!("panic hold: PID 1 stays alive so the kernel does too");
    error!("panic hold: services are no longer supervised");
    error!("panic hold: Ctrl-Alt-Del reboots, the power button powers off");

    // Ctrl-Alt-Del arrives as SIGINT, the power button as SIGPWR; block
    // both so the signalfd below is their only delivery path. (The
    // event loop had them blocked already; this is the panic path, so
    // be explicit.) Failure is not fatal — the hold still reaps.
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGPWR);
    let _ = sigprocmask(SigmaskHow::SIG_BLOCK, Some(&mask), None);
    let signals = SignalFd::with_flags(&mask, SfdFlags::SFD_NONBLOCK | SfdFlags::SFD_CLOEXEC).ok();

    loop {
        // Reap whatever the unsupervised services leave behind.
        loop {
            match waitpid(NixPid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) | Err(_) => break,
                Ok(_) => continue,
            }
        }

        if let Some(sfd) = signals.as_ref() {
            if let Ok(Some(si)) = sfd.read_signal() {
                match Signal::try_from(si.ssi_signo as i32) {
                    Ok(Signal::SIGINT) => {
                        info!("panic hold: SIGINT (Ctrl-Alt-Del) — rebooting");
                        nix::unistd::sync();
                        match reboot(RebootMode::RB_AUTOBOOT) {
                            Ok(never) => match never {},
                            Err(e) => {
                                error!("panic hold: reboot failed: {e} — use the reset button")
                            }
                        }
                    }
                    Ok(Signal::SIGPWR) => {
                        info!("panic hold: SIGPWR (power button) — powering off");
                        nix::unistd::sync();
                        match reboot(RebootMode::RB_POWER_OFF) {
                            Ok(never) => match never {},
                            Err(e) => {
                                error!("panic hold: poweroff failed: {e} — press the power button")
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn pid_ns_depth() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("NSpid:"))
                .map(|v| v.split_whitespace().count())
        })
        .filter(|d| *d > 0)
        .unwrap_or(1)
}
