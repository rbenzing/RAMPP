use crate::paths::{validate_critical_path, InstallPaths};
use crate::state::{ApacheConfig, MysqlConfig, PhpConfig, RampConfig};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// On-disk TOML representation (user-editable).
#[derive(Debug, Serialize, Deserialize)]
struct TomlRoot {
    install_dir: PathBuf,
    apache: TomlApache,
    mysql: TomlMysql,
    #[serde(default)]
    php: TomlPhp,
    #[serde(default)]
    phpmyadmin: TomlPhpMyAdmin,
}

#[derive(Debug, Serialize, Deserialize)]
struct TomlApache {
    port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    document_root: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TomlMysql {
    port: u16,
}

#[derive(Debug, Serialize, Deserialize)]
struct TomlPhp {
    port: u16,
}

impl Default for TomlPhp {
    fn default() -> Self {
        Self { port: 9000 }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TomlPhpMyAdmin {
    #[serde(default = "default_mysql_user")]
    mysql_user: String,
    #[serde(default)]
    mysql_password: String,
}

fn default_mysql_user() -> String {
    "root".to_string()
}

impl Default for TomlPhpMyAdmin {
    fn default() -> Self {
        Self {
            mysql_user: default_mysql_user(),
            mysql_password: String::new(),
        }
    }
}

/// Load and validate rampp.toml from install_dir.
pub fn load_config(install_dir: &Path) -> Result<RampConfig, String> {
    let paths = InstallPaths::from_install_dir(install_dir)?;

    // Reject rampp.toml if it is a symlink — it could redirect config reads/writes
    // to a system file, enabling privilege escalation or config escapes.
    validate_critical_path(&paths.config, install_dir, false)
        .map_err(|e| format!("rampp.toml path rejected: {e}"))?;

    let raw = std::fs::read_to_string(&paths.config)
        .map_err(|e| format!("cannot read rampp.toml: {e}"))?;
    let doc: TomlRoot = toml::from_str(&raw).map_err(|e| format!("rampp.toml parse error: {e}"))?;
    validate_and_build(doc, install_dir)
}

/// Write a default rampp.toml if none exists. Does not overwrite.
pub fn write_default_config(install_dir: &Path) -> Result<(), String> {
    let paths = InstallPaths::from_install_dir(install_dir)?;
    if paths.config.exists() {
        return Ok(());
    }
    let default = format!(
        r#"install_dir = "{}"

[apache]
port = 8080

[mysql]
port = 3306

[php]
port = 9000
"#,
        install_dir.display().to_string().replace('\\', "\\\\")
    );
    atomic_write(&paths.config, default.as_bytes())
}

/// Serialize the current config back to rampp.toml (atomic write). Preserves all
/// known fields (ports, phpMyAdmin credentials) and writes the document_root.
pub fn write_config(cfg: &RampConfig) -> Result<(), String> {
    let paths = InstallPaths::from_install_dir(&cfg.install_dir)?;
    let doc = TomlRoot {
        install_dir: cfg.install_dir.clone(),
        apache: TomlApache {
            port: cfg.apache.port,
            document_root: Some(cfg.apache.document_root.clone()),
        },
        mysql: TomlMysql {
            port: cfg.mysql.port,
        },
        php: TomlPhp { port: cfg.php.port },
        phpmyadmin: TomlPhpMyAdmin {
            mysql_user: cfg.phpmyadmin.mysql_user.clone(),
            mysql_password: cfg.phpmyadmin.mysql_password.clone(),
        },
    };
    let serialized =
        toml::to_string_pretty(&doc).map_err(|e| format!("serialize config failed: {e}"))?;
    atomic_write(&paths.config, serialized.as_bytes())
}

fn validate_and_build(doc: TomlRoot, install_dir: &Path) -> Result<RampConfig, String> {
    let paths = InstallPaths::from_install_dir(install_dir)?;

    // Ports must be unprivileged (>= 1024) and non-zero.
    // Privileged ports require admin rights on Windows and would cause silent
    // startup failures; port 0 is invalid for any bound service.
    fn validate_port(name: &str, port: u16) -> Result<(), String> {
        if port < 1024 {
            return Err(format!(
                "invalid {name}.port {port}: must be >= 1024 (privileged ports are not allowed)"
            ));
        }
        Ok(())
    }
    validate_port("apache", doc.apache.port)?;
    validate_port("mysql", doc.mysql.port)?;
    validate_port("php", doc.php.port)?;
    if doc.apache.port == doc.mysql.port {
        return Err("apache.port and mysql.port must be different".into());
    }
    if doc.apache.port == doc.php.port {
        return Err("apache.port and php.port must be different".into());
    }
    if doc.mysql.port == doc.php.port {
        return Err("mysql.port and php.port must be different".into());
    }

    let document_root = match &doc.apache.document_root {
        Some(p) => {
            crate::paths::validate_document_root(p)
                .map_err(|e| format!("invalid apache.document_root: {e}"))?;
            p.clone()
        }
        None => install_dir.join("apache").join("htdocs"),
    };

    Ok(RampConfig {
        install_dir: install_dir.to_path_buf(),
        apache: ApacheConfig {
            port: doc.apache.port,
            bin: paths.apache_bin,
            conf: paths.apache_conf,
            document_root,
        },
        mysql: MysqlConfig {
            port: doc.mysql.port,
            bin: paths.mysql_bin,
            data_dir: paths.mysql_data,
            ini: paths.mysql_ini,
        },
        php: PhpConfig {
            port: doc.php.port,
            bin: paths.php_bin,
            ini: paths.php_ini,
        },
        phpmyadmin: crate::state::PhpMyAdminConfig {
            mysql_user: doc.phpmyadmin.mysql_user,
            mysql_password: doc.phpmyadmin.mysql_password,
        },
    })
}

/// Rename can transiently fail on Windows when an antivirus scanner or the search
/// indexer holds the destination open. Retry briefly rather than surfacing a
/// spurious config-write failure.
const RENAME_ATTEMPTS: usize = 3;
const RENAME_BACKOFF: std::time::Duration = std::time::Duration::from_millis(50);

/// Monotonic counter used to make each `atomic_write` scratch file unique. Combined
/// with the process id this keeps two concurrent writers (even on the same
/// destination path) from sharing a temp file.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Atomic write: temp file → fsync → rename. Never corrupts the target.
///
/// The temp path APPENDS `.tmp` rather than replacing the extension. Replacing it
/// made `rampp.toml` and `rampp.state` both resolve to `rampp.tmp`, so the two
/// writers shared a scratch file.
///
/// The scratch name is further made unique per call (process id + a monotonic
/// counter) rather than fixed per destination. A fixed `{file_name}.tmp` name
/// still let two concurrent writers to the *same* destination collide on one
/// scratch file — the second writer's rename would find it already consumed by
/// the first. Production never calls this concurrently for the same path (every
/// `provision::reconcile` call runs on the single executor event-loop thread), but
/// the primitive itself should not corrupt data if called concurrently, and the
/// Layer 3 test suite does call it that way.
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<(), String> {
    let dir = path.parent().ok_or("path has no parent")?;
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("cannot create dir {}: {e}", dir.display()))?;

    let file_name = path
        .file_name()
        .ok_or("path has no file name")?
        .to_string_lossy()
        .to_string();
    let pid = std::process::id();
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!("{file_name}.{pid}.{seq}.tmp"));

    let write_result = (|| -> Result<(), String> {
        let mut f =
            std::fs::File::create(&tmp).map_err(|e| format!("cannot create temp file: {e}"))?;
        f.write_all(data)
            .map_err(|e| format!("write failed: {e}"))?;
        f.flush().map_err(|e| format!("flush failed: {e}"))?;
        f.sync_all().map_err(|e| format!("fsync failed: {e}"))?;
        Ok(())
    })();

    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    let mut last_err = String::new();
    for attempt in 0..RENAME_ATTEMPTS {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = format!("atomic rename failed: {e}");
                if attempt + 1 < RENAME_ATTEMPTS {
                    std::thread::sleep(RENAME_BACKOFF);
                }
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Err(last_err)
}

/// Read `rampp.state`, falling back to the all-stopped default when the file is
/// absent or unparsable. Never fails — a missing state file is a normal first run.
pub fn read_persisted_state(path: &Path) -> crate::state::PersistedState {
    std::fs::read(path)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_else(crate::state::PersistedState::default_stopped)
}

/// Persist `rampp.state` atomically.
pub fn write_persisted_state(
    path: &Path,
    state: &crate::state::PersistedState,
) -> Result<(), String> {
    let data =
        serde_json::to_vec_pretty(state).map_err(|e| format!("serialize state failed: {e}"))?;
    atomic_write(path, &data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_toml(dir: &Path, content: &str) {
        std::fs::write(dir.join("rampp.toml"), content).unwrap();
    }

    #[test]
    fn load_valid_config() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        write_toml(
            dir,
            &format!(
                r#"install_dir = "{}"
[apache]
port = 8080
[mysql]
port = 3306
[php]
port = 9000
"#,
                dir.display().to_string().replace('\\', "\\\\")
            ),
        );
        let cfg = load_config(dir).unwrap();
        assert_eq!(cfg.apache.port, 8080);
        assert_eq!(cfg.mysql.port, 3306);
        assert_eq!(cfg.php.port, 9000);
    }

    #[test]
    fn load_config_defaults_php_port_when_absent() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        write_toml(
            dir,
            &format!(
                r#"install_dir = "{}"
[apache]
port = 8080
[mysql]
port = 3306
"#,
                dir.display().to_string().replace('\\', "\\\\")
            ),
        );
        let cfg = load_config(dir).unwrap();
        assert_eq!(cfg.php.port, 9000);
    }

    #[test]
    fn rejects_duplicate_ports() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        write_toml(
            dir,
            &format!(
                r#"install_dir = "{}"
[apache]
port = 8080
[mysql]
port = 8080
"#,
                dir.display().to_string().replace('\\', "\\\\")
            ),
        );
        assert!(load_config(dir).is_err());
    }

    #[test]
    fn rejects_privileged_port() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        write_toml(
            dir,
            &format!(
                r#"install_dir = "{}"
[apache]
port = 80
[mysql]
port = 3306
[php]
port = 9000
"#,
                dir.display().to_string().replace('\\', "\\\\")
            ),
        );
        let err = load_config(dir).unwrap_err();
        assert!(
            err.contains("1024"),
            "expected port>=1024 message, got: {err}"
        );
    }

    #[test]
    fn rejects_apache_php_port_clash() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        write_toml(
            dir,
            &format!(
                r#"install_dir = "{}"
[apache]
port = 9000
[mysql]
port = 3306
[php]
port = 9000
"#,
                dir.display().to_string().replace('\\', "\\\\")
            ),
        );
        assert!(load_config(dir).is_err());
    }

    #[test]
    fn atomic_write_round_trip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.toml");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        // Second write replaces atomically
        atomic_write(&path, b"world").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"world");
        // No .tmp file left behind
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn write_default_does_not_overwrite_existing() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("rampp.toml"), b"original").unwrap();
        write_default_config(dir).unwrap();
        assert_eq!(std::fs::read(dir.join("rampp.toml")).unwrap(), b"original");
    }

    #[test]
    fn rejects_malformed_toml() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("rampp.toml"), b"[not valid toml @@@").unwrap();
        let err = load_config(dir).unwrap_err();
        assert!(
            err.contains("parse error") || err.contains("TOML") || err.contains("toml"),
            "expected parse error message, got: {err}"
        );
    }

    #[test]
    fn rejects_missing_ramp_toml() {
        let tmp = TempDir::new().unwrap();
        let err = load_config(tmp.path()).unwrap_err();
        assert!(
            err.contains("cannot read") || err.contains("rampp.toml"),
            "expected missing file message, got: {err}"
        );
    }

    #[test]
    fn rejects_mysql_php_port_clash() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        write_toml(
            dir,
            &format!(
                r#"install_dir = "{}"
[apache]
port = 8080
[mysql]
port = 9000
[php]
port = 9000
"#,
                dir.display().to_string().replace('\\', "\\\\")
            ),
        );
        let err = load_config(dir).unwrap_err();
        assert!(
            err.contains("mysql") && err.contains("php"),
            "expected mysql/php clash message, got: {err}"
        );
    }

    #[test]
    fn rejects_privileged_port_mysql() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        write_toml(
            dir,
            &format!(
                r#"install_dir = "{}"
[apache]
port = 8080
[mysql]
port = 1023
[php]
port = 9000
"#,
                dir.display().to_string().replace('\\', "\\\\")
            ),
        );
        let err = load_config(dir).unwrap_err();
        assert!(
            err.contains("1024"),
            "expected port>=1024 message, got: {err}"
        );
    }

    #[test]
    fn atomic_write_no_tmp_left_on_success() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("out.toml");
        atomic_write(&path, b"data").unwrap();
        assert!(path.exists());
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn persisted_state_defaults_phpmyadmin_enabled_to_false() {
        let json =
            r#"{"apache_desired":"Stopped","mysql_desired":"Stopped","php_desired":"Stopped"}"#;
        let persisted: crate::state::PersistedState = serde_json::from_str(json).unwrap();
        assert!(!persisted.phpmyadmin_enabled);
        assert!(persisted.phpmyadmin_blowfish_secret.is_none());
    }

    #[test]
    fn write_default_config_creates_parseable_toml() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        write_default_config(dir).unwrap();
        // The generated default must be loadable — no syntax errors
        let cfg = load_config(dir).unwrap();
        assert!(cfg.apache.port >= 1024);
        assert!(cfg.mysql.port >= 1024);
        assert!(cfg.php.port >= 1024);
    }

    #[test]
    fn load_config_defaults_phpmyadmin_when_section_absent() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        write_toml(
            dir,
            &format!(
                r#"install_dir = "{}"
[apache]
port = 8080
[mysql]
port = 3306
[php]
port = 9000
"#,
                dir.display().to_string().replace('\\', "\\\\")
            ),
        );
        let cfg = load_config(dir).unwrap();
        assert_eq!(cfg.phpmyadmin.mysql_user, "root");
        assert_eq!(cfg.phpmyadmin.mysql_password, "");
    }

    #[test]
    fn load_config_reads_phpmyadmin_credentials() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        write_toml(
            dir,
            &format!(
                r#"install_dir = "{}"
[apache]
port = 8080
[mysql]
port = 3306
[php]
port = 9000
[phpmyadmin]
mysql_user = "admin"
mysql_password = "secret"
"#,
                dir.display().to_string().replace('\\', "\\\\")
            ),
        );
        let cfg = load_config(dir).unwrap();
        assert_eq!(cfg.phpmyadmin.mysql_user, "admin");
        assert_eq!(cfg.phpmyadmin.mysql_password, "secret");
    }

    #[test]
    fn document_root_defaults_to_htdocs_when_absent() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        write_toml(
            dir,
            &format!(
                r#"install_dir = "{}"
[apache]
port = 8080
[mysql]
port = 3306
[php]
port = 9000
"#,
                dir.display().to_string().replace('\\', "\\\\")
            ),
        );
        let cfg = load_config(dir).unwrap();
        assert_eq!(cfg.apache.document_root, dir.join("apache").join("htdocs"));
    }

    #[test]
    fn document_root_reads_custom_value() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let custom = dir.join("my_site");
        std::fs::create_dir(&custom).unwrap();
        write_toml(
            dir,
            &format!(
                r#"install_dir = "{}"
[apache]
port = 8080
document_root = "{}"
[mysql]
port = 3306
[php]
port = 9000
"#,
                dir.display().to_string().replace('\\', "\\\\"),
                custom.display().to_string().replace('\\', "\\\\")
            ),
        );
        let cfg = load_config(dir).unwrap();
        assert_eq!(cfg.apache.document_root, custom);
    }

    #[test]
    fn rejects_document_root_that_is_not_a_directory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let missing = dir.join("nope");
        write_toml(
            dir,
            &format!(
                r#"install_dir = "{}"
[apache]
port = 8080
document_root = "{}"
[mysql]
port = 3306
[php]
port = 9000
"#,
                dir.display().to_string().replace('\\', "\\\\"),
                missing.display().to_string().replace('\\', "\\\\")
            ),
        );
        assert!(load_config(dir).is_err());
    }

    #[test]
    fn write_config_round_trips_document_root() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let custom = dir.join("site2");
        std::fs::create_dir(&custom).unwrap();
        write_default_config(dir).unwrap();
        let mut cfg = load_config(dir).unwrap();
        cfg.apache.document_root = custom.clone();
        write_config(&cfg).unwrap();
        let reloaded = load_config(dir).unwrap();
        assert_eq!(reloaded.apache.document_root, custom);
        assert_eq!(reloaded.apache.port, cfg.apache.port);
        assert_eq!(reloaded.php.port, cfg.php.port);
    }

    #[test]
    fn atomic_write_temp_path_does_not_collide_across_extensions() {
        let tmp = TempDir::new().unwrap();
        let toml_path = tmp.path().join("rampp.toml");
        let state_path = tmp.path().join("rampp.state");
        atomic_write(&toml_path, b"toml-content").unwrap();
        atomic_write(&state_path, b"state-content").unwrap();
        // Before the fix both resolved to "rampp.tmp".
        assert_eq!(std::fs::read(&toml_path).unwrap(), b"toml-content");
        assert_eq!(std::fs::read(&state_path).unwrap(), b"state-content");
    }

    #[test]
    fn atomic_write_leaves_no_temp_file_behind() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("rampp.toml");
        atomic_write(&path, b"x").unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn atomic_write_concurrent_writers_same_destination_do_not_corrupt() {
        use std::sync::Arc;
        use std::thread;

        let tmp = TempDir::new().unwrap();
        let path = Arc::new(tmp.path().join("shared.toml"));

        const THREADS: usize = 8;
        // Distinct, same-length payloads so a truncated/interleaved result is
        // detectable but no payload is a prefix/suffix of another.
        let payloads: Vec<Vec<u8>> = (0..THREADS)
            .map(|i| format!("payload-{i}-{}", "x".repeat(64)).into_bytes())
            .collect();

        let handles: Vec<_> = payloads
            .iter()
            .cloned()
            .map(|data| {
                let path = Arc::clone(&path);
                thread::spawn(move || atomic_write(&path, &data))
            })
            .collect();

        for h in handles {
            assert!(h.join().unwrap().is_ok(), "concurrent atomic_write failed");
        }

        let final_contents = std::fs::read(path.as_path()).unwrap();
        assert!(
            payloads.iter().any(|p| p == &final_contents),
            "destination content did not match any single writer's payload intact: {:?}",
            String::from_utf8_lossy(&final_contents)
        );

        let dir = path.parent().unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn read_persisted_state_defaults_when_file_absent() {
        let tmp = TempDir::new().unwrap();
        let p = read_persisted_state(&tmp.path().join("nope.state"));
        assert_eq!(p.apache_desired, crate::state::DesiredServiceState::Stopped);
        assert!(p.phpmyadmin_blowfish_secret.is_none());
    }

    #[test]
    fn read_persisted_state_defaults_when_file_is_garbage() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("rampp.state");
        std::fs::write(&path, b"not json at all").unwrap();
        let p = read_persisted_state(&path);
        assert!(p.phpmyadmin_blowfish_secret.is_none());
    }

    #[test]
    fn write_then_read_persisted_state_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("rampp.state");
        let mut original = crate::state::PersistedState::default_stopped();
        original.phpmyadmin_blowfish_secret = Some("abc123".to_string());
        original.phpmyadmin_enabled = true;
        write_persisted_state(&path, &original).unwrap();
        let back = read_persisted_state(&path);
        assert_eq!(back.phpmyadmin_blowfish_secret.as_deref(), Some("abc123"));
        assert!(back.phpmyadmin_enabled);
    }
}
