//! Layer 3 -- requires a stack provisioned by `scripts/provision-test-stack.ps1`.
//! Set `RAMPP_L3_DIR` to the install directory. All tests are `#[ignore]`d so
//! `cargo test` stays hermetic; run with `-- --ignored --nocapture`.
//!
//! Per the controller ruling for this task, these tests never launch the
//! `rampp.exe` GUI (driving an egui window is out of scope). Instead they drive
//! the real Apache / MySQL / PHP-CGI binaries directly through rampp's library
//! API: `provision::{desired_configs, reconcile}` to render the same config
//! files the executor would render, `process::spawn_service` to start each
//! binary under a real Windows Job Object, and `health::{check_apache_ready,
//! check_php_ready, probe_mysql}` / `mysql_conf::{initialize_mysql,
//! graceful_shutdown}` for the readiness and lifecycle claims under test.
//!
//! Every spawned process is wrapped in `Guard`, whose `Drop` closes the Job
//! Object even during a panic unwind, so a failing assertion can never leave
//! httpd.exe / mysqld.exe / php-cgi.exe running behind.
//!
//! Ports are deliberately far from RAMPP's defaults (8080/3306/9000) so a
//! failure here is unambiguous and this suite cannot collide with a real
//! RAMPP instance on the same machine. Every test also uses a port disjoint
//! from every other test's (see the constants below), so the whole suite is
//! safe to run exactly as recommended above -- cargo's default multi-threaded
//! runner, no `--test-threads=1` needed.

use rampp::process::{spawn_service, ServiceProcess};
use rampp::provision::{desired_configs, reconcile};
use rampp::state::{
    ApacheConfig, MysqlConfig, PhpConfig, PhpMyAdminConfig, PortState, RampConfig, Service,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const APACHE_PORT: u16 = 18080;
const MYSQL_PORT: u16 = 13306;
const PHP_PORT: u16 = 19000;
/// Used only by `fastcgi_probe_answers_real_php_cgi`, distinct from `PHP_PORT`
/// (used by `php_restart_survives_without_a_sixty_second_blackout`): the
/// suite's own usage comment above recommends running with cargo's default
/// multi-threaded runner, and `spawn()` does not pre-check port availability
/// the way production's `do_spawn` does, so two tests binding the same PHP
/// port could otherwise race non-deterministically.
const PHP_PORT_PROBE: u16 = 19001;

/// Serializes the two tests whose real mysqld processes both write to the
/// SAME `logs/mysql_error.log` -- `mysql_shutdown_is_clean_and_...` (on
/// `MYSQL_PORT`) and `do_kill_waits_for_mysqld_...` (on `MYSQL_PORT_DO_KILL`).
/// The log path is derived from `install_dir` alone (see
/// `mysql_conf::generate_my_ini_with_port` and `executor::do_spawn`'s
/// `error_log` computation), never from `mysql.ini`/`data_dir` -- so giving
/// each test its own port and data directory does NOT give them their own
/// log file. Without this lock, under cargo's default parallel runner two
/// real, concurrently-running mysqld instances interleave writes into one
/// file, and each test's substring search (`"shutdown complete"`,
/// `"crash recovery"`, `"Aborted connection"`) uses a byte-offset window
/// computed independently per test -- a message from the OTHER instance can
/// land inside a test's own window, producing a false pass or a spurious
/// failure. Holding this for an entire test body (not just a critical
/// section) is what makes the result deterministic rather than merely
/// likely: the second test cannot begin writing to the shared log until the
/// first has completely finished every assertion that reads it.
static MYSQL_LOG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn stack_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("RAMPP_L3_DIR")
            .expect("set RAMPP_L3_DIR to a stack provisioned by scripts/provision-test-stack.ps1"),
    )
}

fn make_config(dir: &Path) -> RampConfig {
    RampConfig {
        install_dir: dir.to_path_buf(),
        apache: ApacheConfig {
            port: APACHE_PORT,
            bin: dir.join("apache").join("bin").join("httpd.exe"),
            conf: dir.join("apache").join("conf").join("httpd.conf"),
            // A dedicated scratch webroot, not `apache/htdocs`: the vendor Apache
            // zip ships its own `htdocs/index.html`, so `ensure_document_root`'s
            // "only seed when empty" rule would never plant our test index.php
            // there. Same root cause as the vendor `conf/httpd.conf` collision
            // the provisioning script strips -- see its comment.
            document_root: dir.join("webroot"),
        },
        mysql: MysqlConfig {
            port: MYSQL_PORT,
            bin: dir.join("mysql").join("bin").join("mysqld.exe"),
            data_dir: dir.join("mysql").join("data"),
            ini: dir.join("mysql").join("my.ini"),
        },
        php: PhpConfig {
            port: PHP_PORT,
            bin: dir.join("php").join("php-cgi.exe"),
            ini: dir.join("php").join("php.ini"),
        },
        phpmyadmin: PhpMyAdminConfig {
            mysql_user: "root".to_string(),
            mysql_password: String::new(),
        },
    }
}

/// Mirrors `main.rs`'s `create_runtime_dirs` + provisioning prelude, which is
/// private to the binary crate and so cannot be called directly from a test.
fn create_runtime_dirs(cfg: &RampConfig) {
    for dir in [
        cfg.install_dir.join("logs"),
        cfg.install_dir.join("tmp").join("sessions"),
        cfg.install_dir.join("apache").join("conf"),
        cfg.mysql.data_dir.clone(),
    ] {
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create {dir:?}: {e}"));
    }
}

/// Render and write every managed config file for the given ports, exactly as
/// the executor's `do_reconcile` does in production.
fn write_configs(cfg: &RampConfig, ports: &PortState) {
    let desired = desired_configs(cfg, ports, false, false, &"s".repeat(32));
    let report = reconcile(&desired);
    assert!(
        report.errors.is_empty(),
        "config reconcile reported errors: {:?}",
        report.errors
    );
}

fn ports_for(apache: u16, mysql: u16, php: u16) -> PortState {
    let mut ports = PortState::default();
    ports.assign(Service::Apache, apache);
    ports.assign(Service::Mysql, mysql);
    ports.assign(Service::Php, php);
    ports
}

/// RAII guard: closes the process's Job Object on drop (via `ServiceProcess::kill`),
/// even mid panic-unwind, so a failing assertion can never leave a spawned
/// httpd.exe / mysqld.exe / php-cgi.exe running. `take()` lets a test kill (and
/// replace) the process deliberately without double-killing on drop.
struct Guard(Option<ServiceProcess>);
impl Guard {
    fn kill_now(&mut self) {
        if let Some(p) = self.0.take() {
            p.kill();
        }
    }

    /// Non-blocking: has the wrapped process exited on its own? `None` if it is
    /// still running, or if there is nothing left to check.
    fn try_wait(&self) -> Option<u32> {
        self.0.as_ref().and_then(|p| p.try_wait())
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.kill_now();
    }
}

fn spawn(svc: Service, cfg: &RampConfig, port: u16) -> Guard {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let proc = spawn_service(svc, cfg, port, tx)
        .unwrap_or_else(|e| panic!("spawn_service({svc:?}) failed: {e}"));
    Guard(Some(proc))
}

fn get(url: &str) -> Result<u16, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(5))
        .build();
    match agent.get(url).call() {
        Ok(r) => Ok(r.status()),
        Err(ureq::Error::Status(code, _)) => Ok(code),
        Err(e) => Err(e.to_string()),
    }
}

/// Poll `predicate` every 200ms until it returns true or `timeout` elapses.
/// Returns the elapsed time on success.
fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> Option<Duration> {
    let start = Instant::now();
    let deadline = start + timeout;
    loop {
        if predicate() {
            return Some(start.elapsed());
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Claim 2 in isolation: does real `php-cgi.exe` answer a FastCGI
/// `FCGI_GET_VALUES` management record the way `check_php_ready` expects?
/// This is the cheapest possible reproduction -- just PHP, no Apache.
#[test]
#[ignore]
fn fastcgi_probe_answers_real_php_cgi() {
    let dir = stack_dir();
    let cfg = make_config(&dir);
    create_runtime_dirs(&cfg);
    // Apache/MySQL ports here don't matter to this test (only PHP is spawned)
    // and this is deliberately the shared PHP_PORT-based config content, same
    // as the other tests -- see PHP_PORT_PROBE's doc comment for why the
    // *spawn* below uses a different port than this render does.
    let ports = ports_for(APACHE_PORT, MYSQL_PORT, PHP_PORT);
    write_configs(&cfg, &ports);

    let mut php = spawn(Service::Php, &cfg, PHP_PORT_PROBE);

    let ready = wait_until(Duration::from_secs(5), || {
        rampp::health::check_php_ready(PHP_PORT_PROBE)
    });
    println!("check_php_ready({PHP_PORT_PROBE}) became true after: {ready:?}");
    assert!(
        ready.is_some(),
        "real php-cgi.exe did not answer FCGI_GET_VALUES within 5s -- \
         if this fails, check_php_ready must fall back to a plain TCP connect"
    );

    php.kill_now();
}

/// Claims 1 (retry=0 worker-key match) and the access-log health-probe
/// exclusion, driven against real Apache + PHP-CGI.
#[test]
#[ignore]
fn php_restart_survives_without_a_sixty_second_blackout() {
    let dir = stack_dir();
    let cfg = make_config(&dir);
    create_runtime_dirs(&cfg);

    let ports = ports_for(APACHE_PORT, MYSQL_PORT, PHP_PORT);
    write_configs(&cfg, &ports);

    rampp::apache_conf::ensure_health_endpoint(&cfg).expect("ensure_health_endpoint");
    rampp::apache_conf::ensure_document_root(&cfg).expect("ensure_document_root");
    rampp::php_conf::ensure_php_dirs(&cfg).expect("ensure_php_dirs");

    let mut apache = spawn(Service::Apache, &cfg, APACHE_PORT);
    let mut php = spawn(Service::Php, &cfg, PHP_PORT);

    let base = format!("http://127.0.0.1:{APACHE_PORT}");

    let apache_ready = wait_until(Duration::from_secs(10), || {
        rampp::health::check_apache_ready(APACHE_PORT)
    });
    assert!(apache_ready.is_some(), "Apache never became ready");
    let php_ready = wait_until(Duration::from_secs(5), || {
        rampp::health::check_php_ready(PHP_PORT)
    });
    assert!(php_ready.is_some(), "PHP-CGI never became ready");

    // Sanity: a .php request succeeds while everything is healthy.
    let status = get(&format!("{base}/index.php")).expect("request failed outright");
    assert_eq!(
        status, 200,
        "expected 200 from index.php before any restart"
    );

    // Run real production-cadence health probes against the health endpoint
    // for >= 60s so the access-log exclusion claim gets a genuine workout
    // (the same probe run_health_checker performs every 2s in production).
    let health_url = format!("{base}{}", rampp::state::HEALTH_ENDPOINT_PATH);
    let health_deadline = Instant::now() + Duration::from_secs(65);
    let mut checks = 0u32;
    while Instant::now() < health_deadline {
        assert!(
            rampp::health::check_apache_ready(APACHE_PORT),
            "Apache health probe failed mid-run"
        );
        checks += 1;
        std::thread::sleep(Duration::from_secs(2));
    }
    println!("ran {checks} health probes against {health_url} over >=60s");

    // --- The empirical claim under test: retry=0 ---------------------------
    // Kill php-cgi (closing its Job Object -- equivalent, from Apache's POV, to
    // the backend disappearing), then immediately confirm the request fails.
    php.kill_now();
    let after_kill = get(&format!("{base}/index.php")).unwrap_or(0);
    println!("request immediately after php-cgi died: HTTP {after_kill}");
    assert_ne!(
        after_kill, 200,
        "request must fail once php-cgi is actually dead, or this test proves nothing"
    );

    // Respawn php-cgi on the same port as fast as possible, then hit the
    // proxy again the instant it is ready. Without `retry=0` Apache marks the
    // fcgi worker dead for 60s from the *first* failure above, so this request
    // would still 503 even though a healthy php-cgi is listening again.
    let restart_started = Instant::now();
    php = spawn(Service::Php, &cfg, PHP_PORT);
    let php_ready_again = wait_until(Duration::from_secs(10), || {
        rampp::health::check_php_ready(PHP_PORT)
    });
    assert!(php_ready_again.is_some(), "php-cgi never came back up");

    let status = get(&format!("{base}/index.php")).expect("request failed outright");
    let elapsed = restart_started.elapsed();
    println!("request right after php-cgi respawn: HTTP {status} after {elapsed:?}");
    assert_eq!(
        status, 200,
        "got {status} after {elapsed:?} -- without ProxySet retry=0 Apache holds \
         the fcgi worker dead for ~60s after the first failure"
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "recovery took {elapsed:?}, suspiciously close to the 60s dead-worker window"
    );

    php.kill_now();
    apache.kill_now();

    let access_log =
        std::fs::read_to_string(dir.join("logs").join("apache_access.log")).unwrap_or_default();
    assert!(
        !access_log.contains(rampp::state::HEALTH_ENDPOINT_PATH),
        "the {}s health-probe run must never appear in apache_access.log",
        65
    );
}

/// Claim 3: `mysqladmin shutdown` against a Job-Object-managed `mysqld`
/// produces a genuinely clean shutdown (no crash recovery on next start), and
/// the COM_QUIT-based health probe leaves no `Aborted connection` spam.
#[test]
#[ignore]
fn mysql_shutdown_is_clean_and_health_checks_leave_no_aborted_connections() {
    // Serializes with do_kill_waits_for_mysqld_...: both tests' mysqld write
    // to the SAME logs/mysql_error.log (see MYSQL_LOG_LOCK's doc comment
    // above for why per-test port/data-dir isolation does not isolate the
    // log file too). Held for the whole test body.
    let _mysql_log_guard = MYSQL_LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = stack_dir();
    let cfg = make_config(&dir);
    create_runtime_dirs(&cfg);

    let ports = ports_for(APACHE_PORT, MYSQL_PORT, PHP_PORT);
    write_configs(&cfg, &ports);

    if rampp::mysql_conf::needs_initialization(&cfg) {
        println!("initializing MySQL data directory (first run) -- this can take a while");
        rampp::mysql_conf::initialize_mysql(&cfg).expect("mysqld --initialize-insecure failed");
    }

    let error_log = dir.join("logs").join("mysql_error.log");

    let mut mysql = spawn(Service::Mysql, &cfg, MYSQL_PORT);
    let ready = wait_until(Duration::from_secs(30), || {
        rampp::health::check_mysql_ready(MYSQL_PORT)
    });
    assert!(ready.is_some(), "mysqld never became ready: {ready:?}");
    println!("mysqld ready after {ready:?}");

    // Real production cadence: probe_mysql every 2s (as run_health_checker
    // does), each probe doing a real connect + handshake + COM_QUIT, for
    // >= 60s. If COM_QUIT is not a clean disconnect, this is exactly the
    // pattern that spams "Aborted connection" into mysql_error.log.
    let deadline = Instant::now() + Duration::from_secs(65);
    let mut checks = 0u32;
    while Instant::now() < deadline {
        match rampp::health::probe_mysql(MYSQL_PORT) {
            rampp::health::MysqlProbe::Ready => {}
            other => panic!("unexpected probe result mid-run: {other:?}"),
        }
        checks += 1;
        std::thread::sleep(Duration::from_secs(2));
    }
    println!("ran {checks} MySQL health probes over >=60s");

    let log_after_probing = std::fs::read_to_string(&error_log).unwrap_or_default();
    assert!(
        !log_after_probing.contains("Aborted connection"),
        "COM_QUIT should keep the health-check probe from producing Aborted \
         connection warnings; log tail:\n{}",
        tail(&log_after_probing, 4000)
    );

    // --- The empirical claim under test: mysqladmin shutdown -----------------
    let shutdown_started = Instant::now();
    let shutdown_result =
        rampp::mysql_conf::graceful_shutdown(&cfg, MYSQL_PORT, Duration::from_secs(15));
    let client_exit_elapsed = shutdown_started.elapsed();
    println!("graceful_shutdown result: {shutdown_result:?} after {client_exit_elapsed:?}");
    assert!(
        shutdown_result.is_ok(),
        "mysqladmin shutdown failed: {shutdown_result:?}"
    );

    // IMPORTANT: `graceful_shutdown`'s Ok(()) only means the `mysqladmin`
    // CLIENT exited after the shutdown command was accepted -- NOT that mysqld
    // itself has finished its own shutdown sequence (flushing InnoDB, closing
    // the buffer pool, etc.). A prior run of this exact test measured a real
    // ~1.4s gap between the two on this machine. `executor.rs`'s `do_kill`
    // does not account for that gap: it calls `graceful_shutdown` and then
    // *immediately* signals the watcher thread, whose `select!` reacts to a
    // kill signal without waiting even one 100ms poll tick (see the watcher's
    // doc comment) and unconditionally closes the Job Object. That almost
    // certainly races ahead of mysqld's own exit and forcibly kills it mid
    // shutdown -- reproducing defect 17 (unclean stop, crash recovery on next
    // start) even though `graceful_shutdown` itself "succeeded". This test
    // waits for the real process exit before touching the Job Object, which
    // is what `do_kill` would need to do to actually get a clean shutdown;
    // see the Layer 3 report for the measured race and why it is not fixed
    // here (out of this task's file scope: the fix belongs in `executor.rs`).
    let natural_exit = wait_until(Duration::from_secs(10), || mysql.try_wait().is_some());
    println!(
        "mysqld's own process exit observed after {natural_exit:?} (from graceful_shutdown returning)"
    );
    assert!(
        natural_exit.is_some(),
        "mysqld did not exit on its own within 10s of a successful graceful_shutdown"
    );
    // The process is already gone; this just releases the now-empty Job Object
    // handle (matching do_kill's unconditional close, but harmlessly -- there
    // is nothing left to kill).
    mysql.kill_now();

    let log_after_shutdown = std::fs::read_to_string(&error_log).unwrap_or_default();
    let shutdown_offset = log_after_shutdown.len();
    assert!(
        log_after_shutdown
            .to_lowercase()
            .contains("shutdown complete"),
        "expected a normal MySQL shutdown sequence; log tail:\n{}",
        tail(&log_after_shutdown, 4000)
    );

    // --- Restart and confirm no crash-recovery messages ---------------------
    let mut mysql2 = spawn(Service::Mysql, &cfg, MYSQL_PORT);
    let ready2 = wait_until(Duration::from_secs(30), || {
        rampp::health::check_mysql_ready(MYSQL_PORT)
    });
    assert!(ready2.is_some(), "mysqld did not come back up cleanly");

    let full_log = std::fs::read_to_string(&error_log).unwrap_or_default();
    let restart_tail = &full_log[shutdown_offset.min(full_log.len())..];
    let lower = restart_tail.to_lowercase();
    assert!(
        !lower.contains("crash recovery") && !lower.contains("recovering"),
        "restart after a clean shutdown must not trigger InnoDB crash recovery; \
         restart log tail:\n{}",
        tail(restart_tail, 4000)
    );
    println!(
        "restart log after clean shutdown (first 2000 chars):\n{}",
        &restart_tail[..restart_tail.len().min(2000)]
    );

    mysql2.kill_now();
}

/// Used only by `do_kill_waits_for_mysqld_to_actually_exit_before_forcing_the_job_object`,
/// with its own data directory and my.ini so it is fully isolated from
/// `MYSQL_PORT`'s instance (used by
/// `mysql_shutdown_is_clean_and_health_checks_leave_no_aborted_connections`)
/// -- the two are safe to run concurrently under cargo's default runner.
const MYSQL_PORT_DO_KILL: u16 = 13307;

/// Finding 4 from code review: `executor.rs::do_kill` called `graceful_shutdown`
/// (which only waits for the `mysqladmin` CLIENT to exit, ~0.1-0.2s) and then
/// *immediately* signalled the watcher thread, whose `select!` reacts to a kill
/// signal without waiting even one poll tick and unconditionally closes the Job
/// Object -- racing ahead of mysqld's own shutdown, which this same suite
/// measured taking ~1.2-1.5s longer. This test drives the REAL `Executor` (not
/// a reimplementation) through its public `execute`/`start_health_check` API,
/// exactly the way the reducer's side effects would, so it exercises the actual
/// fixed `do_kill` code path against a real, Job-Object-managed mysqld.
#[test]
#[ignore]
fn do_kill_waits_for_mysqld_to_actually_exit_before_forcing_the_job_object() {
    use rampp::events::{Event, SideEffect};
    use rampp::executor::Executor;
    use rampp::logger::SharedLog;
    use rampp::state::AppState;

    // Serializes with mysql_shutdown_is_clean_and_...: both tests' mysqld
    // write to the SAME logs/mysql_error.log (see MYSQL_LOG_LOCK's doc
    // comment above for why per-test port/data-dir isolation does not
    // isolate the log file too). Held for the whole test body.
    let _mysql_log_guard = MYSQL_LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let dir = stack_dir();
    let mut cfg = make_config(&dir);
    // Fully isolated MySQL instance: own port, own data dir, own my.ini.
    cfg.mysql.port = MYSQL_PORT_DO_KILL;
    cfg.mysql.data_dir = dir.join("mysql-do-kill-data");
    cfg.mysql.ini = dir.join("mysql-do-kill").join("my.ini");
    create_runtime_dirs(&cfg);

    // Mirror the fix for a genuinely fresh install (see the Layer 3 report's
    // "main.rs runs initialize_mysql before the reconciler ever writes my.ini"
    // finding, discovered while building this test): mysqld's
    // --defaults-file must point at a real file before --initialize-insecure
    // runs, or mysqld prints "Failed to open required defaults file" /
    // "Fatal error in defaults handling. Program aborted!" and exits 0 --
    // silently succeeding without writing anything to the data directory.
    rampp::config::atomic_write(
        &cfg.mysql.ini,
        rampp::mysql_conf::generate_my_ini_with_port(&cfg, MYSQL_PORT_DO_KILL).as_bytes(),
    )
    .expect("write isolated my.ini before initialize_mysql");

    if rampp::mysql_conf::needs_initialization(&cfg) {
        println!("initializing isolated MySQL data directory for the do_kill race test");
        rampp::mysql_conf::initialize_mysql(&cfg).expect("mysqld --initialize-insecure failed");
    }

    let (tx, rx) = crossbeam_channel::unbounded();
    let mut executor = Executor::new(cfg.clone(), tx, SharedLog::new());

    let mut state = AppState::new(cfg.clone());
    state.ports.assign(Service::Mysql, MYSQL_PORT_DO_KILL);

    // Real production spawn path: do_reconcile (writes the isolated my.ini) ->
    // do_kill (no-op, nothing running yet) -> spawn_service -> watcher thread.
    executor.execute(
        vec![SideEffect::SpawnService {
            service: Service::Mysql,
            port: MYSQL_PORT_DO_KILL,
        }],
        &state,
    );

    let ready = wait_until(Duration::from_secs(30), || {
        rampp::health::check_mysql_ready(MYSQL_PORT_DO_KILL)
    });
    assert!(ready.is_some(), "mysqld never became ready: {ready:?}");
    println!("mysqld ready after {ready:?}");

    // Exactly what the reducer does the moment it sees Running: tells the
    // executor to start health checks, which is also what sets `became_ready`
    // -- the gate `should_attempt_graceful_stop` requires before `do_kill`
    // will even attempt a graceful shutdown at all.
    executor.start_health_check(Service::Mysql, &state);
    // Give the health checker a moment to actually run at least once, matching
    // how long a real Running service would have had before a stop arrives.
    std::thread::sleep(Duration::from_millis(500));

    let error_log = dir.join("logs").join("mysql_error.log");
    let pre_kill_len = std::fs::metadata(&error_log).map(|m| m.len()).unwrap_or(0);

    // The actual code path under test: reachable only through execute() +
    // KillService, exactly as the reducer's SideEffect::KillService(Mysql)
    // would drive it.
    let kill_started = Instant::now();
    executor.execute(vec![SideEffect::KillService(Service::Mysql)], &state);
    let kill_elapsed = kill_started.elapsed();
    println!("do_kill(Mysql) returned after {kill_elapsed:?}");

    // Not asserted on -- just drained so the unbounded channel doesn't matter.
    let drained: Vec<Event> = rx.try_iter().collect();
    println!("events emitted during do_kill: {}", drained.len());

    let log_after_kill = std::fs::read_to_string(&error_log).unwrap_or_default();
    let new_bytes = &log_after_kill[pre_kill_len.min(log_after_kill.len() as u64) as usize..];
    println!("mysql_error.log written during do_kill:\n{new_bytes}");
    assert!(
        log_after_kill.to_lowercase().contains("shutdown complete"),
        "do_kill must wait for mysqld's own exit before forcing the Job Object \
         closed, or the shutdown sequence gets truncated; log tail:\n{}",
        tail(&log_after_kill, 4000)
    );
    let shutdown_offset = log_after_kill.len();

    // Restart (via spawn_service directly, same as the other MySQL test) and
    // confirm no crash-recovery messages -- the actual end-to-end proof that
    // do_kill's shutdown was genuinely clean, not just log-message-clean.
    let mut mysql2 = spawn(Service::Mysql, &cfg, MYSQL_PORT_DO_KILL);
    let ready2 = wait_until(Duration::from_secs(30), || {
        rampp::health::check_mysql_ready(MYSQL_PORT_DO_KILL)
    });
    assert!(
        ready2.is_some(),
        "mysqld did not come back up cleanly after do_kill"
    );

    let full_log = std::fs::read_to_string(&error_log).unwrap_or_default();
    let restart_tail = &full_log[shutdown_offset.min(full_log.len())..];
    let lower = restart_tail.to_lowercase();
    println!(
        "restart log after do_kill (first 2000 chars):\n{}",
        &restart_tail[..restart_tail.len().min(2000)]
    );
    assert!(
        !lower.contains("crash recovery") && !lower.contains("recovering"),
        "restart after do_kill must not trigger InnoDB crash recovery; \
         restart log tail:\n{}",
        tail(restart_tail, 4000)
    );

    mysql2.kill_now();
}

fn tail(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[s.len() - max..]
    }
}
