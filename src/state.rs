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
    /// Port the service is actually bound to. May differ from the configured port when
    /// the configured one was in use and the executor scanned upward for a free one.
    /// None until the first successful spawn. Not persisted.
    pub effective_port: Option<u16>,
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
            effective_port: None,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortState {
    pub apache_bound: bool,
    pub mysql_bound: bool,
    pub php_bound: bool,
}

impl Default for PortState {
    fn default() -> Self {
        Self::new()
    }
}

impl PortState {
    pub fn new() -> Self {
        Self {
            apache_bound: false,
            mysql_bound: false,
            php_bound: false,
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
            ports: PortState::new(),
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

pub const APACHE_READY_TIMEOUT: Duration = Duration::from_secs(3);
pub const MYSQL_READY_TIMEOUT: Duration = Duration::from_secs(5);
pub const PHP_READY_TIMEOUT: Duration = Duration::from_secs(5);
pub const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(2);
pub const HEALTH_FAIL_THRESHOLD: u32 = 3;
#[allow(dead_code)]
pub const SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(5);
pub const COMMAND_DEBOUNCE: Duration = Duration::from_millis(500);
