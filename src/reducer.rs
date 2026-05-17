use crate::events::{Event, SideEffect};
use crate::state::{
    retry_delay, AppState, DesiredServiceState, Service, ServiceState, MAX_RETRIES,
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
                    effects.push(SideEffect::SpawnService(svc));
                    effects.push(SideEffect::StartReadinessCheck(svc));
                    effects.push(SideEffect::LogEvent(format!("{svc}: starting")));
                    effects.push(SideEffect::PersistDesiredState);
                }
                ServiceState::Crashed => {
                    // Treat same as Stopped — reset and try again
                    state.set_starting(svc);
                    state.service_mut(svc).desired = DesiredServiceState::Running;
                    state.service_mut(svc).retry_count = 0;
                    state.service_mut(svc).last_error = None;
                    effects.push(SideEffect::SpawnService(svc));
                    effects.push(SideEffect::StartReadinessCheck(svc));
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
                    effects.push(SideEffect::SpawnService(svc));
                    effects.push(SideEffect::StartReadinessCheck(svc));
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
                state.clear_started_at(svc);
                effects.push(SideEffect::LogEvent(format!("{svc}: ready")));
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
                        effects.push(SideEffect::SpawnService(svc));
                        effects.push(SideEffect::StartReadinessCheck(svc));
                        effects.push(SideEffect::LogEvent(format!(
                            "{svc}: restarting per desired state"
                        )));
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

        Event::PortAssigned { service: svc, port } => {
            state.service_mut(svc).effective_port = Some(port);
            // Only log when the chosen port differs from the configured one.
            let configured = match svc {
                Service::Apache => state.config.apache.port,
                Service::Mysql => state.config.mysql.port,
                Service::Php => state.config.php.port,
            };
            if port != configured {
                effects.push(SideEffect::LogEvent(format!(
                    "{svc}: configured port {configured} in use — using {port} instead"
                )));
            }
        }

        // ── Auto-retry (from executor timer) ────────────────────────────────
        Event::AutoRetry(svc) => {
            if state.service(svc).state == ServiceState::Crashed
                && state.service(svc).desired == DesiredServiceState::Running
            {
                state.set_starting(svc);
                effects.push(SideEffect::SpawnService(svc));
                effects.push(SideEffect::StartReadinessCheck(svc));
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
                effects.push(SideEffect::TogglePhpMyAdmin(target));
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
            install_dir: std::path::PathBuf::from("C:\\ramp"),
            apache: ApacheConfig {
                port: 80,
                bin: std::path::PathBuf::from("C:\\ramp\\apache\\bin\\httpd.exe"),
                conf: std::path::PathBuf::from("C:\\ramp\\apache\\conf\\httpd.conf"),
            },
            mysql: MysqlConfig {
                port: 3306,
                bin: std::path::PathBuf::from("C:\\ramp\\mysql\\bin\\mysqld.exe"),
                data_dir: std::path::PathBuf::from("C:\\ramp\\mysql\\data"),
                ini: std::path::PathBuf::from("C:\\ramp\\mysql\\my.ini"),
            },
            php: PhpConfig {
                port: 9000,
                bin: std::path::PathBuf::from("C:\\ramp\\php\\php-cgi.exe"),
                ini: std::path::PathBuf::from("C:\\ramp\\php\\php.ini"),
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
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::SpawnService(Service::Apache))));
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
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::SpawnService(Service::Apache))));
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
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SideEffect::SpawnService(Service::Apache))));
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
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::SpawnService(Service::Php))));
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
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SideEffect::SpawnService(_) | SideEffect::KillService(_))));
    }

    // ── RestartService edge cases ─────────────────────────────────────────

    #[test]
    fn restart_from_stopped_spawns_directly() {
        // RestartService on a Stopped service should start it directly, not go through Stopping
        let state = make_state(); // apache starts Stopped
        let (new_state, effects) = reducer(state, Event::RestartService(Service::Apache));
        assert_eq!(new_state.apache.state, ServiceState::Starting);
        assert_eq!(new_state.apache.desired, DesiredServiceState::Running);
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::SpawnService(Service::Apache))));
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
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::SpawnService(Service::Apache))));
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
            SideEffect::SpawnService(_) | SideEffect::ScheduleRetry { .. }
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
    fn started_at_set_on_starting_cleared_on_running() {
        let state = make_state();
        let (state, _) = reducer(state, Event::StartService(Service::Apache));
        assert!(
            state.apache.started_at.is_some(),
            "started_at must be set when Starting"
        );
        let (state, _) = reducer(state, Event::ProcessReady(Service::Apache));
        assert!(
            state.apache.started_at.is_none(),
            "started_at must be cleared when Running"
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
                .any(|e| matches!(e, SideEffect::SpawnService(_))));
        }
    }
}
