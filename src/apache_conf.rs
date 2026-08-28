use crate::state::{
    RampConfig, HEALTH_ENDPOINT_BODY, HEALTH_ENDPOINT_DIR, HEALTH_ENDPOINT_FILE,
    HEALTH_ENDPOINT_PATH, PROXY_TIMEOUT_SECS,
};
use std::path::PathBuf;

/// Directory holding RAMPP's health endpoint file, inside the install dir.
pub fn health_endpoint_dir(cfg: &RampConfig) -> PathBuf {
    cfg.install_dir.join("apache").join(HEALTH_ENDPOINT_DIR)
}

/// Full path to the file Apache serves at `HEALTH_ENDPOINT_PATH`.
pub fn health_endpoint_file(cfg: &RampConfig) -> PathBuf {
    health_endpoint_dir(cfg).join(HEALTH_ENDPOINT_FILE)
}

/// Generate a minimal httpd.conf for RAMPP's bundled Apache layout, rendered from
/// the reducer's port ledger — the executor's `provision` reconciler is the only
/// caller in production; ports are always explicit.
pub fn generate_httpd_conf_with_ports(cfg: &RampConfig, port: u16, php_port: u16) -> String {
    let apache_dir = cfg.install_dir.join("apache");
    let apache_dir = apache_dir.display().to_string().replace('\\', "/");
    let logs_dir = cfg.install_dir.join("logs");
    let logs_dir = logs_dir.display().to_string().replace('\\', "/");
    let doc_root = cfg
        .apache
        .document_root
        .display()
        .to_string()
        .replace('\\', "/");
    let health_url = HEALTH_ENDPOINT_PATH;
    let health_dir = health_endpoint_dir(cfg)
        .display()
        .to_string()
        .replace('\\', "/");
    let health_file = health_endpoint_file(cfg)
        .display()
        .to_string()
        .replace('\\', "/");
    let proxy_timeout = PROXY_TIMEOUT_SECS;

    format!(
        r#"# RAMPP — generated httpd.conf (do not remove this line — RAMPP uses it to detect generated configs)
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

# RAMPP readiness/health endpoint — served by Apache itself, never by the user's app.
#
# The DocumentRoot below runs with "AllowOverride All" so project .htaccess files
# work. Front-controller frameworks (Laravel, Symfony, WordPress) ship a rewrite
# like "RewriteRule ^(.*)$ index.php [QSA,L]" that captures EVERY URL that isn't a
# real file — including a probe path. That would route RAMPP's health check through
# mod_proxy_fcgi into PHP, so Apache would only look "up" once PHP-CGI, MySQL and
# the user's application had all booted, and each 2s health check would cost a full
# application request.
#
# mod_alias maps the URL to this directory at translation time, before per-directory
# rewrites are considered, and "AllowOverride None" here means no .htaccess applies.
# The probe therefore stays a static file read that reflects Apache's health alone.
Alias "{health_url}" "{health_file}"
<Directory "{health_dir}">
    AllowOverride None
    Options None
    Require all granted
</Directory>

DocumentRoot "{doc_root}"
<Directory "{doc_root}">
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
#
# Declare the FastCGI worker explicitly so ProxySet can configure it.
#
# retry=0 is the important part: by default Apache marks a failed proxy worker
# dead for 60 seconds, so every PHP-CGI restart caused a full minute of 503s on
# every .php request even after PHP was healthy again.
<Proxy "fcgi://127.0.0.1:{php_port}">
    ProxySet retry=0 timeout={proxy_timeout}
</Proxy>

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

Timeout {proxy_timeout}

<IfModule log_config_module>
    LogFormat "%h %l %u %t \"%r\" %>s %b" common
    # The readiness probe runs every 2 seconds forever — keep it out of the log.
    SetEnvIf Request_URI "^{health_url}$" ramp_health
    CustomLog "{logs_dir}/apache_access.log" common env=!ramp_health
</IfModule>

<IfModule mime_module>
    TypesConfig conf/mime.types
    AddType application/x-compress .Z
    AddType application/x-gzip .gz .tgz
</IfModule>

# phpMyAdmin — managed by RAMPP (do not remove this line)
IncludeOptional "conf/phpmyadmin.conf"
"#
    )
}

/// Ensure the health endpoint file Apache serves at `HEALTH_ENDPOINT_PATH` exists.
/// Rewritten unconditionally: it is RAMPP-owned, tiny, and must never drift.
pub fn ensure_health_endpoint(cfg: &RampConfig) -> Result<(), String> {
    let dir = health_endpoint_dir(cfg);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create health endpoint dir {}: {e}", dir.display()))?;
    let file = health_endpoint_file(cfg);
    crate::config::atomic_write(&file, HEALTH_ENDPOINT_BODY.as_bytes())
        .map_err(|e| format!("cannot write health endpoint file: {e}"))
}

/// Ensure the configured DocumentRoot exists. Seeds a default index.php ONLY when
/// the folder is empty, so user-chosen project folders are never modified.
pub fn ensure_document_root(cfg: &RampConfig) -> Result<(), String> {
    let root = &cfg.apache.document_root;
    std::fs::create_dir_all(root)
        .map_err(|e| format!("cannot create document root {}: {e}", root.display()))?;

    let is_empty = std::fs::read_dir(root)
        .map_err(|e| format!("cannot read document root {}: {e}", root.display()))?
        .next()
        .is_none();
    if is_empty {
        let index = root.join("index.php");
        std::fs::write(&index, b"<?php phpinfo();\n")
            .map_err(|e| format!("cannot write index.php: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        ApacheConfig, MysqlConfig, PhpConfig, PhpMyAdminConfig, RampConfig, HEALTH_ENDPOINT_PATH,
    };
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

    /// The readiness probe must be served by Apache itself, never by the user's
    /// DocumentRoot. A front-controller `.htaccess` (Laravel/Symfony/WordPress)
    /// rewrites every unmatched URL into index.php, which would route the probe
    /// through PHP-CGI and make Apache's health depend on the user's app booting.
    /// Aliasing the probe to a RAMPP-owned directory bypasses the DocumentRoot
    /// entirely, because mod_alias maps the URL before per-directory rewrites run.
    #[test]
    fn health_endpoint_is_aliased_outside_document_root() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = test_cfg(tmp.path());
        cfg.apache.document_root = tmp.path().join("laravel_app").join("public");
        let conf = generate_httpd_conf_with_ports(&cfg, cfg.apache.port, cfg.php.port);

        let expected_target = tmp
            .path()
            .join("apache")
            .join("rampp-health")
            .join("health.txt")
            .display()
            .to_string()
            .replace('\\', "/");
        assert!(
            conf.contains(&format!(
                "Alias \"{HEALTH_ENDPOINT_PATH}\" \"{expected_target}\""
            )),
            "health endpoint must be aliased to a RAMPP-owned file, conf was:\n{conf}"
        );

        let doc_root = cfg
            .apache
            .document_root
            .display()
            .to_string()
            .replace('\\', "/");
        assert!(
            !expected_target.starts_with(&doc_root),
            "health endpoint target must live outside the DocumentRoot"
        );

        // Being outside the DocumentRoot is not enough on its own: if the health
        // directory allowed overrides, an .htaccess dropped beside it could still
        // rewrite the probe into PHP.
        let health_dir = tmp
            .path()
            .join("apache")
            .join("rampp-health")
            .display()
            .to_string()
            .replace('\\', "/");
        let health_block = conf
            .split(&format!("<Directory \"{health_dir}\">"))
            .nth(1)
            .expect("conf must contain a <Directory> block for the health endpoint")
            .split("</Directory>")
            .next()
            .unwrap_or("");
        assert!(
            health_block.contains("AllowOverride None"),
            "health endpoint directory must disable .htaccess overrides, block was:\n{health_block}"
        );
    }

    #[test]
    fn ensure_health_endpoint_creates_file() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        ensure_health_endpoint(&cfg).unwrap();
        let file = health_endpoint_file(&cfg);
        assert!(file.exists(), "health endpoint file must be created");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            crate::state::HEALTH_ENDPOINT_BODY
        );
    }

    #[test]
    fn ensure_health_endpoint_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        ensure_health_endpoint(&cfg).unwrap();
        ensure_health_endpoint(&cfg).unwrap();
        assert_eq!(
            std::fs::read_to_string(health_endpoint_file(&cfg)).unwrap(),
            crate::state::HEALTH_ENDPOINT_BODY
        );
    }

    #[test]
    fn generates_conf_with_correct_port() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let conf = generate_httpd_conf_with_ports(&cfg, cfg.apache.port, cfg.php.port);
        assert!(conf.contains("Listen 127.0.0.1:8080"));
        assert!(conf.contains("ServerName 127.0.0.1:8080"));
    }

    #[test]
    fn generates_conf_with_php_fcgi_proxy() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let conf = generate_httpd_conf_with_ports(&cfg, cfg.apache.port, cfg.php.port);
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
        let conf = generate_httpd_conf_with_ports(&cfg, cfg.apache.port, cfg.php.port);
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
    fn generates_conf_with_phpmyadmin_include() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let conf = generate_httpd_conf_with_ports(&cfg, cfg.apache.port, cfg.php.port);
        assert!(
            conf.contains(r#"IncludeOptional "conf/phpmyadmin.conf""#),
            "httpd.conf must include phpmyadmin.conf"
        );
    }

    #[test]
    fn phpmyadmin_include_is_optional() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let conf = generate_httpd_conf_with_ports(&cfg, cfg.apache.port, cfg.php.port);
        assert!(
            conf.contains(r#"IncludeOptional "conf/phpmyadmin.conf""#),
            "a missing phpmyadmin.conf must not stop Apache from starting"
        );
        assert!(!conf.contains(r#"Include "conf/phpmyadmin.conf""#));
    }

    #[test]
    fn fastcgi_worker_disables_retry_blackout() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let conf = generate_httpd_conf_with_ports(&cfg, 8080, 9000);
        assert!(
            conf.contains(r#"<Proxy "fcgi://127.0.0.1:9000">"#),
            "the worker must be declared so ProxySet can reach it"
        );
        assert!(
            conf.contains("ProxySet retry=0"),
            "without retry=0 Apache marks the worker dead for 60s after PHP restarts"
        );
    }

    #[test]
    fn health_probe_is_excluded_from_the_access_log() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let conf = generate_httpd_conf_with_ports(&cfg, cfg.apache.port, cfg.php.port);
        assert!(conf.contains(r#"SetEnvIf Request_URI "^/__ramp_health$" ramp_health"#));
        assert!(
            conf.contains("env=!ramp_health"),
            "the 2s probe must not be logged forever"
        );
    }

    #[test]
    fn document_root_reflects_config_value() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = test_cfg(tmp.path());
        cfg.apache.document_root = tmp.path().join("custom_site");
        let conf = generate_httpd_conf_with_ports(&cfg, cfg.apache.port, cfg.php.port);
        let expected = tmp
            .path()
            .join("custom_site")
            .display()
            .to_string()
            .replace('\\', "/");
        assert!(
            conf.contains(&format!("DocumentRoot \"{expected}\"")),
            "DocumentRoot must reflect configured document_root"
        );
        assert!(
            conf.contains(&format!("<Directory \"{expected}\">")),
            "<Directory> block must reflect configured document_root"
        );
    }

    #[test]
    fn ensure_document_root_seeds_empty_folder() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        ensure_document_root(&cfg).unwrap();
        let index = cfg.apache.document_root.join("index.php");
        assert!(
            index.exists(),
            "empty document root should be seeded with index.php"
        );
    }

    #[test]
    fn ensure_document_root_leaves_nonempty_folder_untouched() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        std::fs::create_dir_all(&cfg.apache.document_root).unwrap();
        let existing = cfg.apache.document_root.join("app.php");
        std::fs::write(&existing, b"<?php // user file").unwrap();
        ensure_document_root(&cfg).unwrap();
        assert!(!cfg.apache.document_root.join("index.php").exists());
        assert_eq!(std::fs::read(&existing).unwrap(), b"<?php // user file");
    }
}
