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
    AppState, DesiredServiceState, PersistedState, Service, ServiceState, COMMAND_DEBOUNCE,
    SHUTDOWN_GRACE_PERIOD,
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

    log::info!("RAMP starting — install_dir: {}", install_dir.display());

    // Ensure ramp.toml exists
    if let Err(e) = write_default_config(&install_dir) {
        fatal(&format!("cannot write default config: {e}"));
    }

    // Load and validate config
    let config = match load_config(&install_dir) {
        Ok(c) => c,
        Err(e) => fatal(&format!("invalid ramp.toml: {e}")),
    };

    // --- Startup provisioning (idempotent, safe to run every launch) ------

    // 1. Create required runtime directories
    if let Err(e) = create_runtime_dirs(&config) {
        fatal(&format!("cannot create runtime directories: {e}"));
    }

    // 2. Generate httpd.conf if missing
    if let Err(e) = apache_conf::ensure_httpd_conf(&config) {
        log::warn!("cannot generate httpd.conf: {e}");
    }
    if let Err(e) = apache_conf::ensure_htdocs(&config) {
        log::warn!("cannot create htdocs: {e}");
    }

    // 3. Generate my.ini if missing
    if let Err(e) = mysql_conf::ensure_my_ini(&config) {
        log::warn!("cannot generate my.ini: {e}");
    }

    // 4. MySQL data directory initialization — deferred: if mysqld binary is missing
    //    we record the error and let the UI surface it rather than crashing silently.
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

    // 5. Generate php.ini if missing (PHP-CGI is optional — missing binary is not fatal)
    if let Err(e) = php_conf::ensure_php_dirs(&config) {
        log::warn!("cannot create logs dir: {e}");
    }
    if let Err(e) = php_conf::ensure_php_ini(&config) {
        log::warn!("cannot generate php.ini: {e}");
    }

    // --- Event loop setup -------------------------------------------------

    let persisted = load_persisted_state(&install_dir);

    let mut app_state = AppState::new(config.clone());
    app_state.apache.desired = persisted.apache_desired;
    app_state.mysql.desired = persisted.mysql_desired;
    app_state.php.desired = persisted.php_desired;

    // Surface deferred provisioning errors into the UI
    if let Some(e) = mysql_init_error {
        app_state.mysql.state = ServiceState::Error;
        app_state.mysql.last_error = Some(format!("Init failed: {e}"));
        app_state.mysql.desired = DesiredServiceState::Stopped;
    }

    // --- phpMyAdmin startup reconciliation ----------------------------------
    // Must run after AppState and RampConfig are ready, before the event loop starts.
    // Ensures phpmyadmin.conf always exists on disk (Apache Include never fails),
    // and force-disables phpMyAdmin if its directory is missing.
    {
        let phpmyadmin_dir = config.install_dir.join("phpmyadmin");
        let phpmyadmin_dir_exists = phpmyadmin_dir.exists();
        app_state.phpmyadmin_dir_exists = phpmyadmin_dir_exists;

        if persisted.phpmyadmin_enabled && !phpmyadmin_dir_exists {
            log::warn!(
                "phpMyAdmin enabled in state but not found at {} — disabling",
                phpmyadmin_dir.display()
            );
            app_state.phpmyadmin_enabled = false;
            if let Err(e) = phpmyadmin_conf::write_phpmyadmin_apache_conf_disabled(&config) {
                log::error!("Failed to write phpmyadmin.conf (disabled): {e}");
            }
            // Persist the force-disabled state so it survives restart
            // NOTE: must be kept in sync with do_persist in executor.rs
            let state_path = install_dir.join("ramp.state");
            let p = PersistedState {
                phpmyadmin_enabled: false,
                ..persisted.clone()
            };
            match serde_json::to_vec_pretty(&p) {
                Ok(data) => {
                    if let Err(e) = config::atomic_write(&state_path, &data) {
                        log::error!("Failed to persist phpmyadmin disabled state: {e}");
                    }
                }
                Err(e) => log::error!("Failed to serialize phpmyadmin disabled state: {e}"),
            }
        } else if persisted.phpmyadmin_enabled && phpmyadmin_dir_exists {
            app_state.phpmyadmin_enabled = true;
            if let Err(e) =
                phpmyadmin_conf::write_phpmyadmin_apache_conf_enabled(&config, config.php.port)
            {
                log::error!("Failed to write phpmyadmin.conf (enabled): {e}");
            }
        } else {
            // phpmyadmin_enabled == false: write empty conf (idempotent)
            app_state.phpmyadmin_enabled = false;
            if let Err(e) = phpmyadmin_conf::write_phpmyadmin_apache_conf_disabled(&config) {
                log::error!("Failed to write phpmyadmin.conf (disabled): {e}");
            }
        }
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

            let (new_state, effects) = reducer(state, event);
            state = new_state;

            // Start health checks when a service first reaches Running
            if apache_was != ServiceState::Running && state.apache.state == ServiceState::Running {
                executor.start_health_check(Service::Apache);
            }
            if mysql_was != ServiceState::Running && state.mysql.state == ServiceState::Running {
                executor.start_health_check(Service::Mysql);
            }
            if php_was != ServiceState::Running && state.php.state == ServiceState::Running {
                executor.start_health_check(Service::Php);
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
                executor.shutdown_and_join();
                log::info!("shutdown: all processes stopped");
                let _ = shutdown_done_tx.send(());
                break;
            }
        }
    });

    // egui must run on the main thread (Windows GUI requirement)
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RAMP")
            .with_inner_size([520.0, 480.0])
            .with_min_inner_size([400.0, 300.0])
            .with_visible(true),
        ..Default::default()
    };

    eframe::run_native(
        "RAMP",
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

fn load_persisted_state(install_dir: &std::path::Path) -> PersistedState {
    let path = install_dir.join("ramp.state");
    std::fs::read(&path)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_else(PersistedState::default_stopped)
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
    let title: Vec<u16> = "RAMP — Fatal Error\0".encode_utf16().collect();
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
