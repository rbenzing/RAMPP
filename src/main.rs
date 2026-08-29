#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod apache_conf;
mod config;
mod events;
mod executor;
mod health;
mod logger;
mod mysql_conf;
mod paths;
mod php_conf;
mod phpmyadmin_conf;
mod process;
mod provision;
mod reducer;
mod state;
mod tray;
mod ui;

use config::{load_config, write_default_config};
use events::Event;
use executor::Executor;
use logger::SharedLog;
use reducer::reducer;
use state::{
    AppState, DesiredServiceState, Service, ServiceState, COMMAND_DEBOUNCE, SHUTDOWN_GRACE_PERIOD,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use windows::core::PCWSTR;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Resolve install_dir from the executable's location
    let install_dir = std::env::current_exe()
        .expect("cannot resolve executable path")
        .parent()
        .expect("executable has no parent directory")
        .to_path_buf();

    log::info!("RAMPP starting — install_dir: {}", install_dir.display());

    // Ensure rampp.toml exists
    if let Err(e) = write_default_config(&install_dir) {
        fatal(&format!("cannot write default config: {e}"));
    }

    // Load and validate config
    let config = match load_config(&install_dir) {
        Ok(c) => c,
        Err(e) => fatal(&format!("invalid rampp.toml: {e}")),
    };

    // --- Startup provisioning (idempotent, safe to run every launch) ------
    //
    // Ordering matters here and is easy to get wrong again: MySQL initialization
    // (step 5) needs `mysql/my.ini` to already exist, and `provision::reconcile`
    // (step 4) is the ONLY thing that writes it. This block's steps must stay in
    // this order — reconcile before initialize_mysql — or a genuinely fresh
    // install silently fails: `mysqld --initialize-insecure` against a missing
    // `--defaults-file` prints "Fatal error in defaults handling. Program
    // aborted!" but still exits 0, so `initialize_mysql` reports success while
    // the data directory stays completely empty. (This was a real regression:
    // the original code called `ensure_my_ini` immediately before
    // `initialize_mysql`, in the correct order. Task 10 of the
    // config-integrity-and-connection-resilience plan deleted `ensure_my_ini`
    // and moved all config writing into `provision::reconcile`, which — before
    // this fix — ran much later, after MySQL init. See the Layer 3 task's
    // report for the empirical reproduction.)

    // 1. Create required runtime directories
    if let Err(e) = create_runtime_dirs(&config) {
        fatal(&format!("cannot create runtime directories: {e}"));
    }

    // 2. Directories and the static health endpoint (not config content).
    if let Err(e) = apache_conf::ensure_health_endpoint(&config) {
        log::warn!("cannot create health endpoint: {e}");
    }
    if let Err(e) = apache_conf::ensure_document_root(&config) {
        log::warn!("cannot create document root: {e}");
    }
    if let Err(e) = php_conf::ensure_php_dirs(&config) {
        log::warn!("cannot create logs dir: {e}");
    }

    // --- Event loop setup -------------------------------------------------

    // 3. Load persisted state and build app_state. None of this depends on any
    // file `reconcile` writes (my.ini, httpd.conf, php.ini, phpmyadmin.conf,
    // config.inc.php) — only on `rampp.state` and the `phpmyadmin/` directory's
    // mere existence — so it is safe here, before reconcile.
    let persisted = config::read_persisted_state(&install_dir.join("rampp.state"));

    let mut app_state = AppState::new(config.clone());
    app_state.apache.desired = persisted.apache_desired;
    app_state.mysql.desired = persisted.mysql_desired;
    app_state.php.desired = persisted.php_desired;

    // phpMyAdmin cannot be enabled without an install to point at.
    let phpmyadmin_dir = config.install_dir.join("phpmyadmin");
    let phpmyadmin_dir_exists = phpmyadmin_dir.is_dir();
    app_state.phpmyadmin_dir_exists = phpmyadmin_dir_exists;
    app_state.phpmyadmin_enabled = persisted.phpmyadmin_enabled && phpmyadmin_dir_exists;

    // One secret, generated once and persisted, so cookie auth stays stable.
    let mut persisted = persisted;
    if persisted.phpmyadmin_blowfish_secret.is_none() {
        persisted.phpmyadmin_blowfish_secret = Some(phpmyadmin_conf::generate_blowfish_secret());
    }
    persisted.phpmyadmin_enabled = app_state.phpmyadmin_enabled;
    if let Err(e) = config::write_persisted_state(&install_dir.join("rampp.state"), &persisted) {
        log::error!("cannot persist state: {e}");
    }

    // 4. Bring every managed config file in line — including writing `my.ini`
    // for the very first time on a fresh install. Nothing is running yet, so
    // no restarts can be needed and the report is only logged. This MUST run
    // before step 5 (MySQL initialization) — see the ordering note above.
    let secret = persisted
        .phpmyadmin_blowfish_secret
        .clone()
        .unwrap_or_default();
    let report = provision::reconcile(&provision::desired_configs(
        &config,
        &app_state.ports,
        app_state.phpmyadmin_enabled,
        phpmyadmin_dir_exists,
        &secret,
    ));
    for (file, err) in &report.errors {
        log::error!("cannot write {file}: {err}");
    }
    for file in &report.user_owned {
        log::info!("{file} is user-owned — RAMPP will not modify it");
    }

    // 5. MySQL data directory initialization — deferred: if mysqld binary is
    // missing we record the error and let the UI surface it rather than
    // crashing silently. Runs after step 4 so `--defaults-file` always points
    // at a `my.ini` that actually exists, even on the very first launch.
    let mysql_init_error: Option<String> = if mysql_conf::needs_initialization(&config) {
        log::info!("MySQL data directory is empty — running --initialize-insecure");
        match mysql_conf::initialize_mysql(&config) {
            Ok(()) => None,
            Err(e) => {
                log::error!("MySQL initialization failed: {e}");
                Some(e)
            }
        }
    } else {
        None
    };

    // 6. Surface deferred provisioning errors into the UI
    if let Some(e) = mysql_init_error {
        app_state.mysql.state = ServiceState::Error;
        app_state.mysql.last_error = Some(format!("Init failed: {e}"));
        app_state.mysql.desired = DesiredServiceState::Stopped;
    }

    let shared_state = Arc::new(Mutex::new(app_state.clone()));
    let shared_state_writer = shared_state.clone();

    // Bounded event channel — backpressure per spec
    let (tx, rx) = crossbeam_channel::bounded::<Event>(256);

    // Tick timer thread (drives health check cycle)
    let tick_tx = tx.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(state::HEALTH_CHECK_INTERVAL);
        match tick_tx.send(Event::Tick) {
            Ok(()) => {}
            Err(_) => {
                // Receiver dropped — event loop has shut down; exit the tick thread.
                log::debug!("tick thread: event channel closed, exiting");
                break;
            }
        }
    });

    let log = SharedLog::new();
    let log_for_ui = log.clone();

    // Channel: tray → show egui window
    let (show_tx, show_rx) = crossbeam_channel::bounded::<()>(4);

    // System tray thread
    let tray_tx = tx.clone();
    std::thread::spawn(move || tray::run_tray(tray_tx, show_tx));

    // Shutdown coordination: event loop signals this when all processes are dead.
    // main() waits on it after run_native returns, guaranteeing clean process teardown.
    let (shutdown_done_tx, shutdown_done_rx) = crossbeam_channel::bounded::<()>(1);

    // Event loop thread
    let config_for_executor = config.clone();
    let log_for_loop = log.clone();
    let tx_for_loop = tx.clone();
    // Bound before the closure moves config_for_executor (which Executor::new consumes),
    // so the Tick handler below can still probe the phpmyadmin directory each cycle.
    let pma_probe_dir = config.install_dir.join("phpmyadmin");
    std::thread::spawn(move || {
        let mut state = app_state;
        let mut executor = Executor::new(config_for_executor, tx_for_loop.clone(), log_for_loop);
        let mut last_cmd: HashMap<String, Instant> = HashMap::new();

        // Restore desired running services on startup
        for svc in [Service::Apache, Service::Mysql, Service::Php] {
            if state.service(svc).desired == DesiredServiceState::Running {
                let _ = tx_for_loop.send(Event::StartService(svc));
            }
        }

        while let Ok(event) = rx.recv() {
            // Debounce rapid user commands
            if let Some(key) = debounce_key(&event) {
                let now = Instant::now();
                if let Some(&last) = last_cmd.get(&key) {
                    if now.duration_since(last) < COMMAND_DEBOUNCE {
                        log::debug!("debounced: {key}");
                        continue;
                    }
                }
                last_cmd.insert(key, now);
            }

            let is_shutdown = matches!(event, Event::ShutdownAll);

            let apache_was = state.apache.state;
            let mysql_was = state.mysql.state;
            let php_was = state.php.state;

            if matches!(event, Event::Tick) {
                let exists = pma_probe_dir.is_dir();
                if exists != state.phpmyadmin_dir_exists {
                    let _ = tx_for_loop.send(Event::PhpMyAdminDirChanged(exists));
                }
            }

            let (new_state, effects) = reducer(state, event);
            state = new_state;

            // Start health checks when a service first reaches Running
            if apache_was != ServiceState::Running && state.apache.state == ServiceState::Running {
                executor.start_health_check(Service::Apache, &state);
            }
            if mysql_was != ServiceState::Running && state.mysql.state == ServiceState::Running {
                executor.start_health_check(Service::Mysql, &state);
            }
            if php_was != ServiceState::Running && state.php.state == ServiceState::Running {
                executor.start_health_check(Service::Php, &state);
            }

            executor.execute(effects, &state);

            // Recover from a poisoned mutex (e.g. UI thread panicked while holding
            // the lock). The data is still valid — overwrite it with current state.
            let mut s = match shared_state_writer.lock() {
                Ok(guard) => guard,
                Err(poisoned) => {
                    log::error!("state mutex poisoned — recovering (UI thread may have crashed)");
                    poisoned.into_inner()
                }
            };
            *s = state.clone();

            if is_shutdown {
                // Block here until every managed process is confirmed dead.
                // Watcher threads call WaitForSingleObject so this returns as soon
                // as the OS has terminated all process trees — typically < 1ms.
                log::info!("shutdown: waiting for all processes to terminate");
                executor.shutdown_and_join(&state);
                log::info!("shutdown: all processes stopped");
                let _ = shutdown_done_tx.send(());
                break;
            }
        }
    });

    // egui must run on the main thread (Windows GUI requirement)
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RAMPP")
            .with_inner_size([520.0, 480.0])
            .with_min_inner_size([400.0, 300.0])
            .with_visible(true),
        ..Default::default()
    };

    eframe::run_native(
        "RAMPP",
        native_options,
        Box::new(|_cc| {
            Box::new(ui::RampApp::new(shared_state, tx, log_for_ui, show_rx))
                as Box<dyn eframe::App>
        }),
    )
    .unwrap_or_else(|e| {
        eprintln!("GUI error: {e}");
        std::process::exit(1);
    });

    // eframe has returned — on_exit already sent ShutdownAll.
    // Wait for the event loop to confirm all processes are dead before we exit.
    // The timeout is a safety net; in practice shutdown completes in milliseconds.
    log::info!("waiting for clean shutdown (grace period: {SHUTDOWN_GRACE_PERIOD:?})");
    match shutdown_done_rx.recv_timeout(SHUTDOWN_GRACE_PERIOD) {
        Ok(()) => log::info!("clean shutdown complete"),
        Err(_) => log::warn!(
            "shutdown timed out after {SHUTDOWN_GRACE_PERIOD:?} — processes may still be running"
        ),
    }
}

/// Create all runtime directories that must exist before services start.
fn create_runtime_dirs(cfg: &crate::state::RampConfig) -> Result<(), String> {
    let dirs = [
        cfg.install_dir.join("logs"),
        cfg.install_dir.join("tmp").join("sessions"),
        cfg.install_dir.join("apache").join("conf"),
        cfg.mysql.data_dir.clone(),
    ];
    for dir in &dirs {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    Ok(())
}

fn debounce_key(event: &Event) -> Option<String> {
    match event {
        Event::StartService(s) => Some(format!("start:{s}")),
        Event::StopService(s) => Some(format!("stop:{s}")),
        Event::RestartService(s) => Some(format!("restart:{s}")),
        _ => None,
    }
}

/// Show a modal error dialog and exit. Never returns.
/// Safe to call before the egui window exists and with windows_subsystem = "windows".
fn fatal(msg: &str) -> ! {
    log::error!("{msg}");
    let title: Vec<u16> = "RAMPP — Fatal Error\0".encode_utf16().collect();
    let mut body: Vec<u16> = msg.encode_utf16().collect();
    body.push(0);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(body.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
    std::process::exit(1);
}
