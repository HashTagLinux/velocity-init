use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use log::{debug, warn};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

pub const CGROUP_V2_ROOT: &str = "/sys/fs/cgroup";

pub const VELOCITY_ROOT_NAME: &str = "velocity-init";

pub const CONTROLLERS: &[&str] = &["memory", "cpu"];

pub fn cgroup_root() -> PathBuf {
    Path::new(CGROUP_V2_ROOT).join(VELOCITY_ROOT_NAME)
}

pub fn service_cgroup_dir(service_name: &str) -> PathBuf {
    cgroup_root().join(service_name)
}

fn validate_service_name(service_name: &str) -> io::Result<()> {
    if service_name.is_empty()
        || service_name.contains('/')
        || service_name.contains('\0')
        || service_name.split('/').any(|c| c == "..")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid service name for cgroup: {service_name:?}"),
        ));
    }
    Ok(())
}

fn enable_controllers(dir: &Path) -> io::Result<()> {
    const EPERM: i32 = 1;
    const ENOENT: i32 = 2;
    const EACCES: i32 = 13;
    const EBUSY: i32 = 16;
    const EINVAL: i32 = 22;
    const EROFS: i32 = 30;
    const EOPNOTSUPP: i32 = 95;

    let control_file = dir.join("cgroup.subtree_control");
    let desired = CONTROLLERS
        .iter()
        .map(|c| format!("+{}", c))
        .collect::<Vec<_>>()
        .join(" ");

    match fs::write(&control_file, desired) {
        Ok(()) => {
            debug!(
                "enabled controllers [{}] for {}",
                CONTROLLERS.join(", "),
                dir.display()
            );
            Ok(())
        }
        Err(e) => {
            let tolerable = matches!(
                e.kind(),
                io::ErrorKind::PermissionDenied
                    | io::ErrorKind::NotFound
                    | io::ErrorKind::AlreadyExists
                    | io::ErrorKind::ResourceBusy
                    | io::ErrorKind::InvalidInput
                    | io::ErrorKind::Unsupported
                    | io::ErrorKind::ReadOnlyFilesystem
            ) || matches!(
                e.raw_os_error(),
                Some(EPERM) | Some(ENOENT) | Some(EACCES) | Some(EBUSY) | Some(EINVAL) | Some(EROFS) | Some(EOPNOTSUPP)
            );
            if tolerable {
                debug!("controllers not (re)enabled for {}: {}", dir.display(), e);
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

fn write_pid_to_cgroup(dir: &Path, pid: Pid) -> io::Result<()> {
    let procs = dir.join("cgroup.procs");
    fs::write(&procs, format!("{}\n", pid.as_raw()))
}

pub const CPU_MAX_PERIOD: u64 = 100_000;

pub fn limit_value(file: &str, value: &str) -> String {
    if file == "cpu.max" {
        let value = value.trim();
        if let Some(pct) = value.strip_suffix('%') {
            let pct: u64 = pct.trim().parse().unwrap_or(0).min(100);
            let quota = pct.saturating_mul(CPU_MAX_PERIOD / 100);
            return format!("{} {}", quota, CPU_MAX_PERIOD);
        }
        return value.to_string();
    }
    value.trim().to_string()
}

pub fn apply_limits(service_name: &str, limits: &HashMap<String, String>) {
    if validate_service_name(service_name).is_err() {
        warn!("refusing to apply limits for invalid service name {service_name:?}");
        return;
    }
    let dir = service_cgroup_dir(service_name);
    let mut files: Vec<&String> = limits.keys().collect();
    files.sort();
    for file in files {
        let value = limit_value(file, &limits[file]);
        match fs::write(dir.join(file), format!("{}\n", value)) {
            Ok(()) => debug!(
                "{:<20} limit {}={} applied in {}",
                service_name,
                file,
                value,
                dir.display()
            ),
            Err(e) => warn!(
                "could not apply limit {:<15} for {} ({}): {}",
                file, service_name, value, e
            ),
        }
    }
}

pub fn setup_cgroup(
    service_name: &str,
    pid: Pid,
    limits: &HashMap<String, String>,
) -> io::Result<PathBuf> {

    validate_service_name(service_name)?;
    let root = cgroup_root();
    fs::create_dir_all(&root)?;
    let _ = enable_controllers(&root);

    let dir = service_cgroup_dir(service_name);
    fs::create_dir_all(&dir)?;
    write_pid_to_cgroup(&dir, pid)?;

    apply_limits(service_name, limits);

    debug!(
        "service {} attached to cgroup {} (pid {})",
        service_name,
        dir.display(),
        pid
    );
    Ok(dir)
}

fn parse_procs(contents: &str) -> Vec<Pid> {
    contents
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| l.parse::<i32>().ok())
        .map(Pid::from_raw)
        .collect()
}

pub fn read_cgroup_pids(service_name: &str) -> io::Result<Vec<Pid>> {
    validate_service_name(service_name)?;
    let procs = service_cgroup_dir(service_name).join("cgroup.procs");
    let contents = fs::read_to_string(&procs)?;
    Ok(parse_procs(&contents))
}

pub fn kill_cgroup(service_name: &str) -> io::Result<()> {

    if validate_service_name(service_name).is_err() {
        return Ok(());
    }
    let dir = service_cgroup_dir(service_name);

    if !dir.is_dir() {
        return Ok(());
    }

    let kill_path = dir.join("cgroup.kill");
    match fs::write(&kill_path, "1\n") {
        Ok(()) => debug!("cgroup.kill signalled for {}", service_name),
        Err(e) => {
            warn!(
                "cgroup.kill unavailable for {} ({}); falling back to per-PID SIGKILL",
                service_name, e
            );
            for pid in read_cgroup_pids(service_name).unwrap_or_default() {
                let _ = kill(pid, Some(Signal::SIGKILL));
            }
        }
    }

    match fs::remove_dir_all(&dir) {
        Ok(()) => debug!("removed cgroup directory {}", dir.display()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => warn!("failed to remove cgroup {}: {}", dir.display(), e),
    }

    Ok(())
}

pub fn kill_root_cgroup() -> io::Result<()> {
    let dir = cgroup_root();
    if !dir.is_dir() {
        return Ok(());
    }
    let kill_path = dir.join("cgroup.kill");
    match fs::write(&kill_path, "1\n") {
        Ok(()) => {
            debug!("cgroup.kill signalled for root cgroup {}", dir.display());
        }
        Err(e) => {
            warn!(
                "root cgroup.kill unavailable ({}); falling back to per-PID SIGKILL",
                e
            );

            if let Ok(root_procs) = fs::read_to_string(dir.join("cgroup.procs")) {
                for pid in parse_procs(&root_procs) {
                    let _ = kill(pid, Some(Signal::SIGKILL));
                }
            }
            let Ok(children) = fs::read_dir(&dir) else {
                let _ = fs::remove_dir_all(&dir);
                return Ok(());
            };
            for child in children.flatten() {
                let procs = child.path().join("cgroup.procs");
                let Ok(text) = fs::read_to_string(&procs) else {
                    continue;
                };
                for pid in parse_procs(&text) {
                    let _ = kill(pid, Some(Signal::SIGKILL));
                }
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_procs_basic() {
        let pids = parse_procs("123\n456\n\n789\n");
        assert_eq!(
            pids,
            vec![Pid::from_raw(123), Pid::from_raw(456), Pid::from_raw(789)]
        );
    }

    #[test]
    fn test_parse_procs_empty() {
        assert!(parse_procs("").is_empty());
        assert!(parse_procs("\n\n").is_empty());
    }

    #[test]
    fn test_parse_procs_ignores_garbage() {
        let pids = parse_procs("abc\n123\nxyz\n");
        assert_eq!(pids, vec![Pid::from_raw(123)]);
    }

    #[test]
    fn test_cgroup_paths() {
        assert_eq!(
            service_cgroup_dir("ssh"),
            Path::new(CGROUP_V2_ROOT)
                .join(VELOCITY_ROOT_NAME)
                .join("ssh")
        );
        assert_eq!(
            cgroup_root(),
            Path::new(CGROUP_V2_ROOT).join("velocity-init")
        );
    }

    #[test]
    fn test_limit_value_passthrough() {

        assert_eq!(limit_value("memory.max", "512M"), "512M");
        assert_eq!(limit_value("memory.max", "1073741824"), "1073741824");
        assert_eq!(limit_value("pids.max", "64"), "64");
        assert_eq!(limit_value("cpu.weight", "100"), "100");
    }

    #[test]
    fn test_limit_value_cpu_percent() {
        assert_eq!(limit_value("cpu.max", "50%"), "50000 100000");
        assert_eq!(limit_value("cpu.max", "100%"), "100000 100000");
        assert_eq!(limit_value("cpu.max", "25%"), "25000 100000");
        assert_eq!(limit_value("cpu.max", "0%"), "0 100000");

        assert_eq!(limit_value("cpu.max", "lots%"), "0 100000");

        assert_eq!(limit_value("cpu.max", "150%"), "100000 100000");
    }

    #[test]
    fn test_limit_value_cpu_absolute_pair() {

        assert_eq!(limit_value("cpu.max", "25000 100000"), "25000 100000");
        assert_eq!(limit_value("cpu.max", "max 100000"), "max 100000");
    }

    #[test]
    fn test_limit_value_trims_whitespace() {
        assert_eq!(limit_value("cpu.max", " 50% "), "50000 100000");
        assert_eq!(limit_value("cpu.max", " 50 %"), "50000 100000");
        assert_eq!(limit_value("memory.max", " 512M "), "512M");
    }

    #[test]
    fn test_validate_service_name() {
        assert!(validate_service_name("ssh").is_ok());
        assert!(validate_service_name("cron").is_ok());
        assert!(validate_service_name("my-service_1").is_ok());
        assert!(validate_service_name("").is_err());
        assert!(validate_service_name("../etc").is_err());
        assert!(validate_service_name("a/b").is_err());
        assert!(validate_service_name("..").is_err());
    }

    #[test]
    fn test_cgroup_invalid_name_fails_closed() {
        use std::collections::HashMap;

        let limits = HashMap::new();
        assert!(
            setup_cgroup("../escape", Pid::from_raw(1), &limits).is_err(),
            "path traversal must fail closed"
        );
        assert!(setup_cgroup("", Pid::from_raw(1), &limits).is_err());

        assert!(kill_cgroup("../escape").is_ok());
        assert!(kill_cgroup("").is_ok());
        assert!(read_cgroup_pids("../escape").is_err());

        apply_limits("../escape", &limits);
    }
}
