use crate::events::{Event, SideEffect};
use crate::state::{
    retry_delay, AppState, DesiredServiceState, Service, ServiceState, MAX_RETRIES, PORT_SCAN_RANGE,
};

/// Pure reducer: STATE + EVENT → (NEW STATE, SIDE EFFECTS).
/// No I/O. No panics on invalid transitions — they are silently rejected with a log.
/// This function is the only place state may be mutated.
pub fn reducer(mut state: AppState, event: Event) -> (AppState, Vec<SideEffect>) {
    let mut effects = Vec::new();

    match event {
        // ── User commands ────────────────────────────────────────────────────
        Event::StartService(svc) => {
            let status = state.service(svc);
            match status.state {
                ServiceState::Stopped | ServiceState::Error => {
                    state.set_starting(svc);
                    state.service_mut(svc).desired = DesiredServiceState::Running;
                    state.service_mut(svc).retry_count = 0;
                    state.service_mut(svc).last_error = None;
                    begin_start(&mut state, svc, &mut effects);
                    effects.push(SideEffect::LogEvent(format!("{svc}: starting")));
                    effects.push(SideEffect::PersistDesiredState);
                }
                ServiceState::Crashed => {
                    // Treat same as Stopped — reset and try again
                    state.set_starting(svc);
                    state.service_mut(svc).desired = DesiredServiceState::Running;
                    state.service_mut(svc).retry_count = 0;
                    state.service_mut(svc).last_error = None;
                    begin_start(&mut state, svc, &mut effects);
                    effects.push(SideEffect::LogEvent(format!(
                        "{svc}: restarting after crash"
                    )));
                    effects.push(SideEffect::PersistDesiredState);
                }
                other => {
                    effects.push(SideEffect::LogEvent(format!(
                        "{svc}: StartService ignored in state {other}"
                    )));
                }
            }
        }

        Event::StopService(svc) => {
            let status = state.service(svc);
            match status.state {
                ServiceState::Running | ServiceState::Starting => {
                    state.service_mut(svc).state = ServiceState::Stopping;
                    state.clear_started_at(svc);
                    state.service_mut(svc).desired = DesiredServiceState::Stopped;
                    effects.push(SideEffect::StopHealthCheck(svc));
                    effects.push(SideEffect::KillService(svc));
                    effects.push(SideEffect::LogEvent(format!("{svc}: stopping")));
                    effects.push(SideEffect::PersistDesiredState);
                }
                ServiceState::Crashed | ServiceState::Error => {
                    // Already not running — just update desired
                    state.service_mut(svc).state = ServiceState::Stopped;
                    state.service_mut(svc).desired = DesiredServiceState::Stopped;
                    effects.push(SideEffect::PersistDesiredState);
                }
                other => {
                    effects.push(SideEffect::LogEvent(format!(
                        "{svc}: StopService ignored in state {other}"
                    )));
                }
            }
        }

        Event::RestartService(svc) => {
            // Decompose into stop then a queued start via AutoRetry with 0 delay logic.
            // Simpler: emit Stop + Start as two events via effects is not possible (effects
            // can't emit events directly). Instead transition through Stopping and rely on
            // ProcessExit to re-check desired_state.
            let status = state.service(svc);
            match status.state {
                ServiceState::Running | ServiceState::Starting => {
                    state.service_mut(svc).state = ServiceState::Stopping;
                    state.clear_started_at(svc);
                    state.service_mut(svc).desired = DesiredServiceState::Running; // keep desired Running
                    effects.push(SideEffect::StopHealthCheck(svc));
                    effects.push(SideEffect::KillService(svc));
                    effects.push(SideEffect::LogEvent(format!("{svc}: restarting")));
                }
                ServiceState::Stopped | ServiceState::Crashed | ServiceState::Error => {
                    state.set_starting(svc);
                    state.service_mut(svc).desired = DesiredServiceState::Running;
                    state.service_mut(svc).retry_count = 0;
                    state.service_mut(svc).last_error = None;
                    begin_start(&mut state, svc, &mut effects);
                    effects.push(SideEffect::LogEvent(format!("{svc}: starting (restart)")));
                }
                ServiceState::Stopping => {
                    // Already stopping; desired=Running means ProcessExit handler will restart.
                    state.service_mut(svc).desired = DesiredServiceState::Running;
                    effects.push(SideEffect::LogEvent(format!(
                        "{svc}: will restart once stopped"
                    )));
                }
            }
        }

        // ── Process lifecycle ────────────────────────────────────────────────
        Event::ProcessReady(svc) => {
            if state.service(svc).state == ServiceState::Starting {
                state.service_mut(svc).state = ServiceState::Running;
                state.service_mut(svc).retry_count = 0;
                state.service_mut(svc).health_fail_streak = 0;
                effects.push(SideEffect::LogEvent(format!("{svc}: ready")));
                // Open the browser once Apache is ready after a phpMyAdmin toggle-on.
                // Doing it here (not at toggle time) ensures we use the correct port
                // even if Apache restarted on a different port due to a port conflict.
                if svc == Service::Apache && state.open_phpmyadmin_on_apache_ready {
                    state.open_phpmyadmin_on_apache_ready = false;
                    effects.push(SideEffect::OpenPhpMyAdminBrowser);
                }
            } else {
                effects.push(SideEffect::LogEvent(format!(
                    "{svc}: ProcessReady ignored in state {}",
                    state.service(svc).state
                )));
            }
        }

        Event::ProcessExit {
            service: svc,
            exit_code,
        } => {
            match state.service(svc).state {
                ServiceState::Stopping => {
                    state.service_mut(svc).state = ServiceState::Stopped;
                    state.clear_started_at(svc);
                    effects.push(SideEffect::LogEvent(format!(
                        "{svc}: stopped (exit {exit_code:?})"
                    )));
                    // If desired is Running (e.g. after RestartService), auto-start
                    if state.service(svc).desired == DesiredServiceState::Running {
                        state.set_starting(svc);
                        state.service_mut(svc).retry_count = 0;
                        begin_start(&mut state, svc, &mut effects);
                        effects.push(SideEffect::LogEvent(format!(
                            "{svc}: restarting per desired state"
                        )));
                    } else {
                        state.ports.release(svc);
                    }
                }
                ServiceState::Starting | ServiceState::Running => {
                    // Unexpected exit → Crashed.
                    // Note: ProcessExit can also be synthesised by the readiness poller
                    // when readiness times out — in that case the OS process may still
                    // be alive. Emit KillService to guarantee cleanup before any retry.
                    state.service_mut(svc).state = ServiceState::Crashed;
                    state.clear_started_at(svc);
                    state.service_mut(svc).last_error =
                        Some(format!("exited unexpectedly (code {exit_code:?})"));
                    effects.push(SideEffect::StopHealthCheck(svc));
                    effects.push(SideEffect::KillService(svc));
                    effects.push(SideEffect::LogEvent(format!(
                        "{svc}: crashed (exit {exit_code:?})"
                    )));
                    // Auto-retry if desired is Running and retries remain
                    if state.service(svc).desired == DesiredServiceState::Running {
                        let retry = state.service(svc).retry_count;
                        if let Some(delay) = retry_delay(retry) {
                            state.service_mut(svc).retry_count += 1;
                            effects.push(SideEffect::ScheduleRetry {
                                service: svc,
                                delay,
                            });
                            effects.push(SideEffect::LogEvent(format!(
                                "{svc}: retry {} of {} in {:?}",
                                retry + 1,
                                MAX_RETRIES,
                                delay
                            )));
                        } else {
                            state.service_mut(svc).state = ServiceState::Error;
                            state.clear_started_at(svc);
                            state.service_mut(svc).last_error =
                                Some("max retries exceeded".to_string());
                            state.ports.release(svc);
                            effects.push(SideEffect::LogEvent(format!(
                                "{svc}: max retries exceeded → Error"
                            )));
                        }
                    }
                }
                other => {
                    effects.push(SideEffect::LogEvent(format!(
                        "{svc}: ProcessExit ignored in state {other}"
                    )));
                }
            }
            // Auto-disable phpMyAdmin only on unexpected exit (crash), not intentional stop.
            // desired == Stopped means the user stopped the service deliberately; phpMyAdmin
            // state is preserved so it resumes when the service comes back up.
            let is_crash = state.service(svc).desired == DesiredServiceState::Running;
            if state.phpmyadmin_enabled && is_crash && matches!(svc, Service::Mysql | Service::Php)
            {
                effects.push(SideEffect::TogglePhpMyAdmin(false));
            }
        }

        Event::ProcessSpawnFailed {
            service: svc,
            reason,
        } => {
            state.service_mut(svc).state = ServiceState::Error;
            state.clear_started_at(svc);
            state.service_mut(svc).last_error = Some(reason.clone());
            state.ports.release(svc);
            effects.push(SideEffect::LogEvent(format!(
                "{svc}: spawn failed — {reason}"
            )));
        }

        // ── Health checks ────────────────────────────────────────────────────
        Event::HealthCheckPass(svc) => {
            if state.service(svc).state == ServiceState::Running {
                state.service_mut(svc).health_fail_streak = 0;
            }
        }

        Event::HealthCheckFail(svc) => {
            if state.service(svc).state == ServiceState::Running {
                let streak = state.service(svc).health_fail_streak + 1;
                state.service_mut(svc).health_fail_streak = streak;
                effects.push(SideEffect::LogEvent(format!(
                    "{svc}: health check failed ({streak} consecutive)"
                )));
                if streak >= crate::state::HEALTH_FAIL_THRESHOLD {
                    // Treat as unexpected exit for retry logic
                    state.service_mut(svc).state = ServiceState::Crashed;
                    state.clear_started_at(svc);
                    state.service_mut(svc).last_error =
                        Some(format!("{streak} consecutive health check failures"));
                    effects.push(SideEffect::StopHealthCheck(svc));
                    effects.push(SideEffect::KillService(svc));
                    if state.service(svc).desired == DesiredServiceState::Running {
                        let retry = state.service(svc).retry_count;
                        if let Some(delay) = retry_delay(retry) {
                            state.service_mut(svc).retry_count += 1;
                            effects.push(SideEffect::ScheduleRetry {
                                service: svc,
                                delay,
                            });
                        } else {
                            state.service_mut(svc).state = ServiceState::Error;
                            state.clear_started_at(svc);
                            state.service_mut(svc).last_error =
                                Some("max retries exceeded after health failures".to_string());
                            state.ports.release(svc);
                            effects.push(SideEffect::LogEvent(format!(
                                "{svc}: max retries exceeded → Error"
                            )));
                        }
                    }
                }
            }
        }

        // ── Port conflict ────────────────────────────────────────────────────
        Event::PortConflictDetected(svc) => {
            state.service_mut(svc).state = ServiceState::Error;
            state.clear_started_at(svc);
            state.service_mut(svc).last_error = Some("no free port within scan range".to_string());
            effects.push(SideEffect::LogEvent(format!(
                "{svc}: no free port within scan range → Error"
            )));
        }

        Event::PortUnavailable { service: svc, port } => {
            // Only act on a report for the attempt currently in flight. A late report
            // from a previous attempt must not respawn or blacklist anything.
            let in_flight = state.service(svc).state == ServiceState::Starting
                && state.ports.assigned(svc) == Some(port);
            if !in_flight {
                effects.push(SideEffect::LogEvent(format!(
                    "{svc}: ignoring stale PortUnavailable for port {port}"
                )));
            } else {
                state.ports.mark_unavailable(svc, port);
                effects.push(SideEffect::LogEvent(format!(
                    "{svc}: port {port} unavailable — trying the next one"
                )));
                match allocate_port(&state, svc) {
                    Some(next) => {
                        state.ports.assign(svc, next);
                        effects.push(SideEffect::SpawnService {
                            service: svc,
                            port: next,
                        });
                        effects.push(SideEffect::StartReadinessCheck(svc));
                    }
                    None => {
                        state.service_mut(svc).state = ServiceState::Error;
                        state.clear_started_at(svc);
                        state.service_mut(svc).last_error =
                            Some("no free port within scan range".to_string());
                        // Unlike begin_start's None branch, begin_attempt was never called
                        // here (it would have erased the blacklist we just built), so
                        // `assigned` still holds the port we just proved unavailable.
                        // Release it explicitly or it keeps excluding that dead port from
                        // every other service's candidate range indefinitely.
                        state.ports.release(svc);
                        effects.push(SideEffect::LogEvent(format!(
                            "{svc}: no free port within scan range → Error"
                        )));
                    }
                }
            }
        }

        // ── Auto-retry (from executor timer) ────────────────────────────────
        Event::AutoRetry(svc) => {
            if state.service(svc).state == ServiceState::Crashed
                && state.service(svc).desired == DesiredServiceState::Running
            {
                state.set_starting(svc);
                begin_start(&mut state, svc, &mut effects);
                effects.push(SideEffect::LogEvent(format!("{svc}: auto-retry starting")));
            }
        }

        // ── Fatal error ──────────────────────────────────────────────────────
        Event::FatalError {
            service: svc,
            reason,
        } => {
            state.service_mut(svc).state = ServiceState::Error;
            state.clear_started_at(svc);
            state.service_mut(svc).last_error = Some(reason.clone());
            effects.push(SideEffect::StopHealthCheck(svc));
            effects.push(SideEffect::LogEvent(format!("{svc}: FATAL — {reason}")));
        }

        // ── Config reload ────────────────────────────────────────────────────
        Event::ConfigReloaded(new_config) => {
            state.config = *new_config;
            // Re-check whether phpmyadmin dir exists under the (possibly new) install_dir.
            // phpmyadmin_dir_exists is a cache — ConfigReloaded is the designated refresh point.
            let pma_dir = state.config.install_dir.join("phpmyadmin");
            state.phpmyadmin_dir_exists = pma_dir.exists() && pma_dir.is_dir();
            if state.phpmyadmin_enabled && !state.phpmyadmin_dir_exists {
                effects.push(SideEffect::TogglePhpMyAdmin(false));
            }
            effects.push(SideEffect::LogEvent(
                "config reloaded — restart services to apply changes".to_string(),
            ));
        }

        // ── Tick (drives health check cycle — executor owns the timer) ───────
        Event::Tick => {
            // Tick is consumed by the executor to fire health checks.
            // The reducer records nothing for Tick.
        }

        // ── phpMyAdmin toggle ─────────────────────────────────────────────────
        Event::TogglePhpMyAdmin => {
            let all_running = state.apache.state == ServiceState::Running
                && state.mysql.state == ServiceState::Running
                && state.php.state == ServiceState::Running;

            if !state.phpmyadmin_dir_exists {
                effects.push(SideEffect::LogEvent(
                    "phpMyAdmin: directory not found — cannot toggle".to_string(),
                ));
            } else if !all_running {
                effects.push(SideEffect::LogEvent(
                    "phpMyAdmin: MySQL, PHP, and Apache must all be running".to_string(),
                ));
            } else {
                let target = !state.phpmyadmin_enabled;
                if target {
                    // Browser opens after Apache restarts and is ready, so we get the correct port.
                    state.open_phpmyadmin_on_apache_ready = true;
                }
                effects.push(SideEffect::TogglePhpMyAdmin(target));
            }
        }

        Event::OpenPhpMyAdmin => {
            effects.push(SideEffect::OpenPhpMyAdminBrowser);
        }

        Event::ClearLog => {
            // handled in UI — reducer has no state to change
        }

        Event::DismissError(svc) => {
            state.service_mut(svc).last_error = None;
            effects.push(SideEffect::LogEvent(format!("{svc} error dismissed")));
        }

        Event::SetDocumentRoot(path) => {
            state.config.apache.document_root = path;
            effects.push(SideEffect::PersistConfig);
            effects.push(SideEffect::LogEvent(format!(
                "document root set to {}",
                state.config.apache.document_root.display()
            )));
            // Apache reads DocumentRoot only at startup. If it's running, restart it so
            // the change goes live; otherwise it applies on next start. Mirrors the
            // RestartService(Apache) Running/Starting branch.
            match state.apache.state {
                ServiceState::Running | ServiceState::Starting => {
                    state.apache.state = ServiceState::Stopping;
                    state.clear_started_at(Service::Apache);
                    state.apache.desired = DesiredServiceState::Running;
                    effects.push(SideEffect::StopHealthCheck(Service::Apache));
                    effects.push(SideEffect::KillService(Service::Apache));
                    effects.push(SideEffect::LogEvent(
                        "Apache: restarting to apply new document root".to_string(),
                    ));
                }
                _ => {}
            }
        }

        Event::PhpMyAdminToggled(enabled) => {
            state.phpmyadmin_enabled = enabled;
            effects.push(SideEffect::PersistDesiredState);
            effects.push(SideEffect::LogEvent(format!(
                "phpMyAdmin: {}",
                if enabled { "enabled" } else { "disabled" }
            )));
        }

        // ── Diagnostic log lines from background threads ──────────────────────
        Event::DiagnosticLog(msg) => {
            effects.push(SideEffect::LogEvent(msg));
        }

        // ── Shutdown all ─────────────────────────────────────────────────────
        Event::ShutdownAll => {
            for svc in [Service::Apache, Service::Mysql, Service::Php] {
                match state.service(svc).state {
                    ServiceState::Running | ServiceState::Starting => {
                        state.service_mut(svc).state = ServiceState::Stopping;
                        state.clear_started_at(svc);
                        state.service_mut(svc).desired = DesiredServiceState::Stopped;
                        effects.push(SideEffect::StopHealthCheck(svc));
                        effects.push(SideEffect::KillService(svc));
                    }
                    _ => {
                        state.service_mut(svc).desired = DesiredServiceState::Stopped;
                    }
                }
            }
            effects.push(SideEffect::LogEvent(
                "shutting down all services".to_string(),
            ));
            effects.push(SideEffect::PersistDesiredState);
        }
    }

    (state, effects)
}

/// Configured (home) port for a service, straight from rampp.toml.
fn configured_port(state: &AppState, svc: Service) -> u16 {
    match svc {
        Service::Apache => state.config.apache.port,
        Service::Mysql => state.config.mysql.port,
        Service::Php => state.config.php.port,
    }
}

/// Next candidate port for `svc`, or None when the scan range is exhausted.
///
/// Pure. Never returns a port assigned to, or configured for, another service —
/// which is what keeps overlapping scan ranges (e.g. apache 8080 and php 8085,
/// both scanning 20 upward) from colliding.
pub fn allocate_port(state: &AppState, svc: Service) -> Option<u16> {
    let start = configured_port(state, svc);
    let others: Vec<Service> = [Service::Apache, Service::Mysql, Service::Php]
        .into_iter()
        .filter(|&s| s != svc)
        .collect();

    for offset in 0..=PORT_SCAN_RANGE {
        let candidate = start.checked_add(offset)?;
        if state.ports.is_unavailable(svc, candidate) {
            continue;
        }
        let taken_by_other = others.iter().any(|&other| {
            state.ports.assigned(other) == Some(candidate)
                || configured_port(state, other) == candidate
        });
        if taken_by_other {
            continue;
        }
        return Some(candidate);
    }
    None
}

/// Begin a start attempt: clear the previous attempt's ledger entry, allocate a
/// port, and either queue the spawn or fail with a port conflict.
fn begin_start(state: &mut AppState, svc: Service, effects: &mut Vec<SideEffect>) {
    state.ports.begin_attempt(svc);
    match allocate_port(state, svc) {
        Some(port) => {
            state.ports.assign(svc, port);
            if port != configured_port(state, svc) {
                effects.push(SideEffect::LogEvent(format!(
                    "{svc}: configured port {} in use — using {port} instead",
                    configured_port(state, svc)
                )));
            }
            effects.push(SideEffect::SpawnService { service: svc, port });
            effects.push(SideEffect::StartReadinessCheck(svc));
        }
        None => {
            state.service_mut(svc).state = ServiceState::Error;
            state.clear_started_at(svc);
            state.service_mut(svc).last_error = Some("no free port within scan range".to_string());
            effects.push(SideEffect::LogEvent(format!(
                "{svc}: no free port within scan range → Error"
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Event;
    use crate::state::{
        ApacheConfig, AppState, DesiredServiceState, MysqlConfig, PhpConfig, RampConfig, Service,
        ServiceState,
    };

    fn make_state() -> AppState {
        let config = RampConfig {
            install_dir: std::path::PathBuf::from("C:\\rampp"),
            apache: ApacheConfig {
                port: 80,
                bin: std::path::PathBuf::from("C:\\rampp\\apache\\bin\\httpd.exe"),
                conf: std::path::PathBuf::from("C:\\rampp\\apache\\conf\\httpd.conf"),
                document_root: std::path::PathBuf::from("C:\\rampp\\apache\\htdocs"),
            },
            mysql: MysqlConfig {
                port: 3306,
                bin: std::path::PathBuf::from("C:\\rampp\\mysql\\bin\\mysqld.exe"),
                data_dir: std::path::PathBuf::from("C:\\rampp\\mysql\\data"),
                ini: std::path::PathBuf::from("C:\\rampp\\mysql\\my.ini"),
            },
            php: PhpConfig {
                port: 9000,
                bin: std::path::PathBuf::from("C:\\rampp\\php\\php-cgi.exe"),
                ini: std::path::PathBuf::from("C:\\rampp\\php\\php.ini"),
            },
            phpmyadmin: crate::state::PhpMyAdminConfig {
                mysql_user: "root".to_string(),
                mysql_password: String::new(),
            },
        };
        AppState::new(config)
    }

    fn set_state(state: &mut AppState, svc: Service, s: ServiceState) {
        state.service_mut(svc).state = s;
    }

    // ── Valid transitions ──────────────────────────────────────────────────

    #[test]
    fn stopped_start_transitions_to_starting() {
        let state = make_state();
        let (new_state, effects) = reducer(state, Event::StartService(Service::Apache));
        assert_eq!(new_state.apache.state, ServiceState::Starting);
        assert!(effects.iter().any(|e| matches!(
            e,
            SideEffect::SpawnService {
                service: Service::Apache,
                ..
            }
        )));
    }

    #[test]
    fn starting_process_ready_transitions_to_running() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Starting);
        let (new_state, _) = reducer(state, Event::ProcessReady(Service::Apache));
        assert_eq!(new_state.apache.state, ServiceState::Running);
    }

    #[test]
    fn running_stop_transitions_to_stopping() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Running);
        let (new_state, effects) = reducer(state, Event::StopService(Service::Apache));
        assert_eq!(new_state.apache.state, ServiceState::Stopping);
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::KillService(Service::Apache))));
    }

    #[test]
    fn stopping_process_exit_transitions_to_stopped() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Stopping);
        let (new_state, _) = reducer(
            state,
            Event::ProcessExit {
                service: Service::Apache,
                exit_code: Some(0),
            },
        );
        assert_eq!(new_state.apache.state, ServiceState::Stopped);
    }

    #[test]
    fn running_process_exit_transitions_to_crashed() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Running);
        let (new_state, _) = reducer(
            state,
            Event::ProcessExit {
                service: Service::Apache,
                exit_code: Some(1),
            },
        );
        assert_eq!(new_state.apache.state, ServiceState::Crashed);
    }

    #[test]
    fn crashed_auto_retry_transitions_to_starting_when_desired_running() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Crashed);
        state.apache.desired = DesiredServiceState::Running;
        let (new_state, effects) = reducer(state, Event::AutoRetry(Service::Apache));
        assert_eq!(new_state.apache.state, ServiceState::Starting);
        assert!(effects.iter().any(|e| matches!(
            e,
            SideEffect::SpawnService {
                service: Service::Apache,
                ..
            }
        )));
    }

    #[test]
    fn fatal_error_any_state_transitions_to_error() {
        for initial in [
            ServiceState::Stopped,
            ServiceState::Starting,
            ServiceState::Running,
            ServiceState::Stopping,
            ServiceState::Crashed,
        ] {
            let mut state = make_state();
            set_state(&mut state, Service::Apache, initial);
            let (new_state, _) = reducer(
                state,
                Event::FatalError {
                    service: Service::Apache,
                    reason: "test".into(),
                },
            );
            assert_eq!(
                new_state.apache.state,
                ServiceState::Error,
                "FatalError from {initial} should → Error"
            );
        }
    }

    // ── Invalid transitions (must not mutate state) ───────────────────────

    #[test]
    fn start_ignored_when_already_starting() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Starting);
        let (new_state, effects) = reducer(state, Event::StartService(Service::Apache));
        assert_eq!(new_state.apache.state, ServiceState::Starting);
        // No SpawnService should be emitted
        assert!(!effects.iter().any(|e| matches!(
            e,
            SideEffect::SpawnService {
                service: Service::Apache,
                ..
            }
        )));
    }

    #[test]
    fn start_ignored_when_running() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Running);
        let (new_state, _) = reducer(state, Event::StartService(Service::Apache));
        assert_eq!(new_state.apache.state, ServiceState::Running);
    }

    #[test]
    fn stop_ignored_when_already_stopped() {
        let state = make_state();
        let (new_state, _) = reducer(state, Event::StopService(Service::Apache));
        // Stopped stays Stopped (no KillService emitted)
        assert_eq!(new_state.apache.state, ServiceState::Stopped);
    }

    #[test]
    fn process_ready_ignored_when_not_starting() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Running);
        let (new_state, _) = reducer(state, Event::ProcessReady(Service::Apache));
        assert_eq!(new_state.apache.state, ServiceState::Running);
    }

    // ── Retry logic ───────────────────────────────────────────────────────

    #[test]
    fn crash_schedules_retry_when_desired_running() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Running);
        state.apache.desired = DesiredServiceState::Running;
        let (new_state, effects) = reducer(
            state,
            Event::ProcessExit {
                service: Service::Apache,
                exit_code: Some(1),
            },
        );
        assert_eq!(new_state.apache.state, ServiceState::Crashed);
        assert!(effects.iter().any(|e| matches!(
            e,
            SideEffect::ScheduleRetry {
                service: Service::Apache,
                ..
            }
        )));
    }

    #[test]
    fn crash_no_retry_when_desired_stopped() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Running);
        state.apache.desired = DesiredServiceState::Stopped;
        let (new_state, effects) = reducer(
            state,
            Event::ProcessExit {
                service: Service::Apache,
                exit_code: Some(1),
            },
        );
        assert_eq!(new_state.apache.state, ServiceState::Crashed);
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SideEffect::ScheduleRetry { .. })));
    }

    #[test]
    fn max_retries_exceeded_transitions_to_error() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Running);
        state.apache.desired = DesiredServiceState::Running;
        state.apache.retry_count = MAX_RETRIES; // all retries exhausted

        let (new_state, effects) = reducer(
            state,
            Event::ProcessExit {
                service: Service::Apache,
                exit_code: Some(1),
            },
        );
        assert_eq!(new_state.apache.state, ServiceState::Error);
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SideEffect::ScheduleRetry { .. })));
    }

    // ── Invariants ────────────────────────────────────────────────────────

    #[test]
    fn each_service_state_is_independent() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Running);
        let (new_state, _) = reducer(state, Event::StartService(Service::Mysql));
        assert_eq!(new_state.apache.state, ServiceState::Running);
        assert_eq!(new_state.mysql.state, ServiceState::Starting);
    }

    #[test]
    fn restart_from_running_sets_desired_running_and_kills() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Running);
        let (new_state, effects) = reducer(state, Event::RestartService(Service::Apache));
        assert_eq!(new_state.apache.state, ServiceState::Stopping);
        assert_eq!(new_state.apache.desired, DesiredServiceState::Running);
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::KillService(Service::Apache))));
    }

    #[test]
    fn shutdown_all_stops_running_services() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Running);
        set_state(&mut state, Service::Mysql, ServiceState::Running);
        set_state(&mut state, Service::Php, ServiceState::Running);
        let (new_state, effects) = reducer(state, Event::ShutdownAll);
        assert_eq!(new_state.apache.state, ServiceState::Stopping);
        assert_eq!(new_state.mysql.state, ServiceState::Stopping);
        assert_eq!(new_state.php.state, ServiceState::Stopping);
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::KillService(Service::Apache))));
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::KillService(Service::Mysql))));
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::KillService(Service::Php))));
    }

    #[test]
    fn php_service_state_machine_works() {
        // PHP follows the same state machine as Apache/MySQL
        let state = make_state();
        let (new_state, effects) = reducer(state, Event::StartService(Service::Php));
        assert_eq!(new_state.php.state, ServiceState::Starting);
        assert!(effects.iter().any(|e| matches!(
            e,
            SideEffect::SpawnService {
                service: Service::Php,
                ..
            }
        )));
    }

    #[test]
    fn health_fail_streak_accumulates_and_triggers_crash_at_threshold() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Running);
        state.apache.desired = DesiredServiceState::Running;

        // Two failures — still Running
        for _ in 0..2 {
            let (s, _) = reducer(state.clone(), Event::HealthCheckFail(Service::Apache));
            state = s;
        }
        assert_eq!(state.apache.state, ServiceState::Running);

        // Third failure — crosses threshold
        let (new_state, effects) = reducer(state, Event::HealthCheckFail(Service::Apache));
        assert_eq!(new_state.apache.state, ServiceState::Crashed);
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::KillService(Service::Apache))));
    }

    #[test]
    fn spawn_failed_transitions_to_error() {
        let state = make_state();
        let (new_state, _) = reducer(
            state,
            Event::ProcessSpawnFailed {
                service: Service::Apache,
                reason: "binary not found".into(),
            },
        );
        assert_eq!(new_state.apache.state, ServiceState::Error);
    }

    // ── ConfigReloaded ────────────────────────────────────────────────────

    #[test]
    fn config_reloaded_updates_state_config() {
        let state = make_state();
        let mut new_config = state.config.clone();
        new_config.apache.port = 9090;
        let (new_state, effects) = reducer(state, Event::ConfigReloaded(Box::new(new_config)));
        assert_eq!(new_state.config.apache.port, 9090);
        // Must emit a log event confirming reload
        assert!(effects.iter().any(|e| matches!(e, SideEffect::LogEvent(_))));
        // Must NOT emit any spawn/kill — config reload is passive
        assert!(!effects.iter().any(|e| matches!(
            e,
            SideEffect::SpawnService { .. } | SideEffect::KillService(_)
        )));
    }

    // ── RestartService edge cases ─────────────────────────────────────────

    #[test]
    fn restart_from_stopped_spawns_directly() {
        // RestartService on a Stopped service should start it directly, not go through Stopping
        let state = make_state(); // apache starts Stopped
        let (new_state, effects) = reducer(state, Event::RestartService(Service::Apache));
        assert_eq!(new_state.apache.state, ServiceState::Starting);
        assert_eq!(new_state.apache.desired, DesiredServiceState::Running);
        assert!(effects.iter().any(|e| matches!(
            e,
            SideEffect::SpawnService {
                service: Service::Apache,
                ..
            }
        )));
    }

    #[test]
    fn restart_from_stopping_sets_desired_running_for_later_start() {
        // RestartService on a service already Stopping: just sets desired=Running.
        // When ProcessExit fires, the reducer will auto-start because desired=Running.
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Stopping);
        state.apache.desired = DesiredServiceState::Stopped;
        let (new_state, effects) = reducer(state, Event::RestartService(Service::Apache));
        assert_eq!(new_state.apache.state, ServiceState::Stopping); // unchanged
        assert_eq!(new_state.apache.desired, DesiredServiceState::Running); // updated
                                                                            // No kill emitted — already stopping
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SideEffect::KillService(Service::Apache))));
    }

    #[test]
    fn stopping_process_exit_with_desired_running_respawns() {
        // After RestartService: Stopping → ProcessExit → should respawn because desired=Running
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Stopping);
        state.apache.desired = DesiredServiceState::Running;
        let (new_state, effects) = reducer(
            state,
            Event::ProcessExit {
                service: Service::Apache,
                exit_code: None,
            },
        );
        // Should restart directly into Starting
        assert_eq!(new_state.apache.state, ServiceState::Starting);
        assert!(effects.iter().any(|e| matches!(
            e,
            SideEffect::SpawnService {
                service: Service::Apache,
                ..
            }
        )));
    }

    // ── StopService edge cases ────────────────────────────────────────────

    #[test]
    fn stop_from_crashed_transitions_to_stopped() {
        // StopService on Crashed: no kill needed, just set state to Stopped
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Crashed);
        state.apache.desired = DesiredServiceState::Running;
        let (new_state, effects) = reducer(state, Event::StopService(Service::Apache));
        assert_eq!(new_state.apache.state, ServiceState::Stopped);
        assert_eq!(new_state.apache.desired, DesiredServiceState::Stopped);
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SideEffect::KillService(Service::Apache))));
    }

    #[test]
    fn stop_from_error_transitions_to_stopped() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Error);
        let (new_state, _) = reducer(state, Event::StopService(Service::Apache));
        assert_eq!(new_state.apache.state, ServiceState::Stopped);
        assert_eq!(new_state.apache.desired, DesiredServiceState::Stopped);
    }

    // ── PortConflictDetected ──────────────────────────────────────────────

    #[test]
    fn port_conflict_transitions_to_error() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Starting);
        let (new_state, effects) = reducer(state, Event::PortConflictDetected(Service::Apache));
        assert_eq!(new_state.apache.state, ServiceState::Error);
        assert!(new_state
            .apache
            .last_error
            .as_deref()
            .unwrap_or("")
            .contains("port"));
        assert!(effects.iter().any(|e| matches!(e, SideEffect::LogEvent(_))));
    }

    // ── Health check exhausted retries ────────────────────────────────────

    #[test]
    fn health_fail_threshold_with_exhausted_retries_transitions_to_error() {
        // If health check threshold is breached AND retry_count == MAX_RETRIES,
        // must transition to Error not Crashed.
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Running);
        state.apache.desired = DesiredServiceState::Running;
        state.apache.retry_count = MAX_RETRIES; // all retries exhausted

        // Apply HEALTH_FAIL_THRESHOLD failures
        for _ in 0..crate::state::HEALTH_FAIL_THRESHOLD {
            let (s, _) = reducer(state.clone(), Event::HealthCheckFail(Service::Apache));
            state = s;
        }
        assert_eq!(
            state.apache.state,
            ServiceState::Error,
            "should reach Error when retries exhausted at health threshold"
        );
        assert!(!state.apache.last_error.as_deref().unwrap_or("").is_empty());
    }

    #[test]
    fn health_check_pass_resets_streak() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Running);
        // Build up 2 failures (below threshold)
        let (s, _) = reducer(state, Event::HealthCheckFail(Service::Apache));
        let (s, _) = reducer(s, Event::HealthCheckFail(Service::Apache));
        assert_eq!(s.apache.health_fail_streak, 2);
        // Pass resets to 0
        let (s, _) = reducer(s, Event::HealthCheckPass(Service::Apache));
        assert_eq!(s.apache.health_fail_streak, 0);
        assert_eq!(s.apache.state, ServiceState::Running);
    }

    #[test]
    fn health_check_ignored_when_not_running() {
        // HealthCheckFail outside Running state must not change state
        for initial in [
            ServiceState::Starting,
            ServiceState::Stopped,
            ServiceState::Stopping,
            ServiceState::Crashed,
            ServiceState::Error,
        ] {
            let mut state = make_state();
            set_state(&mut state, Service::Apache, initial);
            let (new_state, effects) = reducer(state, Event::HealthCheckFail(Service::Apache));
            assert_eq!(
                new_state.apache.state, initial,
                "HealthCheckFail in {initial} should not change state"
            );
            // No kill or retry emitted
            assert!(!effects.iter().any(|e| matches!(
                e,
                SideEffect::KillService(_) | SideEffect::ScheduleRetry { .. }
            )));
        }
    }

    // ── ProcessExit edge cases ────────────────────────────────────────────

    #[test]
    fn process_exit_in_stopped_state_is_ignored() {
        // ProcessExit arriving when service is already Stopped must be a no-op
        let state = make_state(); // starts Stopped
        let (new_state, effects) = reducer(
            state,
            Event::ProcessExit {
                service: Service::Apache,
                exit_code: Some(0),
            },
        );
        assert_eq!(new_state.apache.state, ServiceState::Stopped);
        assert!(!effects.iter().any(|e| matches!(
            e,
            SideEffect::SpawnService { .. } | SideEffect::ScheduleRetry { .. }
        )));
    }

    #[test]
    fn process_exit_kill_code_none_from_running_still_crashes() {
        // exit_code=None means killed (by us), but if state=Running it means
        // something killed it unexpectedly — should still crash + retry.
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Running);
        state.apache.desired = DesiredServiceState::Running;
        let (new_state, effects) = reducer(
            state,
            Event::ProcessExit {
                service: Service::Apache,
                exit_code: None,
            },
        );
        assert_eq!(new_state.apache.state, ServiceState::Crashed);
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::ScheduleRetry { .. })));
    }

    // ── ShutdownAll edge cases ────────────────────────────────────────────

    #[test]
    fn shutdown_all_with_mixed_states() {
        // Running services get Stopping+Kill; others get desired=Stopped only
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Running);
        set_state(&mut state, Service::Mysql, ServiceState::Stopped);
        set_state(&mut state, Service::Php, ServiceState::Crashed);
        let (new_state, effects) = reducer(state, Event::ShutdownAll);

        assert_eq!(new_state.apache.state, ServiceState::Stopping);
        assert_eq!(new_state.mysql.state, ServiceState::Stopped);
        assert_eq!(new_state.php.state, ServiceState::Crashed);

        // All desired set to Stopped
        assert_eq!(new_state.apache.desired, DesiredServiceState::Stopped);
        assert_eq!(new_state.mysql.desired, DesiredServiceState::Stopped);
        assert_eq!(new_state.php.desired, DesiredServiceState::Stopped);

        // Only Running service gets KillService
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::KillService(Service::Apache))));
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SideEffect::KillService(Service::Mysql))));
    }

    // ── started_at invariant ──────────────────────────────────────────────

    #[test]
    fn started_at_set_on_starting_survives_through_running() {
        let state = make_state();
        let (state, _) = reducer(state, Event::StartService(Service::Apache));
        assert!(
            state.apache.started_at.is_some(),
            "started_at must be set when Starting"
        );
        let (state, _) = reducer(state, Event::ProcessReady(Service::Apache));
        assert!(
            state.apache.started_at.is_some(),
            "started_at must survive transition to Running"
        );
    }

    #[test]
    fn started_at_survives_running_transition() {
        let state = make_state();
        // Transition to Starting
        let (state, _) = reducer(state, Event::StartService(Service::Apache));
        assert!(
            state.apache.started_at.is_some(),
            "started_at must be set on Starting"
        );
        // Simulate ProcessReady — should move to Running without clearing started_at
        let (state, _) = reducer(state, Event::ProcessReady(Service::Apache));
        assert!(
            state.apache.started_at.is_some(),
            "started_at must survive Running transition"
        );
        // Simulate stop — now it should clear
        let (state, _) = reducer(state, Event::StopService(Service::Apache));
        // Transition through Stopping → Stopped via ProcessExit
        let (state, _) = reducer(
            state,
            Event::ProcessExit {
                service: Service::Apache,
                exit_code: Some(0),
            },
        );
        assert!(
            state.apache.started_at.is_none(),
            "started_at must clear on Stopped"
        );
    }

    #[test]
    fn started_at_cleared_on_stopping() {
        let mut state = make_state();
        state.set_starting(Service::Apache);
        assert!(state.apache.started_at.is_some());
        let (new_state, _) = reducer(state, Event::StopService(Service::Apache));
        assert!(
            new_state.apache.started_at.is_none(),
            "started_at must be cleared when Stopping"
        );
    }

    // ── retry_delay boundary ──────────────────────────────────────────────

    #[test]
    fn retry_delay_returns_correct_values() {
        use crate::state::retry_delay;
        use std::time::Duration;
        assert_eq!(retry_delay(0), Some(Duration::from_secs(1)));
        assert_eq!(retry_delay(1), Some(Duration::from_secs(2)));
        assert_eq!(retry_delay(2), Some(Duration::from_secs(4)));
        assert_eq!(retry_delay(3), Some(Duration::from_secs(8)));
        // At MAX_RETRIES (4) and beyond — no more retries
        assert_eq!(retry_delay(4), None);
        assert_eq!(retry_delay(100), None);
    }

    // ── phpMyAdmin toggle ─────────────────────────────────────────────────

    fn make_state_all_running() -> AppState {
        let mut state = make_state();
        for svc in [Service::Apache, Service::Mysql, Service::Php] {
            state.service_mut(svc).state = ServiceState::Running;
            state.service_mut(svc).desired = DesiredServiceState::Running;
        }
        state.phpmyadmin_dir_exists = true;
        state
    }

    #[test]
    fn toggle_phpmyadmin_on_when_services_running_emits_side_effect() {
        let state = make_state_all_running();
        let (new_state, effects) = reducer(state, Event::TogglePhpMyAdmin);
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(true))));
        // State not yet updated — waits for PhpMyAdminToggled
        assert!(!new_state.phpmyadmin_enabled);
        // Flag set so browser opens after Apache restart completes
        assert!(new_state.open_phpmyadmin_on_apache_ready);
    }

    #[test]
    fn apache_ready_after_phpmyadmin_toggle_opens_browser_and_clears_flag() {
        let mut state = make_state_all_running();
        state.open_phpmyadmin_on_apache_ready = true;
        state.apache.state = ServiceState::Starting;
        let (new_state, effects) = reducer(state, Event::ProcessReady(Service::Apache));
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::OpenPhpMyAdminBrowser)));
        assert!(!new_state.open_phpmyadmin_on_apache_ready);
    }

    #[test]
    fn apache_ready_without_flag_does_not_open_browser() {
        let mut state = make_state_all_running();
        state.open_phpmyadmin_on_apache_ready = false;
        state.apache.state = ServiceState::Starting;
        let (_, effects) = reducer(state, Event::ProcessReady(Service::Apache));
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SideEffect::OpenPhpMyAdminBrowser)));
    }

    #[test]
    fn toggle_phpmyadmin_off_when_enabled_emits_side_effect() {
        let mut state = make_state_all_running();
        state.phpmyadmin_enabled = true;
        let (_, effects) = reducer(state, Event::TogglePhpMyAdmin);
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(false))));
    }

    #[test]
    fn toggle_phpmyadmin_ignored_when_mysql_not_running() {
        let mut state = make_state_all_running();
        state.mysql.state = ServiceState::Stopped;
        let (new_state, effects) = reducer(state, Event::TogglePhpMyAdmin);
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(_))));
        assert!(effects.iter().any(|e| matches!(e, SideEffect::LogEvent(_))));
        assert!(!new_state.phpmyadmin_enabled);
    }

    #[test]
    fn toggle_phpmyadmin_ignored_when_php_not_running() {
        let mut state = make_state_all_running();
        state.php.state = ServiceState::Stopped;
        let (_, effects) = reducer(state, Event::TogglePhpMyAdmin);
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(_))));
    }

    #[test]
    fn toggle_phpmyadmin_ignored_when_apache_not_running() {
        let mut state = make_state_all_running();
        state.apache.state = ServiceState::Stopped;
        let (_, effects) = reducer(state, Event::TogglePhpMyAdmin);
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(_))));
    }

    #[test]
    fn toggle_phpmyadmin_ignored_when_dir_missing() {
        let mut state = make_state_all_running();
        state.phpmyadmin_dir_exists = false;
        let (_, effects) = reducer(state, Event::TogglePhpMyAdmin);
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(_))));
    }

    #[test]
    fn phpmyadmin_toggled_true_sets_enabled_and_persists() {
        let state = make_state();
        let (new_state, effects) = reducer(state, Event::PhpMyAdminToggled(true));
        assert!(new_state.phpmyadmin_enabled);
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::PersistDesiredState)));
    }

    #[test]
    fn phpmyadmin_toggled_false_clears_enabled_and_persists() {
        let mut state = make_state();
        state.phpmyadmin_enabled = true;
        let (new_state, effects) = reducer(state, Event::PhpMyAdminToggled(false));
        assert!(!new_state.phpmyadmin_enabled);
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::PersistDesiredState)));
    }

    #[test]
    fn mysql_crash_while_phpmyadmin_enabled_emits_toggle_off() {
        // desired=Running → unexpected exit → auto-disable
        let mut state = make_state_all_running();
        state.phpmyadmin_enabled = true;
        let (_, effects) = reducer(
            state,
            Event::ProcessExit {
                service: Service::Mysql,
                exit_code: Some(1),
            },
        );
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(false))));
    }

    #[test]
    fn php_crash_while_phpmyadmin_enabled_emits_toggle_off() {
        // desired=Running → unexpected exit → auto-disable
        let mut state = make_state_all_running();
        state.phpmyadmin_enabled = true;
        let (_, effects) = reducer(
            state,
            Event::ProcessExit {
                service: Service::Php,
                exit_code: Some(1),
            },
        );
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(false))));
    }

    #[test]
    fn mysql_intentional_stop_while_phpmyadmin_enabled_does_not_toggle_off() {
        // desired=Stopped → user stopped the service → preserve phpmyadmin state
        let mut state = make_state_all_running();
        state.phpmyadmin_enabled = true;
        state.mysql.desired = DesiredServiceState::Stopped;
        state.mysql.state = ServiceState::Stopping;
        let (_, effects) = reducer(
            state,
            Event::ProcessExit {
                service: Service::Mysql,
                exit_code: Some(0),
            },
        );
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(_))));
    }

    #[test]
    fn php_intentional_stop_while_phpmyadmin_enabled_does_not_toggle_off() {
        // desired=Stopped → user stopped the service → preserve phpmyadmin state
        let mut state = make_state_all_running();
        state.phpmyadmin_enabled = true;
        state.php.desired = DesiredServiceState::Stopped;
        state.php.state = ServiceState::Stopping;
        let (_, effects) = reducer(
            state,
            Event::ProcessExit {
                service: Service::Php,
                exit_code: Some(0),
            },
        );
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(_))));
    }

    #[test]
    fn mysql_process_exit_while_phpmyadmin_disabled_does_not_emit_toggle() {
        let mut state = make_state_all_running();
        state.phpmyadmin_enabled = false;
        let (_, effects) = reducer(
            state,
            Event::ProcessExit {
                service: Service::Mysql,
                exit_code: Some(1),
            },
        );
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(_))));
    }

    #[test]
    fn config_reloaded_rechecks_phpmyadmin_dir_exists() {
        let mut state = make_state();
        state.phpmyadmin_enabled = true;
        state.phpmyadmin_dir_exists = true;
        let mut new_config = state.config.clone();
        // Use a path that definitely does not have a phpmyadmin subdir
        new_config.install_dir = std::path::PathBuf::from("C:\\nonexistent_ramp_test_dir_12345");
        let (new_state, effects) = reducer(state, Event::ConfigReloaded(Box::new(new_config)));
        assert!(!new_state.phpmyadmin_dir_exists);
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(false))));
    }

    // ── AutoRetry ignored when not Crashed ───────────────────────────────

    #[test]
    fn auto_retry_ignored_when_not_crashed() {
        // AutoRetry must be a no-op if state is not Crashed (stale timer event)
        for initial in [
            ServiceState::Stopped,
            ServiceState::Starting,
            ServiceState::Running,
            ServiceState::Stopping,
            ServiceState::Error,
        ] {
            let mut state = make_state();
            set_state(&mut state, Service::Apache, initial);
            state.apache.desired = DesiredServiceState::Running;
            let (new_state, effects) = reducer(state, Event::AutoRetry(Service::Apache));
            assert_eq!(
                new_state.apache.state, initial,
                "AutoRetry in {initial} should be ignored"
            );
            assert!(!effects
                .iter()
                .any(|e| matches!(e, SideEffect::SpawnService { .. })));
        }
    }

    // ── DismissError ──────────────────────────────────────────────────────

    #[test]
    fn dismiss_error_clears_last_error() {
        let mut state = make_state();
        state.apache.last_error = Some("something broke".into());
        let (state, effects) = reducer(state, Event::DismissError(Service::Apache));
        assert!(
            state.apache.last_error.is_none(),
            "last_error must be cleared after DismissError"
        );
        assert!(
            effects.iter().any(|e| matches!(e, SideEffect::LogEvent(_))),
            "should emit a log event"
        );
    }

    // ── SetDocumentRoot ───────────────────────────────────────────────────

    #[test]
    fn set_document_root_persists_and_restarts_when_apache_running() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Running);
        let new_root = std::path::PathBuf::from("C:\\sites\\myapp");
        let (new_state, effects) = reducer(state, Event::SetDocumentRoot(new_root.clone()));
        assert_eq!(new_state.config.apache.document_root, new_root);
        assert_eq!(new_state.apache.state, ServiceState::Stopping);
        assert_eq!(new_state.apache.desired, DesiredServiceState::Running);
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::PersistConfig)));
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::KillService(Service::Apache))));
    }

    #[test]
    fn set_document_root_persist_only_when_apache_stopped() {
        let state = make_state(); // apache Stopped
        let new_root = std::path::PathBuf::from("C:\\sites\\other");
        let (new_state, effects) = reducer(state, Event::SetDocumentRoot(new_root.clone()));
        assert_eq!(new_state.config.apache.document_root, new_root);
        assert_eq!(new_state.apache.state, ServiceState::Stopped);
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::PersistConfig)));
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SideEffect::KillService(Service::Apache))));
    }

    // ── Port ledger / allocate_port ─────────────────────────────────────────

    #[test]
    fn allocate_port_returns_the_configured_port_when_free() {
        let mut state = make_state();
        state.config.apache.port = 8080;
        assert_eq!(allocate_port(&state, Service::Apache), Some(8080));
    }

    #[test]
    fn allocate_port_skips_ports_marked_unavailable() {
        let mut state = make_state();
        state.config.apache.port = 8080;
        state.ports.mark_unavailable(Service::Apache, 8080);
        state.ports.mark_unavailable(Service::Apache, 8081);
        assert_eq!(allocate_port(&state, Service::Apache), Some(8082));
    }

    #[test]
    fn allocate_port_never_returns_a_port_assigned_to_another_service() {
        let mut state = make_state();
        state.config.apache.port = 8080;
        state.config.php.port = 8085;
        // PHP already holds 8085; Apache scanning upward must step over it.
        state.ports.assign(Service::Php, 8085);
        for _ in 0..5 {
            state.ports.mark_unavailable(Service::Apache, 8080);
            state.ports.mark_unavailable(Service::Apache, 8081);
            state.ports.mark_unavailable(Service::Apache, 8082);
            state.ports.mark_unavailable(Service::Apache, 8083);
            state.ports.mark_unavailable(Service::Apache, 8084);
        }
        assert_eq!(allocate_port(&state, Service::Apache), Some(8086));
    }

    #[test]
    fn allocate_port_never_steals_another_services_configured_port() {
        let mut state = make_state();
        state.config.apache.port = 8080;
        state.config.php.port = 8081;
        // PHP has not started, but 8081 is its home port and must stay reserved.
        state.ports.mark_unavailable(Service::Apache, 8080);
        assert_eq!(allocate_port(&state, Service::Apache), Some(8082));
    }

    #[test]
    fn allocate_port_returns_none_when_the_scan_range_is_exhausted() {
        let mut state = make_state();
        state.config.apache.port = 8080;
        for offset in 0..=PORT_SCAN_RANGE {
            state.ports.mark_unavailable(Service::Apache, 8080 + offset);
        }
        assert_eq!(allocate_port(&state, Service::Apache), None);
    }

    #[test]
    fn begin_attempt_clears_previous_unavailable_ports() {
        let mut state = make_state();
        state.config.apache.port = 8080;
        state.ports.mark_unavailable(Service::Apache, 8080);
        state.ports.begin_attempt(Service::Apache);
        assert_eq!(allocate_port(&state, Service::Apache), Some(8080));
    }

    #[test]
    fn release_clears_the_assignment() {
        let mut state = make_state();
        state.ports.assign(Service::Mysql, 3307);
        assert_eq!(state.ports.assigned(Service::Mysql), Some(3307));
        state.ports.release(Service::Mysql);
        assert_eq!(state.ports.assigned(Service::Mysql), None);
    }

    // ── Ledger wiring (task 8) ────────────────────────────────────────────

    fn spawn_port(effects: &[SideEffect]) -> Option<u16> {
        effects.iter().find_map(|e| match e {
            SideEffect::SpawnService { port, .. } => Some(*port),
            _ => None,
        })
    }

    #[test]
    fn start_service_allocates_and_records_a_port() {
        let mut state = make_state();
        state.config.apache.port = 8080;
        let (new_state, effects) = reducer(state, Event::StartService(Service::Apache));
        assert_eq!(spawn_port(&effects), Some(8080));
        assert_eq!(new_state.ports.assigned(Service::Apache), Some(8080));
    }

    #[test]
    fn starting_all_three_never_assigns_the_same_port_twice() {
        let mut state = make_state();
        // Overlapping ranges: apache and php would both scan through 8085.
        state.config.apache.port = 8080;
        state.config.php.port = 8085;
        state.config.mysql.port = 3306;
        for svc in [Service::Apache, Service::Mysql, Service::Php] {
            let (s, _) = reducer(state, Event::StartService(svc));
            state = s;
        }
        let a = state.ports.assigned(Service::Apache).unwrap();
        let m = state.ports.assigned(Service::Mysql).unwrap();
        let p = state.ports.assigned(Service::Php).unwrap();
        assert_ne!(a, m);
        assert_ne!(a, p);
        assert_ne!(m, p);
    }

    #[test]
    fn port_unavailable_advances_to_the_next_candidate() {
        let mut state = make_state();
        state.config.apache.port = 8080;
        let (state, _) = reducer(state, Event::StartService(Service::Apache));
        let (state, effects) = reducer(
            state,
            Event::PortUnavailable {
                service: Service::Apache,
                port: 8080,
            },
        );
        assert_eq!(spawn_port(&effects), Some(8081));
        assert_eq!(state.ports.assigned(Service::Apache), Some(8081));
        assert_eq!(
            state.apache.state,
            ServiceState::Starting,
            "still starting — a blocked port is not a crash"
        );
    }

    #[test]
    fn stale_port_unavailable_for_a_different_port_is_ignored() {
        let mut state = make_state();
        state.config.apache.port = 8080;
        let (state, _) = reducer(state, Event::StartService(Service::Apache));
        let (state, effects) = reducer(
            state,
            Event::PortUnavailable {
                service: Service::Apache,
                port: 9999,
            },
        );
        assert!(
            spawn_port(&effects).is_none(),
            "must not respawn on a stale report"
        );
        assert_eq!(state.ports.assigned(Service::Apache), Some(8080));
    }

    #[test]
    fn port_unavailable_is_ignored_when_the_service_is_not_starting() {
        let state = make_state();
        let (new_state, effects) = reducer(
            state,
            Event::PortUnavailable {
                service: Service::Apache,
                port: 8080,
            },
        );
        assert!(spawn_port(&effects).is_none());
        assert_eq!(new_state.apache.state, ServiceState::Stopped);
    }

    #[test]
    fn exhausting_the_scan_range_reaches_error() {
        let mut state = make_state();
        state.config.apache.port = 8080;
        let (mut state, _) = reducer(state, Event::StartService(Service::Apache));
        for offset in 0..=PORT_SCAN_RANGE {
            let port = state
                .ports
                .assigned(Service::Apache)
                .unwrap_or(8080 + offset);
            let (s, _) = reducer(
                state,
                Event::PortUnavailable {
                    service: Service::Apache,
                    port,
                },
            );
            state = s;
        }
        assert_eq!(state.apache.state, ServiceState::Error);
        assert_eq!(
            state.ports.assigned(Service::Apache),
            None,
            "a dead, definitively-unavailable port must not stay in the ledger — it would \
             keep excluding that port from every other service's candidate range forever"
        );
    }

    #[test]
    fn stopping_releases_the_assigned_port() {
        let mut state = make_state();
        state.config.apache.port = 8080;
        let (state, _) = reducer(state, Event::StartService(Service::Apache));
        let (state, _) = reducer(state, Event::StopService(Service::Apache));
        let (state, _) = reducer(
            state,
            Event::ProcessExit {
                service: Service::Apache,
                exit_code: Some(0),
            },
        );
        assert_eq!(state.ports.assigned(Service::Apache), None);
    }
}
