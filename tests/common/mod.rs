#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::{kill, Signal};
use nix::sys::socket::{
    connect, send, socket, AddressFamily, MsgFlags, SockFlag, SockType, UnixAddr,
};
use nix::unistd::Pid;

use velocity_init::supervisor::notify_addr_bytes_for;

pub fn safe_getty() -> PathBuf {
    data_root().join("data_getty").join("idle_getty.sh")
}

pub const NO_INITTAB: &str = "/nonexistent/velocity-init-test-inittab";

pub const PROC_TIMEOUT: Duration = Duration::from_secs(30);

pub fn acquire_e2e_lock() -> Flock<std::fs::File> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(".e2e-init.lock");
    let f = std::fs::File::create(&path).expect("create e2e lock file");
    Flock::lock(f, FlockArg::LockExclusive).expect("acquire e2e lock")
}

pub fn socket_path(tag: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("{tag}-{}.sock", std::process::id()))
}

pub fn data_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests")
}

pub struct TestProc {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
}

pub struct SpawnSpec<'a> {

    pub socket: Option<&'a Path>,

    pub env: Vec<(&'a str, &'a str)>,

    pub args: Vec<&'a str>,
}

impl<'a> Default for SpawnSpec<'a> {
    fn default() -> Self {
        Self {
            socket: None,
            env: Vec::new(),
            args: Vec::new(),
        }
    }
}

impl<'a> SpawnSpec<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn socket(mut self, path: &'a Path) -> Self {
        self.socket = Some(path);
        self
    }

    pub fn env(mut self, key: &'a str, value: &'a str) -> Self {
        self.env.push((key, value));
        self
    }

    pub fn envs(mut self, pairs: &[(&'a str, &'a str)]) -> Self {
        self.env.extend_from_slice(pairs);
        self
    }

    pub fn arg(mut self, value: &'a str) -> Self {
        self.args.push(value);
        self
    }

    pub fn args(mut self, values: &[&'a str]) -> Self {
        self.args.extend_from_slice(values);
        self
    }
}

impl TestProc {

    pub fn spawn(init_dir: &Path) -> Self {
        Self::spawn_spec(init_dir, &SpawnSpec::new())
    }

    pub fn with_socket(init_dir: &Path, sock: &Path) -> Self {
        Self::spawn_spec(init_dir, &SpawnSpec::new().socket(sock))
    }

    pub fn spawn_spec(init_dir: &Path, spec: &SpawnSpec<'_>) -> Self {
        let bin = env!("CARGO_BIN_EXE_velocity-init");
        let mut cmd = Command::new(bin);
        cmd.arg("--init-dir")
            .arg(init_dir)
            .args(&spec.args)
            .env("RUST_LOG", "info")

            .env("VELOCITY_INIT_GETTY", safe_getty())
            .env("VELOCITY_INITTAB", NO_INITTAB)

            .env_remove("VELOCITY_INIT_SOCKET")
            .env_remove("VELOCITY_JOURNAL")
            .env_remove("VELOCITY_RUNTIME_DIR");
        if let Some(sock) = spec.socket {
            cmd.env("VELOCITY_INIT_SOCKET", sock);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let getty = safe_getty();
            if getty.exists() {
                let _ = std::fs::set_permissions(&getty, std::fs::Permissions::from_mode(0o755));
            }
        }

        let mut child = cmd.spawn().expect("spawn velocity-init");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let drain = |stream: Box<dyn std::io::Read + Send>, lines: Arc<Mutex<Vec<String>>>| {
            thread::spawn(move || {
                let reader = BufReader::new(stream);
                for line in reader.lines().map_while(Result::ok) {
                    lines.lock().expect("lock").push(line);
                }
            });
        };
        drain(Box::new(stdout), Arc::clone(&lines));
        drain(Box::new(stderr), Arc::clone(&lines));
        Self { child, lines }
    }

    pub fn wait_for(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .lines
                .lock()
                .expect("lock")
                .iter()
                .any(|l| l.contains(needle))
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn wait_for_count(&self, needle: &str, min: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.count_lines(needle) >= min {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn count_lines(&self, needle: &str) -> usize {
        self.lines
            .lock()
            .expect("lock")
            .iter()
            .filter(|l| l.contains(needle))
            .count()
    }

    pub fn snapshot(&self) -> Vec<String> {
        self.lines.lock().expect("lock").clone()
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn terminate(&mut self) {
        let _ = kill(Pid::from_raw(self.child.id() as i32), Signal::SIGTERM);
    }

    pub fn wait_exit(&mut self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return status.code(),
                Ok(None) => {}
                Err(e) => panic!("try_wait: {e}"),
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for TestProc {
    fn drop(&mut self) {

        let _ = kill(Pid::from_raw(self.child.id() as i32), Signal::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(_) => break,
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }

        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn send_command(sock: &Path, cmd: &str) -> String {
    let mut stream = UnixStream::connect(sock).expect("connect to control socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    stream.write_all(cmd.as_bytes()).expect("write command");
    let mut resp = String::new();
    stream.read_to_string(&mut resp).expect("read response");
    resp.trim_end().to_string()
}

pub fn send_readiness(init_pid: u32, name: &str) {
    let fd = socket(
        AddressFamily::Unix,
        SockType::Datagram,
        SockFlag::empty(),
        None,
    )
    .expect("socket");
    let addr = UnixAddr::new_abstract(&notify_addr_bytes_for(init_pid, name))
        .expect("abstract addr");
    connect(fd.as_raw_fd(), &addr)
        .unwrap_or_else(|e| panic!("connect to notify socket of {name}: {e}"));
    send(fd.as_raw_fd(), b"READY=1", MsgFlags::empty()).expect("send READY=1");
}

pub fn velocityctl(args: &[&str], env: &[(&str, &Path)]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_velocityctl"));
    cmd.args(args)
        .env("VELOCITY_INIT_GETTY", safe_getty())
        .env("VELOCITY_INITTAB", NO_INITTAB)
        .env_remove("VELOCITY_INIT_SOCKET")
        .env_remove("VELOCITY_JOURNAL")
        .env_remove("VELOCITY_RUNTIME_DIR");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("run velocityctl")
}
