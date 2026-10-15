mod common;

use std::process::Command;
use std::time::Duration;

use common::{acquire_e2e_lock, data_root, socket_path, SpawnSpec, TestProc, PROC_TIMEOUT};

#[test]
fn test_unexecutable_script_reports_exec_failure() {
    use std::os::unix::fs::PermissionsExt;

    let _e2e_lock = acquire_e2e_lock();
    let dir = std::env::temp_dir().join(format!("velocity-exec-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let script = dir.join("broken");
    std::fs::write(
        &script,
        "#!/bin/sh\n### BEGIN INIT INFO\n# Provides: broken\n# Required-Start:\n# Required-Stop:\n# Default-Start: 2 3 4 5\n# Default-Stop: 0 1 6\n# Short-Description: not executable\n### END INIT INFO\necho should never run\n",
    )
    .expect("write fixture");

    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644))
        .expect("chmod fixture");

    let sock = socket_path("exec-fail");
    let mut proc = TestProc::with_socket(&dir, &sock);

    assert!(
        proc.wait_for("exec failed for broken", PROC_TIMEOUT),
        "the child must report why it could not start:\n{}",
        proc.snapshot().join("\n")
    );
    assert!(
        proc.wait_for_count("exited unexpectedly with code 127", 1, Duration::from_secs(10)),
        "and exit with the 127 marker:\n{}",
        proc.snapshot().join("\n")
    );

    proc.terminate();
    assert_eq!(
        proc.wait_exit(Duration::from_secs(15)).expect("exited"),
        0
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_getty_injected_and_restarted_always() {
    let _e2e_lock = acquire_e2e_lock();
    let sock = socket_path("getty-inject");
    let getty = data_root().join("data_getty/fake_getty.sh");
    let mut proc = TestProc::spawn_spec(
        &data_root().join("data_getty_none"),
        &SpawnSpec::new().socket(&sock).envs(&[

            ("VELOCITY_INIT_GETTY", getty.to_str().unwrap()),
            ("GETTY_DELAY", "5"),
        ]),
    );

    assert!(
        proc.wait_for("boot complete", PROC_TIMEOUT),
        "boot complete:\n{}",
        proc.snapshot().join("\n")
    );
    assert!(
        proc.wait_for(
            "no getty-like service found — injected built-in console getty",
            Duration::from_secs(5)
        ),
        "getty injection logged:\n{}",
        proc.snapshot().join("\n")
    );

    assert!(
        proc.wait_for(
            "[getty@console] FAKE-GETTY invoked with: -a root console 115200 linux",
            Duration::from_secs(10)
        ),
        "first getty invocation:\n{}",
        proc.snapshot().join("\n")
    );

    assert!(
        proc.wait_for(
            "scheduled restart in 200ms (attempt 1)",
            Duration::from_secs(15)
        ),
        "immediate restart logged:\n{}",
        proc.snapshot().join("\n")
    );

    assert!(
        proc.wait_for_count(
            "FAKE-GETTY invoked with: -a root console 115200 linux",
            2,
            Duration::from_secs(15)
        ),
        "getty must be respawned: {}",
        proc.snapshot().join("\n")
    );

    proc.terminate();
    let code = proc
        .wait_exit(Duration::from_secs(15))
        .expect("exited cleanly");
    assert_eq!(code, 0);
}

#[test]
fn test_getty_suppressed_when_user_getty_exists() {
    let _e2e_lock = acquire_e2e_lock();
    let sock = socket_path("getty-suppress");
    let mut proc = TestProc::with_socket(&data_root().join("data_getty_present"), &sock);

    assert!(
        proc.wait_for("boot complete", PROC_TIMEOUT),
        "boot complete:\n{}",
        proc.snapshot().join("\n")
    );
    assert!(
        proc.wait_for(
            "getty-like service present; keeping user configuration",
            Duration::from_secs(5)
        ),
        "suppression logged:\n{}",
        proc.snapshot().join("\n")
    );
    assert!(
        proc.wait_for(
            "[getty@tty1] user getty@tty1 running",
            Duration::from_secs(10)
        ),
        "user getty booted:\n{}",
        proc.snapshot().join("\n")
    );

    assert_eq!(
        proc.count_lines("FAKE-GETTY"),
        0,
        "fake getty must not run:\n{}",
        proc.snapshot().join("\n")
    );
    assert_eq!(
        proc.count_lines("injected built-in console getty"),
        0,
        "built-in getty must not be injected:\n{}",
        proc.snapshot().join("\n")
    );

    proc.terminate();
    let code = proc
        .wait_exit(Duration::from_secs(15))
        .expect("exited cleanly");
    assert_eq!(code, 0);
}

#[test]
fn test_env_file_injection() {
    let _e2e_lock = acquire_e2e_lock();
    let sock = socket_path("env");
    let mut proc = TestProc::spawn_spec(
        &data_root().join("data_env"),
        &SpawnSpec::new().socket(&sock).envs(&[

            ("FOO", "from-outer"),
            ("INHERITED", "outer-passthrough"),
        ]),
    );

    assert!(
        proc.wait_for(
            "env FOO=from-file BAZ=quoted-val EMPTY='' INHERITED=outer-passthrough",
            PROC_TIMEOUT
        ),
        "env layering:\n{}",
        proc.snapshot().join("\n")
    );

    proc.terminate();
    let code = proc
        .wait_exit(Duration::from_secs(15))
        .expect("exited cleanly");
    assert_eq!(code, 0);
}

#[test]
fn test_dry_run_shows_getty() {
    let out = Command::new(env!("CARGO_BIN_EXE_velocity-init"))
        .arg("--dry-run")
        .arg("--init-dir")
        .arg(data_root().join("data_getty_none"))
        .env("RUST_LOG", "info")

        .env("VELOCITY_INIT_GETTY", common::safe_getty())
        .env("VELOCITY_INITTAB", common::NO_INITTAB)
        .output()
        .expect("run dry-run");
    assert!(out.status.success(), "dry-run exits 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("getty@console"), "plan: {stdout}");
    assert!(
        stdout.contains("netif"),
        "regular service in plan: {stdout}"
    );
}
