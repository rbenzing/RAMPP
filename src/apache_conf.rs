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
# Why this is the way it is:
# - The naive `<FilesMatch> + SetHandler "proxy:fcgi://..."` pattern breaks on
#   Windows because mod_proxy_fcgi appends the script's drive-lettered path
#   directly to the proxy URL ("fcgi://host:9000c:/..."), which the URL parser
#   sees as a malformed authority → "DNS lookup failure".
# - ProxyPassMatch with an explicit script-path target avoids the URL parsing
#   bug. The capture group `$1` holds the relative script path; PHP-CGI then
#   joins it against `doc_root` from php.ini to find the real file. We rely on
#   `doc_root` (set in php.ini) rather than absolute paths in the proxy URL,
#   because absolute Windows paths in fcgi:// URLs trigger the same parser bug.
ProxyFCGIBackendType GENERIC
ProxyPassMatch "^/(?!phpmyadmin/)(.+\.php(/.*)?)$" "fcgi://127.0.0.1:{php_port}/$1"

# Deny .htaccess and .htpasswd access
<Files ".ht*">
    Require all denied
</Files>

ErrorLog "logs/error.log"
LogLevel warn

<IfModule log_config_module>
    LogFormat "%h %l %u %t \"%r\" %>s %b" common
    CustomLog "logs/access.log" common
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
        assert!(conf.contains("fcgi://127.0.0.1:9000"));
        assert!(conf.contains("ProxyPassMatch"));
        assert!(conf.contains("mod_proxy_fcgi.so"));
    }

    #[test]
    fn global_proxy_pass_match_excludes_phpmyadmin() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let conf = generate_httpd_conf(&cfg);
        assert!(
            conf.contains("(?!phpmyadmin/)"),
            "global ProxyPassMatch must exclude /phpmyadmin/ so phpmyadmin.conf handles those requests"
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
