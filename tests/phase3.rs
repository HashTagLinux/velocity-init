mod common;

use std::net::TcpStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use common::{acquire_e2e_lock, data_root, send_readiness, TestProc, PROC_TIMEOUT};

fn find_port(lines: &[String]) -> Option<u16> {
    let digits_of = |idx: usize, line: &str| -> String {
        line[idx..].chars().take_while(char::is_ascii_digit).collect()
    };
    lines.iter().find_map(|l| {
        let idx = l.find("0.0.0.0:")?;
        let digits = digits_of(idx + "0.0.0.0:".len(), l);
        (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
    })
}

fn parse_kv(lines: &[String], needle: &str) -> i32 {
    for l in lines {
        if let Some(pos) = l.find(needle) {
            let digits: String = l[pos + needle.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if !digits.is_empty() {
                return digits.parse().unwrap_or(-1);
            }
        }
    }
    -1
}

#[test]
fn test_sd_notify_readiness_unblocks_boot() {
    let _e2e_lock = acquire_e2e_lock();
    let mut proc = TestProc::spawn(&data_root().join("data_notify"));

    assert!(
        proc.wait_for("Spawned pid", PROC_TIMEOUT),
        "service spawned"
    );

    send_readiness(proc.pid(), "notify_me");
    assert!(
        proc.wait_for("READY=1 from", Duration::from_secs(5)),
        "READY=1 processed"
    );
    assert!(
        proc.wait_for("boot complete", Duration::from_secs(5)),
        "boot finished"
    );

    proc.terminate();
    let code = proc
        .wait_exit(Duration::from_secs(10))
        .expect("exited cleanly");
    assert_eq!(code, 0);
    assert!(proc
        .snapshot()
        .iter()
        .any(|l| l.contains("teardown complete")));
}

#[test]
fn test_socket_activation_handoff_and_listen() {
    let _e2e_lock = acquire_e2e_lock();

    // The fixture names its unix socket as an absolute path. Run it
    // from a fresh per-run copy under `target/` with the path repointed
    // there: a fixed global path (this used to be a literal
    // /tmp/velocity-sockd.sock) turns any leftover — e.g. a root-owned
    // file a previous test run left behind that this user cannot
    // unlink — into EADDRINUSE, one fewer inherited fd, and a failure
    // that has nothing to do with the code under test.
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("data_sock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create data_sock dir");
    let script = std::fs::read_to_string(data_root().join("data_sock").join("sockd"))
        .expect("read sockd fixture");
    let patched = script.replace(
        "/tmp/velocity-sockd.sock",
        dir.join("velocity-sockd.sock").to_str().expect("utf-8 path"),
    );
    let sockd = dir.join("sockd");
    std::fs::write(&sockd, patched).expect("write patched sockd");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sockd, std::fs::Permissions::from_mode(0o755))
            .expect("chmod sockd");
    }

    let mut proc = TestProc::spawn(&dir);

    let port = loop {
        if let Some(p) = find_port(&proc.snapshot()) {
            break p;
        }
        if !proc.wait_for("listening on 0.0.0.0:", Duration::from_millis(250)) {
            panic!("no listening log line:\n{}", proc.snapshot().join("\n"));
        }
    };

    assert!(
        proc.wait_for("LISTEN_FDS=2", Duration::from_secs(5)),
        "LISTEN_FDS=2"
    );
    let lines = proc.snapshot();
    assert!(lines.iter().any(|l| l.contains("LISTEN_FDS_START=3")));

    let pid_env = parse_kv(&lines, "LISTEN_PID=");
    let pid_self = parse_kv(&lines, "own pid=");
    assert!(pid_self > 0, "script reports its own pid");
    assert_eq!(pid_env, pid_self, "LISTEN_PID must equal the child pid");

    assert!(
        proc.wait_for("fd3=", Duration::from_secs(5)),
        "fd3 diagnostics"
    );
    assert!(
        proc.wait_for("fd4=", Duration::from_secs(5)),
        "fd4 diagnostics"
    );
    let lines = proc.snapshot();
    assert!(lines
        .iter()
        .any(|l| l.contains("fd3=") && l.contains("socket:[")));
    assert!(lines
        .iter()
        .any(|l| l.contains("fd4=") && l.contains("socket:[")));

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut connected = false;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            connected = true;
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(connected, "TCP connection to :{} should succeed", port);

    proc.terminate();
    let code = proc.wait_exit(Duration::from_secs(10)).expect("exited");
    assert_eq!(code, 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_sysv_boot_outside_pid1_is_refused() {
    use std::process::{Command, Stdio};

    let host_inittab = if Path::new("/etc/inittab").exists() {
        "/etc/inittab"
    } else {
        return;
    };

    let out = Command::new(env!("CARGO_BIN_EXE_velocity-init"))
        .arg("--mode")
        .arg("sysv")
        .arg("--inittab")
        .arg(host_inittab)
        .arg("--init-dir")
        .arg(data_root().join("data"))
        .env("RUST_LOG", "info")
        .env_remove("VELOCITY_ALLOW_SYSV_NONPID1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run velocity-init");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "a non-PID-1 sysv boot must fail:\n{stderr}"
    );
    assert!(
        stderr.contains("refusing to run the sysv boot"),
        "the refusal must be explicit and actionable:\n{stderr}"
    );

    assert!(
        stderr.contains("VELOCITY_ALLOW_SYSV_NONPID1") && stderr.contains("--mode native"),
        "the error must tell the operator what to do:\n{stderr}"
    );
    assert!(
        !stderr.contains("sysv boot:"),
        "no boot line may have run:\n{stderr}"
    );
}

#[test]
fn test_dry_run_needs_no_opt_in() {
    use std::process::{Command, Stdio};

    let out = Command::new(env!("CARGO_BIN_EXE_velocity-init"))
        .arg("--mode")
        .arg("sysv")
        .arg("--inittab")
        .arg("/etc/inittab")
        .arg("--init-dir")
        .arg(data_root().join("data"))
        .arg("--dry-run")
        .env("RUST_LOG", "info")
        .env_remove("VELOCITY_ALLOW_SYSV_NONPID1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run velocity-init");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a dry run starts nothing, so it must be allowed:\n{stderr}"
    );
    assert!(
        !stderr.contains("refusing to run the sysv boot"),
        "the gate must not fire for a dry run:\n{stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("SysV inittab Plan") || stdout.contains("Startup Plan"),
        "the plan must still be printed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
fn test_full_boot_supervise_and_shuts_down() {
    let _e2e_lock = acquire_e2e_lock();
    let mut proc = TestProc::spawn(&data_root().join("data"));

    assert!(
        proc.wait_for("boot complete", PROC_TIMEOUT),
        "boot complete"
    );

    assert!(
        proc.wait_for("scheduled restart in", Duration::from_secs(15)),
        "crasher restart scheduled"
    );

    assert!(proc.wait_for("ssh", Duration::from_secs(5)), "ssh spawned");
    assert!(
        proc.snapshot()
            .iter()
            .any(|l| l.contains("ssh") && l.contains("Running")),
        "ssh running"
    );

    proc.terminate();
    let code = proc
        .wait_exit(Duration::from_secs(15))
        .expect("exited cleanly");
    assert_eq!(code, 0);
    assert!(proc
        .snapshot()
        .iter()
        .any(|l| l.contains("teardown complete")));
}
