use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServiceState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Crashed,
    Error,
}

impl std::fmt::Display for ServiceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceState::Stopped => write!(f, "Stopped"),
            ServiceState::Starting => write!(f, "Starting"),
            ServiceState::Running => write!(f, "Running"),
            ServiceState::Stopping => write!(f, "Stopping"),
            ServiceState::Crashed => write!(f, "Crashed"),
            ServiceState::Error => write!(f, "Error"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesiredServiceState {
    Running,
    Stopped,
}

impl DesiredServiceState {
    pub fn default_stopped() -> Self {
        Self::Stopped
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Service {
    Apache,
    Mysql,
    Php,
}

impl std::fmt::Display for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Service::Apache => write!(f, "Apache"),
            Service::Mysql => write!(f, "MySQL"),
            Service::Php => write!(f, "PHP"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServiceStatus {
    pub state: ServiceState,
    pub desired: DesiredServiceState,
    pub retry_count: u32,
    pub last_error: Option<String>,
    pub health_fail_streak: u32,
    /// Set when the service transitions to Starting; cleared on Running/Stopped/Error/Crashed.
    /// Used by the UI to display elapsed startup time. Not persisted.
    pub started_at: Option<Instant>,
}

impl Default for ServiceStatus {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceStatus {
    pub fn new() -> Self {
        Self {
            state: ServiceState::Stopped,
            desired: DesiredServiceState::Stopped,
            retry_count: 0,
            last_error: None,
            health_fail_streak: 0,
            started_at: None,
        }
    }
}

/// Maximum number of ports to scan upward from the configured port when looking
/// for a free one (e.g. 8080 → 8081 → … → 8100). Beyond this we surrender.
pub const PORT_SCAN_RANGE: u16 = 20;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApacheConfig {
    pub port: u16,
    pub bin: PathBuf,
    pub conf: PathBuf,
    pub document_root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MysqlConfig {
    pub port: u16,
    pub bin: PathBuf,
    pub data_dir: PathBuf,
    pub ini: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhpConfig {
    pub port: u16,
    pub bin: PathBuf,
    pub ini: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhpMyAdminConfig {
    pub mysql_user: String,
    pub mysql_password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RampConfig {
    pub install_dir: PathBuf,
    pub apache: ApacheConfig,
    pub mysql: MysqlConfig,
    pub php: PhpConfig,
    pub phpmyadmin: PhpMyAdminConfig,
}

/// One service's slot in the port ledger.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PortSlot {
    /// Port allocated for the next or current bind. None before the first start,
    /// and after the service reaches Stopped or Error.
    assigned: Option<u16>,
    /// Ports proven unbindable during the current start attempt. Cleared by
    /// `begin_attempt`. This is what bounds the allocation retry loop.
    unavailable: Vec<u16>,
}

/// Reducer-owned port allocation ledger — the single source of truth for which
/// port each service will bind. Runtime state; never persisted.
///
/// Allocation happens inside the single-threaded reducer, which is what makes
/// two services sharing a port structurally impossible: each StartService sees
/// the ledger already updated by the one before it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortState {
    apache: PortSlot,
    mysql: PortSlot,
    php: PortSlot,
}

impl PortState {
    fn slot(&self, svc: Service) -> &PortSlot {
        match svc {
            Service::Apache => &self.apache,
            Service::Mysql => &self.mysql,
            Service::Php => &self.php,
        }
    }

    fn slot_mut(&mut self, svc: Service) -> &mut PortSlot {
        match svc {
            Service::Apache => &mut self.apache,
            Service::Mysql => &mut self.mysql,
            Service::Php => &mut self.php,
        }
    }

    pub fn assigned(&self, svc: Service) -> Option<u16> {
        self.slot(svc).assigned
    }

    pub fn assign(&mut self, svc: Service, port: u16) {
        self.slot_mut(svc).assigned = Some(port);
    }

    pub fn is_unavailable(&self, svc: Service, port: u16) -> bool {
        self.slot(svc).unavailable.contains(&port)
    }

    pub fn mark_unavailable(&mut self, svc: Service, port: u16) {
        let slot = self.slot_mut(svc);
        if !slot.unavailable.contains(&port) {
            slot.unavailable.push(port);
        }
    }

    /// Start a fresh attempt: forget the previous assignment and re-probe ports
    /// that were blocked last time, since whatever held them may have released.
    pub fn begin_attempt(&mut self, svc: Service) {
        let slot = self.slot_mut(svc);
        slot.assigned = None;
        slot.unavailable.clear();
    }

    /// Drop the assignment when a service is no longer running, so the UI stops
    /// advertising a port nothing is listening on.
    pub fn release(&mut self, svc: Service) {
        self.slot_mut(svc).assigned = None;
    }
}

/// Every config file RAMPP generates and owns.
// Not yet consumed by `src/main.rs`'s standalone binary crate (which does not
// inherit `src/lib.rs`'s crate-wide `#![allow(dead_code)]`) — task 10 wires
// `provision` into the executor and `main.rs`.
#[allow(dead_code)] // wired up in task 10
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedFile {
    HttpdConf,
    MyIni,
    PhpIni,
    PhpMyAdminConf,
    PhpMyAdminConfigInc,
}

impl std::fmt::Display for ManagedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManagedFile::HttpdConf => write!(f, "apache/conf/httpd.conf"),
            ManagedFile::MyIni => write!(f, "mysql/my.ini"),
            ManagedFile::PhpIni => write!(f, "php/php.ini"),
            ManagedFile::PhpMyAdminConf => write!(f, "apache/conf/phpmyadmin.conf"),
            ManagedFile::PhpMyAdminConfigInc => write!(f, "phpmyadmin/config.inc.php"),
        }
    }
}

/// The complete application state. Owned exclusively by the reducer.
/// Never mutated outside of reducer(state, event) → (state, effects).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct AppState {
    pub apache: ServiceStatus,
    pub mysql: ServiceStatus,
    pub php: ServiceStatus,
    pub config: RampConfig,
    pub ports: PortState,
    pub phpmyadmin_enabled: bool,
    pub phpmyadmin_dir_exists: bool,
    /// Set when phpMyAdmin is toggled on; cleared and acted on when Apache next becomes Ready.
    /// Ensures the browser opens with the correct port after Apache restarts.
    pub open_phpmyadmin_on_apache_ready: bool,
}

impl AppState {
    pub fn new(config: RampConfig) -> Self {
        Self {
            apache: ServiceStatus::new(),
            mysql: ServiceStatus::new(),
            php: ServiceStatus::new(),
            config,
            ports: PortState::default(),
            phpmyadmin_enabled: false,
            phpmyadmin_dir_exists: false,
            open_phpmyadmin_on_apache_ready: false,
        }
    }

    pub fn service(&self, svc: Service) -> &ServiceStatus {
        match svc {
            Service::Apache => &self.apache,
            Service::Mysql => &self.mysql,
            Service::Php => &self.php,
        }
    }

    pub fn service_mut(&mut self, svc: Service) -> &mut ServiceStatus {
        match svc {
            Service::Apache => &mut self.apache,
            Service::Mysql => &mut self.mysql,
            Service::Php => &mut self.php,
        }
    }

    /// Transition a service to Starting and record when it began.
    /// Use this instead of setting state directly to keep started_at consistent.
    pub fn set_starting(&mut self, svc: Service) {
        let s = self.service_mut(svc);
        s.state = ServiceState::Starting;
        s.started_at = Some(Instant::now());
    }

    /// Clear started_at when a service leaves the Starting state.
    pub fn clear_started_at(&mut self, svc: Service) {
        self.service_mut(svc).started_at = None;
    }
}

/// Persisted across restarts — records what the user wants running.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedState {
    pub apache_desired: DesiredServiceState,
    pub mysql_desired: DesiredServiceState,
    #[serde(default = "DesiredServiceState::default_stopped")]
    pub php_desired: DesiredServiceState,
    #[serde(default)]
    pub phpmyadmin_enabled: bool,
    #[serde(default)]
    pub phpmyadmin_blowfish_secret: Option<String>,
}

impl PersistedState {
    pub fn default_stopped() -> Self {
        Self {
            apache_desired: DesiredServiceState::Stopped,
            mysql_desired: DesiredServiceState::Stopped,
            php_desired: DesiredServiceState::Stopped,
            phpmyadmin_enabled: false,
            phpmyadmin_blowfish_secret: None,
        }
    }
}

/// Retry backoff schedule per spec: 1s → 2s → 4s → 8s → STOP (max 4 retries).
pub const MAX_RETRIES: u32 = 4;
pub const RETRY_DELAYS: [u64; 4] = [1, 2, 4, 8];

pub fn retry_delay(retry_count: u32) -> Option<Duration> {
    let idx = retry_count as usize;
    RETRY_DELAYS.get(idx).map(|&s| Duration::from_secs(s))
}

/// URL path RAMPP probes to decide whether Apache is up. Apache serves this from a
/// RAMPP-owned directory (see `apache_conf`), so the probe never touches the user's
/// DocumentRoot, `.htaccess`, or PHP.
pub const HEALTH_ENDPOINT_PATH: &str = "/__ramp_health";

/// Directory (relative to `<install_dir>/apache`) holding the health endpoint file.
pub const HEALTH_ENDPOINT_DIR: &str = "rampp-health";

/// File name served at `HEALTH_ENDPOINT_PATH`.
pub const HEALTH_ENDPOINT_FILE: &str = "health.txt";

/// Body written to the health endpoint file.
pub const HEALTH_ENDPOINT_BODY: &str = "RAMPP OK\n";

/// Per-request timeout for a single HTTP readiness probe.
pub const HEALTH_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Apache gets a wider readiness budget than the other services: a cold Windows
/// start can take ~2.5s to begin accepting connections, and a single stalled probe
/// costs `HEALTH_PROBE_TIMEOUT`. Three seconds left no room for a retry, so a slow
/// start was being reported as a crash.
pub const APACHE_READY_TIMEOUT: Duration = Duration::from_secs(10);
pub const MYSQL_READY_TIMEOUT: Duration = Duration::from_secs(5);
pub const PHP_READY_TIMEOUT: Duration = Duration::from_secs(5);
pub const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(2);
pub const HEALTH_FAIL_THRESHOLD: u32 = 3;

/// Apache `Timeout` and the FastCGI worker timeout. Sits above php.ini's
/// `max_execution_time = 30` with margin so PHP decides its own limit.
pub const PROXY_TIMEOUT_SECS: u64 = 60;
pub const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(15);
pub const COMMAND_DEBOUNCE: Duration = Duration::from_millis(500);

/// Budget for a clean `mysqladmin shutdown` before RAMPP falls back to closing
/// the Job Object. A loopback shutdown normally completes well under a second.
pub const MYSQL_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
