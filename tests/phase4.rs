mod common;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::{
    acquire_e2e_lock, data_root, send_command, send_readiness, socket_path, velocityctl, SpawnSpec,
    TestProc, PROC_TIMEOUT,
};

#[test]
fn test_control_socket_lifecycle() {
    let _e2e_lock = acquire_e2e_lock();
    let sock = socket_path("lifecycle");
    let mut proc = TestProc::with_socket(&data_root().join("data_ctl"), &sock);

    assert!(
        proc.wait_for("boot complete", PROC_TIMEOUT),
        "boot complete"
    );

    assert!(proc.wait_for("ssh", Duration::from_secs(5)), "ssh booted");

    let resp = send_command(&sock, "STATUS ssh\n");
    assert!(
        resp.starts_with("OK ssh state=running"),
        "unexpected status response: {resp}"
    );

    let resp = send_command(&sock, "STOP ssh\n");
    assert!(resp.starts_with("OK stopping ssh"), "unexpected: {resp}");
    assert!(
        proc.wait_for("stopped", Duration::from_secs(5)),
        "stop logged:\n{}",
        proc.snapshot().join("\n")
    );

    let resp = send_command(&sock, "STATUS ssh\n");
    assert!(
        resp.contains("state=stopped"),
        "expected Stopped after stop: {resp}"
    );

    let resp = send_command(&sock, "LIST\n");
    assert!(
        resp.contains("ssh state=stopped"),
        "unexpected list: {resp}"
    );

    let resp = send_command(&sock, "START ssh\n");
    assert!(resp.starts_with("OK started ssh"), "unexpected: {resp}");
    let resp = send_command(&sock, "STATUS ssh\n");
    assert!(
        resp.contains("state=starting"),
        "expected Starting before readiness: {resp}"
    );

    send_readiness(proc.pid(), "ssh");
    assert!(
        proc.wait_for("received READY=1 from ssh", Duration::from_secs(5)),
        "readiness processed:\n{}",
        proc.snapshot().join("\n")
    );
    let resp = send_command(&sock, "STATUS ssh\n");
    assert!(
        resp.contains("state=running"),
        "expected Running after READY=1: {resp}"
    );

    let resp = send_command(&sock, "RESTART ssh\n");
    assert!(resp.starts_with("OK restarting ssh"), "unexpected: {resp}");
    assert!(
        proc.wait_for_count("will be restarted once it has exited", 1, Duration::from_secs(5)),
        "restart requested:\n{}",
        proc.snapshot().join("\n")
    );

    let spawns = proc.count_lines("Spawned pid");
    assert!(
        proc.wait_for_count("Spawned pid", spawns + 1, Duration::from_secs(10)),
        "the service must be spawned again:\n{}",
        proc.snapshot().join("\n")
    );

    let resp = send_command(&sock, "GARBAGE\n");
    assert!(
        resp.starts_with("ERR unknown command"),
        "unexpected: {resp}"
    );

    proc.terminate();
    let code = proc
        .wait_exit(Duration::from_secs(15))
        .expect("exited cleanly");
    assert_eq!(code, 0);
    assert!(!sock.exists(), "control socket file must be unlinked");
}

#[test]
fn test_stop_does_not_block_the_event_loop() {
    let _e2e_lock = acquire_e2e_lock();
    let sock = socket_path("stop-nonblocking");
    let mut proc = TestProc::with_socket(&data_root().join("data_ctl"), &sock);
    assert!(
        proc.wait_for("boot complete", PROC_TIMEOUT),
        "boot complete"
    );

    let started = Instant::now();
    let resp = send_command(&sock, "STOP ssh\n");
    assert!(resp.starts_with("OK stopping ssh"), "unexpected: {resp}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "STOP must be acknowledged immediately, took {:?}",
        started.elapsed()
    );

    let answered = Instant::now();
    let resp = send_command(&sock, "LIST\n");
    assert!(
        resp.starts_with("OK") && resp.contains("ssh state="),
        "got: {resp}"
    );
    assert!(
        answered.elapsed() < Duration::from_secs(2),
        "the loop must keep answering commands, took {:?}",
        answered.elapsed()
    );

    proc.terminate();
    assert_eq!(
        proc.wait_exit(Duration::from_secs(15)).expect("exited"),
        0
    );
}

#[test]
fn test_log_routing_routes_stdout_and_stderr() {
    let _e2e_lock = acquire_e2e_lock();
    let sock = socket_path("log");
    let mut proc = TestProc::with_socket(&data_root().join("data_log"), &sock);

    assert!(
        proc.wait_for("[printer] printer says hello stdout", PROC_TIMEOUT),
        "stdout line routed:\n{}",
        proc.snapshot().join("\n")
    );
    assert!(
        proc.wait_for(
            "[printer] printer says hello stderr",
            Duration::from_secs(5)
        ),
        "stderr line routed:\n{}",
        proc.snapshot().join("\n")
    );

    proc.terminate();
    let code = proc
        .wait_exit(Duration::from_secs(15))
        .expect("exited cleanly");
    assert_eq!(code, 0);
}

#[test]
fn test_control_socket_malformed_clients_survive() {
    let _e2e_lock = acquire_e2e_lock();
    let sock = socket_path("malformed");
    let mut proc = TestProc::with_socket(&data_root().join("data_ctl"), &sock);
    assert!(
        proc.wait_for("boot complete", PROC_TIMEOUT),
        "boot complete"
    );

    {
        let mut stream = UnixStream::connect(&sock).expect("connect");
        stream.write_all(b"HELLO NO NEWLINE").expect("write");
        let _ = stream.shutdown(std::net::Shutdown::Write);
        let mut resp = String::new();
        let _ = stream.read_to_string(&mut resp);
    }

    let huge = format!("{}a", "x".repeat(5000));
    let resp = send_command(&sock, &format!("{}\n", huge));
    assert!(
        resp.starts_with("ERR command too long"),
        "unexpected: {resp}"
    );

    let resp = send_command(&sock, "\n");
    assert!(resp.starts_with("ERR empty command"), "unexpected: {resp}");

    let resp = send_command(&sock, "STATUS nope\n");
    assert!(
        resp.starts_with("ERR unknown service"),
        "unexpected: {resp}"
    );

    let resp = send_command(&sock, "STATUS ssh\n");
    assert!(resp.starts_with("OK ssh"), "init must survive: {resp}");

    proc.terminate();
    let code = proc
        .wait_exit(Duration::from_secs(15))
        .expect("exited cleanly");
    assert_eq!(code, 0);
}

#[test]
fn test_pipelined_commands_in_one_write_all_run() {
    let _e2e_lock = acquire_e2e_lock();
    let sock = socket_path("pipeline");
    let mut proc = TestProc::with_socket(&data_root().join("data_ctl"), &sock);
    assert!(
        proc.wait_for("boot complete", PROC_TIMEOUT),
        "boot complete"
    );

    let mut stream = UnixStream::connect(&sock).expect("connect");
    stream
        .write_all(b"STATUS ssh\nLIST\nSTATUS nope\n")
        .expect("write");
    let mut resp = String::new();
    stream.read_to_string(&mut resp).expect("read");
    let lines: Vec<&str> = resp.lines().collect();
    assert_eq!(lines.len(), 3, "one reply per command: {resp:?}");
    assert!(lines[0].starts_with("OK ssh state="), "got: {}", lines[0]);
    assert!(lines[1].contains("ssh state="), "got: {}", lines[1]);
    assert!(
        lines[2].starts_with("ERR unknown service"),
        "got: {}",
        lines[2]
    );

    proc.terminate();
    assert_eq!(
        proc.wait_exit(Duration::from_secs(15)).expect("exited"),
        0
    );
}

#[test]
fn test_start_of_a_running_service_is_refused() {
    let _e2e_lock = acquire_e2e_lock();
    let sock = socket_path("double-start");
    let mut proc = TestProc::with_socket(&data_root().join("data_ctl"), &sock);
    assert!(
        proc.wait_for("boot complete", PROC_TIMEOUT),
        "boot complete"
    );

    let resp = send_command(&sock, "START ssh\n");
    assert!(
        resp.starts_with("ERR") && resp.contains("already running"),
        "unexpected: {resp}"
    );

    let resp = send_command(&sock, "STATUS ssh\n");
    assert!(resp.contains("state=running"), "unexpected: {resp}");

    let resp = send_command(&sock, "RESTART ssh\n");
    assert!(resp.starts_with("OK restarting ssh"), "unexpected: {resp}");

    proc.terminate();
    assert_eq!(
        proc.wait_exit(Duration::from_secs(15)).expect("exited"),
        0
    );
}

#[test]
fn test_non_root_peer_cannot_halt_the_machine() {
    use nix::sys::wait::waitpid;
    use nix::unistd::{fork, setuid, ForkResult, Uid};
    use std::os::unix::fs::PermissionsExt;

    let _e2e_lock = acquire_e2e_lock();
    let sock = socket_path("peercred");
    let mut proc = TestProc::with_socket(&data_root().join("data_ctl"), &sock);
    assert!(
        proc.wait_for("boot complete", PROC_TIMEOUT),
        "boot complete"
    );

    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666))
        .expect("chmod control socket");

    fn exchange(sock: &std::path::Path, cmd: &[u8]) -> String {
        let mut stream = UnixStream::connect(sock).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        stream.write_all(cmd).expect("write");
        let mut resp = String::new();
        stream.read_to_string(&mut resp).expect("read");
        resp.trim_end().to_string()
    }

    if nix::unistd::geteuid().is_root() {

        match unsafe { fork() }.expect("fork probe") {
            ForkResult::Parent { child } => {
                let status = waitpid(child, None).expect("waitpid probe");
                assert!(
                    matches!(status, nix::sys::wait::WaitStatus::Exited(_, 0)),
                    "the unprivileged probe must see a refusal, not an action \
                     (child exit: {status:?}):\n{}",
                    proc.snapshot().join("\n")
                );
            }
            ForkResult::Child => {
                let ok = (|| -> Option<()> {
                    setuid(Uid::from_raw(65534)).ok()?;
                    let mut stream = UnixStream::connect(&sock).ok()?;
                    stream
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .ok()?;
                    stream.write_all(b"POWEROFF\n").ok()?;
                    let mut power = String::new();
                    stream.read_to_string(&mut power).ok()?;
                    let power = power.trim_end().to_string();
                    if !power.starts_with("ERR") || !power.contains("requires root") {
                        return None;
                    }

                    let mut monitor = UnixStream::connect(&sock).ok()?;
                    monitor
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .ok()?;
                    monitor.write_all(b"STATUS ssh\n").ok()?;
                    let mut status = String::new();
                    monitor.read_to_string(&mut status).ok()?;
                    if !status.trim_end().starts_with("OK ssh state=") {
                        return None;
                    }
                    Some(())
                })();
                unsafe { libc::_exit(if ok.is_some() { 0 } else { 1 }) }
            }
        }
    } else {

        let stop = exchange(&sock, b"STOP ghost-probe-service\n");
        assert!(
            stop.starts_with("ERR unknown service"),
            "owner must pass the privilege gate (got: {stop})"
        );
        assert!(
            !stop.contains("requires root"),
            "owner must not be refused as a stranger, got: {stop}"
        );
        let status = exchange(&sock, b"STATUS ssh\n");
        assert!(
            status.starts_with("OK ssh state="),
            "STATUS must keep working, got: {status}"
        );
    }

    let resp = send_command(&sock, "LIST\n");
    assert!(resp.contains("ssh state="), "init must be alive: {resp}");

    proc.terminate();
    assert_eq!(
        proc.wait_exit(Duration::from_secs(15)).expect("exited"),
        0
    );
}

#[test]
fn test_idle_control_connections_are_reclaimed() {
    let _e2e_lock = acquire_e2e_lock();
    let sock = socket_path("idle");
    let mut proc = TestProc::spawn_spec(
        &data_root().join("data_ctl"),
        &SpawnSpec::new()
            .socket(&sock)

            .env("VELOCITY_CTL_IDLE_TIMEOUT_SECS", "1"),
    );
    assert!(
        proc.wait_for("boot complete", PROC_TIMEOUT),
        "boot complete"
    );

    let mut quiet = UnixStream::connect(&sock).expect("connect");
    quiet
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    quiet
        .write_all(b"STA")
        .expect("partial command without a newline");

    assert!(
        proc.wait_for("dropping connection fd", Duration::from_secs(10)),
        "the idle connection must be reaped:\n{}",
        proc.snapshot().join("\n")
    );

    let mut tail = [0u8; 16];
    assert_eq!(
        quiet.read(&mut tail).expect("read"),
        0,
        "the idle connection must be closed by the server"
    );

    let resp = send_command(&sock, "LIST\n");
    assert!(resp.contains("ssh state="), "socket must serve again: {resp}");

    proc.terminate();
    assert_eq!(
        proc.wait_exit(Duration::from_secs(15)).expect("exited"),
        0
    );
}

#[test]
fn test_unprivileged_run_falls_back_to_the_runtime_dir() {

    let _e2e_lock = acquire_e2e_lock();
    let rt = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("phase4-rt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&rt);

    let rt_str = rt.to_str().expect("runtime dir is utf-8").to_string();
    let mut proc = TestProc::spawn_spec(
        &data_root().join("data_ctl"),
        &SpawnSpec::new().env("VELOCITY_RUNTIME_DIR", &rt_str),
    );
    assert!(
        proc.wait_for("boot complete", PROC_TIMEOUT),
        "boot complete:\n{}",
        proc.snapshot().join("\n")
    );

    let sock = rt.join("velocity-init.sock");
    assert!(sock.exists(), "socket must land in the runtime dir: {rt:?}");

    let journal = rt.join("journal.0");
    assert!(
        journal.exists() && std::fs::metadata(&journal).unwrap().len() > 0,
        "journal active segment must land in the runtime dir: {rt:?}"
    );

    let ctl = |args: &[&str]| velocityctl(args, &[("VELOCITY_RUNTIME_DIR", rt.as_path())]);

    let out = ctl(&["status", "ssh"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "velocityctl exits 0: {stdout}");
    assert!(stdout.contains("OK ssh state="), "got: {stdout}");

    let out = ctl(&["journal", "--tail", "200"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "journal exits 0: {stdout}");
    assert!(
        stdout.contains("velocity-init starting up"),
        "the very first boot record must be in the journal: {stdout}"
    );

    proc.terminate();
    assert_eq!(
        proc.wait_exit(Duration::from_secs(15)).expect("exited"),
        0
    );
    let _ = std::fs::remove_dir_all(&rt);
}

#[test]
fn test_velocityctl_binary_end_to_end() {
    let _e2e_lock = acquire_e2e_lock();
    let sock = socket_path("ctl-bin");
    let mut proc = TestProc::with_socket(&data_root().join("data_ctl"), &sock);
    assert!(
        proc.wait_for("boot complete", PROC_TIMEOUT),
        "boot complete"
    );

    let ctl = |args: &[&str]| velocityctl(args, &[("VELOCITY_INIT_SOCKET", &sock)]);

    let out = ctl(&["status", "ssh"]);
    assert!(out.status.success(), "status exits 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("OK ssh state=running"), "got: {stdout}");

    let out = ctl(&["stop", "ssh"]);
    assert!(out.status.success(), "stop exits 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("OK stopping ssh"), "got: {stdout}");

    let out = ctl(&["status", "nope"]);
    assert_eq!(out.status.code(), Some(1), "ERR → exit 1");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ERR unknown service"), "got: {stdout}");

    let out = ctl(&["status"]);
    assert_eq!(out.status.code(), Some(2), "missing service → exit 2");

    proc.terminate();
    let code = proc
        .wait_exit(Duration::from_secs(15))
        .expect("exited cleanly");
    assert_eq!(code, 0);
}
