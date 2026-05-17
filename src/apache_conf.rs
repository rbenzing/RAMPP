use crate::state::RampConfig;

/// Generate a minimal httpd.conf for RAMP's bundled Apache layout.
/// Only called when the file does not already exist (never overwrites user edits).
pub fn generate_httpd_conf(cfg: &RampConfig) -> String {
    generate_httpd_conf_with_ports(cfg, cfg.apache.port, cfg.php.port)
}

/// Same as `generate_httpd_conf` but with explicit port overrides — used by the
/// executor when the configured port was occupied and a different one was chosen.
pub fn generate_httpd_conf_with_ports(cfg: &RampConfig, port: u16, php_port: u16) -> String {
    let apache_dir = cfg.install_dir.join("apache");
    let apache_dir = apache_dir.display().to_string().replace('\\', "/");
    let logs_dir = cfg.install_dir.join("logs");
    let logs_dir = logs_dir.display().to_string().replace('\\', "/");

    format!(
        r#"# RAMP — generated httpd.conf (do not remove this line — RAMP uses it to detect generated configs)
ServerRoot "{apache_dir}"

Listen 127.0.0.1:{port}

# Note: on Windows, the MPM (mpm_winnt) and unixd are linked statically into
# httpd.exe — there is no LoadModule line for them.

# Core modules required for basic operation
LoadModule authn_core_module modules/mod_authn_core.so
LoadModule authz_core_module modules/mod_authz_core.so
LoadModule authz_host_module modules/mod_authz_host.so
LoadModule access_compat_module modules/mod_access_compat.so
LoadModule log_config_module modules/mod_log_config.so
LoadModule mime_module modules/mod_mime.so
LoadModule dir_module modules/mod_dir.so
LoadModule env_module modules/mod_env.so
LoadModule headers_module modules/mod_headers.so
LoadModule rewrite_module modules/mod_rewrite.so
LoadModule deflate_module modules/mod_deflate.so
LoadModule filter_module modules/mod_filter.so
LoadModule setenvif_module modules/mod_setenvif.so
LoadModule version_module modules/mod_version.so
LoadModule autoindex_module modules/mod_autoindex.so
LoadModule negotiation_module modules/mod_negotiation.so
LoadModule alias_module modules/mod_alias.so
LoadModule socache_shmcb_module modules/mod_socache_shmcb.so

# PHP via mod_proxy_fcgi → PHP-CGI listening on 127.0.0.1:{php_port}
LoadModule proxy_module modules/mod_proxy.so
LoadModule proxy_fcgi_module modules/mod_proxy_fcgi.so

ServerAdmin local@localhost
ServerName 127.0.0.1:{port}

<Directory />
    AllowOverride none
    Require all denied
</Directory>

DocumentRoot "{apache_dir}/htdocs"
<Directory "{apache_dir}/htdocs">
    Options Indexes FollowSymLinks
    AllowOverride All
    Require all granted
</Directory>

<IfModule dir_module>
    DirectoryIndex index.php index.html index.htm
</IfModule>

# Proxy .php requests to PHP-CGI FastCGI listener.
#
# Windows mod_proxy_fcgi bug + workaround (Apache Lounge thread t=8899):
# On Windows, when SetHandler="proxy:fcgi://host:port", mod_proxy_fcgi appends
# the resolved script filename (e.g. "C:/path/to/file.php") directly onto the
# URL authority, producing "//host:portC:/path...". The URL parser then sees
# the host as "127.0.0.1:9000c" → DNS lookup failure → "AH00898".
#
# Variant A workaround (empirically verified on Apache 2.4.66 + PHP 8.5 CGI):
# - Suffix the SetHandler URL with "//./" so the URL has a non-empty path
#   component before the filename is appended. This closes the authority parse
#   correctly: "//host:port//./C:/path..." → host="host:port", path="/./C:/...".
# - ProxyFCGIBackendType GENERIC MUST be inside the same <FilesMatch> block.
#   It tells mod_proxy_fcgi to not apply PHP-FPM-specific SCRIPT_FILENAME
#   mangling (which on Windows would re-introduce the corruption) and to
#   strip the "proxy:fcgi://" prefix that PHP-CGI cannot handle (unlike FPM).
<FilesMatch "\.php$">
    SetHandler "proxy:fcgi://127.0.0.1:{php_port}//./"
    ProxyFCGIBackendType GENERIC
</FilesMatch>

# Deny .htaccess and .htpasswd access
<Files ".ht*">
    Require all denied
</Files>

ErrorLog "{logs_dir}/apache_error.log"
LogLevel warn

<IfModule log_config_module>
    LogFormat "%h %l %u %t \"%r\" %>s %b" common
    CustomLog "{logs_dir}/apache_access.log" common
</IfModule>

<IfModule mime_module>
    TypesConfig conf/mime.types
    AddType application/x-compress .Z
    AddType application/x-gzip .gz .tgz
</IfModule>

# phpMyAdmin — managed by RAMP (do not remove this line)
Include "conf/phpmyadmin.conf"
"#
    )
}

/// Force-rewrite httpd.conf with explicit port overrides. Used when the executor
/// has resolved Apache or PHP to a different port than the configured one.
pub fn rewrite_httpd_conf_with_ports(
    cfg: &RampConfig,
    port: u16,
    php_port: u16,
) -> Result<(), String> {
    let conf_path = &cfg.apache.conf;
    let dir = conf_path.parent().ok_or("httpd.conf has no parent dir")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create apache/conf dir: {e}"))?;
    let content = generate_httpd_conf_with_ports(cfg, port, php_port);
    crate::config::atomic_write(conf_path, content.as_bytes())
        .map_err(|e| format!("cannot rewrite httpd.conf: {e}"))
}

/// Write httpd.conf only if it doesn't already exist.
pub fn ensure_httpd_conf(cfg: &RampConfig) -> Result<(), String> {
    let conf_path = &cfg.apache.conf;
    if conf_path.exists() {
        return Ok(());
    }
    let dir = conf_path.parent().ok_or("httpd.conf has no parent dir")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create apache/conf dir: {e}"))?;
    let content = generate_httpd_conf(cfg);
    crate::config::atomic_write(conf_path, content.as_bytes())
        .map_err(|e| format!("cannot write httpd.conf: {e}"))
}

/// Ensure htdocs directory exists (Apache requires DocumentRoot to exist).
pub fn ensure_htdocs(cfg: &RampConfig) -> Result<(), String> {
    let htdocs = cfg.install_dir.join("apache").join("htdocs");
    std::fs::create_dir_all(&htdocs).map_err(|e| format!("cannot create apache/htdocs: {e}"))?;

    // Drop a default index.php only on first run
    let index = htdocs.join("index.php");
    if !index.exists() {
        std::fs::write(&index, b"<?php phpinfo();\n")
            .map_err(|e| format!("cannot write index.php: {e}"))?;
    }
    Ok(())
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
    fn generates_conf_with_correct_port() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let conf = generate_httpd_conf(&cfg);
        assert!(conf.contains("Listen 127.0.0.1:8080"));
        assert!(conf.contains("ServerName 127.0.0.1:8080"));
    }

    #[test]
    fn generates_conf_with_php_fcgi_proxy() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let conf = generate_httpd_conf(&cfg);
        // Variant A: SetHandler URL must end with "//./" to dodge the
        // Windows mod_proxy_fcgi drive-letter URL parsing bug.
        assert!(
            conf.contains("SetHandler \"proxy:fcgi://127.0.0.1:9000//./\""),
            "SetHandler must use //./ suffix to avoid Windows URL parse bug"
        );
        assert!(conf.contains("mod_proxy_fcgi.so"));
    }

    #[test]
    fn proxy_fcgi_backend_type_generic_is_in_filesmatch_block() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let conf = generate_httpd_conf(&cfg);
        // GENERIC must be INSIDE the <FilesMatch> block, not at server scope.
        // At server scope it gets overridden by FPM-mode defaults inside the
        // FilesMatch and the Windows URL bug returns. Verified empirically.
        let filesmatch_block = conf
            .split("<FilesMatch \"\\.php$\">")
            .nth(1)
            .unwrap_or("")
            .split("</FilesMatch>")
            .next()
            .unwrap_or("");
        assert!(
            filesmatch_block.contains("ProxyFCGIBackendType GENERIC"),
            "ProxyFCGIBackendType GENERIC must be inside the <FilesMatch> block"
        );
    }

    #[test]
    fn ensure_httpd_conf_creates_file() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        ensure_httpd_conf(&cfg).unwrap();
        assert!(cfg.apache.conf.exists());
    }

    #[test]
    fn ensure_httpd_conf_does_not_overwrite() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        std::fs::create_dir_all(cfg.apache.conf.parent().unwrap()).unwrap();
        std::fs::write(&cfg.apache.conf, b"custom").unwrap();
        ensure_httpd_conf(&cfg).unwrap();
        assert_eq!(std::fs::read(&cfg.apache.conf).unwrap(), b"custom");
    }

    #[test]
    fn generates_conf_with_phpmyadmin_include() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let conf = generate_httpd_conf(&cfg);
        assert!(
            conf.contains(r#"Include "conf/phpmyadmin.conf""#),
            "httpd.conf must include phpmyadmin.conf"
        );
    }
}
