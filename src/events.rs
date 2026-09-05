use crate::state::Service;
use std::time::Duration;

#[allow(dead_code)]
/// All system mutations MUST originate from one of these events.
/// Events are processed in FIFO order by the single-threaded reducer loop.
#[derive(Debug, Clone)]
pub enum Event {
    // User / IPC commands
    StartService(Service),
    StopService(Service),
    RestartService(Service),

    // OS / process signals
    ProcessExit {
        service: Service,
        exit_code: Option<u32>,
    },
    /// The readiness poller confirmed the service answering on `port`.
    ProcessReady {
        service: Service,
        port: u16,
    },
    /// The readiness poller gave up waiting for `port` to answer within the
    /// service's readiness timeout. Distinct from `ProcessExit`: the OS process
    /// may still be alive (see `poll_until_ready_with_timeout`), so this is only
    /// authoritative for the start attempt currently in flight on that exact
    /// port — a stale report (e.g. from an attempt the reducer already moved
    /// past after a `PortUnavailable` reallocation) must be dropped, not treated
    /// as a crash of whatever is running now.
    ReadinessTimeout {
        service: Service,
        port: u16,
    },
    ProcessSpawnFailed {
        service: Service,
        reason: String,
    },

    // Health check results
    HealthCheckPass(Service),
    HealthCheckFail(Service),

    // Port management
    PortConflictDetected(Service),
    /// A port the reducer allocated could not be used — the pre-check found a
    /// listener, the bind failed, or the error log was diagnosed as a bind
    /// failure. The reducer blacklists it and allocates the next candidate.
    PortUnavailable {
        service: Service,
        port: u16,
    },

    // Config — boxed to keep the enum variant size uniform
    ConfigReloaded(Box<crate::state::RampConfig>),
    /// Result of a config reconcile pass. `changed` lists files whose content on
    /// disk actually differed; `spawning` is the service whose spawn triggered
    /// this pass, which must not be restarted by its own reconcile.
    ConfigsReconciled {
        changed: Vec<crate::state::ManagedFile>,
        spawning: Option<Service>,
    },

    // Internal
    FatalError {
        service: Service,
        reason: String,
    },
    AutoRetry(Service),
    Tick,

    // Shutdown
    ShutdownAll,

    // phpMyAdmin
    TogglePhpMyAdmin,
    PhpMyAdminToggled(bool),
    OpenPhpMyAdmin,
    /// Emitted by the event loop's Tick handler when the on-disk phpmyadmin
    /// directory's existence differs from `state.phpmyadmin_dir_exists`.
    PhpMyAdminDirChanged(bool),

    // UI actions
    ClearLog,
    DismissError(Service),
    /// User picked a new Apache DocumentRoot (already validated by the UI).
    SetDocumentRoot(std::path::PathBuf),

    // Diagnostics — emitted by background threads to surface log output
    DiagnosticLog(String),
}

/// Side effects produced by the reducer. Executed by the executor AFTER state mutation.
/// Side effects MUST never mutate state directly — they emit follow-up events.
#[allow(dead_code)]
#[derive(Debug)]
pub enum SideEffect {
    SpawnService {
        service: Service,
        port: u16,
    },
    KillService(Service),
    ScheduleRetry {
        service: Service,
        delay: Duration,
    },
    /// The port to poll is fixed here, at the moment the reducer queues the
    /// effect — not re-derived later from possibly-stale `state.ports`, which
    /// could have moved on to a different attempt by the time the executor
    /// gets around to running this.
    StartReadinessCheck {
        service: Service,
        port: u16,
    },
    StopHealthCheck(Service),
    LogEvent(String),
    PersistDesiredState,
    /// Refresh the executor's config copy from state and persist rampp.toml.
    PersistConfig,
    TogglePhpMyAdmin(bool),
    OpenPhpMyAdminBrowser,
}
