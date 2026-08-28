use crate::state::RampConfig;

/// Generate a minimal my.ini for MySQL 9.x compatible with RAMPP's layout.
/// Only called when the file does not already exist.
pub fn generate_my_ini(cfg: &RampConfig) -> String {
    generate_my_ini_with_port(cfg, cfg.mysql.port)
}

/// Same as `generate_my_ini` but with an explicit port — used by the executor
/// when the configured port was occupied and a different one was chosen.
pub fn generate_my_ini_with_port(cfg: &RampConfig, port: u16) -> String {
    let mysql_dir_path = cfg.install_dir.join("mysql");
    let mysql_dir = mysql_dir_path.display().to_string().replace('\\', "/");
    let data_dir = cfg.mysql.data_dir.display().to_string().replace('\\', "/");
    let logs_dir = cfg
        .install_dir
        .join("logs")
        .display()
        .to_string()
        .replace('\\', "/");

    format!(
        r#"# RAMPP — generated my.ini
[mysqld]
basedir     = "{mysql_dir}"
datadir     = "{data_dir}"
port        = {port}
bind-address = 127.0.0.1
# Loopback only — reverse DNS on every connection is pure latency and can stall.
skip-name-resolve

# Character set
character-set-server  = utf8mb4
collation-server      = utf8mb4_unicode_ci

# Logging
log_error = "{logs_dir}/mysql_error.log"
general_log = 0

# InnoDB
innodb_buffer_pool_size = 128M
innodb_flush_log_at_trx_commit = 1

# Disable strict mode for local dev convenience
sql_mode = ""

[client]
port        = {port}
default-character-set = utf8mb4
"#
    )
}

/// Force-rewrite my.ini with an explicit port override. Used when the executor
/// has resolved MySQL to a different port than the configured one.
pub fn rewrite_my_ini_with_port(cfg: &RampConfig, port: u16) -> Result<(), String> {
    let ini_path = &cfg.mysql.ini;
    let dir = ini_path.parent().ok_or("my.ini has no parent dir")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create mysql dir: {e}"))?;
    let content = generate_my_ini_with_port(cfg, port);
    crate::config::atomic_write(ini_path, content.as_bytes())
        .map_err(|e| format!("cannot rewrite my.ini: {e}"))
}

/// Write my.ini only if it doesn't already exist.
pub fn ensure_my_ini(cfg: &RampConfig) -> Result<(), String> {
    let ini_path = &cfg.mysql.ini;
    if ini_path.exists() {
        return Ok(());
    }
    let dir = ini_path.parent().ok_or("my.ini has no parent dir")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create mysql dir: {e}"))?;
    let content = generate_my_ini(cfg);
    crate::config::atomic_write(ini_path, content.as_bytes())
        .map_err(|e| format!("cannot write my.ini: {e}"))
}

/// Run `mysqld --initialize-insecure` to set up a fresh data directory.
/// Blocks until completion. Returns Err if the process exits non-zero.
pub fn initialize_mysql(cfg: &RampConfig) -> Result<(), String> {
    log::info!("MySQL: initializing data directory (first run)…");

    let mysql_logs = cfg.install_dir.join("logs");
    std::fs::create_dir_all(&mysql_logs).map_err(|e| format!("cannot create logs: {e}"))?;

    // mysqld refuses --initialize-insecure if the data dir is non-empty.
    // Clear any leftovers from a prior failed init so this run can succeed.
    let data_dir = &cfg.mysql.data_dir;
    if data_dir.exists() {
        let entries = std::fs::read_dir(data_dir)
            .map_err(|e| format!("cannot read data dir {}: {e}", data_dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let result = if path.is_dir() {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
            if let Err(e) = result {
                return Err(format!(
                    "cannot clear stale data dir entry {}: {e}",
                    path.display()
                ));
            }
        }
    }

    let bin = &cfg.mysql.bin;
    let ini = &cfg.mysql.ini;
    let work_dir = bin
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(&cfg.install_dir);

    let output = std::process::Command::new(bin)
        .arg(format!("--defaults-file={}", ini.display()))
        .arg("--initialize-insecure")
        .arg("--console")
        .current_dir(work_dir)
        .env_clear()
        .env("SystemRoot", crate::paths::system_root())
        .output()
        .map_err(|e| format!("failed to run mysqld --initialize-insecure: {e}"))?;

    let stderr = String::from_utf8_lossy(&output.stderr);
    log::info!("MySQL init output:\n{stderr}");

    if !output.status.success() {
        return Err(format!(
            "mysqld --initialize-insecure failed (exit {:?}):\n{stderr}",
            output.status.code()
        ));
    }

    log::info!("MySQL: data directory initialized successfully");
    Ok(())
}

/// Returns true if the data directory looks uninitialized (empty or missing).
pub fn needs_initialization(cfg: &RampConfig) -> bool {
    let data_dir = &cfg.mysql.data_dir;
    if !data_dir.exists() {
        return true;
    }
    // MySQL init creates several files including ibdata1 and mysql/ subdirectory.
    // If neither exists, the directory is uninitialized.
    !data_dir.join("ibdata1").exists() && !data_dir.join("mysql").exists()
}

/// Ask mysqld to shut down cleanly so InnoDB does not perform crash recovery on
/// the next start.
///
/// Best-effort only. The caller MUST still close the Job Object afterwards — that
/// remains the termination guarantee and the no-orphan invariant.
///
/// Not yet called from `Executor::do_kill` — that wiring is a later task, so this
/// function is presently unused by `src/main.rs`'s standalone binary crate (which
/// does not inherit `src/lib.rs`'s crate-wide `#![allow(dead_code)]`).
#[allow(dead_code)]
pub fn graceful_shutdown(
    cfg: &RampConfig,
    port: u16,
    grace: std::time::Duration,
) -> Result<(), String> {
    let paths = crate::paths::InstallPaths::from_install_dir(&cfg.install_dir)?;
    let bin = paths.mysqladmin_bin;
    if !bin.exists() {
        return Err(format!("mysqladmin not found at {}", bin.display()));
    }

    let mut cmd = std::process::Command::new(&bin);
    cmd.arg("--protocol=TCP")
        .arg("--host=127.0.0.1")
        .arg(format!("--port={port}"))
        .arg(format!("--user={}", cfg.phpmyadmin.mysql_user))
        .arg("--connect-timeout=2")
        .arg("shutdown")
        .current_dir(&cfg.install_dir)
        .env_clear()
        .env("SystemRoot", crate::paths::system_root())
        // Password via env, never argv — argv is visible in the process list.
        .env("MYSQL_PWD", &cfg.phpmyadmin.mysql_password);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot run mysqladmin shutdown: {e}"))?;

    let deadline = std::time::Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(format!("mysqladmin shutdown exited {:?}", status.code()))
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    return Err(format!("mysqladmin shutdown timed out after {grace:?}"));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(format!("waiting on mysqladmin failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ApacheConfig, MysqlConfig, PhpConfig, PhpMyAdminConfig, RampConfig};
    use std::path::Path;
    use tempfile::TempDir;

    fn test_cfg(dir: &Path) -> RampConfig {
        RampConfig {
            install_dir: dir.to_path_buf(),
            apache: ApacheConfig {
                port: 80,
                bin: dir.join("apache").join("bin").join("httpd.exe"),
                conf: dir.join("apache").join("conf").join("httpd.conf"),
                document_root: dir.join("apache").join("htdocs"),
            },
            mysql: MysqlConfig {
                port: 3306,
                bin: dir.join("mysql").join("bin").join("mysqld.exe"),
                data_dir: dir.join("mysql").join("data"),
                ini: dir.join("mysql").join("my.ini"),
            },
            php: PhpConfig {
                port: 9000,
                bin: dir.join("php").join("php-cgi.exe"),
                ini: dir.join("php").join("php.ini"),
            },
            phpmyadmin: PhpMyAdminConfig {
                mysql_user: "root".to_string(),
                mysql_password: String::new(),
            },
        }
    }

    #[test]
    fn generates_ini_with_correct_port() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let ini = generate_my_ini(&cfg);
        assert!(ini.contains("port        = 3306"));
        assert!(ini.contains("bind-address = 127.0.0.1"));
    }

    #[test]
    fn ensure_my_ini_creates_file() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        ensure_my_ini(&cfg).unwrap();
        assert!(cfg.mysql.ini.exists());
    }

    #[test]
    fn ensure_my_ini_does_not_overwrite() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        std::fs::create_dir_all(cfg.mysql.ini.parent().unwrap()).unwrap();
        std::fs::write(&cfg.mysql.ini, b"custom").unwrap();
        ensure_my_ini(&cfg).unwrap();
        assert_eq!(std::fs::read(&cfg.mysql.ini).unwrap(), b"custom");
    }

    #[test]
    fn needs_initialization_true_when_missing() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        assert!(needs_initialization(&cfg));
    }

    #[test]
    fn needs_initialization_false_when_ibdata1_exists() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        std::fs::create_dir_all(&cfg.mysql.data_dir).unwrap();
        std::fs::write(cfg.mysql.data_dir.join("ibdata1"), b"").unwrap();
        assert!(!needs_initialization(&cfg));
    }

    #[test]
    fn my_ini_skips_name_resolution() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let ini = generate_my_ini(&cfg);
        assert!(
            ini.contains("skip-name-resolve"),
            "RAMPP binds loopback only; reverse DNS is pure latency and a stall source"
        );
    }

    #[test]
    fn graceful_shutdown_errors_when_mysqladmin_is_missing() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let err = graceful_shutdown(&cfg, 3306, std::time::Duration::from_millis(200))
            .expect_err("must not panic or succeed without the binary");
        assert!(
            err.contains("mysqladmin"),
            "error should name the tool: {err}"
        );
    }
}
