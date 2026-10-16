mod common;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use common::{acquire_e2e_lock, data_root, socket_path, SpawnSpec, TestProc, PROC_TIMEOUT};

fn booted_ctl(tag: &str) -> (TestProc, PathBuf) {
    let sock = socket_path(tag);
    let proc = TestProc::with_socket(&data_root().join("data_ctl"), &sock);
    assert!(
        proc.wait_for("boot complete", PROC_TIMEOUT),
        "boot complete:\n{}",
        proc.snapshot().join("\n")
    );
    (proc, sock)
}

fn send_command(sock: &Path, cmd: &str) -> String {
    let mut stream = UnixStream::connect(sock).expect("connect to control socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    stream.write_all(cmd.as_bytes()).expect("write command");
    let mut resp = String::new();
    let _ = stream.read_to_string(&mut resp);
    resp.trim_end().to_string()
}

#[test]
fn test_poweroff_via_control_socket() {
    let _e2e_lock = acquire_e2e_lock();
    let (mut proc, sock) = booted_ctl("poweroff");

    let resp = send_command(&sock, "POWEROFF\n");
    assert!(resp.starts_with("OK powering off"), "unexpected: {resp}");

    assert!(
        proc.wait_for("system state → shutting-down", Duration::from_secs(5)),
        "state transition:\n{}",
        proc.snapshot().join("\n")
    );
    assert!(
        proc.wait_for(
            "teardown complete — services stopped, system shutting down",
            Duration::from_secs(15)
        ),
        "teardown finished:\n{}",
        proc.snapshot().join("\n")
    );
    let code = proc
        .wait_exit(Duration::from_secs(15))
        .expect("exited cleanly");
    assert_eq!(code, 0);
    assert!(
        !sock.exists(),
        "control socket file must be unlinked after shutdown"
    );
}

#[test]
fn test_reboot_via_control_socket() {
    let _e2e_lock = acquire_e2e_lock();
    let (mut proc, sock) = booted_ctl("reboot");

    let resp = send_command(&sock, "REBOOT\n");
    assert!(resp.starts_with("OK rebooting"), "unexpected: {resp}");

    assert!(
        proc.wait_for("system state → shutting-down", Duration::from_secs(5)),
        "state transition:\n{}",
        proc.snapshot().join("\n")
    );
    let code = proc
        .wait_exit(Duration::from_secs(15))
        .expect("exited cleanly");
    assert_eq!(code, 0);
}

#[test]
fn test_halt_via_control_socket() {
    let _e2e_lock = acquire_e2e_lock();
    let (mut proc, sock) = booted_ctl("halt");

    let resp = send_command(&sock, "HALT\n");
    assert!(resp.starts_with("OK halting"), "unexpected: {resp}");

    let code = proc
        .wait_exit(Duration::from_secs(15))
        .expect("exited cleanly");
    assert_eq!(code, 0);
}

#[test]
fn test_velocityctl_shutdown_requires_root() {
    if nix::unistd::geteuid().is_root() {

        return;
    }
    for verb in ["poweroff", "reboot", "halt"] {
        let never = socket_path(&format!("no-{verb}"));
        let out = common::velocityctl(&[verb], &[("VELOCITY_INIT_SOCKET", &never)]);
        assert_eq!(out.status.code(), Some(2), "{verb} must exit 2");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("requires root"),
            "{verb} must refuse: got {stderr}"
        );
    }
}

#[test]
fn test_sigterm_full_teardown() {
    let _e2e_lock = acquire_e2e_lock();
    let (mut proc, _sock) = booted_ctl("sigterm");

    proc.terminate();
    assert!(
        proc.wait_for("received SIGTERM", Duration::from_secs(5)),
        "signal logged:\n{}",
        proc.snapshot().join("\n")
    );
    assert!(
        proc.wait_for("graceful shutdown (halt): stopping", Duration::from_secs(5)),
        "teardown begins:\n{}",
        proc.snapshot().join("\n")
    );
    assert!(
        proc.wait_for(
            "teardown complete — services stopped, system shutting down",
            Duration::from_secs(15)
        ),
        "teardown finished:\n{}",
        proc.snapshot().join("\n")
    );
    let code = proc
        .wait_exit(Duration::from_secs(15))
        .expect("exited cleanly");
    assert_eq!(code, 0);
}

#[test]
fn test_restarts_suppressed_during_shutdown() {
    let _e2e_lock = acquire_e2e_lock();
    let sock = socket_path("suppress");
    let mut proc = TestProc::with_socket(&data_root().join("data_crash"), &sock);
    assert!(
        proc.wait_for("boot complete", PROC_TIMEOUT),
        "boot complete:\n{}",
        proc.snapshot().join("\n")
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    while proc.count_lines("Spawned pid") < 3 {
        assert!(
            Instant::now() < deadline,
            "crash loop not running:\n{}",
            proc.snapshot().join("\n")
        );
        thread::sleep(Duration::from_millis(50));
    }

    proc.terminate();
    assert!(
        proc.wait_for("system state → shutting-down", Duration::from_secs(10)),
        "state transition:\n{}",
        proc.snapshot().join("\n")
    );
    let spawns_at_shutdown = proc.count_lines("Spawned pid");

    let code = proc
        .wait_exit(Duration::from_secs(20))
        .expect("exited cleanly");
    assert_eq!(code, 0);

    let spawns_after = proc.count_lines("Spawned pid");
    assert!(
        spawns_after <= spawns_at_shutdown + 1,
        "restarts continued during shutdown: {} → {} spawns:\n{}",
        spawns_at_shutdown,
        spawns_after,
        proc.snapshot().join("\n")
    );
}

#[test]
fn test_sigterm_during_a_slow_sysv_boot_aborts_immediately() {
    let _e2e_lock = acquire_e2e_lock();
    let sock = socket_path("slowboot");
    let inittab = data_root().join("data_sysv_slow").join("inittab");

    let mut proc = TestProc::spawn_spec(

        &data_root().join("data_ctl"),
        &SpawnSpec::new()
            .socket(&sock)
            .args(&["--mode", "sysv"])
            .arg("--inittab")
            .arg(inittab.to_str().unwrap())

            .env("VELOCITY_ALLOW_SYSV_NONPID1", "1")
            .env(
                "VELOCITY_UTMP",
                socket_path("slowboot-utmp").to_str().unwrap(),
            ),
    );

    assert!(
        proc.wait_for("slow-rcS-started", PROC_TIMEOUT),
        "the slow sysinit line never started:\n{}",
        proc.snapshot().join("\n")
    );

    let t0 = Instant::now();
    proc.terminate();
    let code = proc.wait_exit(Duration::from_secs(10));
    let elapsed = t0.elapsed();

    assert!(
        code.is_some(),
        "SIGTERM did not abort the boot — init is still waiting on a 300s script \
         ({elapsed:?}):\n{}",
        proc.snapshot().join("\n")
    );
    assert_eq!(code, Some(0), "teardown after the abort must still be orderly");
    assert!(
        elapsed < Duration::from_secs(5),
        "the boot abort took {elapsed:?} — the wait is not signal-driven"
    );
    assert!(
        proc.wait_for("aborting the wait", Duration::from_secs(1)),
        "the wait must report why it stopped:\n{}",
        proc.snapshot().join("\n")
    );
}

#[test]
fn test_boot_line_does_not_stall_the_boot() {
    let _e2e_lock = acquire_e2e_lock();
    let sock = socket_path("bootasync");
    let inittab = data_root().join("data_sysv_bootasync").join("inittab");

    let proc = TestProc::spawn_spec(
        &data_root().join("data_ctl"),
        &SpawnSpec::new()
            .socket(&sock)
            .args(&["--mode", "sysv"])
            .arg("--inittab")
            .arg(inittab.to_str().unwrap())
            .env("VELOCITY_ALLOW_SYSV_NONPID1", "1")
            .env(
                "VELOCITY_UTMP",
                socket_path("bootasync-utmp").to_str().unwrap(),
            ),
    );

    assert!(
        proc.wait_for("bootlong-started", PROC_TIMEOUT),
        "the boot: line never started:\n{}",
        proc.snapshot().join("\n")
    );

    let t0 = Instant::now();
    assert!(
        proc.wait_for("rc-runlevel-2", Duration::from_secs(10)),
        "the boot stalled on the boot: line — the wait: line never ran:\n{}",
        proc.snapshot().join("\n")
    );
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "the boot took {elapsed:?} to get past the boot: line — it was waited on"
    );

    assert!(
        proc.wait_for("boot complete", PROC_TIMEOUT),
        "boot did not complete:\n{}",
        proc.snapshot().join("\n")
    );
}

#[test]
fn test_boot_is_not_stalled_by_a_service_that_floods_stdout() {

    let _e2e_lock = acquire_e2e_lock();
    let proc = TestProc::spawn(&data_root().join("data_chatter"));

    assert!(
        proc.wait_for("after started", PROC_TIMEOUT),
        "the service ordered after the chatty one must run:\n{}",
        proc.snapshot().join("\n")
    );

    let lines = proc.snapshot();
    let done = lines
        .iter()
        .position(|l| l.contains("chatter done"))
        .expect("the chatty service's last line was never logged");
    let boot_complete = lines
        .iter()
        .position(|l| l.contains("boot complete"))
        .expect("boot did not complete");
    assert!(
        done < boot_complete,
        "the chatty service finished only after the boot was over: its output was still \
         stuck in a full log pipe\n{}",
        lines.join("\n")
    );

    assert!(
        proc.count_lines("chatter line") > 4000,
        "expected a flood, got {} lines",
        proc.count_lines("chatter line")
    );
}

#[test]
fn test_control_socket_and_journal_survive_a_run_wipe() {

    let _e2e_lock = acquire_e2e_lock();
    let sock = socket_path("wipe");
    let journal = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("wipe-{}.journal", std::process::id()));

    let seg = |s: &str| PathBuf::from(format!("{}.{}", journal.display(), s));
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&journal);
    let _ = std::fs::remove_file(seg("0"));
    let _ = std::fs::remove_file(seg("1"));

    let spec = SpawnSpec::new().socket(&sock).envs(&[
        ("VELOCITY_JOURNAL", journal.to_str().unwrap()),
        ("VELOCITY_TEST_SOCKET", sock.to_str().unwrap()),
        ("VELOCITY_TEST_JOURNAL", journal.to_str().unwrap()),
    ]);
    let proc = TestProc::spawn_spec(&data_root().join("data_wipe"), &spec);

    assert!(
        proc.wait_for("boot complete", PROC_TIMEOUT),
        "boot never completed:\n{}",
        proc.snapshot().join("\n")
    );

    thread::sleep(Duration::from_millis(2500));
    assert!(
        sock.exists(),
        "control socket was not re-established:\n{}",
        proc.snapshot().join("\n")
    );

    let reply = common::send_command(&sock, "LIST\n");
    assert!(
        reply.starts_with("OK"),
        "the control socket must answer again after a /run wipe, got {:?}",
        reply
    );
    assert!(
        seg("0").exists(),
        "journal active segment was not re-created:\n{}",
        proc.snapshot().join("\n")
    );

    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&journal);
    let _ = std::fs::remove_file(seg("0"));
    let _ = std::fs::remove_file(seg("1"));
}
