use crate::config::atomic_write;
use crate::events::{Event, SideEffect};
use crate::health::{poll_until_ready, run_health_checker};
use crate::logger::SharedLog;
use crate::phpmyadmin_conf;
use crate::process::{spawn_service, ServiceProcess};
use crate::state::{AppState, PersistedState, RampConfig, Service};
use crossbeam_channel::Sender;
use std::collections::HashMap;

/// Per-service runtime handles.
struct ServiceHandles {
    /// Channel to signal the watcher thread to force-kill the process.
    kill_tx: crossbeam_channel::Sender<()>,
    /// Join handle for the watcher thread — used during graceful shutdown to
    /// block until the process is confirmed dead before RAMPP exits.
    watcher_join: Option<std::thread::JoinHandle<()>>,
    /// Channel to stop the health checker.
    health_stop_tx: Option<crossbeam_channel::Sender<()>>,
    /// Join handle for the health checker thread — joined during shutdown to
    /// bound its lifetime and prevent sends to a dead event channel.
    health_join: Option<std::thread::JoinHandle<()>>,
    /// Set true by `start_health_check`, which the event loop calls exactly
    /// when this spawn's service first transitions to `Running`. A handle
    /// exists from spawn time — before anything has bound a port — so this
    /// is what actually distinguishes "was listening and serving" from "we
    /// merely tried to start it and it may have failed to bind."
    became_ready: bool,
}

/// Executor translates SideEffects into real I/O. Owns all live process/thread handles.
pub struct Executor {
    config: RampConfig,
    tx: Sender<Event>,
    log: SharedLog,
    handles: HashMap<Service, ServiceHandles>,
}

impl Executor {
    pub fn new(config: RampConfig, tx: Sender<Event>, log: SharedLog) -> Self {
        Self {
            config,
            tx,
            log,
            handles: HashMap::new(),
        }
    }

    pub fn execute(&mut self, effects: Vec<SideEffect>, state: &AppState) {
        for effect in effects {
            match effect {
                SideEffect::SpawnService { service, port } => self.do_spawn(service, port, state),
                SideEffect::KillService(svc) => {
                    if !self.do_kill(svc, state) {
                        // No live handle existed — the service never actually
                        // finished spawning (port conflict, user-owned-config
                        // block, reconcile failure, or spawn_service itself
                        // failed). Nothing else will ever tell the reducer
                        // this KillService is resolved, so synthesize the
                        // ProcessExit a real one would have sent. Safe in
                        // every state this can arrive in: it resolves
                        // Stopping → Stopped exactly like a genuine exit
                        // (the actual fix — see Finding B), and is a harmless
                        // no-op everywhere else (e.g. Crashed, from
                        // crash_and_retry's own KillService when that attempt
                        // never spawned), identical to how a genuine
                        // ProcessExit arriving in those states is already
                        // handled.
                        let _ = self.tx.send(Event::ProcessExit {
                            service: svc,
                            exit_code: None,
                        });
                    }
                }
                SideEffect::StartReadinessCheck {
                    service,
                    port,
                    attempt,
                } => self.do_readiness_check(service, port, attempt),
                SideEffect::StopHealthCheck(svc) => self.do_stop_health(svc),
                SideEffect::ScheduleRetry { service, delay } => {
                    self.do_schedule_retry(service, delay)
                }
                SideEffect::LogEvent(msg) => {
                    log::info!("{msg}");
                    self.log.push(msg);
                }
                SideEffect::PersistDesiredState => self.do_persist(state),
                SideEffect::PersistConfig => self.do_persist_config(state),
                SideEffect::TogglePhpMyAdmin(enable) => self.do_toggle_phpmyadmin(enable, state),
                SideEffect::OpenPhpMyAdminBrowser => self.do_open_phpmyadmin_browser(state),
            }
        }
    }

    /// Start health checks for a service that just became Running.
    pub fn start_health_check(&mut self, svc: Service, state: &AppState) {
        self.do_stop_health(svc);
        let port = state.ports.assigned(svc).unwrap_or_else(|| self.port(svc));
        let (stop_tx, stop_rx) = crossbeam_channel::bounded(1);
        let entry = self.handles.entry(svc).or_insert_with(|| {
            let (kill_tx, _) = crossbeam_channel::bounded(1);
            ServiceHandles {
                kill_tx,
                watcher_join: None,
                health_stop_tx: None,
                health_join: None,
                became_ready: false,
            }
        });
        // The event loop calls this exactly when `svc` first transitions to
        // Running — the one reliable signal that this spawn actually bound
        // its port and answered a real protocol handshake, not just that a
        // process object exists.
        entry.became_ready = true;
        entry.health_stop_tx = Some(stop_tx);
        let tx = self.tx.clone();
        let join = std::thread::spawn(move || run_health_checker(svc, port, tx, stop_rx));
        entry.health_join = Some(join);
    }

    /// Spawn a service on the port the reducer allocated. The executor no longer
    /// chooses ports — it verifies the reducer's choice and reports failure.
    fn do_spawn(&mut self, svc: Service, port: u16, state: &AppState) {
        if !crate::process::check_port_available(port) {
            let _ = self.tx.send(Event::PortUnavailable { service: svc, port });
            return;
        }

        // A user-owned file cannot be rewritten, so a port that must move is a hard
        // failure rather than a silent misconfiguration.
        let configured = self.port(svc);
        if port != configured {
            let blocking = self.user_owned_blocking(svc);
            if let Some(file) = blocking {
                let _ = self.tx.send(Event::ProcessSpawnFailed {
                    service: svc,
                    reason: format!(
                        "{file} is user-owned (RAMPP marker absent) and {svc} must move \
                         from port {configured} to {port} — edit or remove the file"
                    ),
                });
                return;
            }
        }
        if !self.do_reconcile(state, Some(svc), state.phpmyadmin_enabled) {
            let _ = self.tx.send(Event::ProcessSpawnFailed {
                service: svc,
                reason: "could not write service configuration".to_string(),
            });
            return;
        }

        // Kill any existing handles for this service. Return value intentionally
        // ignored here: `false` (no prior handle) is the normal case for a
        // service's very first spawn — unlike `execute`'s `KillService` arm,
        // this call site must never synthesize a ProcessExit for it, since that
        // would be caught by the Starting/Running crash arm and wrongly declare
        // a crash immediately after a perfectly normal fresh spawn.
        self.do_kill(svc, state);

        let (kill_tx, kill_rx) = crossbeam_channel::bounded::<()>(1);

        // Capture the log's length before spawning — only bytes past this offset
        // were written by this run, so a bind failure logged by a previous run
        // (these are long-lived error logs) can never be misattributed to it.
        let error_log = match svc {
            Service::Apache => self
                .config
                .install_dir
                .join("logs")
                .join("apache_error.log"),
            Service::Mysql => self.config.install_dir.join("logs").join("mysql_error.log"),
            Service::Php => self.config.install_dir.join("logs").join("php_errors.log"),
        };
        let log_offset = log_len(&error_log);

        match spawn_service(svc, &self.config, port, self.tx.clone()) {
            Ok(proc) => {
                let tx = self.tx.clone();
                let join = std::thread::spawn(move || {
                    watcher(proc, tx, kill_rx, error_log, port, log_offset)
                });
                self.handles.insert(
                    svc,
                    ServiceHandles {
                        kill_tx,
                        watcher_join: Some(join),
                        health_stop_tx: None,
                        health_join: None,
                        became_ready: false,
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

    /// Reconcile every managed config file and report what changed, so the reducer
    /// can restart exactly the services that need it.
    ///
    /// `pma_enabled` is taken as a parameter rather than read from `state` because
    /// a phpMyAdmin toggle's own reconcile pass (see `do_toggle_phpmyadmin`) runs
    /// before the reducer has applied the toggle to `state` — `phpmyadmin_enabled`
    /// only flips once `Event::PhpMyAdminToggled` round-trips through the event
    /// queue, and nothing reconciles again after that. Reading `state` here would
    /// silently reconcile against the pre-toggle value and drop the toggle.
    fn do_reconcile(
        &mut self,
        state: &AppState,
        spawning: Option<Service>,
        pma_enabled: bool,
    ) -> bool {
        if let Err(e) = crate::apache_conf::ensure_health_endpoint(&self.config) {
            self.log.push(format!("warn: health endpoint — {e}"));
        }

        let secret = self.load_or_generate_blowfish_secret();
        let pma_dir = self.config.install_dir.join("phpmyadmin");
        let desired = crate::provision::desired_configs(
            &self.config,
            &state.ports,
            pma_enabled,
            pma_dir.is_dir(),
            &secret,
        );
        let report = crate::provision::reconcile(&desired);

        for (file, err) in &report.errors {
            self.log
                .push(format!("ERROR: could not write {file} — {err}"));
        }
        for file in &report.user_owned {
            log::debug!("{file} is user-owned — leaving it alone");
        }

        let _ = self.tx.send(Event::ConfigsReconciled {
            changed: report.changed,
            spawning,
        });

        report.errors.is_empty()
    }

    /// The managed file whose port this service owns, if the user has taken it over.
    fn user_owned_blocking(&self, svc: Service) -> Option<crate::state::ManagedFile> {
        use crate::state::ManagedFile;
        let file = match svc {
            Service::Apache => ManagedFile::HttpdConf,
            Service::Mysql => ManagedFile::MyIni,
            Service::Php => return None, // PHP's port is a CLI flag, not a config file
        };
        let path = match svc {
            Service::Apache => self.config.apache.conf.clone(),
            Service::Mysql => self.config.mysql.ini.clone(),
            Service::Php => unreachable!(),
        };
        let content = std::fs::read_to_string(&path).ok()?;
        if crate::provision::is_rampp_owned(file, &content) {
            None
        } else {
            Some(file)
        }
    }

    /// Whether a best-effort MySQL graceful shutdown should be attempted before
    /// the Job Object close.
    ///
    /// Gated on the current handle having actually reached readiness
    /// (`became_ready`), not merely on a handle existing. A handle is present
    /// from the moment a process is spawned — before anything has bound a
    /// port — so gating on presence alone is not enough: a MySQL spawn that
    /// failed to bind (the reserved-port / bind-failure retry path this
    /// feature targets) has a handle right up until this same call removes it
    /// below, with nothing ever listening on the other end. `became_ready` is
    /// only set once `start_health_check` runs, which the event loop calls
    /// exactly when the service first reaches `Running` — i.e. only after a
    /// real MySQL protocol handshake succeeded. This is also why `Running`
    /// itself can't be used as the gate: on the normal stop/restart path the
    /// reducer sets the state to `Stopping` before `KillService` runs, so by
    /// the time `do_kill` executes the state has already moved past
    /// `Running`, while `became_ready` — set once and never cleared — still
    /// correctly remembers that this instance was really up.
    ///
    /// Cold first start / crash auto-retry / bind-failure retry: no handle,
    /// or a handle that never became ready → false. Normal stop, restart from
    /// Running, and ShutdownAll from Running: became ready → true.
    ///
    /// Pure and side-effect-free so it can be unit-tested directly.
    fn should_attempt_graceful_stop(&self, svc: Service) -> bool {
        svc == Service::Mysql && self.handles.get(&svc).is_some_and(|h| h.became_ready)
    }

    /// Kills the live handle for `svc`, if one exists. Returns `true` when a
    /// handle was found and processed, `false` when there was nothing to do —
    /// which happens whenever a service never successfully finished spawning
    /// (a port conflict, a user-owned-config-file block, a reconcile failure,
    /// or `spawn_service` itself returning `Err` all leave no handle behind).
    /// Callers that dispatch `KillService` expecting a real process teardown
    /// (see `execute`'s `SideEffect::KillService` arm) use this to detect that
    /// case and resolve it themselves — `do_kill` has no other way to tell
    /// anyone.
    fn do_kill(&mut self, svc: Service, state: &AppState) -> bool {
        self.do_stop_health(svc);

        // Ask MySQL to close InnoDB cleanly first. Best-effort only: the Job
        // Object close below is unconditional and remains the termination
        // guarantee — a failed or timed-out attempt here must never skip it.
        let mut graceful_shutdown_accepted = false;
        if self.should_attempt_graceful_stop(svc) {
            if let Some(port) = state.ports.assigned(Service::Mysql) {
                match crate::mysql_conf::graceful_shutdown(
                    &self.config,
                    port,
                    crate::state::MYSQL_SHUTDOWN_GRACE,
                ) {
                    Ok(()) => {
                        self.log.push("MySQL: clean shutdown complete".to_string());
                        graceful_shutdown_accepted = true;
                    }
                    Err(e) => log::debug!("MySQL graceful shutdown skipped: {e}"),
                }
            }
        }

        if let Some(h) = self.handles.remove(&svc) {
            // `graceful_shutdown`'s Ok(()) only means the `mysqladmin` CLIENT
            // exited after the server accepted the shutdown command — NOT that
            // mysqld itself has finished its own shutdown sequence (flushing
            // InnoDB, closing the buffer pool). Measured against a real server
            // this can lag by ~1-1.5s. Give the watcher a chance to observe
            // mysqld exiting on its own before forcing the Job Object closed:
            // the watcher's existing try_wait loop notices the natural exit,
            // emits ProcessExit, and returns, so polling its JoinHandle is
            // exactly the signal that mysqld is actually done. If it does not
            // finish within the grace period, fall through to the unconditional
            // force-kill below exactly as before — a graceful attempt that
            // times out must never skip the termination guarantee.
            if graceful_shutdown_accepted {
                if let Some(join) = &h.watcher_join {
                    let deadline = std::time::Instant::now() + crate::state::MYSQL_SHUTDOWN_GRACE;
                    while !join.is_finished() && std::time::Instant::now() < deadline {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            }

            // Signal watcher to kill its process tree. A no-op if the watcher
            // already returned above (natural exit): the channel's receiver is
            // gone, this send fails harmlessly, and the Job Object it would
            // have closed was already closed by the watcher's own natural-exit
            // path.
            let _ = h.kill_tx.send(());
            // Join the watcher so we know the kill completed before we return.
            // This prevents a stale ProcessExit event arriving after a restart
            // has already moved the service back to Starting, which would cause
            // the reducer to incorrectly transition Starting → Crashed. Instant
            // if the watcher already finished above.
            if let Some(join) = h.watcher_join {
                let _ = join.join();
            }
            true
        } else {
            false
        }
    }

    /// `port` and `attempt` are exactly what the reducer queued this check
    /// for — fixed at the moment the `StartReadinessCheck` side effect was
    /// created, not re-derived here from `state.ports`, which may have already
    /// moved on to a different attempt by the time this side effect actually
    /// executes. `attempt` is echoed back verbatim on the eventual
    /// `ProcessReady`/`ReadinessTimeout` so the reducer can tell this poller
    /// apart from one superseded by a later reallocation.
    fn do_readiness_check(&self, svc: Service, port: u16, attempt: u32) {
        let tx = self.tx.clone();
        std::thread::spawn(move || poll_until_ready(svc, port, attempt, tx));
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
        let state_path = self.config.install_dir.join("rampp.state");
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

    fn do_persist_config(&mut self, state: &AppState) {
        // Refresh the executor's config copy so a subsequent respawn regenerates
        // httpd.conf with the new document root. The executor otherwise keeps the
        // config it was constructed with.
        self.config = state.config.clone();

        if let Err(e) = crate::config::write_config(&self.config) {
            log::error!("config persist failed: {e}");
            self.log.push(format!("ERROR: config persist failed — {e}"));
            return;
        }
        // Ensure the new document root exists (seed index.php only if empty).
        if let Err(e) = crate::apache_conf::ensure_document_root(&self.config) {
            self.log
                .push(format!("warn: could not prepare document root — {e}"));
        }
        self.log.push(format!(
            "document root saved: {}",
            self.config.apache.document_root.display()
        ));
        self.do_reconcile(state, None, state.phpmyadmin_enabled);
    }

    fn do_toggle_phpmyadmin(&mut self, enable: bool, state: &AppState) {
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

        let _ = self.tx.send(Event::PhpMyAdminToggled(enable));
        // The Apache restart, if one is needed, comes from ConfigsReconciled — a side
        // effect must not emit a command event. Pass `enable` (not
        // state.phpmyadmin_enabled) — see do_reconcile's doc comment.
        self.do_reconcile(state, None, enable);
    }

    fn do_open_phpmyadmin_browser(&mut self, state: &AppState) {
        let apache_port = state
            .ports
            .assigned(Service::Apache)
            .unwrap_or_else(|| self.port(Service::Apache));
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
        let state_path = self.config.install_dir.join("rampp.state");
        let mut persisted = crate::config::read_persisted_state(&state_path);
        if let Some(secret) = persisted.phpmyadmin_blowfish_secret.clone() {
            return secret;
        }
        let secret = phpmyadmin_conf::generate_blowfish_secret();
        persisted.phpmyadmin_blowfish_secret = Some(secret.clone());
        if let Err(e) = crate::config::write_persisted_state(&state_path, &persisted) {
            log::error!("cannot persist phpMyAdmin blowfish secret: {e}");
        }
        secret
    }

    /// Graceful shutdown: for every service that still has a live handle, stop
    /// its health checker, then kill and join its watcher thread — blocking
    /// until each managed process is confirmed dead. Called by the event loop
    /// after processing ShutdownAll.
    ///
    /// In the normal case this drains an already-empty (or near-empty) map:
    /// `ShutdownAll`'s own reducer handling emits `KillService` for every
    /// Running/Starting service, and `execute()` processes those effects —
    /// removing their handles via `do_kill` — before this function is ever
    /// called. This exists as a defensive backstop for any handle that
    /// survives that pass (e.g. a service that was mid-transition and so
    /// wasn't Running/Starting when ShutdownAll ran), not as the primary path
    /// for MySQL's graceful stop — that happens earlier, inside the
    /// `KillService` handling above, if MySQL still has a handle at that point.
    ///
    /// This guarantees no orphaned processes remain when RAMPP exits. The caller should
    /// enforce an external timeout (SHUTDOWN_GRACE_PERIOD) as a safety net.
    pub fn shutdown_and_join(&mut self, state: &AppState) {
        let services: Vec<Service> = self.handles.keys().copied().collect();
        for svc in services {
            // Reuses do_kill rather than duplicating its stop-health/kill/join
            // sequence — harmless and keeps the two paths from drifting apart.
            self.do_kill(svc, state);
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
/// `error_log`: the service's error log path. Only bytes past `log_offset` (the
/// log's length captured immediately before spawn) are read, so a bind failure
/// logged by a previous run is never misattributed to this one.
///
/// `port`: the port this attempt was assigned — needed to report `PortUnavailable`,
/// which (unlike `ProcessExit`) carries no port of its own.
fn watcher(
    proc: ServiceProcess,
    tx: Sender<Event>,
    kill_rx: crossbeam_channel::Receiver<()>,
    error_log: std::path::PathBuf,
    port: u16,
    log_offset: u64,
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
                    // A zero exit is never a bind failure — e.g. MySQL logs this same
                    // "Can't start server: Bind on TCP/IP port" signature when its X
                    // Protocol listener (33060) collides while mysqld keeps running
                    // normally. Diagnosing a clean exit could misroute a user-initiated
                    // stop into PortUnavailable, which the reducer's stale-report guard
                    // then silently drops — stranding the service in Stopping forever,
                    // since nothing would convert it back into the ProcessExit the
                    // reducer needs. So the whole diagnosis block is gated on code != 0.
                    if code != 0 {
                        let tail = read_log_tail_from(&error_log, log_offset, 20);
                        if let Some(ref t) = tail {
                            let _ = tx.send(Event::DiagnosticLog(format!(
                                "{svc}: error log tail:\n{t}"
                            )));
                        }
                        // A failed bind is not a crash — report it so allocation advances
                        // to the next port rather than burning one of four retries.
                        let diagnosis = tail
                            .as_deref()
                            .map(|t| crate::process::diagnose_exit(svc, t))
                            .unwrap_or(crate::process::ExitDiagnosis::Unknown);
                        if let crate::process::ExitDiagnosis::PortBindFailure { reserved } =
                            diagnosis
                        {
                            if reserved {
                                let _ = tx.send(Event::DiagnosticLog(format!(
                                    "{svc}: Windows refused port {port} — it is probably \
                                     inside a reserved range. Check: netsh interface ipv4 \
                                     show excludedportrange protocol=tcp"
                                )));
                            }
                            let _ = tx.send(Event::PortUnavailable { service: svc, port });
                            return;
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

/// Read up to `max_lines` from a log file, starting at `offset` bytes.
///
/// The offset is recorded immediately before spawn, so a bind failure written by
/// a previous run can never be misattributed to the current one.
pub fn read_log_tail_from(path: &std::path::Path, offset: u64, max_lines: usize) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = String::new();
    f.read_to_string(&mut buf).ok()?;
    if buf.trim().is_empty() {
        return None;
    }
    let lines: Vec<&str> = buf.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    Some(lines[start..].join("\n"))
}

/// Size of a log file right now, or 0 if it does not exist yet.
fn log_len(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ApacheConfig, MysqlConfig, PhpConfig, PhpMyAdminConfig};
    use std::path::Path;
    use tempfile::TempDir;

    fn test_cfg(dir: &Path) -> RampConfig {
        RampConfig {
            install_dir: dir.to_path_buf(),
            apache: ApacheConfig {
                port: 8080,
                bin: dir.join("apache").join("bin").join("httpd.exe"),
                conf: dir.join("apache").join("conf").join("httpd.conf"),
                document_root: dir.join("apache").join("htdocs"),
            },
            mysql: MysqlConfig {
                port: 3306,
                bin: dir.join("mysql").join("bin").join("mysqld.exe"),
                data_dir: dir.join("mysql").join("data"),
                ini: dir.join("mysql").join("my.ini"),
            },
            php: PhpConfig {
                port: 9000,
                bin: dir.join("php").join("php-cgi.exe"),
                ini: dir.join("php").join("php.ini"),
            },
            phpmyadmin: PhpMyAdminConfig {
                mysql_user: "root".to_string(),
                mysql_password: String::new(),
            },
        }
    }

    /// Ruling F: `load_or_generate_blowfish_secret` must persist the secret it
    /// generates even when `rampp.state` does not exist yet. The old
    /// implementation nested the persist inside `if let Ok(data) =
    /// fs::read(&state_path)`, so a missing state file meant the freshly
    /// generated secret was silently dropped and a new one was generated on
    /// every call.
    #[test]
    fn blowfish_secret_persists_when_state_file_did_not_previously_exist() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let state_path = tmp.path().join("rampp.state");
        assert!(!state_path.exists(), "precondition: no state file yet");

        let (tx, _rx) = crossbeam_channel::unbounded();
        let log = SharedLog::new();
        let executor = Executor::new(cfg, tx, log);

        let first = executor.load_or_generate_blowfish_secret();
        assert!(
            state_path.exists(),
            "generating a secret must create rampp.state, not just return a value"
        );

        let second = executor.load_or_generate_blowfish_secret();
        assert_eq!(
            first, second,
            "secret must be reused on the next call, not regenerated"
        );

        // Confirm it actually landed on disk under the field the toggle-on path reads.
        let persisted = crate::config::read_persisted_state(&state_path);
        assert_eq!(persisted.phpmyadmin_blowfish_secret, Some(first));
    }

    /// A minimal fake handle for tests that only need `handles` to be
    /// non-empty for a service — no real process or thread is involved.
    /// `ready` sets `became_ready`, i.e. whether this fake spawn is meant to
    /// simulate one that reached `Running` (a real protocol handshake
    /// succeeded) versus one that is merely spawned (e.g. still starting, or
    /// failed to bind and never got there).
    fn fake_handle(ready: bool) -> ServiceHandles {
        let (kill_tx, _kill_rx) = crossbeam_channel::bounded::<()>(1);
        ServiceHandles {
            kill_tx,
            watcher_join: None,
            health_stop_tx: None,
            health_join: None,
            became_ready: ready,
        }
    }

    /// Review finding: `do_kill` used to gate the graceful-MySQL-shutdown
    /// attempt on `state.ports.assigned(Mysql).is_some()`, not on whether a
    /// MySQL process was actually running. Combined with the reducer
    /// assigning a port before emitting `SpawnService`, and `do_spawn`'s
    /// unconditional pre-spawn `do_kill`, that meant a cold first start, every
    /// crash auto-retry, and every bind-failure retry all paid for a doomed
    /// `mysqladmin shutdown` against a port nothing was listening on. The gate
    /// must key off a live handle instead.
    #[test]
    fn graceful_stop_is_skipped_without_a_live_handle() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let (tx, _rx) = crossbeam_channel::unbounded();
        let log = SharedLog::new();
        let executor = Executor::new(cfg, tx, log);

        assert!(
            !executor.should_attempt_graceful_stop(Service::Mysql),
            "a cold start / crash retry / bind-failure retry has no live MySQL \
             handle yet — attempting a graceful shutdown there would spawn a \
             doomed mysqladmin against nothing"
        );
    }

    /// Regression pin for the second review round: a handle existing is not
    /// enough. A MySQL spawn that failed to bind (the reserved-port /
    /// bind-failure retry this feature exists to fix) has a handle right up
    /// until `do_kill` removes it, but that handle never reached readiness —
    /// nothing was ever listening on the other end, so a graceful shutdown
    /// attempt there would be a doomed `mysqladmin` call every single retry.
    #[test]
    fn graceful_stop_is_skipped_when_the_handle_never_became_ready() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let (tx, _rx) = crossbeam_channel::unbounded();
        let log = SharedLog::new();
        let mut executor = Executor::new(cfg, tx, log);
        executor.handles.insert(Service::Mysql, fake_handle(false));

        assert!(
            !executor.should_attempt_graceful_stop(Service::Mysql),
            "a handle that never reached readiness (e.g. a bind-failure retry) \
             must not trigger a graceful shutdown attempt"
        );
    }

    #[test]
    fn graceful_stop_is_attempted_once_the_handle_became_ready() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let (tx, _rx) = crossbeam_channel::unbounded();
        let log = SharedLog::new();
        let mut executor = Executor::new(cfg, tx, log);
        executor.handles.insert(Service::Mysql, fake_handle(true));

        assert!(executor.should_attempt_graceful_stop(Service::Mysql));
    }

    #[test]
    fn graceful_stop_never_applies_to_non_mysql_services() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let (tx, _rx) = crossbeam_channel::unbounded();
        let log = SharedLog::new();
        let mut executor = Executor::new(cfg, tx, log);
        // Give both non-MySQL services a *ready* handle too — the predicate
        // must still say no, since only MySQL gets a graceful mysqladmin
        // shutdown regardless of readiness.
        executor.handles.insert(Service::Apache, fake_handle(true));
        executor.handles.insert(Service::Php, fake_handle(true));

        assert!(!executor.should_attempt_graceful_stop(Service::Apache));
        assert!(!executor.should_attempt_graceful_stop(Service::Php));
    }

    // ── Finding B: do_kill's return value and execute()'s handling of it ────
    //
    // A service that never actually spawned (a port conflict, a user-owned
    // config block, a reconcile failure, or spawn_service itself returning
    // Err all leave no handle behind) has nothing for do_kill to remove. If
    // KillService is then dispatched for it — e.g. StopService arriving while
    // Starting with no handle yet, or crash_and_retry's own KillService for an
    // attempt that never spawned — do_kill used to silently no-op, and nothing
    // ever told the reducer this was resolved: the service could get stuck in
    // Stopping forever. do_kill now reports whether it found anything, and
    // execute()'s KillService arm synthesizes the ProcessExit a real one would
    // have sent whenever it did not.

    #[test]
    fn do_kill_returns_false_when_no_handle_exists() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let (tx, _rx) = crossbeam_channel::unbounded();
        let log = SharedLog::new();
        let mut executor = Executor::new(cfg, tx, log);
        let state = AppState::new(test_cfg(tmp.path()));

        assert!(
            !executor.do_kill(Service::Apache, &state),
            "no handle was ever inserted for Apache — do_kill has nothing to do"
        );
    }

    #[test]
    fn do_kill_returns_true_when_a_handle_exists() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let (tx, _rx) = crossbeam_channel::unbounded();
        let log = SharedLog::new();
        let mut executor = Executor::new(cfg, tx, log);
        let state = AppState::new(test_cfg(tmp.path()));
        executor.handles.insert(Service::Apache, fake_handle(false));

        assert!(
            executor.do_kill(Service::Apache, &state),
            "a live (fake) handle existed and was processed"
        );
    }

    /// The actual Finding B fix: `execute()`'s `KillService` arm must
    /// synthesize `Event::ProcessExit{exit_code: None}` when `do_kill` reports
    /// nothing was there to kill. This is what lets a service stuck in
    /// Stopping with no handle (the reproduction above) actually resolve: the
    /// synthetic event round-trips through the reducer's ordinary
    /// `ProcessExit` handling exactly like a real exit would.
    #[test]
    fn execute_synthesizes_process_exit_when_kill_finds_no_handle() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let (tx, rx) = crossbeam_channel::unbounded();
        let log = SharedLog::new();
        let mut executor = Executor::new(cfg, tx, log);
        let state = AppState::new(test_cfg(tmp.path()));

        executor.execute(vec![SideEffect::KillService(Service::Apache)], &state);

        let event = rx
            .try_recv()
            .expect("execute must synthesize a ProcessExit when do_kill found nothing to kill");
        assert!(
            matches!(
                event,
                Event::ProcessExit {
                    service: Service::Apache,
                    exit_code: None
                }
            ),
            "expected a synthetic ProcessExit{{exit_code: None}}, got {event:?}"
        );
    }

    /// Complementary case: when a live handle DOES exist, `execute()` must not
    /// synthesize anything extra on top of the real teardown — only a genuine
    /// exit (from the watcher, or the natural-exit path inside do_kill itself)
    /// should ever produce the ProcessExit that resolves this KillService.
    #[test]
    fn execute_does_not_synthesize_process_exit_when_a_handle_existed() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let (tx, rx) = crossbeam_channel::unbounded();
        let log = SharedLog::new();
        let mut executor = Executor::new(cfg, tx, log);
        let state = AppState::new(test_cfg(tmp.path()));
        executor.handles.insert(Service::Apache, fake_handle(false));

        executor.execute(vec![SideEffect::KillService(Service::Apache)], &state);

        assert!(
            rx.try_recv().is_err(),
            "a live handle existed — execute must not synthesize a ProcessExit"
        );
    }

    /// End-to-end walk of the extra scenario the fix must also cover, beyond
    /// the generic "some service is stuck in Stopping" case above:
    /// `ShutdownAll` arriving while a service is `Starting` with no handle at
    /// all yet — e.g. `do_spawn` never got far enough to insert one (a port
    /// conflict, a user-owned-config-file block, or a reconcile failure all
    /// leave exactly this state). Before the fix this service would sit in
    /// `Stopping` forever: the reducer's `ShutdownAll` handling emits
    /// `KillService`, `do_kill` finds nothing and silently no-ops, and
    /// nothing else ever tells the reducer this is resolved. Drives the real
    /// reducer -> executor -> reducer round trip, exactly as the event loop
    /// in `main.rs` would.
    #[test]
    fn shutdown_all_on_a_starting_service_with_no_handle_still_resolves_to_stopped() {
        use crate::reducer::reducer;
        use crate::state::ServiceState;

        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let (tx, rx) = crossbeam_channel::unbounded();
        let log = SharedLog::new();
        let mut executor = Executor::new(cfg.clone(), tx, log);

        let mut state = AppState::new(cfg);
        state.set_starting(Service::Apache); // Starting, but do_spawn never got
                                             // far enough to insert a handle.

        let (state, effects) = reducer(state, Event::ShutdownAll);
        assert_eq!(state.apache.state, ServiceState::Stopping);
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::KillService(Service::Apache))));

        // No handle was ever inserted for Apache — execute() must detect that
        // via do_kill's false return and synthesize the ProcessExit that
        // resolves it.
        executor.execute(effects, &state);
        let event = rx
            .try_recv()
            .expect("execute must synthesize ProcessExit for the handle-less Apache");
        assert!(matches!(
            event,
            Event::ProcessExit {
                service: Service::Apache,
                exit_code: None
            }
        ));

        // Feed the synthetic event back through the reducer, exactly as the
        // real event loop would on its next cycle.
        let (state, _) = reducer(state, event);
        assert_eq!(
            state.apache.state,
            ServiceState::Stopped,
            "the synthetic ProcessExit must resolve Stopping -> Stopped, unsticking \
             a service that never actually spawned"
        );
    }
}
