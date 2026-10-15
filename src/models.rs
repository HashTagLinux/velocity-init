use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use nix::unistd::Pid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceState {

    Stopped,

    Starting,

    Running,

    Stopping,

    Failed,
}

impl std::fmt::Display for ServiceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => write!(f, "stopped"),
            Self::Starting => write!(f, "starting"),
            Self::Running => write!(f, "running"),
            Self::Stopping => write!(f, "stopping"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

impl ServiceState {

    pub fn can_transition_to(self, next: Self) -> bool {
        use ServiceState::*;
        if self == next {
            return true;
        }
        if next == Failed {
            return true;
        }
        match (self, next) {
            (Stopped, Starting) => true,
            (Starting, Running) | (Starting, Stopping) | (Starting, Stopped) => true,
            (Running, Stopping) | (Running, Starting) | (Running, Stopped) => true,
            (Stopping, Stopped) | (Stopping, Starting) => true,
            (Failed, Starting) | (Failed, Stopped) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemState {

    Booting,

    Running,

    ShuttingDown,
}

impl std::fmt::Display for SystemState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Booting => write!(f, "booting"),
            Self::Running => write!(f, "running"),
            Self::ShuttingDown => write!(f, "shutting-down"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownAction {

    PowerOff,

    Reboot,

    Halt,
}

impl std::fmt::Display for ShutdownAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PowerOff => write!(f, "poweroff"),
            Self::Reboot => write!(f, "reboot"),
            Self::Halt => write!(f, "halt"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyType {
    Requires,
    ShouldStart,
    #[allow(dead_code)]
    Provides,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestartPolicy {

    Always,

    #[default]
    OnFailure,

    NoRestart,
}

impl std::fmt::Display for RestartPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Always => write!(f, "always"),
            Self::OnFailure => write!(f, "on-failure"),
            Self::NoRestart => write!(f, "no-restart"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketKind {

    Tcp(u16),

    Unix(PathBuf),
}

impl SocketKind {

    pub fn parse(token: &str) -> Option<Self> {
        let (kind, rest) = token.split_once(':')?;
        match kind {
            "tcp" => rest.parse::<u16>().ok().map(SocketKind::Tcp),
            "unix" if !rest.is_empty() => Some(SocketKind::Unix(PathBuf::from(rest))),
            _ => None,
        }
    }
}

impl std::fmt::Display for SocketKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp(port) => write!(f, "tcp:{}", port),
            Self::Unix(path) => write!(f, "unix:{}", path.display()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LsbHeader {

    pub provides: Vec<String>,

    pub required_start: Vec<String>,

    pub required_stop: Vec<String>,

    pub should_start: Vec<String>,

    pub should_stop: Vec<String>,

    pub default_start: Vec<String>,

    pub default_stop: Vec<String>,

    pub short_description: Option<String>,

    pub description: Option<String>,

    pub sockets: Vec<SocketKind>,

    pub limits: HashMap<String, String>,

    pub env_files: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Service {

    pub name: String,

    pub script_path: PathBuf,

    pub header: Option<LsbHeader>,

    state: ServiceState,

    pub pid: Option<Pid>,

    pub running_since: Option<Instant>,

    pub restart_attempts: u32,

    pub restart_pending: bool,

    pub restart_policy: RestartPolicy,

    pub restart_after_stop: bool,

    pub cmd: Option<Vec<String>>,
}

impl Service {

    pub fn state(&self) -> ServiceState {
        self.state
    }

    pub fn transition(&mut self, next: ServiceState) -> bool {
        if self.state.can_transition_to(next) {
            self.state = next;
            true
        } else {
            false
        }
    }

    pub fn new(name: String, script_path: PathBuf) -> Self {
        Self {
            name,
            script_path,
            header: None,
            state: ServiceState::Stopped,
            pid: None,
            running_since: None,
            restart_attempts: 0,
            restart_pending: false,
            restart_policy: RestartPolicy::OnFailure,
            restart_after_stop: false,
            cmd: None,
        }
    }

    pub fn with_header(name: String, script_path: PathBuf, header: LsbHeader) -> Self {
        let mut svc = Self::new(name, script_path);
        svc.header = Some(header);
        svc
    }

    pub fn builtin(
        name: String,
        cmd: Vec<String>,
        restart_policy: RestartPolicy,
        header: LsbHeader,
    ) -> Self {
        let script_path = PathBuf::from(format!("/etc/init.d/{}", name));
        Self {
            name,
            script_path,
            header: Some(header),
            state: ServiceState::Stopped,
            pid: None,
            running_since: None,
            restart_attempts: 0,
            restart_pending: false,
            restart_policy,
            restart_after_stop: false,
            cmd: Some(cmd),
        }
    }

    pub fn builtin_getty(executable: &str) -> Self {
        let header = LsbHeader {
            short_description: Some("login prompt on the console".to_string()),
            provides: vec!["$console".to_string()],
            ..Default::default()
        };
        Service::builtin(
            "getty@console".to_string(),
            vec![
                executable.to_string(),
                "-a".to_string(),
                "root".to_string(),
                "console".to_string(),
                "115200".to_string(),
                "linux".to_string(),
            ],
            RestartPolicy::Always,
            header,
        )
    }
}

pub fn is_getty_service(svc: &Service) -> bool {
    if svc.name == "getty" || svc.name.starts_with("getty@") || svc.name.starts_with("getty-") {
        return true;
    }
    if let Some(header) = svc.header.as_ref() {
        if header.provides.iter().any(|p| p == "$console") {
            return true;
        }
    }
    false
}

pub fn add_builtin_getty_if_missing(services: &mut Vec<Service>, executable: &str) -> bool {
    if services.iter().any(is_getty_service) {
        return false;
    }
    services.push(Service::builtin_getty(executable));
    true
}

pub const DEFAULT_INIT_DIR: &str = "/etc/init.d";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BootMode {

    #[default]
    Auto,

    Native,

    Sysv,
}

impl std::fmt::Display for BootMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
            Self::Native => write!(f, "native"),
            Self::Sysv => write!(f, "sysv"),
        }
    }
}

impl BootMode {

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "native" | "lsb" => Ok(Self::Native),
            "sysv" | "sysvinit" => Ok(Self::Sysv),
            other => Err(format!(
                "unknown boot mode: {other} (expected auto, native or sysv)"
            )),
        }
    }

    pub fn resolve(self, inittab_present: bool, init_dir: &str) -> Self {
        match self {
            Self::Native => Self::Native,
            Self::Sysv => Self::Sysv,
            Self::Auto => {
                if inittab_present {
                    Self::Sysv
                } else {
                    Self::Native
                }
            }
        }
        .apply_init_dir_hint(init_dir, self)
    }

    fn apply_init_dir_hint(self, init_dir: &str, requested: Self) -> Self {
        if requested == Self::Auto && self == Self::Sysv && !is_default_init_dir(init_dir) {
            Self::Native
        } else {
            self
        }
    }
}

pub fn is_default_init_dir(dir: &str) -> bool {
    normalize_init_dir(dir) == DEFAULT_INIT_DIR
}

fn normalize_init_dir(dir: &str) -> String {
    let absolute = dir.starts_with('/');
    let parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();

    if parts.is_empty() {
        return if absolute { "/".to_string() } else { String::new() };
    }
    let mut out = String::new();
    if absolute {
        out.push('/');
    }
    out.push_str(&parts.join("/"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_restart_policy_defaults() {
        let svc = Service::new("ssh".into(), PathBuf::from("/etc/init.d/ssh"));
        assert_eq!(svc.restart_policy, RestartPolicy::OnFailure);
        assert!(svc.cmd.is_none());
    }

    #[test]
    fn test_builtin_getty_shape() {
        let g = Service::builtin_getty("/sbin/agetty");
        assert_eq!(g.name, "getty@console");
        assert_eq!(g.restart_policy, RestartPolicy::Always);
        let cmd = g.cmd.expect("getty has explicit cmd");
        assert_eq!(cmd[0], "/sbin/agetty");
        assert_eq!(cmd[1], "-a");
        assert!(g.header.unwrap().provides.contains(&"$console".to_string()));
    }

    #[test]
    fn test_state_machine_rejects_stopped_to_stopping() {
        use ServiceState::*;

        assert!(!Stopped.can_transition_to(Stopping));
        let mut svc = Service::new("x".into(), PathBuf::from("/etc/init.d/x"));
        assert_eq!(svc.state(), Stopped);
        assert!(!svc.transition(Stopping));
        assert_eq!(svc.state(), Stopped, "rejected edge must not move");
    }

    #[test]
    fn test_state_machine_legal_lifecycle() {
        use ServiceState::*;

        let edges = [
            (Stopped, Starting),
            (Starting, Running),
            (Running, Stopping),
            (Stopping, Stopped),
            (Stopped, Starting),
            (Starting, Stopped),
            (Stopped, Starting),
            (Starting, Stopping),
            (Stopping, Starting),
            (Starting, Running),
            (Running, Starting),
            (Starting, Running),
            (Running, Stopped),
        ];
        let mut svc = Service::new("x".into(), PathBuf::from("/etc/init.d/x"));
        for (from, to) in edges {
            assert_eq!(svc.state(), from, "precondition");
            assert!(svc.transition(to), "{from:?} -> {to:?} must be legal");
        }

        for s in [Stopped, Starting, Running, Stopping, Failed] {
            let mut svc = Service::new("x".into(), PathBuf::from("/etc/init.d/x"));
            match s {
                Stopped => {}
                Starting => assert!(svc.transition(Starting)),
                Running => {
                    assert!(svc.transition(Starting));
                    assert!(svc.transition(Running));
                }
                Stopping => {
                    assert!(svc.transition(Starting));
                    assert!(svc.transition(Stopping));
                }
                Failed => assert!(svc.transition(Failed)),
            }
            assert_eq!(svc.state(), s);
            assert!(svc.transition(s), "{s:?} -> {s:?} must hold");
            assert_eq!(svc.state(), s);
        }

        let mut svc = Service::new("x".into(), PathBuf::from("/etc/init.d/x"));
        assert!(svc.transition(Starting));
        assert!(svc.transition(Failed));
        assert_eq!(svc.state(), Failed);
        assert!(svc.transition(Starting));

        for (from, to) in [
            (Stopped, Running),
            (Stopped, Failed),
            (Stopping, Running),
            (Failed, Running),
            (Failed, Stopping),
            (Running, Failed),
        ] {

            let legal = to == Failed;
            assert_eq!(
                from.can_transition_to(to),
                legal,
                "{from:?} -> {to:?}"
            );
        }
        assert!(!Failed.can_transition_to(Stopping));
        assert!(!Failed.can_transition_to(Running));
    }

    #[test]
    fn test_add_builtin_getty_if_missing() {
        let mut services = Vec::new();
        assert!(add_builtin_getty_if_missing(&mut services, "/sbin/agetty"));
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].name, "getty@console");

        let mut services = vec![Service::new("getty@tty1".into(), PathBuf::from("/x"))];
        assert!(!add_builtin_getty_if_missing(&mut services, "/sbin/agetty"));
        assert_eq!(services.len(), 1);
    }

    #[test]
    fn test_getty_detection_is_narrow_not_substring() {
        let mk = |name: &str| Service::new(name.into(), PathBuf::from("/x"));

        assert!(is_getty_service(&mk("getty")));
        assert!(is_getty_service(&mk("getty@tty1")));
        assert!(is_getty_service(&mk("getty@console")));
        assert!(is_getty_service(&mk("getty-serial")));

        assert!(!is_getty_service(&mk("forgetty")));
        assert!(!is_getty_service(&mk("mygettyd")));
        assert!(!is_getty_service(&mk("ssh")));

        let mut svc = Service::new("console-login".into(), PathBuf::from("/x"));
        svc.header = Some(LsbHeader {
            provides: vec!["$console".to_string()],
            ..Default::default()
        });
        assert!(is_getty_service(&svc));

        let mut services = vec![mk("forgetty")];
        assert!(add_builtin_getty_if_missing(&mut services, "/sbin/agetty"));
        assert_eq!(services.len(), 2);
    }

    #[test]
    fn test_is_default_init_dir_normalizes_separators() {
        assert!(is_default_init_dir("/etc/init.d"));
        assert!(is_default_init_dir("/etc/init.d/"));
        assert!(is_default_init_dir("/etc/init.d///"));
        assert!(is_default_init_dir("/etc//init.d"));
        assert!(is_default_init_dir("/etc//init.d/"));
        assert!(!is_default_init_dir("/etc/init.d.d"));
        assert!(!is_default_init_dir("/etc/init.d2"));
        assert!(!is_default_init_dir("etc/init.d"));
        assert!(!is_default_init_dir("/"));
        assert!(!is_default_init_dir(""));
    }
}
