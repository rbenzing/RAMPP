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
    /// Executor resolved the port the service will actually bind to.
    /// Emitted before a successful spawn so the UI/reducer know the chosen port.
    PortAssigned {
        service: Service,
        port: u16,
    },

    // Config — boxed to keep the enum variant size uniform
    ConfigReloaded(Box<crate::state::RampConfig>),

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
}

/// Side effects produced by the reducer. Executed by the executor AFTER state mutation.
/// Side effects MUST never mutate state directly — they emit follow-up events.
#[allow(dead_code)]
#[derive(Debug)]
pub enum SideEffect {
    SpawnService(Service),
    KillService(Service),
    ScheduleRetry { service: Service, delay: Duration },
    StartReadinessCheck(Service),
    StopHealthCheck(Service),
    LogEvent(String),
    PersistDesiredState,
    TogglePhpMyAdmin(bool),
}
