use crate::state::RampConfig;

/// Generate a minimal php.ini for PHP-CGI running under RAMPP.
/// Only called when the file does not already exist.
pub fn generate_php_ini(cfg: &RampConfig) -> String {
    generate_php_ini_with_port(cfg, cfg.mysql.port)
}

/// Same as `generate_php_ini` but with an explicit MySQL port — used by the
/// reconciler so `mysqli.default_port` follows MySQL's real port.
pub fn generate_php_ini_with_port(cfg: &RampConfig, mysql_port: u16) -> String {
    let php_dir = cfg.install_dir.join("php");
    let ext_dir = php_dir.join("ext");
    let ext_dir_s = ext_dir.display().to_string().replace('\\', "/");
    let logs_dir = cfg
        .install_dir
        .join("logs")
        .display()
        .to_string()
        .replace('\\', "/");
    let session_dir = cfg
        .install_dir
        .join("tmp")
        .join("sessions")
        .display()
        .to_string()
        .replace('\\', "/");

    format!(
        r#"; RAMPP — generated php.ini
[PHP]
engine = On
short_open_tag = Off
precision = 14
; output_buffering must be Off — numbered values cause blank pages in phpMyAdmin
output_buffering = Off
zlib.output_compression = Off
implicit_flush = Off
serialize_precision = -1
disable_functions =
disable_classes =
expose_php = Off
max_execution_time = 30
max_input_time = 60
memory_limit = 128M
error_reporting = E_ALL & ~E_DEPRECATED & ~E_STRICT
display_errors = On
display_startup_errors = On
log_errors = On
error_log = "{logs_dir}/php_errors.log"
variables_order = "GPCS"
request_order = "GP"
register_argc_argv = Off
auto_globals_jit = On
post_max_size = 64M
default_mimetype = "text/html"
default_charset = "UTF-8"
; doc_root must be empty — a non-empty value causes PHP-CGI to reject SCRIPT_FILENAME
; paths that don't start with doc_root, producing "No input file specified" for phpMyAdmin
; (which lives outside apache/htdocs). SCRIPT_FILENAME is set explicitly via ProxyFCGISetEnvIf.
doc_root =
user_dir =
cgi.fix_pathinfo = 1
cgi.force_redirect = 0
; discard_path=1 lets PHP-CGI handle PATH_INFO URLs (e.g. index.php/route) used by phpMyAdmin
cgi.discard_path = 1
extension_dir = "{ext_dir}"
enable_dl = Off
file_uploads = On
upload_max_filesize = 64M
max_file_uploads = 20
allow_url_fopen = On
allow_url_include = Off
default_socket_timeout = 60

; Extensions required by phpMyAdmin 5.x
extension=mysqli
extension=openssl
extension=mbstring

; Extensions useful for local development — uncomment as needed
;extension=curl
;extension=fileinfo
;extension=gd
;extension=intl
;extension=exif
;extension=pdo_mysql
;extension=pdo_sqlite
;extension=sqlite3
;extension=zip

[CLI Server]
cli_server.color = On

[Date]
date.timezone = UTC

[Pdo_mysql]
pdo_mysql.default_socket=

[mail function]
SMTP = localhost
smtp_port = 25
mail.add_x_header = Off

[ODBC]
odbc.allow_persistent = On
odbc.check_persistent = On
odbc.max_persistent = -1
odbc.max_links = -1
odbc.defaultlrl = 4096
odbc.defaultbinmode = 1

[MySQLi]
mysqli.max_persistent = -1
mysqli.allow_persistent = On
mysqli.max_links = -1
mysqli.default_port = {mysql_port}
mysqli.default_socket =
mysqli.default_host = 127.0.0.1
mysqli.default_user =
mysqli.default_pw =
mysqli.reconnect = Off
mysqli.local_infile = On

[mysqlnd]
mysqlnd.collect_statistics = On
mysqlnd.collect_memory_statistics = Off

[bcmath]
bcmath.scale = 0

[Session]
session.save_handler = files
session.save_path = "{session_dir}"
session.use_strict_mode = 0
session.use_cookies = 1
session.use_only_cookies = 1
session.name = PHPSESSID
session.auto_start = 0
session.cookie_lifetime = 0
session.cookie_path = /
session.cookie_domain =
session.cookie_httponly =
session.cookie_samesite =
session.serialize_handler = php
session.gc_probability = 1
session.gc_divisor = 1000
session.gc_maxlifetime = 1440
session.referer_check =
session.cache_limiter = nocache
session.cache_expire = 180
session.use_trans_sid = 0
session.sid_length = 26
session.trans_sid_tags = "a=href,area=href,frame=src,form="
session.sid_bits_per_character = 5
"#,
        ext_dir = ext_dir_s,
        logs_dir = logs_dir,
        session_dir = session_dir,
        mysql_port = mysql_port,
    )
}

/// Write php.ini only if it doesn't already exist.
pub fn ensure_php_ini(cfg: &RampConfig) -> Result<(), String> {
    let ini_path = &cfg.php.ini;
    if ini_path.exists() {
        return Ok(());
    }
    let dir = ini_path.parent().ok_or("php.ini has no parent dir")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create php dir: {e}"))?;
    let content = generate_php_ini(cfg);
    crate::config::atomic_write(ini_path, content.as_bytes())
        .map_err(|e| format!("cannot write php.ini: {e}"))
}

/// Ensure the root logs directory exists (used by PHP, Apache, and MySQL).
pub fn ensure_php_dirs(cfg: &RampConfig) -> Result<(), String> {
    let logs_dir = cfg.install_dir.join("logs");
    std::fs::create_dir_all(&logs_dir).map_err(|e| format!("cannot create logs: {e}"))?;
    let pma_logs = logs_dir.join("phpmyadmin");
    std::fs::create_dir_all(&pma_logs).map_err(|e| format!("cannot create logs/phpmyadmin: {e}"))
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
    fn generates_ini_with_mysql_port() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let ini = generate_php_ini(&cfg);
        assert!(ini.contains("mysqli.default_port = 3306"));
        assert!(ini.contains("expose_php = Off"));
    }

    #[test]
    fn doc_root_is_empty_for_phpmyadmin_compatibility() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let ini = generate_php_ini(&cfg);
        // doc_root must be empty — a non-empty value causes PHP-CGI to reject SCRIPT_FILENAME
        // paths outside it, producing "No input file specified" for phpMyAdmin.
        assert!(
            ini.contains("\ndoc_root =\n"),
            "doc_root must be empty so PHP-CGI accepts SCRIPT_FILENAME from ProxyFCGISetEnvIf"
        );
    }

    #[test]
    fn required_extensions_are_enabled() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let ini = generate_php_ini(&cfg);
        assert!(
            ini.contains("\nextension=mysqli\n"),
            "mysqli must be enabled for phpMyAdmin"
        );
        assert!(
            ini.contains("\nextension=openssl\n"),
            "openssl must be enabled for phpMyAdmin cookie auth"
        );
        assert!(
            ini.contains("\nextension=mbstring\n"),
            "mbstring must be enabled for phpMyAdmin"
        );
    }

    #[test]
    fn output_buffering_is_off() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let ini = generate_php_ini(&cfg);
        assert!(
            ini.contains("\noutput_buffering = Off\n"),
            "output_buffering must be Off — numeric values cause blank pages in phpMyAdmin"
        );
    }

    #[test]
    fn ensure_php_ini_creates_file() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        ensure_php_ini(&cfg).unwrap();
        assert!(cfg.php.ini.exists());
    }

    #[test]
    fn ensure_php_ini_does_not_overwrite() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        std::fs::create_dir_all(cfg.php.ini.parent().unwrap()).unwrap();
        std::fs::write(&cfg.php.ini, b"custom").unwrap();
        ensure_php_ini(&cfg).unwrap();
        assert_eq!(std::fs::read(&cfg.php.ini).unwrap(), b"custom");
    }
}
