use crate::state::RampConfig;

/// Escape a string for embedding inside a MySQL single-quoted string literal.
/// Only `\` and `'` are special under the default (non-ANSI_QUOTES,
/// non-NO_BACKSLASH_ESCAPES) `sql_mode` this crate generates — see
/// `generate_my_ini_with_port`'s `sql_mode = ""`. Order matters: backslash first.
fn sql_single_quoted(s: &str) -> String {
    s.replace('\\', r"\\").replace('\'', r"\'")
}

/// Generate a minimal my.ini for MySQL 9.x compatible with RAMPP's layout,
/// rendered from the reducer's port ledger — the executor's `provision`
/// reconciler is the only caller in production; the port is always explicit.
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

    // Empirically required (Layer 3, tests/system_stack.rs): `--initialize-insecure`
    // only ever creates `root@localhost` with an empty password — it ignores
    // `cfg.phpmyadmin.mysql_password` entirely, and `localhost` is a distinct grant
    // from any IP. Combined with `skip-name-resolve` in the my.ini above (which
    // stops the reverse-DNS lookup that could otherwise resolve the connecting
    // 127.0.0.1 back to the literal hostname "localhost"), NO TCP client — not
    // `probe_mysql`'s handshake read, not `mysqladmin`, not phpMyAdmin — can even
    // complete a connection: mysqld rejects it pre-authentication with
    // `Host '127.0.0.1' is not allowed to connect to this MySQL server`, because
    // no grant exists for that host at all. Every RAMPP-initiated connection is
    // TCP to 127.0.0.1 (the security constraint is loopback-only), so this
    // bootstrap file grants `cfg.phpmyadmin.mysql_user`@'127.0.0.1' with
    // `cfg.phpmyadmin.mysql_password` — the credentials RAMPP actually uses —
    // full privileges (but not the ability to grant privileges to *other*
    // accounts: phpMyAdmin never needs `WITH GRANT OPTION`, and granting it
    // unconditionally would silently hand DBA-with-grant-option to a scoped,
    // non-root `phpmyadmin.mysql_user` a security-conscious user configured).
    // `--init-file` is honored during `--initialize-insecure` itself (confirmed
    // empirically), so this needs no separate bootstrap server.
    let bootstrap_sql = cfg.install_dir.join("mysql").join(".rampp-bootstrap.sql");
    let user = sql_single_quoted(&cfg.phpmyadmin.mysql_user);
    let password = sql_single_quoted(&cfg.phpmyadmin.mysql_password);
    std::fs::write(
        &bootstrap_sql,
        format!(
            "CREATE USER IF NOT EXISTS '{user}'@'127.0.0.1' IDENTIFIED BY '{password}';\n\
             ALTER USER '{user}'@'127.0.0.1' IDENTIFIED BY '{password}';\n\
             GRANT ALL PRIVILEGES ON *.* TO '{user}'@'127.0.0.1';\n\
             FLUSH PRIVILEGES;\n"
        ),
    )
    .map_err(|e| format!("cannot write MySQL bootstrap init-file: {e}"))?;

    // Empirically required (Layer 3, tests/system_stack.rs): with only SystemRoot
    // set, real mysqld 9.7.0 aborts initialization with
    // `mysqld: Can't get stat of '' (OS errno 2 - No such file or directory)`
    // and refuses to initialize the data directory at all. mysqld resolves its
    // temp path from the TEMP/TMP environment variables directly rather than
    // falling back to a Windows API default, so env_clear() silently breaks
    // --initialize-insecure on every machine, regardless of what the parent
    // rampp.exe process's own environment contains. `process::spawn_service`
    // already sets TEMP for the long-running mysqld; --initialize-insecure
    // needs the same for its own one-shot run.
    let temp = crate::paths::temp_dir();
    let output = std::process::Command::new(bin)
        .arg(format!("--defaults-file={}", ini.display()))
        .arg("--initialize-insecure")
        .arg(format!("--init-file={}", bootstrap_sql.display()))
        .arg("--console")
        .current_dir(work_dir)
        .env_clear()
        .env("SystemRoot", crate::paths::system_root())
        .env("TEMP", &temp)
        .env("TMP", &temp)
        .output()
        .map_err(|e| format!("failed to run mysqld --initialize-insecure: {e}"))?;

    // Best-effort cleanup: the grant is now durably persisted in the data
    // directory's own system tables (InnoDB, innodb_flush_log_at_trx_commit=1),
    // so the file on disk was only ever needed for this one invocation. It is
    // not a "managed file" the reconciler tracks, and it may contain a
    // plaintext password, so remove it regardless of whether init succeeded.
    // Not crash-safe by construction — a kill or panic between the write above
    // and this delete leaves the file behind indefinitely (rampp.toml and
    // config.inc.php already hold this same password permanently, so this is
    // not a new class of exposure, just a longer-than-intended window for one
    // more copy of it). Logging on failure is what makes a leftover file
    // discoverable instead of silently forgotten.
    if let Err(e) = std::fs::remove_file(&bootstrap_sql) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log::warn!(
                "could not remove MySQL bootstrap init-file {} (contains a plaintext password): {e}",
                bootstrap_sql.display()
            );
        }
    }

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
        let ini = generate_my_ini_with_port(&cfg, cfg.mysql.port);
        assert!(ini.contains("port        = 3306"));
        assert!(ini.contains("bind-address = 127.0.0.1"));
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
        let ini = generate_my_ini_with_port(&cfg, cfg.mysql.port);
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
