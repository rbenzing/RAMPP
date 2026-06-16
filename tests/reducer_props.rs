/// Layer 4 property-based tests: reducer invariants hold across randomised event sequences.
///
/// Uses proptest to generate arbitrary (state, event) pairs and assert that the reducer
/// never panics and always preserves the documented invariants.
///
/// Run with: cargo test --test reducer_props
use proptest::prelude::*;
use rampp::events::{Event, SideEffect};
use rampp::reducer::reducer;
use rampp::state::{
    ApacheConfig, AppState, DesiredServiceState, MysqlConfig, PhpConfig, PhpMyAdminConfig,
    RampConfig, Service, ServiceState, MAX_RETRIES,
};
use std::path::PathBuf;

// ── Strategies ────────────────────────────────────────────────────────────────

fn make_base_state() -> AppState {
    AppState::new(RampConfig {
        install_dir: PathBuf::from("C:\\ramp"),
        apache: ApacheConfig {
            port: 8080,
            bin: PathBuf::from("C:\\ramp\\apache\\bin\\httpd.exe"),
            conf: PathBuf::from("C:\\ramp\\apache\\conf\\httpd.conf"),
            document_root: PathBuf::from("C:\\ramp\\apache\\htdocs"),
        },
        mysql: MysqlConfig {
            port: 3306,
            bin: PathBuf::from("C:\\ramp\\mysql\\bin\\mysqld.exe"),
            data_dir: PathBuf::from("C:\\ramp\\mysql\\data"),
            ini: PathBuf::from("C:\\ramp\\mysql\\my.ini"),
        },
        php: PhpConfig {
            port: 9000,
            bin: PathBuf::from("C:\\ramp\\php\\php-cgi.exe"),
            ini: PathBuf::from("C:\\ramp\\php\\php.ini"),
        },
        phpmyadmin: PhpMyAdminConfig {
            mysql_user: "root".to_string(),
            mysql_password: String::new(),
        },
    })
}

fn arb_service() -> impl Strategy<Value = Service> {
    prop_oneof![
        Just(Service::Apache),
        Just(Service::Mysql),
        Just(Service::Php),
    ]
}

fn arb_service_state() -> impl Strategy<Value = ServiceState> {
    prop_oneof![
        Just(ServiceState::Stopped),
        Just(ServiceState::Starting),
        Just(ServiceState::Running),
        Just(ServiceState::Stopping),
        Just(ServiceState::Crashed),
        Just(ServiceState::Error),
    ]
}

fn arb_desired() -> impl Strategy<Value = DesiredServiceState> {
    prop_oneof![
        Just(DesiredServiceState::Running),
        Just(DesiredServiceState::Stopped),
    ]
}

fn arb_event() -> impl Strategy<Value = Event> {
    prop_oneof![
        arb_service().prop_map(Event::StartService),
        arb_service().prop_map(Event::StopService),
        arb_service().prop_map(Event::RestartService),
        (arb_service(), proptest::option::of(0u32..=10)).prop_map(|(svc, code)| {
            Event::ProcessExit {
                service: svc,
                exit_code: code,
            }
        }),
        arb_service().prop_map(Event::ProcessReady),
        (arb_service(), any::<String>()).prop_map(|(svc, reason)| {
            Event::ProcessSpawnFailed {
                service: svc,
                reason,
            }
        }),
        arb_service().prop_map(Event::HealthCheckPass),
        arb_service().prop_map(Event::HealthCheckFail),
        arb_service().prop_map(Event::PortConflictDetected),
        (arb_service(), any::<String>()).prop_map(|(svc, reason)| Event::FatalError {
            service: svc,
            reason,
        }),
        arb_service().prop_map(Event::AutoRetry),
        Just(Event::Tick),
        Just(Event::ShutdownAll),
    ]
}

// ── Invariant helpers ─────────────────────────────────────────────────────────

fn check_invariants(state: &AppState) {
    for svc in [Service::Apache, Service::Mysql, Service::Php] {
        let s = state.service(svc);
        // retry_count must never exceed MAX_RETRIES
        assert!(
            s.retry_count <= MAX_RETRIES,
            "{svc} retry_count {} exceeds MAX_RETRIES {MAX_RETRIES}",
            s.retry_count
        );
        // started_at must only be Some when in Starting or Running state.
        // Conversely: when state is Stopped/Stopping/Crashed/Error, started_at must be None.
        if s.started_at.is_some() {
            assert!(
                matches!(s.state, ServiceState::Starting | ServiceState::Running),
                "{svc}: started_at is Some but state is {:?} (must be Starting or Running)",
                s.state
            );
        }
        if matches!(
            s.state,
            ServiceState::Stopped
                | ServiceState::Stopping
                | ServiceState::Crashed
                | ServiceState::Error
        ) {
            assert!(
                s.started_at.is_none(),
                "{svc}: state is {:?} (terminal) but started_at is Some (must be None)",
                s.state
            );
        }
    }
}

// ── Properties ───────────────────────────────────────────────────────────────

proptest! {
    /// The reducer must never panic on any combination of state and event.
    #[test]
    fn reducer_never_panics(
        apache in arb_service_state(),
        mysql in arb_service_state(),
        php in arb_service_state(),
        apache_desired in arb_desired(),
        mysql_desired in arb_desired(),
        php_desired in arb_desired(),
        apache_retry in 0u32..=MAX_RETRIES,
        event in arb_event(),
    ) {
        let mut state = make_base_state();
        state.apache.state = apache;
        state.mysql.state = mysql;
        state.php.state = php;
        state.apache.desired = apache_desired;
        state.mysql.desired = mysql_desired;
        state.php.desired = php_desired;
        state.apache.retry_count = apache_retry;

        // Must not panic
        let (_new_state, _effects) = reducer(state, event);
    }

    /// ShutdownAll always sets desired=Stopped for all services, regardless of current state.
    #[test]
    fn shutdown_all_always_sets_desired_stopped(
        apache in arb_service_state(),
        mysql in arb_service_state(),
        php in arb_service_state(),
    ) {
        let mut state = make_base_state();
        state.apache.state = apache;
        state.mysql.state = mysql;
        state.php.state = php;

        let (new_state, _) = reducer(state, Event::ShutdownAll);

        prop_assert_eq!(new_state.apache.desired, DesiredServiceState::Stopped);
        prop_assert_eq!(new_state.mysql.desired, DesiredServiceState::Stopped);
        prop_assert_eq!(new_state.php.desired, DesiredServiceState::Stopped);
    }

    /// KillService must accompany StopService when the service is Running or Starting.
    #[test]
    fn stop_from_active_state_always_emits_kill(
        initial in prop_oneof![Just(ServiceState::Running), Just(ServiceState::Starting)],
        svc in arb_service(),
    ) {
        let mut state = make_base_state();
        state.service_mut(svc).state = initial;

        let (_, effects) = reducer(state, Event::StopService(svc));

        let has_kill = effects.iter().any(|e| matches!(e, SideEffect::KillService(s) if *s == svc));
        prop_assert!(has_kill, "KillService({svc}) missing from effects after StopService in {initial:?}");
    }

    /// retry_count never exceeds MAX_RETRIES after any sequence of crash events.
    #[test]
    fn retry_count_bounded_after_crashes(
        n_crashes in 1usize..20,
    ) {
        let mut state = make_base_state();
        state.apache.state = ServiceState::Running;
        state.apache.desired = DesiredServiceState::Running;

        for _ in 0..n_crashes {
            let (new_state, _) = reducer(state.clone(), Event::ProcessExit {
                service: Service::Apache,
                exit_code: Some(1),
            });
            state = new_state;
            // Reset to Running/Crashed to keep crashing
            if state.apache.state == ServiceState::Error {
                // Max retries hit — retry_count must be exactly MAX_RETRIES
                prop_assert_eq!(state.apache.retry_count, MAX_RETRIES);
                return Ok(());
            }
        }
        prop_assert!(state.apache.retry_count <= MAX_RETRIES);
    }

    /// FatalError always transitions the target service to Error state.
    #[test]
    fn fatal_error_always_reaches_error_state(
        initial in arb_service_state(),
        svc in arb_service(),
    ) {
        let mut state = make_base_state();
        state.service_mut(svc).state = initial;

        let (new_state, _) = reducer(state, Event::FatalError {
            service: svc,
            reason: "test".into(),
        });

        prop_assert_eq!(new_state.service(svc).state, ServiceState::Error);
    }

    /// Invariants hold after every single reducer call.
    #[test]
    fn invariants_hold_after_any_event(
        apache in arb_service_state(),
        mysql in arb_service_state(),
        php in arb_service_state(),
        event in arb_event(),
    ) {
        let mut state = make_base_state();
        state.apache.state = apache;
        state.mysql.state = mysql;
        state.php.state = php;

        let (new_state, _) = reducer(state, event);
        check_invariants(&new_state);
    }

    /// Invariants hold after a sequence of up to 10 events.
    #[test]
    fn invariants_hold_after_event_sequence(
        events in prop::collection::vec(arb_event(), 1..10),
    ) {
        let mut state = make_base_state();
        for event in events {
            let (new_state, _) = reducer(state, event);
            state = new_state;
            check_invariants(&state);
        }
    }
}
