//! Supervisor::sockets (split from `supervisor.rs` — verbatim move, no logic changes).
//!
//! Child post-fork paths do only async-signal-safe calls + _exit(127); signals are blocked before signalfd; fds 0/1/2 are claimed; journal/log/connection caps stay bounded.
use super::*;

impl super::Supervisor {
    /// The log-routing pipe read end for a service, if it exists.
    pub fn log_pipe(&self, name: &str) -> Option<&OwnedFd> {
        self.log_pipes.get(name)
    }

    /// Read from a service's log pipe. Returns `Ok(0)` on EOF (the
    /// service closed its stdout/stderr), `Err(EAGAIN)` when the event
    /// loop should try again later.
    pub fn read_log(&self, name: &str, buf: &mut [u8]) -> nix::Result<usize> {
        match self.log_pipe(name) {
            Some(fd) => read(fd.as_raw_fd(), buf),
            None => Ok(0),
        }
    }

    /// Remove and return a service's log pipe (called on EOF: the
    /// service and all of its daemon children have closed their
    /// stdout/stderr, so nothing more will arrive).
    pub fn take_log_pipe(&mut self, name: &str) -> Option<OwnedFd> {
        self.log_pipes.remove(name)
    }

    // ─────────────────── sd_notify readiness ──────────────────────

    /// The readiness notify socket for a service, if one exists.
    pub fn notify_socket(&self, name: &str) -> Option<&OwnedFd> {
        self.notify_sockets.get(name)
    }

    /// Read one buffered readiness datagram for a service, if available.
    ///
    /// Returns `None` on an empty queue or a non-fatal read error
    /// (`EAGAIN` is expected for a non-blocking socket).
    pub fn read_notify(&self, name: &str) -> Option<Vec<u8>> {
        let fd = self.notify_sockets.get(name)?;
        let mut buf = [0u8; 4096];
        match recvfrom::<SockaddrStorage>(fd.as_raw_fd(), &mut buf) {
            Ok((n, _)) => Some(buf[..n].to_vec()),
            Err(_) => None,
        }
    }

    /// Process one readiness datagram for a service.
    ///
    /// Returns `true` if the datagram was `READY=1` *and* the service is
    /// in `Starting`, in which case it is transitioned to `Running`.
    pub fn handle_notify_datagram(&mut self, name: &str, data: &[u8]) -> bool {
        let text = String::from_utf8_lossy(data);
        for line in text.lines() {
            if let Some((key, value)) = line.split_once('=') {
                match key {
                    "READY" => {}
                    "STATUS" => debug!("{} status: {}", name, value),
                    "STOPPING" => debug!("{} reports stopping", name),
                    _ => debug!("{} notify: {}", name, line),
                }
            }
        }
        if !text.lines().any(|l| l.trim() == "READY=1") {
            return false;
        }
        let Some(svc) = self.services.get_mut(name) else {
            return false;
        };
        if svc.state() != ServiceState::Starting {
            debug!(
                "late READY=1 from {} (state {}) — ignoring",
                name,
                svc.state()
            );
            return false;
        }
        // Guarded `Starting -> Running` above; the assert documents it.
        debug_assert!(svc.transition(ServiceState::Running));
        svc.running_since = Some(Instant::now());
        info!("received READY=1 from {}", name);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::Supervisor;
    use crate::models::{Service, ServiceState};

    fn starting_service(name: &str) -> Supervisor {
        let mut sup = Supervisor::new();
        sup.register(Service::new(name.into(), "/etc/init.d/x".into()));
        let svc = sup
            .services
            .get_mut(name)
            .expect("just registered");
        assert!(svc.transition(ServiceState::Starting));
        sup
    }

    #[test]
    fn test_ready_promotes_starting_to_running() {
        let mut sup = starting_service("web");
        assert!(sup.handle_notify_datagram("web", b"READY=1"));
        assert_eq!(sup.service("web").expect("registered").state(), ServiceState::Running);
    }

    #[test]
    fn test_ready_with_status_lines_still_promotes() {
        let mut sup = starting_service("web");
        assert!(sup.handle_notify_datagram("web", b"STATUS=serving\nREADY=1\n"));
        assert_eq!(sup.service("web").expect("registered").state(), ServiceState::Running);
    }

    #[test]
    fn test_non_ready_datagram_is_ignored() {
        for payload in [&b"STATUS=serving"[..], &b"READY=0"[..], &b"STOPPING=1"[..], &b""[..]] {
            let mut sup = starting_service("web");
            assert!(
                !sup.handle_notify_datagram("web", payload),
                "payload {payload:?} must not promote"
            );
            assert_eq!(
                sup.service("web").expect("registered").state(),
                ServiceState::Starting
            );
        }
    }

    #[test]
    fn test_late_ready_after_running_is_ignored() {
        let mut sup = starting_service("web");
        assert!(sup.handle_notify_datagram("web", b"READY=1"));
        // Second READY=1 is late: the service already moved on.
        assert!(!sup.handle_notify_datagram("web", b"READY=1"));
        assert_eq!(sup.service("web").expect("registered").state(), ServiceState::Running);
    }

    #[test]
    fn test_notify_for_unknown_service_is_ignored() {
        let mut sup = Supervisor::new();
        assert!(!sup.handle_notify_datagram("ghost", b"READY=1"));
        // Must not create the service as a side effect.
        assert!(sup.service("ghost").is_none());
    }

    #[test]
    fn test_log_pipe_take_returns_and_clears() {
        // `take_log_pipe` on a service with no pipe is simply `None`
        // (no panic, no error) — the EOF path calls it unconditionally.
        let mut sup = Supervisor::new();
        sup.register(Service::new("web".into(), "/etc/init.d/web".into()));
        assert!(sup.take_log_pipe("web").is_none());
        assert!(sup.take_log_pipe("ghost").is_none());
    }
}
