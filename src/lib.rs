// Library facade exposing internal modules for integration tests.
// Production binary entry point is src/main.rs.
// Dead-code warnings are expected: functions are called from main.rs, not the lib.
#![allow(dead_code)]

pub mod apache_conf;
pub mod config;
pub mod events;
pub mod health;
pub mod logger;
pub mod paths;
pub mod process;
pub mod provision;
pub mod reducer;
pub mod state;

// Internal modules only needed to satisfy transitive dependencies of the above.
pub mod executor;
// pub: Layer 3 system tests drive the MySQL lifecycle (initialize_mysql,
// graceful_shutdown) directly against a real mysqld.
pub mod mysql_conf;
// pub: Layer 3 system tests call ensure_php_dirs to mirror main.rs's real
// startup provisioning sequence (logs dir, logs/phpmyadmin) before spawning.
pub mod php_conf;
pub mod phpmyadmin_conf;
mod tray;
mod ui;
