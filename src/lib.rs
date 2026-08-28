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
mod executor;
mod mysql_conf;
mod php_conf;
pub mod phpmyadmin_conf;
mod tray;
mod ui;
