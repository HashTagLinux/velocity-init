use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use nix::unistd::{geteuid, Uid};

fn shutdown_pidfile() -> PathBuf {
    std::env::var("VELOCITY_SHUTDOWN_PID")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/run/velocity-shutdown.pid"))
}

fn is_shutdown_helper(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    if cmdline.is_empty() {
        return false;
    }
    let text = String::from_utf8_lossy(&cmdline);
    text.contains("velocityctl") || text.contains("shutdown")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SAction {

    Halt,

    PowerOff,

    Reboot,
}

impl SAction {
    fn wire(self) -> &'static str {
        match self {
            SAction::Halt => "HALT",
            SAction::PowerOff => "POWEROFF",
            SAction::Reboot => "REBOOT",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SWhen {
    Now,

    Plus(u32),

    At(u8, u8),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownPlan {

    pub cancel: bool,

    pub warn_only: bool,

    pub action: Option<SAction>,

    pub delay_secs: i64,

    pub message: String,
}

pub fn parse_shutdown(args: &[String]) -> Result<ShutdownPlan, String> {
    let mut cancel = false;
    let mut warn_only = false;
    let mut action: Option<SAction> = None;
    let mut when: Option<SWhen> = None;
    let mut message: Vec<String> = Vec::new();

    let mut it = args.iter().map(String::as_str).peekable();
    while let Some(tok) = it.next() {
        if tok == "--" {

            if let Some(time_tok) = it.next() {
                if !cancel {
                    when = Some(parse_when(time_tok)?);
                }
                message.extend(it.map(str::to_string));
            }
            break;
        }
        if let Some(flags) = tok.strip_prefix('-').filter(|f| !f.is_empty()) {

            let bytes = flags.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                let f = bytes[i] as char;
                match f {
                    'h' => action = Some(SAction::PowerOff),
                    'p' => action = Some(SAction::PowerOff),
                    'P' => action = Some(SAction::PowerOff),
                    'H' => action = Some(SAction::Halt),
                    'r' => action = Some(SAction::Reboot),
                    'c' => cancel = true,
                    'k' => warn_only = true,

                    'a' | 'f' | 'F' | 'g' | 'i' => {}

                    't' => {
                        if i + 1 < bytes.len() {

                            i = bytes.len();
                        } else {

                            it.next();
                        }
                    }
                    other => return Err(format!("invalid option: -{other}")),
                }
                i += 1;
            }
            continue;
        }

        if !cancel {
            when = Some(parse_when(tok)?);
        }
        message.extend(it.map(str::to_string));
        break;
    }

    if cancel {
        return Ok(ShutdownPlan {
            cancel,
            warn_only,
            action: None,
            delay_secs: 0,
            message: String::new(),
        });
    }
    if warn_only {
        return Ok(ShutdownPlan {
            cancel,
            warn_only,
            action: None,
            delay_secs: 0,
            message: message.join(" "),
        });
    }
    let Some(action) = action else {
        return Err("need one of -h, -H or -r (or -c to cancel)".to_string());
    };
    let when = when.ok_or("time argument required (now, +mins, or HH:MM)")?;
    let unix_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(ShutdownPlan {
        cancel,
        warn_only,
        action: Some(action),
        delay_secs: when_delay(&when, unix_now)?,
        message: message.join(" "),
    })
}

fn parse_when(tok: &str) -> Result<SWhen, String> {
    if tok == "now" || tok == "0" {
        return Ok(SWhen::Now);
    }
    if let Some(mins) = tok.strip_prefix('+') {
        let m: u32 = mins.parse().map_err(|_| format!("bad delay: {tok}"))?;
        return Ok(SWhen::Plus(m));
    }
    let (h, m) = tok.split_once(':').ok_or_else(|| format!("bad time: {tok}"))?;
    let h: u8 = h.parse().map_err(|_| format!("bad hour: {h}"))?;
    let m: u8 = m.parse().map_err(|_| format!("bad minute: {m}"))?;
    if h > 23 || m > 59 {
        return Err(format!("time out of range: {tok}"));
    }
    Ok(SWhen::At(h, m))
}

fn when_delay(when: &SWhen, unix_now: i64) -> Result<i64, String> {
    Ok(match when {
        SWhen::Now => 0,
        SWhen::Plus(m) => *m as i64 * 60,
        SWhen::At(h, m) => {

            let tz = local_utc_offset_secs(unix_now);
            let now_local_secs_of_day = (unix_now + tz).rem_euclid(86_400);
            let target_local = *h as i64 * 3600 + *m as i64 * 60;
            let mut delta = target_local - now_local_secs_of_day;
            if delta < 0 {
                delta += 86_400;
            }
            delta
        }
    })
}

fn local_utc_offset_secs(unix_now: i64) -> i64 {
    unsafe {
        let t: libc::time_t = unix_now as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return 0;
        }
        tm.tm_gmtoff as i64
    }
}

fn wall(msg: &str) {
    let _ = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("printf '%s\\n' \"$VELOCITY_WALL_MSG\" | wall 2>/dev/null")
        .env("VELOCITY_WALL_MSG", msg)
        .status();
}

const SOCKET_TIMEOUT: Duration = Duration::from_secs(10);

fn send_command(cmd: &str) -> Result<String, String> {
    let (mut stream, path) = connect()?;
    stream
        .set_read_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|e| format!("cannot arm a read timeout on {}: {e}", path.display()))?;
    stream
        .write_all(format!("{cmd}\n").as_bytes())
        .map_err(|e| format!("write failed: {e}"))?;
    let mut response = String::new();
    stream.read_to_string(&mut response).map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut
        {
            format!("no reply from init within {}s — is PID 1 wedged?", SOCKET_TIMEOUT.as_secs())
        } else {
            format!("read failed: {e}")
        }
    })?;
    Ok(response)
}

fn require_root(action: &str) {
    if geteuid() != Uid::from_raw(0) {
        eprintln!("error: {action} requires root");
        std::process::exit(2);
    }
}

fn direct_reboot(base: &str) -> ! {
    use nix::sys::reboot::{reboot, RebootMode};
    require_root(base);
    let mode = match base {
        "reboot" => RebootMode::RB_AUTOBOOT,
        "poweroff" => RebootMode::RB_POWER_OFF,
        _ => RebootMode::RB_HALT_SYSTEM,
    };
    match reboot(mode) {
        Ok(never) => match never {},
        Err(e) => {
            eprintln!("{base}: reboot(2) failed: {e}");
            std::process::exit(1);
        }
    }
}

fn run_shutdown(args: Vec<String>) -> ! {
    let plan = match parse_shutdown(&args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("shutdown: {e}");
            eprintln!("usage: shutdown [-h|-H|-r] [-c|-k] [-a|-f|-F] [-t sec] time [message]");
            std::process::exit(2);
        }
    };

    if plan.cancel {
        let pidfile = shutdown_pidfile();
        match std::fs::read_to_string(&pidfile) {
            Ok(text) => match text.trim().parse::<i32>() {
                Ok(pid) => {

                    if !is_shutdown_helper(pid) {
                        let _ = std::fs::remove_file(&pidfile);
                        eprintln!("shutdown: no shutdown scheduled (stale pidfile removed)");
                        std::process::exit(1);
                    }
                    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
                    let _ = std::fs::remove_file(&pidfile);
                    if rc != 0 {
                        eprintln!("shutdown: could not signal helper {pid}");
                        std::process::exit(1);
                    }
                    println!("Shutdown cancelled.");
                    std::process::exit(0);
                }
                Err(_) => {
                    eprintln!("shutdown: corrupt pidfile {}", pidfile.display());
                    std::process::exit(1);
                }
            },
            Err(_) => {
                eprintln!("shutdown: no shutdown scheduled");
                std::process::exit(1);
            }
        }
    }

    if plan.warn_only {
        let msg = if plan.message.is_empty() {
            "The system will be shut down soon!".to_string()
        } else {
            plan.message.clone()
        };
        wall(&msg);
        println!("Warning broadcast sent.");
        std::process::exit(0);
    }

    let action = plan.action.expect("checked by parse_shutdown");

    if plan.delay_secs == 0 {
        require_root("shutdown");
        match send_command(action.wire()) {
            Ok(reply) => {
                print!("{reply}");
                let ok = reply.trim_end().starts_with("OK");
                std::process::exit(if ok { 0 } else { 1 });
            }
            Err(e) => {
                eprintln!("shutdown: {e}");
                std::process::exit(1);
            }
        }
    }

    require_root("shutdown");
    let fire_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        + plan.delay_secs;
    let fire_iso = velocity_init::journal::format_time(fire_at * 1_000_000);
    let message = if plan.message.is_empty() {
        format!("The system is going down at {fire_iso}!")
    } else {
        format!("{} (at {fire_iso})", plan.message)
    };
    wall(&message);

    let fork_rc = unsafe { libc::fork() };
    if fork_rc < 0 {
        eprintln!("shutdown: fork failed: {}", nix::errno::Errno::last());
        std::process::exit(1);
    }
    if fork_rc > 0 {
        println!("Shutdown scheduled for {fire_iso}, use 'shutdown -c' to cancel.");
        std::process::exit(0);
    }

    unsafe { libc::setsid() };

    let pidfile = shutdown_pidfile();
    if let Some(parent) = pidfile.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&pidfile, format!("{}\n", std::process::id()));

    loop {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(i64::MAX);
        if now >= fire_at {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(
            ((fire_at - now).min(5) * 1000) as u64,
        ));
    }
    wall("The system is going down NOW!");
    let _ = std::fs::remove_file(&pidfile);
    let _ = send_command(action.wire());
    std::process::exit(0);
}

fn connect() -> Result<(UnixStream, PathBuf), String> {
    let mut err = "no control socket path resolved".to_string();
    for path in velocity_init::paths::control_socket_candidates() {
        match UnixStream::connect(&path) {
            Ok(s) => return Ok((s, path)),
            Err(e) => err = format!("cannot connect to {}: {e}", path.display()),
        }
    }
    Err(err)
}

fn build_command(verb: &str, service: Option<&String>) -> Option<String> {
    let upper = verb.to_ascii_uppercase();
    match (upper.as_str(), service) {
        ("LIST", None) => Some("LIST".to_string()),
        ("POWEROFF" | "REBOOT" | "HALT", None) => Some(upper),
        ("CHANGE", Some(level)) => Some(format!("CHANGE {}", level)),
        ("RELOAD", None) => Some("RELOAD".to_string()),
        ("STATUS" | "START" | "STOP" | "RESTART", Some(name)) => {
            Some(format!("{} {}", upper, name))
        }
        _ => None,
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: velocityctl <status|start|stop|restart|list|change|reload|runlevel|journal|poweroff|reboot|halt> [arg]"
    );
    std::process::exit(2);
}

fn print_runlevel() -> ! {
    let path = velocity_init::utmp::utmp_path();
    if let Some(rl) = velocity_init::utmp::read_runlevel(&path) {
        println!("{} {}", rl.previous, rl.current);
        std::process::exit(0);
    }

    let rl_path = velocity_init::utmp::runlevel_path();
    if let Some(current) = velocity_init::utmp::read_runlevel_file(&rl_path) {
        println!("N {current}");
        std::process::exit(0);
    }
    eprintln!("velocityctl: runlevel: no RUN_LVL record in {path} (nor in {rl_path})");
    std::process::exit(1);
}

fn argv0_dispatch() -> Option<String> {
    let argv0 = std::env::args().next().unwrap_or_default();
    let base = argv0
        .rsplit('/')
        .next()
        .unwrap_or(&argv0)
        .to_ascii_lowercase();
    let arg = std::env::args().nth(1);
    match base.as_str() {
        "telinit" => {
            let level = arg?;
            match level.trim() {

                "q" | "Q" | "qq" => Some("RELOAD".to_string()),

                "u" | "U" => {
                    eprintln!("telinit: re-exec (`u`) is not supported by velocity-init");
                    std::process::exit(2);
                }
                t if t.starts_with('-') => {
                    eprintln!("telinit: flag-style invocation is not supported");
                    std::process::exit(2);
                }
                t => Some(format!("CHANGE {t}")),
            }
        }
        "reboot" => Some("REBOOT".to_string()),
        "halt" => Some("HALT".to_string()),
        "poweroff" => Some("POWEROFF".to_string()),
        "runlevel" => Some("runlevel".to_string()),
        _ => None,
    }
}

fn journal_path() -> String {
    use std::path::Path;
    fn live(base: &Path) -> bool {
        if base.exists() {
            return true;
        }
        let s = base.to_string_lossy();
        Path::new(&format!("{s}.0")).exists() || Path::new(&format!("{s}.1")).exists()
    }
    let candidates = velocity_init::paths::journal_candidates();
    candidates
        .iter()
        .find(|p| live(p))
        .or_else(|| candidates.first())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| velocity_init::journal::DEFAULT_JOURNAL_PATH.to_string())
}

fn print_journal(args: &mut impl Iterator<Item = String>) -> ! {
    let mut tail_n: usize = 50;
    let mut follow = false;
    let mut rest: Vec<String> = args.collect();
    while !rest.is_empty() {
        let arg = rest.remove(0);
        if let Some(n) = arg.strip_prefix("--tail=") {
            tail_n = match n.parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("velocityctl: journal --tail requires a number");
                    std::process::exit(2);
                }
            };
            continue;
        }
        match arg.as_str() {
            "--tail" | "-n" => {
                tail_n = match rest.first().and_then(|v| v.parse().ok()) {
                    Some(n) => {
                        rest.remove(0);
                        n
                    }
                    None => {
                        eprintln!("velocityctl: journal --tail requires a number");
                        std::process::exit(2);
                    }
                }
            }
            "--follow" | "-f" => follow = true,
            other => {
                eprintln!("velocityctl: journal: unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }
    let path = journal_path();
    let entries = match velocity_init::journal::read_file(std::path::Path::new(&path)) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("velocityctl: journal: cannot read {}: {}", path, e);
            std::process::exit(1);
        }
    };
    let start = entries.len().saturating_sub(tail_n);
    for e in entries.iter().skip(start) {
        println!("{}", e.display());
    }
    let mut printed = entries.len();
    if !follow {
        std::process::exit(0);
    }

    loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let fresh = match velocity_init::journal::read_file(std::path::Path::new(&path)) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("velocityctl: journal: read failed: {}", e);
                std::process::exit(1);
            }
        };
        if fresh.len() < printed {
            printed = 0;
        }
        for e in fresh.iter().skip(printed) {
            println!("{}", e.display());
        }
        printed = fresh.len();
    }
}

fn main() {

    let argv0 = std::env::args()
        .next()
        .unwrap_or_default()
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();

    if argv0 == "shutdown" {
        run_shutdown(std::env::args().skip(1).collect());
    }

    if matches!(argv0.as_str(), "reboot" | "halt" | "poweroff")
        && std::env::args().skip(1).any(|a| a == "-f" || a == "--force")
    {
        direct_reboot(&argv0);
    }

    let dispatched = argv0_dispatch();
    let is_dispatch = dispatched.is_some();

    let mut args = std::env::args().skip(1);
    let verb = match dispatched {
        Some(cmd) => {
            if cmd.is_empty() {
                eprintln!("telinit: flag-style invocation is not supported");
                std::process::exit(2);
            }
            cmd
        }
        None => match args.next() {
            Some(v) => v,
            None => usage(),
        },
    };

    if verb.eq_ignore_ascii_case("runlevel") {
        print_runlevel();
    }
    if verb.eq_ignore_ascii_case("journal") {
        print_journal(&mut args);
    }

    let service = args.next();

    if !is_dispatch && args.next().is_some() {
        eprintln!("error: too many arguments");
        usage();
    }

    let upper = verb
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    let is_shutdown_verb = matches!(
        upper.as_str(),
        "POWEROFF" | "REBOOT" | "HALT" | "CHANGE" | "RELOAD"
    );
    if is_shutdown_verb && geteuid() != Uid::from_raw(0) {
        eprintln!("error: {} requires root", upper.to_lowercase());
        std::process::exit(2);
    }

    let mut wire = if is_dispatch {
        verb
    } else {
        match build_command(&verb, service.as_ref()) {
            Some(w) => w,
            None => usage(),
        }
    };
    wire.push('\n');
    let (mut stream, path) = match connect() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("velocityctl: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = stream.set_read_timeout(Some(SOCKET_TIMEOUT)) {
        eprintln!("velocityctl: cannot arm a read timeout: {e}");
        std::process::exit(1);
    }

    if let Err(e) = stream.write_all(wire.as_bytes()) {
        eprintln!("velocityctl: write to {} failed: {}", path.display(), e);
        std::process::exit(1);
    }

    let mut response = String::new();
    if let Err(e) = stream.read_to_string(&mut response) {
        if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut {
            eprintln!(
                "velocityctl: no reply from init within {}s — is PID 1 wedged?",
                SOCKET_TIMEOUT.as_secs()
            );
        } else {
            eprintln!("velocityctl: read from {} failed: {}", path.display(), e);
        }
        std::process::exit(1);
    }

    let trimmed = response.trim_end();
    print!("{}", response);
    if trimmed.is_empty() {
        eprintln!("velocityctl: no response from server");
        std::process::exit(1);
    }
    if trimmed.starts_with("ERR") {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_shutdown_immediate_reboot() {
        let p = parse_shutdown(&args(&["-r", "now"])).expect("parses");
        assert_eq!(p.action, Some(SAction::Reboot));
        assert!(!p.cancel);
        assert_eq!(p.delay_secs, 0);
    }

    #[test]
    fn test_shutdown_h_plus_minutes_with_message() {
        let p = parse_shutdown(&args(&[
            "-h",
            "+10",
            "Maintenance downtime",
        ]))
        .expect("parses");
        assert_eq!(p.action, Some(SAction::PowerOff));
        assert_eq!(p.delay_secs, 600);
        assert_eq!(p.message, "Maintenance downtime");
    }

    #[test]
    fn test_shutdown_cancel_is_recognised_before_action_requirement() {

        let p = parse_shutdown(&args(&["-c", "Power Restored; Shutdown Cancelled"]))
            .expect("parses");
        assert!(p.cancel);
        assert_eq!(p.action, None);
    }

    #[test]
    fn test_shutdown_hh_mm_delay() {

        let p = parse_shutdown(&args(&["-r", "00:00"])).expect("parses");
        assert!(p.delay_secs > 0 && p.delay_secs <= 86_400, "{}", p.delay_secs);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let tz = local_utc_offset_secs(now);
        let target = now + tz + 120;
        let sod = target.rem_euclid(86_400);
        let h = (sod / 3600) as u8;
        let m = ((sod % 3600) / 60) as u8;
        let tok = format!("{h:02}:{m:02}");
        let p = parse_shutdown(&args(&["-h", &tok])).expect("parses");
        assert!((60..=180).contains(&p.delay_secs), "{} vs {}", p.delay_secs, tok);
    }

    #[test]
    fn test_shutdown_warn_only_and_noop_flags() {
        let p = parse_shutdown(&args(&["-k", "-a", "-f", "+5", "bye"])).expect("parses");
        assert!(p.warn_only);
        assert_eq!(p.action, None);
        assert_eq!(p.message, "bye");
    }

    #[test]
    fn test_shutdown_requires_action_and_time() {
        assert!(parse_shutdown(&args(&["now"])).is_err(), "no -h/-r");
        assert!(parse_shutdown(&args(&["-r"])).is_err(), "no time");
        assert!(parse_shutdown(&args(&["-X", "now"])).is_err(), "bad flag");
        assert!(parse_shutdown(&args(&["-r", "+abc"])).is_err(), "bad delay");
        assert!(parse_shutdown(&args(&["-r", "25:99"])).is_err(), "bad clock");
    }

    #[test]
    fn test_shutdown_t_flag_consumes_its_argument() {

        let p = parse_shutdown(&args(&["-r", "-t", "5", "now"])).expect("parses");
        assert_eq!(p.action, Some(SAction::Reboot));
        assert_eq!(p.delay_secs, 0);
    }

    #[test]
    fn test_shutdown_t_flag_with_attached_value() {

        let p = parse_shutdown(&args(&["-r", "-t5", "now"])).expect("parses");
        assert_eq!(p.action, Some(SAction::Reboot));
        assert_eq!(p.delay_secs, 0);

        let p = parse_shutdown(&args(&["-rt5", "now"])).expect("parses");
        assert_eq!(p.action, Some(SAction::Reboot));
        assert_eq!(p.delay_secs, 0);
    }

    #[test]
    fn test_shutdown_double_dash_ends_flag_parsing() {

        let p = parse_shutdown(&args(&["-h", "--", "now"])).expect("parses");
        assert_eq!(p.action, Some(SAction::PowerOff));
        assert_eq!(p.delay_secs, 0);

        let p = parse_shutdown(&args(&["-h", "--", "+5", "going down"])).expect("parses");
        assert_eq!(p.delay_secs, 300);
        assert_eq!(p.message, "going down");
    }

    #[test]
    fn test_shutdown_helper_identity_probe() {

        assert!(!is_shutdown_helper(-1));
        assert!(!is_shutdown_helper(0));

        assert!(!is_shutdown_helper(1));
    }

    #[test]
    fn test_telinit_style_when_forms() {
        assert_eq!(parse_when("now").unwrap(), SWhen::Now);
        assert_eq!(parse_when("+3").unwrap(), SWhen::Plus(3));
        assert_eq!(parse_when("07:30").unwrap(), SWhen::At(7, 30));
        assert_eq!(
            when_delay(&SWhen::Plus(4), 1_000_000).unwrap(),
            240
        );
        assert_eq!(when_delay(&SWhen::Now, 0).unwrap(), 0);
    }
}
