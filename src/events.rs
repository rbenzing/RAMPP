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
    ProcessReady(Service),
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
    StartReadinessCheck(Service),
    StopHealthCheck(Service),
    LogEvent(String),
    PersistDesiredState,
    /// Refresh the executor's config copy from state and persist rampp.toml.
    PersistConfig,
    TogglePhpMyAdmin(bool),
    OpenPhpMyAdminBrowser,
}
