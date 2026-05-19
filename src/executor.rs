use crate::apache_conf::rewrite_httpd_conf_with_ports;
use crate::config::atomic_write;
use crate::events::{Event, SideEffect};
use crate::health::{poll_until_ready, run_health_checker};
use crate::logger::SharedLog;
use crate::mysql_conf::rewrite_my_ini_with_port;
use crate::phpmyadmin_conf;
use crate::process::{find_available_port, spawn_service, ServiceProcess};
use crate::state::{AppState, PersistedState, RampConfig, Service, PORT_SCAN_RANGE};
use crossbeam_channel::Sender;
use std::collections::HashMap;

/// Per-service runtime handles.
struct ServiceHandles {
    /// Channel to signal the watcher thread to force-kill the process.
    kill_tx: crossbeam_channel::Sender<()>,
    /// Join handle for the watcher thread — used during graceful shutdown to
    /// block until the process is confirmed dead before RAMP exits.
    watcher_join: Option<std::thread::JoinHandle<()>>,
    /// Channel to stop the health checker.
    health_stop_tx: Option<crossbeam_channel::Sender<()>>,
    /// Join handle for the health checker thread — joined during shutdown to
    /// bound its lifetime and prevent sends to a dead event channel.
    health_join: Option<std::thread::JoinHandle<()>>,
}

/// Executor translates SideEffects into real I/O. Owns all live process/thread handles.
pub struct Executor {
    config: RampConfig,
    tx: Sender<Event>,
    log: SharedLog,
    handles: HashMap<Service, ServiceHandles>,
    /// Effective port per service — set when do_spawn resolves a free port.
    /// Used by readiness/health checks and by config regen for cross-service ports
    /// (e.g. Apache's httpd.conf needs PHP's effective port for the FastCGI proxy).
    effective_ports: HashMap<Service, u16>,
}

impl Executor {
    pub fn new(config: RampConfig, tx: Sender<Event>, log: SharedLog) -> Self {
        Self {
            config,
            tx,
            log,
            handles: HashMap::new(),
            effective_ports: HashMap::new(),
        }
    }

    /// Effective port to bind/probe — falls back to the configured port until a spawn resolves one.
    fn effective_port(&self, svc: Service) -> u16 {
        self.effective_ports
            .get(&svc)
            .copied()
            .unwrap_or_else(|| self.port(svc))
    }

    pub fn execute(&mut self, effects: Vec<SideEffect>, state: &AppState) {
        for effect in effects {
            match effect {
                SideEffect::SpawnService(svc) => self.do_spawn(svc),
                SideEffect::KillService(svc) => self.do_kill(svc),
                SideEffect::StartReadinessCheck(svc) => self.do_readiness_check(svc),
                SideEffect::StopHealthCheck(svc) => self.do_stop_health(svc),
                SideEffect::ScheduleRetry { service, delay } => {
                    self.do_schedule_retry(service, delay)
                }
                SideEffect::LogEvent(msg) => {
                    log::info!("{msg}");
                    self.log.push(msg);
                }
                SideEffect::PersistDesiredState => self.do_persist(state),
                SideEffect::TogglePhpMyAdmin(enable) => self.do_toggle_phpmyadmin(enable, state),
                SideEffect::OpenPhpMyAdminBrowser => self.do_open_phpmyadmin_browser(),
            }
        }
    }

    /// Start health checks for a service that just became Running.
    pub fn start_health_check(&mut self, svc: Service) {
        self.do_stop_health(svc);
        let port = self.effective_port(svc);
        let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        let entry = self.handles.entry(svc).or_insert_with(|| {
            let (kill_tx, _) = crossbeam_channel::bounded(1);
            ServiceHandles {
                kill_tx,
                watcher_join: None,
                health_stop_tx: None,
                health_join: None,
            }
        });
        entry.health_stop_tx = Some(stop_tx);
        let tx = self.tx.clone();
        let join = std::thread::spawn(move || run_health_checker(svc, port, tx, stop_rx));
        entry.health_join = Some(join);
    }

    fn do_spawn(&mut self, svc: Service) {
        let configured = self.port(svc);

        // Scan upward for a free port. None → every port in the range is occupied.
        let chosen = match find_available_port(configured, PORT_SCAN_RANGE) {
            Some(p) => p,
            None => {
                let _ = self.tx.send(Event::PortConflictDetected(svc));
                return;
            }
        };

        // Always regenerate the service config file from the chosen port. This is
        // critical: skipping the rewrite when chosen == configured leaves any stale
        // config on disk (e.g. Listen 127.0.0.1:8081 from a previous crash-loop run)
        // pointing at a different port than the one we'll probe for readiness, which
        // turns into a phantom crash loop. PHP doesn't need a config rewrite — its
        // port is a CLI flag.
        let result = match svc {
            Service::Apache => {
                let php_port = self.effective_port(Service::Php);
                rewrite_httpd_conf_with_ports(&self.config, chosen, php_port)
            }
            Service::Mysql => rewrite_my_ini_with_port(&self.config, chosen),
            Service::Php => Ok(()),
        };
        if let Err(reason) = result {
            let _ = self.tx.send(Event::ProcessSpawnFailed {
                service: svc,
                reason: format!("config regen for port {chosen}: {reason}"),
            });
            return;
        }

        // If this is PHP and Apache is currently running, refresh httpd.conf so the
        // FastCGI proxy points at PHP's chosen port. Apache stays on its current port.
        if svc == Service::Php && self.handles.contains_key(&Service::Apache) {
            let apache_port = self.effective_port(Service::Apache);
            if let Err(reason) = rewrite_httpd_conf_with_ports(&self.config, apache_port, chosen) {
                self.log.push(format!(
                    "warn: could not refresh httpd.conf for new PHP port: {reason}"
                ));
            }
        }

        // Record the resolution before spawn so later emits/queries see it.
        self.effective_ports.insert(svc, chosen);
        let _ = self.tx.send(Event::PortAssigned {
            service: svc,
            port: chosen,
        });

        // Kill any existing handles for this service
        self.do_kill(svc);

        let (kill_tx, kill_rx) = crossbeam_channel::bounded::<()>(1);

        match spawn_service(svc, &self.config, chosen, self.tx.clone()) {
            Ok(proc) => {
                let tx = self.tx.clone();
                let error_log = if svc == Service::Mysql {
                    Some(self.config.install_dir.join("logs").join("mysql_error.log"))
                } else {
                    None
                };
                let join = std::thread::spawn(move || watcher(proc, tx, kill_rx, error_log));
                self.handles.insert(
                    svc,
                    ServiceHandles {
                        kill_tx,
                        watcher_join: Some(join),
                        health_stop_tx: None,
                        health_join: None,
                    },
                );
            }
            Err(reason) => {
                let _ = self.tx.send(Event::ProcessSpawnFailed {
                    service: svc,
                    reason,
                });
            }
        }
    }

    fn do_kill(&mut self, svc: Service) {
        self.do_stop_health(svc);
        if let Some(h) = self.handles.remove(&svc) {
            // Signal watcher to kill its process tree.
            let _ = h.kill_tx.send(());
            // Join the watcher so we know the kill completed before we return.
            // This prevents a stale ProcessExit event arriving after a restart
            // has already moved the service back to Starting, which would cause
            // the reducer to incorrectly transition Starting → Crashed.
            if let Some(join) = h.watcher_join {
                let _ = join.join();
            }
        }
    }

    fn do_readiness_check(&self, svc: Service) {
        let port = self.effective_port(svc);
        let tx = self.tx.clone();
        std::thread::spawn(move || poll_until_ready(svc, port, tx));
    }

    fn do_stop_health(&mut self, svc: Service) {
        if let Some(h) = self.handles.get_mut(&svc) {
            if let Some(stop) = h.health_stop_tx.take() {
                let _ = stop.send(());
            }
            // Join the health checker thread to bound its lifetime.
            // The thread exits promptly after receiving the stop signal
            // (run_health_checker uses select! so it reacts immediately).
            if let Some(join) = h.health_join.take() {
                let _ = join.join();
            }
        }
    }

    fn do_schedule_retry(&self, svc: Service, delay: std::time::Duration) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            let _ = tx.send(Event::AutoRetry(svc));
        });
    }

    fn do_persist(&self, state: &AppState) {
        let state_path = self.config.install_dir.join("ramp.state");
        // Preserve blowfish_secret from the existing state file so it survives PersistDesiredState calls
        let existing_secret = std::fs::read(&state_path)
            .ok()
            .and_then(|data| serde_json::from_slice::<PersistedState>(&data).ok())
            .and_then(|p| p.phpmyadmin_blowfish_secret);

        let persisted = PersistedState {
            apache_desired: state.apache.desired,
            mysql_desired: state.mysql.desired,
            php_desired: state.php.desired,
            phpmyadmin_enabled: state.phpmyadmin_enabled,
            phpmyadmin_blowfish_secret: existing_secret,
        };
        let result = serde_json::to_vec_pretty(&persisted)
            .map_err(|e| format!("serialize state failed: {e}"))
            .and_then(|data| atomic_write(&state_path, &data));

        if let Err(e) = result {
            // State persistence failure means desired state will be lost on restart.
            // Log at error level and surface in the UI log buffer directly.
            log::error!("PERSIST FAILED — desired service state will not survive restart: {e}");
            let msg =
                format!("ERROR: state persist failed — restart may not restore services: {e}");
            self.log.push(msg);
        }
    }

    fn do_toggle_phpmyadmin(&mut self, enable: bool, state: &AppState) {
        let _ = state; // state not needed here but kept for signature symmetry
        let pma_dir = self.config.install_dir.join("phpmyadmin");

        if enable && !pma_dir.exists() {
            log::error!("phpMyAdmin: directory not found at {}", pma_dir.display());
            self.log.push(format!(
                "ERROR: phpMyAdmin directory not found at {}",
                pma_dir.display()
            ));
            let _ = self.tx.send(Event::PhpMyAdminToggled(false));
            return;
        }

        let php_port = self.effective_port(Service::Php);

        if enable {
            let config_path = pma_dir.join("config.inc.php");
            let should_write =
                !config_path.exists() || phpmyadmin_conf::is_ramp_owned_config(&config_path);

            if should_write {
                let blowfish_secret = self.load_or_generate_blowfish_secret();
                let mysql_port = self.effective_port(Service::Mysql);
                let content = phpmyadmin_conf::generate_config_inc_php(
                    &self.config.install_dir,
                    mysql_port,
                    &self.config.phpmyadmin.mysql_user,
                    &self.config.phpmyadmin.mysql_password,
                    &blowfish_secret,
                );
                if let Err(e) = atomic_write(&config_path, content.as_bytes()) {
                    log::error!("phpMyAdmin: cannot write config.inc.php: {e}");
                    self.log.push(format!(
                        "ERROR: phpMyAdmin config.inc.php write failed: {e}"
                    ));
                    let _ = self.tx.send(Event::PhpMyAdminToggled(false));
                    return;
                }
            }
        }

        let result = if enable {
            phpmyadmin_conf::write_phpmyadmin_apache_conf_enabled(&self.config, php_port)
        } else {
            phpmyadmin_conf::write_phpmyadmin_apache_conf_disabled(&self.config)
        };

        if let Err(e) = result {
            log::error!("phpMyAdmin: cannot write phpmyadmin.conf: {e}");
            self.log
                .push(format!("ERROR: phpMyAdmin conf write failed: {e}"));
            let _ = self.tx.send(Event::PhpMyAdminToggled(!enable));
            return;
        }

        let _ = self.tx.send(Event::PhpMyAdminToggled(enable));
        let _ = self.tx.send(Event::RestartService(Service::Apache));
        // Browser is opened by do_open_phpmyadmin_browser, called when Apache becomes Ready
        // after the restart, so we always use the correct (possibly port-rotated) port.
    }

    fn do_open_phpmyadmin_browser(&mut self) {
        let apache_port = self.effective_port(Service::Apache);
        let url = format!("http://127.0.0.1:{apache_port}/phpmyadmin/");
        log::info!("phpMyAdmin: opening {url}");
        self.log.push(format!("phpMyAdmin: opening {url}"));
        if let Err(e) = std::process::Command::new("cmd")
            .args(["/c", "start", "", &url])
            .spawn()
        {
            log::warn!("phpMyAdmin: could not open browser: {e}");
        }
    }

    fn load_or_generate_blowfish_secret(&self) -> String {
        let state_path = self.config.install_dir.join("ramp.state");
        if let Ok(data) = std::fs::read(&state_path) {
            if let Ok(persisted) = serde_json::from_slice::<PersistedState>(&data) {
                if let Some(secret) = persisted.phpmyadmin_blowfish_secret {
                    return secret;
                }
            }
        }
        // Generate a fresh secret and persist it immediately
        let secret = phpmyadmin_conf::generate_blowfish_secret(&self.config.install_dir);
        if let Ok(data) = std::fs::read(&state_path) {
            if let Ok(mut persisted) = serde_json::from_slice::<PersistedState>(&data) {
                persisted.phpmyadmin_blowfish_secret = Some(secret.clone());
                if let Ok(json) = serde_json::to_vec_pretty(&persisted) {
                    let _ = atomic_write(&state_path, &json);
                }
            }
        }
        secret
    }

    /// Graceful shutdown: signal all watcher threads to kill their processes, stop all
    /// health checkers, then join every watcher thread — blocking until each managed
    /// process is confirmed dead. Called by the event loop after processing ShutdownAll.
    ///
    /// This guarantees no orphaned processes remain when RAMP exits. The caller should
    /// enforce an external timeout (SHUTDOWN_GRACE_PERIOD) as a safety net.
    pub fn shutdown_and_join(&mut self) {
        // Signal every health checker to stop first so it doesn't send events
        // to a dying event loop.
        for h in self.handles.values_mut() {
            if let Some(stop) = h.health_stop_tx.take() {
                let _ = stop.send(());
            }
        }

        // Signal every watcher to kill its process, then collect all join handles.
        let handles: Vec<_> = self.handles.drain().collect();
        for (_svc, h) in handles {
            let _ = h.kill_tx.send(());
            if let Some(join) = h.watcher_join {
                // Blocks until proc.kill() + WaitForSingleObject complete.
                // In practice this is sub-millisecond — the Job Object close is instant.
                let _ = join.join();
            }
            // Health checker threads were already stopped above, but join any that
            // weren't stopped yet (e.g. if shutdown_and_join is called directly).
            if let Some(join) = h.health_join {
                let _ = join.join();
            }
        }
    }

    fn port(&self, svc: Service) -> u16 {
        match svc {
            Service::Apache => self.config.apache.port,
            Service::Mysql => self.config.mysql.port,
            Service::Php => self.config.php.port,
        }
    }
}

/// Watches a running process. Kills it if a kill signal arrives, or emits ProcessExit naturally.
///
/// Uses crossbeam select! so kill signals are acted on immediately rather than
/// waiting for the next 100ms poll interval.
///
/// `error_log`: if provided, the tail of this file is emitted as a LogEvent on non-zero exit
/// so crash reasons are visible without leaving the RAMP UI.
fn watcher(
    proc: ServiceProcess,
    tx: Sender<Event>,
    kill_rx: crossbeam_channel::Receiver<()>,
    error_log: Option<std::path::PathBuf>,
) {
    let svc = proc.service;
    let poll_interval = std::time::Duration::from_millis(100);

    loop {
        crossbeam_channel::select! {
            recv(kill_rx) -> _ => {
                // Kill requested: close Job Object → terminates entire process tree,
                // then WaitForSingleObject blocks until the main process is gone.
                proc.kill();
                let _ = tx.send(Event::ProcessExit { service: svc, exit_code: None });
                return;
            }
            default(poll_interval) => {
                // Non-blocking poll: has the process exited on its own?
                if let Some(code) = proc.try_wait() {
                    drop(proc);
                    if code != 0 {
                        if let Some(ref log_path) = error_log {
                            if let Some(tail) = read_log_tail(log_path, 20) {
                                let _ = tx.send(Event::DiagnosticLog(format!(
                                    "{svc}: error log tail:\n{tail}"
                                )));
                            }
                        }
                    }
                    let _ = tx.send(Event::ProcessExit {
                        service: svc,
                        exit_code: Some(code),
                    });
                    return;
                }
            }
        }
    }
}

/// Read the last `max_lines` lines of a text file. Returns None if the file cannot be read.
fn read_log_tail(path: &std::path::Path, max_lines: usize) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    if content.is_empty() {
        return None;
    }
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    Some(lines[start..].join("\n"))
}
