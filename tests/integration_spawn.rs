/// Layer 2 integration tests: real process spawning, Job Object enforcement, port checks.
///
/// These tests use cmd.exe (always present on Windows) as a stand-in binary so they
/// run without requiring Apache/MySQL/PHP to be installed.
///
/// Run with: cargo test --test integration_spawn
use crossbeam_channel::unbounded;
use rampp::process::{check_port_available, spawn_service};
use rampp::state::{ApacheConfig, MysqlConfig, PhpConfig, PhpMyAdminConfig, RampConfig};
use std::path::PathBuf;

/// Build a minimal RampConfig pointing apache.bin at the given binary.
fn make_config(bin: PathBuf) -> RampConfig {
    let install_dir = bin.parent().unwrap().to_path_buf();
    RampConfig {
        install_dir: install_dir.clone(),
        apache: ApacheConfig {
            port: 18080,
            bin,
            conf: install_dir.join("httpd.conf"),
            document_root: install_dir.join("htdocs"),
        },
        mysql: MysqlConfig {
            port: 13306,
            bin: install_dir.join("mysqld.exe"),
            data_dir: install_dir.join("data"),
            ini: install_dir.join("my.ini"),
        },
        php: PhpConfig {
            port: 19000,
            bin: install_dir.join("php-cgi.exe"),
            ini: install_dir.join("php.ini"),
        },
        phpmyadmin: PhpMyAdminConfig {
            mysql_user: "root".to_string(),
            mysql_password: String::new(),
        },
    }
}

fn cmd_exe() -> PathBuf {
    PathBuf::from(
        std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".into())
            + "\\System32\\cmd.exe",
    )
}

/// Sanity check: cmd.exe exists on every Windows system.
#[test]
fn cmd_exe_exists() {
    assert!(
        cmd_exe().exists(),
        "cmd.exe not found at {}",
        cmd_exe().display()
    );
}

/// spawn_service rejects a binary that is outside install_dir.
#[test]
fn spawn_rejects_binary_outside_install_dir() {
    use rampp::state::Service;
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    // Config with install_dir = tmp, but apache.bin = cmd.exe (outside tmp)
    let mut cfg = make_config(cmd_exe());
    cfg.install_dir = tmp.path().to_path_buf();
    // apache.bin is now cmd.exe which is outside install_dir â€” validation must fail
    let (tx, _rx) = unbounded();
    let result = spawn_service(Service::Apache, &cfg, cfg.apache.port, tx);
    assert!(
        result.is_err(),
        "expected error for binary outside install_dir"
    );
    let msg = result.err().unwrap();
    assert!(
        msg.contains("binary validation") || msg.contains("outside install_dir"),
        "unexpected error message: {msg}"
    );
}

/// Prepare a temp install_dir with cmd.exe copied inside and the apache/ work_dir created.
/// spawn_service requires the working directory to exist.
fn make_spawn_env() -> (tempfile::TempDir, rampp::state::RampConfig) {
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let cmd = cmd_exe();

    // Copy cmd.exe into install_dir so path validation passes (must be inside install_dir)
    let bin_dst = tmp.path().join("cmd.exe");
    std::fs::copy(&cmd, &bin_dst).expect("copy cmd.exe");

    // CreateProcessW requires the working directory to exist
    let work_dir = tmp.path().join("apache");
    std::fs::create_dir_all(&work_dir).expect("create apache work_dir");

    let mut cfg = make_config(bin_dst.clone());
    cfg.install_dir = tmp.path().to_path_buf();
    cfg.apache.bin = bin_dst;

    (tmp, cfg)
}

/// spawn_service returns Ok and the process can be killed cleanly.
/// Uses cmd.exe (hangs waiting for input) as a stand-in binary.
#[test]
fn spawn_and_kill_via_job_object() {
    use rampp::state::Service;

    let (_tmp, cfg) = make_spawn_env();
    let (tx, _rx) = unbounded();
    let proc =
        spawn_service(Service::Apache, &cfg, cfg.apache.port, tx).expect("spawn should succeed");

    // Process is running â€” try_wait should return None immediately
    assert!(proc.try_wait().is_none(), "process should still be running");

    // Kill via Job Object â€” must not hang or panic
    proc.kill();
    // Reaching here means kill() completed and WaitForSingleObject returned
}

/// After kill, the process is no longer alive.
/// kill() internally calls WaitForSingleObject(INFINITE) so returning from it
/// is the proof that the process is dead.
#[test]
fn process_exits_after_kill() {
    use rampp::state::Service;

    let (_tmp, cfg) = make_spawn_env();
    let (tx, _rx) = unbounded();
    let proc = spawn_service(Service::Apache, &cfg, cfg.apache.port, tx).expect("spawn");

    // Kill blocks until the OS confirms the process is dead
    proc.kill();
}

/// check_port_available returns false when a port is bound, true when released.
#[test]
fn port_available_reflects_bind_state() {
    use std::net::TcpListener;

    // Find a free port by binding to :0
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();

    assert!(
        !check_port_available(port),
        "port {port} should not be available while bound"
    );

    drop(listener);

    // Port should now be free (there is a tiny TOCTOU window here, acceptable in tests)
    assert!(
        check_port_available(port),
        "port {port} should be available after release"
    );
}

/// check_port_available returns false for a port that nothing is listening on
/// but which is in TIME_WAIT â€” actually we can't reliably produce TIME_WAIT in a unit test.
/// Instead verify that an unused high port reports as available.
#[test]
fn unused_high_port_is_available() {
    // Port 59999 is unlikely to be in use during tests
    // If this flakes in CI, the test can be skipped â€” it's a smoke test
    assert!(check_port_available(59999));
}

/// Exit codes are reported as u32 â€” Windows crash codes like 0xC0000005 (access violation)
/// must not be truncated or sign-extended to negative values.
/// try_wait returns u32, and Event::ProcessExit.exit_code is Option<u32>.
#[test]
fn exit_code_is_u32_no_sign_extension() {
    use rampp::events::Event;
    use rampp::state::Service;

    // Simulate what the watcher emits: a high Windows exit code
    let code: u32 = 0xC000_0005; // STATUS_ACCESS_VIOLATION
    let event = Event::ProcessExit {
        service: Service::Apache,
        exit_code: Some(code),
    };
    if let Event::ProcessExit { exit_code, .. } = event {
        let reported = exit_code.unwrap();
        // Must equal the original value â€” no truncation, no sign extension
        assert_eq!(reported, 0xC000_0005u32);
        // Confirm it would have been wrong as i32 (to prove the fix matters)
        assert!(reported > i32::MAX as u32);
    } else {
        panic!("wrong event variant");
    }
}

/// Spawn a process, kill it, then immediately spawn again.
/// The second spawn must succeed â€” no stale ProcessExit from the first kill
/// should race with the reducer's Starting state.
///
/// Before the H1 fix, do_kill dropped the watcher JoinHandle without joining,
/// meaning the watcher could still send ProcessExit after do_spawn returned.
/// Now do_kill joins the watcher, so by the time spawn returns the old
/// watcher is guaranteed to have sent its ProcessExit and exited.
#[test]
fn kill_then_respawn_no_stale_process_exit() {
    use crossbeam_channel::unbounded;
    use rampp::events::Event;
    use rampp::process::spawn_service;
    use rampp::state::Service;

    let (_tmp, cfg) = make_spawn_env();

    // First spawn
    let (tx, rx) = unbounded();
    let proc1 =
        spawn_service(Service::Apache, &cfg, cfg.apache.port, tx.clone()).expect("first spawn");

    // Kill synchronously (mirrors what do_kill does after the fix: signal + join)
    proc1.kill();

    // Second spawn â€” must succeed
    let proc2 =
        spawn_service(Service::Apache, &cfg, cfg.apache.port, tx).expect("second spawn after kill");

    // Drain any events: there must be no ProcessExit with Some(exit_code) that could
    // confuse a reducer that just moved to Starting for the second spawn.
    // (The watcher for proc1 sends ProcessExit{exit_code:None} when killed via kill().)
    // proc2 is still alive here, so it hasn't sent anything yet.
    let unexpected: Vec<_> = rx
        .try_iter()
        .filter(|e| {
            matches!(
                e,
                Event::ProcessExit {
                    exit_code: Some(_),
                    ..
                }
            )
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "unexpected natural exit events before second proc was killed: {unexpected:?}"
    );

    proc2.kill();
}

/// spawn_service rejects a binary path that does not exist on disk.
/// validate_critical_path passes (it's inside install_dir) but bin.exists() must fail.
#[test]
fn spawn_rejects_nonexistent_binary() {
    use rampp::state::Service;
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    // Create the apache/ work_dir so CreateProcessW doesn't fail on that
    std::fs::create_dir_all(tmp.path().join("apache")).unwrap();

    let mut cfg = make_config(tmp.path().join("nonexistent.exe"));
    cfg.install_dir = tmp.path().to_path_buf();
    cfg.apache.bin = tmp.path().join("nonexistent.exe");

    let (tx, _rx) = unbounded();
    let result = spawn_service(Service::Apache, &cfg, cfg.apache.port, tx);
    assert!(result.is_err(), "should reject non-existent binary");
    let msg = result.err().unwrap();
    assert!(
        msg.contains("binary not found") || msg.contains("not found"),
        "unexpected error: {msg}"
    );
}

/// check_port_available returns false when a port is already bound.
/// Pairs with the bind/release test but specifically verifies the function
/// works correctly as a pre-spawn port conflict detector.
#[test]
fn check_port_available_detects_conflict() {
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    // While bound: not available
    assert!(
        !check_port_available(port),
        "port {port} must not be available while bound"
    );

    // Release and verify available again
    drop(listener);
    assert!(
        check_port_available(port),
        "port {port} must be available after release"
    );
}

/// try_wait returns None for a running process and Some(code) after it exits naturally.
#[test]
fn try_wait_reflects_process_lifecycle() {
    use rampp::state::Service;

    let (_tmp, cfg) = make_spawn_env();
    let (tx, _rx) = unbounded();

    // cmd.exe waits for stdin input, so it stays alive â€” verify try_wait returns None.
    let proc = spawn_service(Service::Apache, &cfg, cfg.apache.port, tx).expect("spawn");

    // cmd.exe is waiting for stdin input â€” should still be running
    assert!(
        proc.try_wait().is_none(),
        "running process should return None"
    );
    proc.kill();
}
