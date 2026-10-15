use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::os::unix::io::{AsRawFd, BorrowedFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use log::{debug, error, warn};
use nix::errno::Errno;
use nix::sys::socket::{
    accept4, bind, getsockopt, listen, socket, AddressFamily, Backlog, SockFlag, SockType, UnixAddr,
};
use nix::sys::socket::sockopt::PeerCredentials;
use nix::sys::stat::{fchmod, Mode};
use nix::unistd::{read, unlink, write};

use crate::models::ShutdownAction;
use crate::supervisor::Supervisor;

pub fn control_socket_path() -> PathBuf {
    crate::paths::control_socket_candidates()
        .into_iter()
        .next()
        .unwrap_or_else(|| PathBuf::from(crate::paths::DEFAULT_SOCKET_PATH))
}

pub fn control_socket_candidates() -> Vec<PathBuf> {
    crate::paths::control_socket_candidates()
}

pub fn listen_first_available(
    candidates: &[PathBuf],
) -> Result<(ControlSocket, PathBuf), ControlSocketError> {
    let mut last: Option<ControlSocketError> = None;
    for path in candidates {
        match ControlSocket::listen(path) {
            Ok(s) => return Ok((s, path.clone())),
            Err(e) => {
                warn!("cannot listen on {}: {}", path.display(), e);
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or(ControlSocketError::Syscall(Errno::ENOENT)))
}

const MAX_COMMAND_BYTES: usize = 4096;

const MAX_CONNECTIONS: usize = 64;

const MAX_OUTPUT_BYTES: usize = 64 * 1024;

pub(crate) const CONN_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

pub const CONN_IDLE_TIMEOUT_ENV: &str = "VELOCITY_CTL_IDLE_TIMEOUT_SECS";

pub fn parse_idle_timeout(raw: Option<&str>) -> Duration {
    match raw.and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(secs) if secs > 0 => Duration::from_secs(secs),
        _ => CONN_IDLE_TIMEOUT,
    }
}

pub fn conn_idle_timeout() -> Duration {
    parse_idle_timeout(std::env::var(CONN_IDLE_TIMEOUT_ENV).ok().as_deref())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {

    Start(String),

    Stop(String),

    Restart(String),

    Status(String),

    List,

    PowerOff,

    Reboot,

    Halt,

    Change(String),

    Reload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeRequest {

    Level(String),

    Reload,
}

impl Command {

    pub fn shutdown_action(&self) -> Option<ShutdownAction> {
        match self {
            Command::PowerOff => Some(ShutdownAction::PowerOff),
            Command::Reboot => Some(ShutdownAction::Reboot),
            Command::Halt => Some(ShutdownAction::Halt),
            _ => None,
        }
    }
}

pub fn parse_command(line: &str) -> Result<Command, String> {
    let mut it = line.split_whitespace();
    let verb = it.next().unwrap_or("").to_ascii_uppercase();
    let arg = it.next().map(str::to_string);

    if it.next().is_some() {
        return Err(format!("too many arguments: {}", line.trim()));
    }

    match (verb.as_str(), arg) {
        ("START", Some(a)) => Ok(Command::Start(a)),
        ("STOP", Some(a)) => Ok(Command::Stop(a)),
        ("RESTART", Some(a)) => Ok(Command::Restart(a)),
        ("STATUS", Some(a)) => Ok(Command::Status(a)),
        ("LIST", None) => Ok(Command::List),
        ("POWEROFF", None) => Ok(Command::PowerOff),
        ("REBOOT", None) => Ok(Command::Reboot),
        ("HALT", None) => Ok(Command::Halt),

        ("CHANGE", Some(a)) if crate::inittab::Inittab::valid_level(&a) => Ok(Command::Change(a)),
        ("CHANGE", Some(a)) => Err(format!("invalid runlevel {:?}", a)),
        ("RELOAD", None) => Ok(Command::Reload),

        ("", None) => Err("empty command".to_string()),
        _ => Err(format!("unknown command: {}", line.trim())),
    }
}

pub fn execute_command(cmd: &Command, sup: &mut Supervisor) -> String {
    match cmd {
        Command::Start(name) => {

            sup.reset_respawn_history(name);
            match sup.start(name) {
                Ok(pid) => format!("OK started {} (pid {})", name, pid),
                Err(e) => format!("ERR {}", e),
            }
        }

        Command::Stop(name) => match sup.request_stop(name) {
            Ok(()) => format!("OK stopping {}", name),
            Err(e) => format!("ERR {}", e),
        },
        Command::Restart(name) => match sup.request_restart(name) {
            Ok(()) => format!("OK restarting {}", name),
            Err(e) => format!("ERR {}", e),
        },
        Command::Status(name) => match sup.service(name) {
            Some(s) => {
                let pid = s
                    .pid
                    .map(|p| p.as_raw().to_string())
                    .unwrap_or_else(|| "-".to_string());
                format!(
                    "OK {} state={} pid={} restarts={}",
                    name, s.state(), pid, s.restart_attempts
                )
            }
            None => format!("ERR unknown service: {}", name),
        },
        Command::List => {
            let mut rows: Vec<String> = sup
                .services()
                .map(|s| {
                    let pid = s
                        .pid
                        .map(|p| p.as_raw().to_string())
                        .unwrap_or_else(|| "-".to_string());
                    format!("{} state={} pid={}", s.name, s.state(), pid)
                })
                .collect();
            rows.sort();
            if rows.is_empty() {
                "OK (no services)".to_string()
            } else {
                format!("OK {}", rows.join("; "))
            }
        }

        Command::PowerOff => "OK powering off".to_string(),
        Command::Reboot => "OK rebooting".to_string(),
        Command::Halt => "OK halting".to_string(),

        Command::Change(level) => format!("OK changing to runlevel {}", level.trim()),
        Command::Reload => "OK re-reading /etc/inittab".to_string(),
    }
}

pub fn requires_root(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::PowerOff
            | Command::Reboot
            | Command::Halt
            | Command::Change(_)
            | Command::Reload
            | Command::Start(_)
            | Command::Stop(_)
            | Command::Restart(_)
    )
}

pub fn is_privileged_peer(peer_uid: u32) -> bool {
    peer_uid == 0 || peer_uid == nix::unistd::geteuid().as_raw()
}

type CommandOutcome = (String, Option<ShutdownAction>, Option<ChangeRequest>);

fn handle_command_line(line: &str, sup: &mut Supervisor, peer_uid: u32) -> CommandOutcome {
    match parse_command(line) {
        Ok(cmd) => {
            if requires_root(&cmd) && !is_privileged_peer(peer_uid) {
                let verdict = "ERR permission denied: this socket command requires root \
                               (or the owner of this init instance)";
                return (verdict.to_string(), None, None);
            }
            let action = cmd.shutdown_action();
            let change = match &cmd {
                Command::Change(level) => Some(ChangeRequest::Level(level.trim().to_string())),
                Command::Reload => Some(ChangeRequest::Reload),
                _ => None,
            };
            (execute_command(&cmd, sup), action, change)
        }
        Err(e) => (format!("ERR {}", e), None, None),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnAction {

    Keep,

    Close,
}

pub struct ControlSocket {
    listener: Option<OwnedFd>,
    path: PathBuf,
    conns: HashMap<i32, OwnedFd>,

    in_bufs: HashMap<i32, Vec<u8>>,

    overflowing: HashMap<i32, bool>,
    out_bufs: HashMap<i32, Vec<u8>>,

    peer_uids: HashMap<i32, u32>,

    last_active: HashMap<i32, Instant>,

    dropped_conns: u64,
}

impl ControlSocket {

    pub fn disabled(path: PathBuf) -> Self {
        Self {
            listener: None,
            path,
            conns: HashMap::new(),
            in_bufs: HashMap::new(),
            overflowing: HashMap::new(),
            out_bufs: HashMap::new(),
            peer_uids: HashMap::new(),
            last_active: HashMap::new(),
            dropped_conns: 0,
        }
    }

    pub fn listen(path: &Path) -> Result<Self, ControlSocketError> {
        let listener = Self::make_listener(path)?;
        debug!("operator control socket listening on {}", path.display());
        Ok(Self {
            listener: Some(listener),
            path: path.to_path_buf(),
            conns: HashMap::new(),
            in_bufs: HashMap::new(),
            overflowing: HashMap::new(),
            out_bufs: HashMap::new(),
            peer_uids: HashMap::new(),
            last_active: HashMap::new(),
            dropped_conns: 0,
        })
    }

    fn make_listener(path: &Path) -> Result<OwnedFd, ControlSocketError> {
        let listener = socket(
            AddressFamily::Unix,
            SockType::Stream,
            SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
            None,
        )?;
        let addr = UnixAddr::new(path)?;

        match bind(listener.as_raw_fd(), &addr) {
            Ok(()) => {}
            Err(Errno::EADDRINUSE) => {

                if std::os::unix::net::UnixStream::connect(path).is_ok() {
                    return Err(ControlSocketError::Syscall(Errno::EADDRINUSE));
                }

                let _ = unlink(path);
                bind(listener.as_raw_fd(), &addr)?;
            }
            Err(e) => return Err(ControlSocketError::Syscall(e)),
        }

        fchmod(listener.as_raw_fd(), Mode::from_bits_truncate(0o600))?;
        listen(&listener, Backlog::new(16)?)?;
        Ok(listener)
    }

    pub fn ensure_listening(&mut self) -> Result<(), ControlSocketError> {
        if self.listener.is_some() && self.path.exists() {
            return Ok(());
        }
        if self.listener.is_none() {

            return Ok(());
        }
        let listener = Self::make_listener(&self.path)?;
        debug!(
            "control socket was gone from {} — re-bound",
            self.path.display()
        );
        self.listener = Some(listener);
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn listener_fd(&self) -> Option<i32> {
        self.listener.as_ref().map(AsRawFd::as_raw_fd)
    }

    pub fn has_conn(&self, fd: i32) -> bool {
        self.conns.contains_key(&fd)
    }

    pub fn conn_fd(&self, fd: i32) -> Option<&OwnedFd> {
        self.conns.get(&fd)
    }

    pub fn pending_write(&self, fd: i32) -> bool {
        self.out_bufs.get(&fd).is_some_and(|b| !b.is_empty())
    }

    pub fn accept_pending(&mut self) -> Vec<i32> {
        let mut accepted = Vec::new();
        let Some(listener) = self.listener.as_ref() else {
            return accepted;
        };
        let lfd = listener.as_raw_fd();
        loop {
            match accept4(lfd, SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC) {
                Ok(raw) => {
                    if self.conns.len() >= MAX_CONNECTIONS {

                        drop(unsafe { OwnedFd::from_raw_fd(raw) });
                        self.dropped_conns += 1;
                        if self.dropped_conns % 64 == 1 {
                            warn!(
                                "control socket: too many connections (limit {}), refusing \
                                 new clients ({} dropped so far)",
                                MAX_CONNECTIONS, self.dropped_conns
                            );
                        }
                        continue;
                    }

                    let uid = {
                        let bfd = unsafe { BorrowedFd::borrow_raw(raw) };
                        match getsockopt(&bfd, PeerCredentials) {
                            Ok(cred) => cred.uid(),
                            Err(e) => {
                                debug!("control: could not read peer credentials: {e}");

                                u32::MAX
                            }
                        }
                    };
                    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
                    self.conns.insert(raw, owned);
                    self.in_bufs.insert(raw, Vec::new());
                    self.overflowing.insert(raw, false);
                    self.out_bufs.insert(raw, Vec::new());
                    self.peer_uids.insert(raw, uid);
                    self.last_active.insert(raw, Instant::now());
                    accepted.push(raw);
                }
                Err(Errno::EAGAIN) => break,
                Err(Errno::EINTR) => continue,
                Err(e) => {
                    warn!("accept on control socket failed: {}", e);
                    break;
                }
            }
        }
        accepted
    }

    pub fn remove_conn(&mut self, fd: i32) {
        self.conns.remove(&fd);
        self.in_bufs.remove(&fd);
        self.overflowing.remove(&fd);
        self.out_bufs.remove(&fd);
        self.peer_uids.remove(&fd);
        self.last_active.remove(&fd);
    }

    pub fn expired_conns(&self, now: Instant, timeout: Duration) -> Vec<i32> {
        let mut expired: Vec<(Instant, i32)> = self
            .last_active
            .iter()
            .filter(|(_, last)| {
                now.checked_duration_since(**last)
                    .is_some_and(|idle| idle >= timeout)
            })
            .map(|(fd, last)| (*last, *fd))
            .collect();

        expired.sort_unstable();
        expired.into_iter().map(|(_, fd)| fd).collect()
    }

pub fn on_readable(
        &mut self,
        fd: i32,
        sup: &mut Supervisor,
    ) -> (ConnAction, Option<ShutdownAction>, Option<ChangeRequest>) {

let mut buf = [0u8; 4096];

        let mut action: Option<ShutdownAction> = None;
        let mut change: Option<ChangeRequest> = None;
        let mut executed = 0usize;
        let mut closed = false;

        loop {
            let n = match read(fd, &mut buf) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(n) => n,

                Err(Errno::EAGAIN) => break,
                Err(Errno::EINTR) => continue,
                Err(e) => {
                    error!("read on control connection {}: {}", fd, e);
                    closed = true;
                    break;
                }
            };
            if n == 0 {
                break;
            }

            self.last_active.insert(fd, Instant::now());

            for &b in &buf[..n] {
            let overflowing = self.overflowing.get(&fd).copied().unwrap_or(false);
            let Some(in_buf) = self.in_bufs.get_mut(&fd) else {
                return (ConnAction::Close, None, None);
            };

            if b == b'\n' {
                if overflowing {

                    in_buf.clear();
                    self.overflowing.insert(fd, false);
                    executed += 1;
                    continue;
                }

                let line = String::from_utf8_lossy(in_buf).into_owned();
                in_buf.clear();
                let peer_uid = self.peer_uids.get(&fd).copied().unwrap_or(u32::MAX);
                let (resp, cmd_action, cmd_change) = handle_command_line(&line, sup, peer_uid);
                self.queue(fd, &resp);
                executed += 1;
                action = action.or(cmd_action);
                change = change.or(cmd_change);
                continue;
            }

            if overflowing {
                continue;
            }
            if in_buf.len() >= MAX_COMMAND_BYTES {

                in_buf.clear();
                warn!("control client fd {} exceeded the command limit", fd);
                self.overflowing.insert(fd, true);
                self.queue(fd, "ERR command too long");
                executed += 1;
                continue;
            }
            in_buf.push(b);
            }
        }

        if closed {

            return if executed > 0 {

                (self.flush(fd), action, change)
            } else {
                (ConnAction::Close, None, None)
            };
        }
        if executed == 0 {

            return (ConnAction::Keep, None, None);
        }
        (self.flush(fd), action, change)
    }

fn queue(&mut self, fd: i32, response: &str) {
        if let Some(out) = self.out_bufs.get_mut(&fd) {
            if out.len() >= MAX_OUTPUT_BYTES {
                debug!(
                    "control client fd {} is not reading; dropping a reply to bound memory",
                    fd
                );
                return;
            }
            out.extend_from_slice(response.as_bytes());
            out.push(b'\n');
        }
    }

    pub fn on_writable(&mut self, fd: i32) -> ConnAction {
        self.flush(fd)
    }

    fn flush(&mut self, fd: i32) -> ConnAction {
        loop {

            {
                let Some(buf) = self.out_bufs.get_mut(&fd) else {
                    return ConnAction::Close;
                };
                if buf.is_empty() {
                    return ConnAction::Close;
                }
                let wfd = unsafe { BorrowedFd::borrow_raw(fd) };
                match write(wfd, buf) {
                    Ok(0) => return ConnAction::Close,
                    Ok(n) => {
                        buf.drain(..n);
                    }
                    Err(Errno::EAGAIN) => return ConnAction::Keep,
                    Err(Errno::EINTR) => continue,
                    Err(e) => {
                        error!("write on control connection {}: {}", fd, e);
                        return ConnAction::Close;
                    }
                }
            }

            self.last_active.insert(fd, Instant::now());
            if self.out_bufs.get(&fd).is_some_and(|b| b.is_empty()) {
                return ConnAction::Close;
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ControlSocketError {
    #[error("control socket syscall failed: {0}")]
    Syscall(#[from] Errno),
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::socket::shutdown;
use nix::sys::socket::Shutdown;
    use std::path::PathBuf;

    #[test]
    fn test_parse_command_verbs() {
        assert_eq!(
            parse_command("START ssh").unwrap(),
            Command::Start("ssh".to_string())
        );
        assert_eq!(
            parse_command("stop cron").unwrap(),
            Command::Stop("cron".to_string())
        );
        assert_eq!(
            parse_command("  restart   network  ").unwrap(),
            Command::Restart("network".to_string())
        );
        assert_eq!(
            parse_command("STATUS ssh").unwrap(),
            Command::Status("ssh".to_string())
        );
        assert_eq!(parse_command("LIST").unwrap(), Command::List);
        assert_eq!(parse_command("list").unwrap(), Command::List);

        assert_eq!(parse_command("POWEROFF").unwrap(), Command::PowerOff);
        assert_eq!(parse_command("reboot").unwrap(), Command::Reboot);
        assert_eq!(parse_command("HALT").unwrap(), Command::Halt);

        assert_eq!(
            parse_command("CHANGE 3").unwrap(),
            Command::Change("3".to_string())
        );
        assert_eq!(
            parse_command("change S").unwrap(),
            Command::Change("S".to_string())
        );
    }

    #[test]
    fn test_change_command_acknowledges_but_has_no_shutdown_action() {
        let mut sup = Supervisor::new();
        assert_eq!(
            execute_command(&Command::Change("3".into()), &mut sup),
            "OK changing to runlevel 3"
        );
        assert_eq!(Command::Change("0".into()).shutdown_action(), None);
    }

    #[test]
    fn test_shutdown_verbs_reject_arguments() {
        assert!(parse_command("POWEROFF now").is_err(), "no arguments");
        assert!(parse_command("REBOOT /").is_err());
        assert!(parse_command("HALT tty1").is_err());
        assert!(parse_command("POWEROFF ssh").is_err());
    }

    #[test]
    fn test_shutdown_action_mapping() {
        use crate::models::ShutdownAction;
        assert_eq!(
            Command::PowerOff.shutdown_action(),
            Some(ShutdownAction::PowerOff)
        );
        assert_eq!(
            Command::Reboot.shutdown_action(),
            Some(ShutdownAction::Reboot)
        );
        assert_eq!(Command::Halt.shutdown_action(), Some(ShutdownAction::Halt));
        assert_eq!(Command::List.shutdown_action(), None);
        assert_eq!(Command::Start("ssh".into()).shutdown_action(), None);
    }

    #[test]
    fn test_execute_shutdown_verbs_acknowledge() {
        let mut sup = Supervisor::new();
        assert_eq!(
            execute_command(&Command::PowerOff, &mut sup),
            "OK powering off"
        );
        assert_eq!(execute_command(&Command::Reboot, &mut sup), "OK rebooting");
        assert_eq!(execute_command(&Command::Halt, &mut sup), "OK halting");
    }

    #[test]
    fn test_parse_command_rejects_garbage() {
        assert!(parse_command("").is_err());
        assert!(parse_command("   ").is_err());
        assert!(parse_command("BANANA ssh").is_err());
        assert!(parse_command("START").is_err(), "missing argument");
        assert!(
            parse_command("START ssh now").is_err(),
            "too many arguments"
        );
        assert!(parse_command("LIST extra").is_err());
        assert!(
            parse_command("START ssh STOP cron").is_err(),
            "multiple commands"
        );
        assert!(
            parse_command("START\tssh\trestart\n").is_err(),
            "mixed verbs"
        );
    }

    #[test]
    fn test_execute_command_status_and_unknown() {
        let mut sup = Supervisor::new();
        let svc = crate::models::Service::new("ssh".to_string(), PathBuf::from("/etc/init.d/ssh"));
        sup.register(svc);

        let status = execute_command(&Command::Status("ssh".to_string()), &mut sup);
        assert!(status.starts_with("OK ssh state="), "got: {}", status);

        let missing = execute_command(&Command::Status("nope".to_string()), &mut sup);
        assert!(
            missing.starts_with("ERR unknown service"),
            "got: {}",
            missing
        );

        let list = execute_command(&Command::List, &mut sup);
        assert!(list.contains("ssh state="), "got: {}", list);
    }

    fn control_with_conn() -> (ControlSocket, OwnedFd) {
        use nix::sys::socket::{socketpair, AddressFamily, SockFlag, SockType};

        let (server, client) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        )
        .expect("socketpair");
        let fd = server.as_raw_fd();
        let mut ctl = ControlSocket::disabled(PathBuf::from("/nonexistent/test.sock"));
        ctl.conns.insert(fd, server);
        ctl.in_bufs.insert(fd, Vec::new());
        ctl.overflowing.insert(fd, false);
        ctl.out_bufs.insert(fd, Vec::new());
        ctl.peer_uids.insert(fd, 0);
        ctl.last_active.insert(fd, Instant::now());
        (ctl, client)
    }

    fn control_with_unprivileged_conn(uid: u32) -> (ControlSocket, OwnedFd) {
        use nix::sys::socket::{socketpair, AddressFamily, SockFlag, SockType};

        let (server, client) = socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC,
        )
        .expect("socketpair");
        let fd = server.as_raw_fd();
        let mut ctl = ControlSocket::disabled(PathBuf::from("/nonexistent/test.sock"));
        ctl.conns.insert(fd, server);
        ctl.in_bufs.insert(fd, Vec::new());
        ctl.overflowing.insert(fd, false);
        ctl.out_bufs.insert(fd, Vec::new());
        ctl.peer_uids.insert(fd, uid);
        ctl.last_active.insert(fd, Instant::now());
        (ctl, client)
    }

    fn send(fd: i32, bytes: &[u8]) {
        use std::os::fd::BorrowedFd;
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let mut sent = 0;
        while sent < bytes.len() {
            match write(borrowed, &bytes[sent..]) {
                Ok(0) => panic!("short write"),
                Ok(n) => sent += n,
                Err(Errno::EINTR) => continue,
                Err(Errno::EAGAIN) => std::thread::sleep(std::time::Duration::from_millis(5)),
                Err(e) => panic!("write to socketpair: {e}"),
            }
        }
    }

    fn drain(fd: i32) -> String {
        let mut out = String::new();
        let mut buf = [0u8; 4096];
        loop {
            match read(fd, &mut buf) {
                Ok(0) => break,
                Ok(n) => out.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(Errno::EAGAIN) | Err(Errno::EINTR) => break,
                Err(_) => break,
            }
        }
        out
    }

    fn supervisor_with_ssh() -> Supervisor {
        let mut sup = Supervisor::new();
        sup.register(crate::models::Service::new(
            "ssh".to_string(),
            PathBuf::from("/etc/init.d/ssh"),
        ));
        sup
    }

    #[test]
    fn test_on_readable_executes_every_pipelined_command() {
        let (mut ctl, client) = control_with_conn();
        let mut sup = supervisor_with_ssh();
        let fd = client.as_raw_fd();
        let srv = ctl.conns.keys().next().copied().expect("conn");

        send(fd, b"STATUS ssh\nLIST\n");
        let (verdict, action, change) = ctl.on_readable(srv, &mut sup);

        assert_eq!(verdict, ConnAction::Close);
        assert!(action.is_none() && change.is_none());
        let reply = drain(fd);
        assert_eq!(reply.lines().count(), 2, "both replies: {reply:?}");
        assert!(reply.lines().next().unwrap().starts_with("OK ssh state="));
        assert!(reply.lines().nth(1).unwrap().contains("ssh state="));
    }

    #[test]
    fn test_on_readable_keeps_a_command_split_across_reads() {
        let (mut ctl, client) = control_with_conn();
        let mut sup = supervisor_with_ssh();
        let fd = client.as_raw_fd();
        let srv = ctl.conns.keys().next().copied().expect("conn");

        send(fd, b"STATUS ");
        let (verdict, _, _) = ctl.on_readable(srv, &mut sup);
        assert_eq!(verdict, ConnAction::Keep, "incomplete line: wait");
        assert!(drain(fd).is_empty(), "no reply before the line ends");

        send(fd, b"ssh\n");
        let (verdict, _, _) = ctl.on_readable(srv, &mut sup);
        assert_eq!(verdict, ConnAction::Close);
        assert!(drain(fd).starts_with("OK ssh state="));
    }

    #[test]
    fn test_over_long_line_is_rejected_and_the_next_command_still_runs() {
        let (mut ctl, client) = control_with_conn();
        let mut sup = supervisor_with_ssh();
        let fd = client.as_raw_fd();
        let srv = ctl.conns.keys().next().copied().expect("conn");

        let mut payload = vec![b'A'; MAX_COMMAND_BYTES + 64];
        payload.push(b'\n');
        payload.extend_from_slice(b"LIST\n");
        send(fd, &payload);

        let mut reply = String::new();
        for _ in 0..8 {
            if ctl.on_readable(srv, &mut sup).0 == ConnAction::Close {
                break;
            }
            reply.push_str(&drain(fd));
        }
        reply.push_str(&drain(fd));

        assert!(reply.contains("ERR command too long"), "{reply:?}");
        assert!(
            reply.lines().any(|l| l.contains("ssh state=")),
            "the command after the rejected line must still run: {reply:?}"
        );
        assert!(
            !reply.contains("AAAAAAAAAA"),
            "the rejected payload must not be echoed: {reply:?}"
        );
    }

    #[test]
    fn test_stop_and_restart_are_acknowledged_not_awaited() {

        let mut sup = supervisor_with_ssh();
        assert_eq!(
            execute_command(&Command::Stop("ssh".to_string()), &mut sup),
            "OK stopping ssh"
        );
        assert_eq!(
            execute_command(&Command::Restart("ssh".to_string()), &mut sup),
            "OK restarting ssh"
        );
    }

    #[test]
    fn test_stop_of_an_unknown_service_is_an_error() {
        let mut sup = Supervisor::new();
        let reply = execute_command(&Command::Stop("ghost".to_string()), &mut sup);
        assert!(reply.starts_with("ERR unknown service"), "got: {reply}");
    }

    #[test]
    fn test_connection_limit_is_enforced() {

        let dir = std::env::temp_dir().join(format!("velocity-ctl-limit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("limit.sock");
        let _ = std::fs::remove_file(&path);

        let mut ctl = ControlSocket::listen(&path).expect("listener");
        let mut clients = Vec::new();
        for _ in 0..(MAX_CONNECTIONS + 5) {
            clients.push(std::os::unix::net::UnixStream::connect(&path).expect("connect"));

            ctl.accept_pending();
        }
        ctl.accept_pending();

        assert!(
            ctl.conns.len() <= MAX_CONNECTIONS,
            "tracked connections must stop at the ceiling, got {}",
            ctl.conns.len()
        );
        assert!(
            ctl.dropped_conns >= 1,
            "the surplus clients must be dropped and counted, got {}",
            ctl.dropped_conns
        );

        let fd = *ctl.conns.keys().next().expect("a tracked connection");
        ctl.remove_conn(fd);
        let before = ctl.conns.len();
        clients.push(std::os::unix::net::UnixStream::connect(&path).expect("connect"));
        ctl.accept_pending();
        assert!(
            ctl.conns.len() > before,
            "the freed slot must be reusable ({} -> {})",
            before,
            ctl.conns.len()
        );

        drop(clients);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_first_shutdown_verb_in_a_batch_wins() {
        let (mut ctl, client) = control_with_conn();
        let mut sup = supervisor_with_ssh();
        let fd = client.as_raw_fd();
        let srv = ctl.conns.keys().next().copied().expect("conn");

        send(fd, b"LIST\nPOWEROFF\nHALT\n");
        let (verdict, action, change) = ctl.on_readable(srv, &mut sup);

        assert_eq!(verdict, ConnAction::Close);
        assert_eq!(action, Some(crate::models::ShutdownAction::PowerOff));
        assert!(change.is_none());
    }

    #[test]
    fn test_half_closed_client_still_gets_reply_and_action() {

        let (mut ctl, client) = control_with_conn();
        let mut sup = supervisor_with_ssh();
        let fd = client.as_raw_fd();
        let srv = ctl.conns.keys().next().copied().expect("conn");

        send(fd, b"POWEROFF\n");
        shutdown(client.as_raw_fd(), Shutdown::Write).expect("half-close");
        let (verdict, action, change) = ctl.on_readable(srv, &mut sup);

        assert_eq!(
            action,
            Some(crate::models::ShutdownAction::PowerOff),
            "the shutdown must survive the half-close"
        );
        assert!(change.is_none());
        assert_eq!(drain(fd).trim(), "OK powering off");
        assert!(matches!(verdict, ConnAction::Close | ConnAction::Keep));
    }

    #[test]
    fn test_half_close_without_a_command_just_closes() {

        let (mut ctl, client) = control_with_conn();
        let mut sup = supervisor_with_ssh();
        let srv = ctl.conns.keys().next().copied().expect("conn");

        shutdown(client.as_raw_fd(), Shutdown::Write).expect("half-close");
        let (verdict, action, change) = ctl.on_readable(srv, &mut sup);

        assert_eq!(verdict, ConnAction::Close);
        assert!(action.is_none() && change.is_none());
    }

    #[test]
    fn test_output_buffer_is_bounded() {

        let (mut ctl, client) = control_with_conn();
        let mut sup = supervisor_with_ssh();
        let fd = client.as_raw_fd();
        let srv = ctl.conns.keys().next().copied().expect("conn");

        let batch = "LIST\n".repeat(1024);
        for _ in 0..64 {
            send(fd, batch.as_bytes());
            let _ = ctl.on_readable(srv, &mut sup);

            let _ = drain(fd);
        }
        let buffered = ctl.out_bufs.get(&srv).map(Vec::len).unwrap_or(0);
        assert!(
            buffered <= MAX_OUTPUT_BYTES + batch.len(),
            "reply buffer grew to {buffered} bytes, past the cap"
        );
    }

    #[test]
    fn test_requires_root_covers_every_mutating_verb() {

        for verb in [
            Command::PowerOff,
            Command::Reboot,
            Command::Halt,
            Command::Change("2".to_string()),
            Command::Reload,
            Command::Start("ssh".to_string()),
            Command::Stop("ssh".to_string()),
            Command::Restart("ssh".to_string()),
        ] {
            assert!(requires_root(&verb), "{verb:?} mutates supervised state");
        }
        for verb in [
            Command::Status("ssh".to_string()),
            Command::List,
        ] {
            assert!(!requires_root(&verb), "{verb:?} must stay usable by a monitor");
        }
    }

    fn stranger_uid() -> u32 {
        let own = nix::unistd::geteuid().as_raw();
        if own != 60_000 { 60_000 } else { 60_001 }
    }

    #[test]
    fn test_power_verbs_are_refused_for_a_stranger_peer() {

        let (mut ctl, client) = control_with_unprivileged_conn(stranger_uid());
        let mut sup = supervisor_with_ssh();
        let fd = client.as_raw_fd();
        let srv = ctl.conns.keys().next().copied().expect("conn");

        send(fd, b"POWEROFF\nREBOOT\nHALT\nCHANGE 2\n");
        let (verdict, action, change) = ctl.on_readable(srv, &mut sup);

        assert_eq!(verdict, ConnAction::Close);
        assert!(
            action.is_none() && change.is_none(),
            "a non-root peer must trigger no machine action: {action:?} {change:?}"
        );
        let reply = drain(fd);
        assert_eq!(reply.lines().count(), 4, "every verb gets its own refusal: {reply:?}");
        for line in reply.lines() {
            assert!(
                line.starts_with("ERR") && line.contains("requires root"),
                "refusal must be explicit and actionable: {line:?}"
            );
        }
    }

    #[test]
    fn test_unknown_peer_identity_is_never_treated_as_root() {

        let (mut ctl, client) = control_with_conn();
        let mut sup = supervisor_with_ssh();
        let fd = client.as_raw_fd();
        let srv = ctl.conns.keys().next().copied().expect("conn");
        ctl.peer_uids.remove(&srv);

        send(fd, b"POWEROFF\n");
        let (_, action, _) = ctl.on_readable(srv, &mut sup);
        assert!(
            action.is_none(),
            "unknown identity must not halt the machine"
        );
        assert!(drain(fd).contains("requires root"));
    }

    #[test]
    fn test_mutating_verbs_are_refused_for_a_stranger_peer() {

        let (mut ctl, client) = control_with_unprivileged_conn(stranger_uid());
        let mut sup = supervisor_with_ssh();
        let fd = client.as_raw_fd();
        let srv = ctl.conns.keys().next().copied().expect("conn");

        send(fd, b"START ssh\nSTOP ssh\nRESTART ssh\nRELOAD\n");
        let (verdict, action, change) = ctl.on_readable(srv, &mut sup);

        assert_eq!(verdict, ConnAction::Close);
        assert!(
            action.is_none() && change.is_none(),
            "a stranger must trigger no action: {action:?} {change:?}"
        );
        let reply = drain(fd);
        assert_eq!(reply.lines().count(), 4, "every verb gets its own refusal: {reply:?}");
        for line in reply.lines() {
            assert!(
                line.starts_with("ERR") && line.contains("requires root"),
                "refusal must be explicit and actionable: {line:?}"
            );
        }
    }

    #[test]
    fn test_owner_may_use_mutating_verbs() {

        let own = nix::unistd::geteuid().as_raw();
        if own == 0 {

            return;
        }
        let (mut ctl, client) = control_with_unprivileged_conn(own);
        let mut sup = supervisor_with_ssh();
        let fd = client.as_raw_fd();
        let srv = ctl.conns.keys().next().copied().expect("conn");

        send(fd, b"STATUS ssh\n");
        let (verdict, _, _) = ctl.on_readable(srv, &mut sup);
        assert_eq!(verdict, ConnAction::Close);
        assert!(drain(fd).starts_with("OK ssh state="));
    }

    #[test]
    fn test_read_only_commands_still_work_for_a_stranger_peer() {
        let (mut ctl, client) = control_with_unprivileged_conn(stranger_uid());
        let mut sup = supervisor_with_ssh();
        let fd = client.as_raw_fd();
        let srv = ctl.conns.keys().next().copied().expect("conn");

        send(fd, b"STATUS ssh\nLIST\n");
        let (verdict, action, change) = ctl.on_readable(srv, &mut sup);

        assert_eq!(verdict, ConnAction::Close);
        assert!(action.is_none() && change.is_none());
        let reply = drain(fd);
        assert!(reply.lines().next().unwrap().starts_with("OK ssh state="));
        assert!(reply.lines().nth(1).unwrap().contains("ssh state="));
    }

    #[test]
    fn test_expired_conns_reports_only_the_quiet_ones_oldest_first() {
        let (mut ctl, _client) = control_with_conn();
        let srv = ctl.conns.keys().next().copied().expect("conn");
        let now = Instant::now();
        let timeout = Duration::from_secs(10);

        ctl.last_active.insert(srv, now - timeout + Duration::from_millis(100));
        assert!(
            ctl.expired_conns(now, timeout).is_empty(),
            "a connection idle just under the timeout must be kept"
        );

        ctl.last_active.insert(srv, now - timeout - Duration::from_millis(1));
        assert_eq!(ctl.expired_conns(now, timeout), vec![srv]);
    }

    #[test]
    fn test_conn_idle_timeout_env_override_is_honoured_and_sane() {

        assert_eq!(parse_idle_timeout(Some("1")), Duration::from_secs(1));
        assert_eq!(parse_idle_timeout(Some(" 30 ")), Duration::from_secs(30));

        assert_eq!(parse_idle_timeout(Some("0")), CONN_IDLE_TIMEOUT);
        assert_eq!(parse_idle_timeout(Some("soon")), CONN_IDLE_TIMEOUT);
        assert_eq!(parse_idle_timeout(None), CONN_IDLE_TIMEOUT);
    }
}
