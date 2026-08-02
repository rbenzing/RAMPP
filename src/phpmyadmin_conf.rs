use crate::state::RampConfig;

pub fn generate_phpmyadmin_apache_conf(install_dir: &std::path::Path, php_port: u16) -> String {
    let pma_dir = install_dir.join("phpmyadmin");
    let pma_dir_s = pma_dir.display().to_string().replace('\\', "/");

    format!(
        r#"# RAMPP — phpMyAdmin enabled (do not remove this line)
#
# Alias maps /phpmyadmin/ → the phpMyAdmin install directory. Apache's normal
# URI-to-filename translation produces an absolute Windows path (C:/.../index.php).
# The <FilesMatch> block uses the same "//./" SetHandler trick + GENERIC backend
# as httpd.conf — required on Windows to dodge the mod_proxy_fcgi URL bug that
# would otherwise produce a malformed "//host:portC:/path..." URL.
# See the long comment in httpd.conf for the full explanation.
Alias /phpmyadmin "{pma_dir_s}"
<Directory "{pma_dir_s}">
    Options None
    AllowOverride None
    Require local
    DirectoryIndex index.php

    <FilesMatch "\.php$">
        SetHandler "proxy:fcgi://127.0.0.1:{php_port}//./"
        ProxyFCGIBackendType GENERIC
    </FilesMatch>
</Directory>
"#
    )
}

pub fn generate_config_inc_php(
    install_dir: &std::path::Path,
    mysql_port: u16,
    mysql_user: &str,
    mysql_password: &str,
    blowfish_secret: &str,
) -> String {
    let temp_dir = install_dir
        .join("logs")
        .join("phpmyadmin")
        .display()
        .to_string()
        .replace('\\', "/");
    format!(
        r#"<?php
// RAMPP — generated config.inc.php (do not remove this line — RAMPP uses it to detect generated configs)
$cfg['blowfish_secret'] = '{blowfish_secret}';
$cfg['Servers'][1]['auth_type'] = 'config';
$cfg['Servers'][1]['host'] = '127.0.0.1';
$cfg['Servers'][1]['port'] = {mysql_port};
$cfg['Servers'][1]['user'] = '{mysql_user}';
$cfg['Servers'][1]['password'] = '{mysql_password}';
$cfg['Servers'][1]['AllowNoPassword'] = true;
$cfg['UploadDir'] = '';
$cfg['SaveDir'] = '';
$cfg['TempDir'] = '{temp_dir}';
"#
    )
}

pub fn is_ramp_owned_config(path: &std::path::Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    content.contains("RAMPP — generated config.inc.php")
}

pub fn generate_blowfish_secret(install_dir: &std::path::Path) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0xdeadbeef);
    let pid = std::process::id();
    let path_hash: u64 = install_dir
        .display()
        .to_string()
        .bytes()
        .enumerate()
        .fold(0u64, |acc, (i, b)| {
            acc.wrapping_add((b as u64).wrapping_mul(i as u64 + 1))
        });
    let combined = (nanos as u64).wrapping_mul(0x9e3779b97f4a7c15)
        ^ (pid as u64).wrapping_mul(0x6c62272e07bb0142)
        ^ path_hash;
    let half = format!("{combined:016x}");
    format!("{half}{half}").chars().take(32).collect()
}

pub fn write_phpmyadmin_apache_conf_enabled(cfg: &RampConfig, php_port: u16) -> Result<(), String> {
    let conf_path = cfg
        .install_dir
        .join("apache")
        .join("conf")
        .join("phpmyadmin.conf");
    let dir = conf_path.parent().ok_or("phpmyadmin.conf has no parent")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create apache/conf dir: {e}"))?;
    let content = generate_phpmyadmin_apache_conf(&cfg.install_dir, php_port);
    crate::config::atomic_write(&conf_path, content.as_bytes())
        .map_err(|e| format!("cannot write phpmyadmin.conf: {e}"))
}

pub fn write_phpmyadmin_apache_conf_disabled(cfg: &RampConfig) -> Result<(), String> {
    let conf_path = cfg
        .install_dir
        .join("apache")
        .join("conf")
        .join("phpmyadmin.conf");
    let dir = conf_path.parent().ok_or("phpmyadmin.conf has no parent")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create apache/conf dir: {e}"))?;
    crate::config::atomic_write(&conf_path, b"")
        .map_err(|e| format!("cannot write phpmyadmin.conf: {e}"))
}

#[allow(dead_code)]
pub fn ensure_phpmyadmin_apache_conf(
    cfg: &RampConfig,
    enabled: bool,
    php_port: u16,
) -> Result<(), String> {
    if enabled {
        write_phpmyadmin_apache_conf_enabled(cfg, php_port)
    } else {
        write_phpmyadmin_apache_conf_disabled(cfg)
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
                port: 8080,
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
    fn apache_conf_contains_alias() {
        let tmp = TempDir::new().unwrap();
        let conf = generate_phpmyadmin_apache_conf(tmp.path(), 9000);
        assert!(conf.contains("Alias /phpmyadmin"));
        assert!(conf.contains("phpmyadmin"));
    }

    #[test]
    fn apache_conf_uses_variant_a_workaround() {
        let tmp = TempDir::new().unwrap();
        let conf = generate_phpmyadmin_apache_conf(tmp.path(), 9000);
        // Variant A: SetHandler "proxy:fcgi://host:port//./" + ProxyFCGIBackendType
        // GENERIC inside the same FilesMatch block. Empirically verified fix for
        // the Windows drive-letter URL parsing bug in mod_proxy_fcgi 2.4.66.
        assert!(
            conf.contains("SetHandler \"proxy:fcgi://127.0.0.1:9000//./\""),
            "SetHandler must use //./ suffix to avoid Windows URL parse bug"
        );
        assert!(
            conf.contains("ProxyFCGIBackendType GENERIC"),
            "ProxyFCGIBackendType GENERIC required inside the FilesMatch block"
        );
    }

    #[test]
    fn apache_conf_requires_local() {
        let tmp = TempDir::new().unwrap();
        let conf = generate_phpmyadmin_apache_conf(tmp.path(), 9000);
        assert!(conf.contains("Require local"));
    }

    #[test]
    fn apache_conf_has_directory_index() {
        let tmp = TempDir::new().unwrap();
        let conf = generate_phpmyadmin_apache_conf(tmp.path(), 9000);
        assert!(
            conf.contains("DirectoryIndex index.php"),
            "must set DirectoryIndex so /phpmyadmin/ resolves to index.php"
        );
    }

    #[test]
    fn config_inc_php_contains_marker() {
        let tmp = TempDir::new().unwrap();
        let php = generate_config_inc_php(
            tmp.path(),
            3306,
            "root",
            "",
            "secret12345678901234567890123456",
        );
        assert!(php.contains("RAMPP — generated config.inc.php"));
    }

    #[test]
    fn config_inc_php_contains_credentials() {
        let tmp = TempDir::new().unwrap();
        let php = generate_config_inc_php(
            tmp.path(),
            3306,
            "admin",
            "pass123",
            "secret12345678901234567890123456",
        );
        assert!(php.contains("'admin'"));
        assert!(php.contains("'pass123'"));
        assert!(php.contains("3306"));
    }

    #[test]
    fn config_inc_php_contains_blowfish_secret() {
        let tmp = TempDir::new().unwrap();
        let secret = "abcdefghij1234567890abcdefghij12";
        let php = generate_config_inc_php(tmp.path(), 3306, "root", "", secret);
        assert!(php.contains(secret));
    }

    #[test]
    fn config_inc_php_sets_temp_dir_to_logs_phpmyadmin() {
        let tmp = TempDir::new().unwrap();
        let php = generate_config_inc_php(
            tmp.path(),
            3306,
            "root",
            "",
            "secret12345678901234567890123456",
        );
        assert!(
            php.contains("logs/phpmyadmin"),
            "TempDir must point to logs/phpmyadmin"
        );
        assert!(
            php.contains("$cfg['TempDir']"),
            "TempDir must be set in config.inc.php"
        );
    }

    #[test]
    fn is_ramp_owned_config_true_when_marker_present() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.inc.php");
        std::fs::write(&path, "<?php\n// RAMPP — generated config.inc.php (do not remove this line — RAMPP uses it to detect generated configs)\n").unwrap();
        assert!(is_ramp_owned_config(&path));
    }

    #[test]
    fn is_ramp_owned_config_false_when_marker_absent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.inc.php");
        std::fs::write(&path, "<?php\n// User config\n").unwrap();
        assert!(!is_ramp_owned_config(&path));
    }

    #[test]
    fn is_ramp_owned_config_false_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nonexistent.php");
        assert!(!is_ramp_owned_config(&path));
    }

    #[test]
    fn blowfish_secret_is_32_chars() {
        let tmp = TempDir::new().unwrap();
        let secret = generate_blowfish_secret(tmp.path());
        assert_eq!(secret.len(), 32, "blowfish_secret must be exactly 32 chars");
    }

    #[test]
    fn blowfish_secret_is_alphanumeric() {
        let tmp = TempDir::new().unwrap();
        let secret = generate_blowfish_secret(tmp.path());
        assert!(
            secret.chars().all(|c| c.is_ascii_hexdigit()),
            "blowfish_secret must be hex"
        );
    }

    #[test]
    fn write_enabled_creates_populated_conf() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        std::fs::create_dir_all(tmp.path().join("apache").join("conf")).unwrap();
        write_phpmyadmin_apache_conf_enabled(&cfg, 9000).unwrap();
        let content = std::fs::read_to_string(
            tmp.path()
                .join("apache")
                .join("conf")
                .join("phpmyadmin.conf"),
        )
        .unwrap();
        assert!(content.contains("Alias /phpmyadmin"));
    }

    #[test]
    fn write_disabled_creates_empty_conf() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        std::fs::create_dir_all(tmp.path().join("apache").join("conf")).unwrap();
        write_phpmyadmin_apache_conf_disabled(&cfg).unwrap();
        let content = std::fs::read_to_string(
            tmp.path()
                .join("apache")
                .join("conf")
                .join("phpmyadmin.conf"),
        )
        .unwrap();
        assert!(content.is_empty());
    }
}
