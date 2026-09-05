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
    RampConfig, Service, ServiceState, MAX_RETRIES, PORT_SCAN_RANGE,
};
use std::path::PathBuf;

// ── Strategies ────────────────────────────────────────────────────────────────

fn make_base_state() -> AppState {
    AppState::new(RampConfig {
        install_dir: PathBuf::from("C:\\rampp"),
        apache: ApacheConfig {
            port: 8080,
            bin: PathBuf::from("C:\\rampp\\apache\\bin\\httpd.exe"),
            conf: PathBuf::from("C:\\rampp\\apache\\conf\\httpd.conf"),
            document_root: PathBuf::from("C:\\rampp\\apache\\htdocs"),
        },
        mysql: MysqlConfig {
            port: 3306,
            bin: PathBuf::from("C:\\rampp\\mysql\\bin\\mysqld.exe"),
            data_dir: PathBuf::from("C:\\rampp\\mysql\\data"),
            ini: PathBuf::from("C:\\rampp\\mysql\\my.ini"),
        },
        php: PhpConfig {
            port: 9000,
            bin: PathBuf::from("C:\\rampp\\php\\php-cgi.exe"),
            ini: PathBuf::from("C:\\rampp\\php\\php.ini"),
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

/// Ports plausible enough to collide with a live assignment, rather than almost
/// certainly missing and only ever exercising `PortUnavailable`'s stale-report
/// branch. Spans each service's home port through its scan range under
/// `make_base_state()`'s *default* (disjoint) config — apache 8080..=8100,
/// mysql 3306..=3326, php 9000..=9020. This is shared by every property that
/// draws events from `arb_event()`. A property that overrides the config to
/// something else (e.g. `no_two_services_ever_share_an_assigned_port`, which
/// puts apache and mysql on the same home port) will find the sub-ranges that
/// don't match its overridden config are dead weight for its own purposes —
/// those `PortUnavailable` events just land on ports nothing is ever assigned
/// to and get treated as stale, harmless no-ops. That's expected: this helper
/// is calibrated for the shared default config, not tailored per property.
fn arb_port() -> impl Strategy<Value = u16> {
    prop_oneof![8080u16..=8100, 3306u16..=3326, 9000u16..=9020]
}

/// A small, deliberately-collision-prone range — mirrors `arb_port()`'s
/// calibration rationale. `PortState::current_attempt` starts most services
/// at 1 or 2 real attempts deep during a short generated event sequence, so a
/// tight range like this actually lands on the live value often enough to
/// exercise the "matching, in-flight report" branch of the correlation guard,
/// not just its "stale" branch — a huge sparse range would almost always miss.
fn arb_attempt() -> impl Strategy<Value = u32> {
    0u32..=3
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
        (arb_service(), arb_port(), arb_attempt()).prop_map(|(svc, port, attempt)| {
            Event::ProcessReady {
                service: svc,
                port,
                attempt,
            }
        }),
        (arb_service(), arb_port(), arb_attempt()).prop_map(|(svc, port, attempt)| {
            Event::ReadinessTimeout {
                service: svc,
                port,
                attempt,
            }
        }),
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
        (arb_service(), arb_port())
            .prop_map(|(svc, port)| Event::PortUnavailable { service: svc, port }),
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

    /// An edge-case stress test of the port ledger, *not* a scenario a live
    /// system can reach: apache and mysql are configured with the exact same
    /// home port (8080). `src/config.rs`'s validation rejects any real
    /// `rampp.toml` where two services share a port, so this state can only
    /// arise from a hand-built `AppState` like this one — but the reducer must
    /// still never let it produce two services holding the same assigned
    /// port, and this is a cheap, reliable way to hammer that.
    ///
    /// With both home ports identical, `allocate_port`'s `taken_by_other`
    /// check has real work to do from the very first `StartService` for each
    /// service: whichever starts first is pushed off offset 0 (and usually
    /// offset 1) by the *other's configured* port, and the *other's assigned*
    /// port becomes the clause that matters once both are competing for
    /// offset 2 onward — no drift needed, so the collision is reachable with
    /// high probability on almost every generated event sequence. See
    /// `drifting_into_another_services_territory_never_collides` below for the
    /// complementary, config-valid scenario this one does not cover: distinct
    /// home ports whose scan ranges merely overlap, colliding only through
    /// `PortUnavailable`-driven drift.
    #[test]
    fn no_two_services_ever_share_an_assigned_port(
        events in prop::collection::vec(arb_event(), 0..60),
    ) {
        let mut state = make_base_state();
        state.config.apache.port = 8080;
        state.config.mysql.port = 8080; // deliberately identical to apache's —
        // config.rs would reject this in a real rampp.toml; see the docs above.
        state.config.php.port = 8081;
        for event in events {
            let (next, _) = reducer(state, event);
            state = next;
            let assigned: Vec<u16> = [Service::Apache, Service::Mysql, Service::Php]
                .into_iter()
                .filter_map(|s| state.ports.assigned(s))
                .collect();
            let mut unique = assigned.clone();
            unique.sort_unstable();
            unique.dedup();
            prop_assert_eq!(
                assigned.len(),
                unique.len(),
                "two services hold the same port: {:?}",
                assigned
            );
        }
    }

    /// The realistic counterpart to `no_two_services_ever_share_an_assigned_port`
    /// above: apache and php are given *distinct*, config-valid home ports
    /// (`src/config.rs` would accept this — unlike the identical-port scenario
    /// above) whose 20-port scan ranges nonetheless overlap. A live system can
    /// only produce a collision here the way it actually would in production:
    /// one service's port getting reported unavailable over and over — real
    /// bind failures, one per retry — walks its assignment forward until it
    /// lands squarely on a port the other service already holds.
    ///
    /// Drives that drift deterministically, the same technique
    /// `a_start_attempt_never_exceeds_the_scan_range` uses (feed
    /// `PortUnavailable` the service's own currently-assigned, in-flight port,
    /// over and over), rather than hoping `arb_event()`'s independent random
    /// port field stumbles into it. It doesn't: the first draft of the
    /// property above used exactly this overlapping-but-distinct setup
    /// (apache 8080, mysql 8085, php 8090) and relied on random events alone,
    /// and with `allocate_port`'s `taken_by_other` guard deliberately
    /// disabled, that draft still passed every run — the odds of a random
    /// port field matching a service's live assignment several times in a row
    /// are negligible. Forcing the drift explicitly is what makes this
    /// version actually reach the collision-eligible state.
    #[test]
    fn drifting_into_another_services_territory_never_collides(gap in 1u16..=PORT_SCAN_RANGE) {
        let mut state = make_base_state();
        state.config.apache.port = 8080;
        state.config.php.port = 8080 + gap; // distinct, config-valid, within
        // apache's scan range. mysql stays at its default 3306 — far enough
        // away to stay uninvolved.

        // Php starts first and settles on its home port: a real, already-
        // running service holding a port that sits squarely in apache's
        // future scan path.
        let (state1, _) = reducer(state, Event::StartService(Service::Php));
        state = state1;
        prop_assert_eq!(state.ports.assigned(Service::Php), Some(8080 + gap));

        // Apache starts and gets its own home port cleanly (php isn't there yet).
        let (state2, _) = reducer(state, Event::StartService(Service::Apache));
        state = state2;
        prop_assert_eq!(state.ports.assigned(Service::Apache), Some(8080));

        // Walk apache's assignment forward one blacklisted port at a time —
        // exactly what a real client sees on repeated bind failures — until
        // its candidate reaches php's live port.
        for _ in 0..gap {
            let Some(port) = state.ports.assigned(Service::Apache) else {
                break;
            };
            let (next, _) = reducer(
                state,
                Event::PortUnavailable {
                    service: Service::Apache,
                    port,
                },
            );
            state = next;
            let assigned: Vec<u16> = [Service::Apache, Service::Mysql, Service::Php]
                .into_iter()
                .filter_map(|s| state.ports.assigned(s))
                .collect();
            let mut unique = assigned.clone();
            unique.sort_unstable();
            unique.dedup();
            prop_assert_eq!(
                assigned.len(),
                unique.len(),
                "apache's drift collided with php's territory: {:?}",
                assigned
            );
        }
    }

    /// A reconcile that changed nothing must not restart anything — this is what
    /// stops the config-restart chain from looping. Runs a random warm-up event
    /// sequence first so the property is checked from states where services have
    /// actually reached Running or Starting (not just the fresh Stopped state
    /// `ConfigsReconciled`'s restart branch requires to have anything to do).
    #[test]
    fn an_empty_reconcile_never_restarts_anything(
        events in prop::collection::vec(arb_event(), 0..40),
    ) {
        let mut state = make_base_state();
        for event in events {
            let (next, _) = reducer(state, event);
            state = next;
        }
        let (_, effects) = reducer(
            state,
            Event::ConfigsReconciled {
                changed: vec![],
                spawning: None,
            },
        );
        prop_assert!(
            !effects.iter().any(|e| matches!(e, SideEffect::KillService(_))),
            "an idempotent reconcile must terminate the restart chain"
        );
    }

    /// Allocation always terminates: a single start attempt cannot issue more
    /// spawns than the scan range allows. Forces the worst case — every single
    /// candidate port in the scan range reported unavailable, back to back — by
    /// always feeding `PortUnavailable` the service's own currently-assigned
    /// (in-flight) port, so the property actually reaches full scan-range
    /// exhaustion rather than stopping after a handful of iterations.
    #[test]
    fn a_start_attempt_never_exceeds_the_scan_range(svc in arb_service()) {
        let state = make_base_state();
        let (mut state, first_effects) = reducer(state, Event::StartService(svc));
        prop_assert!(
            first_effects
                .iter()
                .any(|e| matches!(e, SideEffect::SpawnService { .. })),
            "StartService on a clean, disjoint-port config must spawn immediately"
        );
        let mut spawns = 1usize;
        for _ in 0..(PORT_SCAN_RANGE as usize + 5) {
            let Some(port) = state.ports.assigned(svc) else {
                break;
            };
            let (next, effects) = reducer(
                state,
                Event::PortUnavailable {
                    service: svc,
                    port,
                },
            );
            state = next;
            if effects
                .iter()
                .any(|e| matches!(e, SideEffect::SpawnService { .. }))
            {
                spawns += 1;
            }
        }
        prop_assert!(
            spawns <= PORT_SCAN_RANGE as usize + 1,
            "allocation issued {spawns} spawns, above the {} bound",
            PORT_SCAN_RANGE as usize + 1
        );
        // Every candidate in the scan range was reported unavailable, so the loop
        // must have run all the way to exhaustion (Error), not stopped early —
        // proving the assert above actually rides the bound rather than sitting
        // well under it by coincidence.
        prop_assert_eq!(
            state.service(svc).state,
            ServiceState::Error,
            "worst-case exhaustion must reach Error, proving the loop ran to the bound"
        );
        prop_assert_eq!(
            spawns,
            PORT_SCAN_RANGE as usize + 1,
            "worst-case exhaustion should spawn exactly once per candidate port"
        );
    }
}
