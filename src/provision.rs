use crate::state::{ManagedFile, PortState, RampConfig, Service};
use std::path::PathBuf;

/// One file's desired content.
pub struct Desired {
    pub file: ManagedFile,
    pub path: PathBuf,
    pub content: String,
}

/// What a reconcile pass actually did.
#[derive(Debug, Default)]
pub struct ReconcileReport {
    /// Files whose content on disk changed. Drives restart decisions.
    pub changed: Vec<ManagedFile>,
    /// Files left alone because the user owns them (no RAMPP marker).
    pub user_owned: Vec<ManagedFile>,
    pub errors: Vec<(ManagedFile, String)>,
}

/// Substring identifying a file as RAMPP-generated. A file without its marker is
/// the user's and is never written.
pub fn marker(file: ManagedFile) -> &'static str {
    match file {
        ManagedFile::HttpdConf => "# RAMPP — generated httpd.conf (do not remove this line",
        ManagedFile::MyIni => "# RAMPP — generated my.ini",
        ManagedFile::PhpIni => "; RAMPP — generated php.ini",
        ManagedFile::PhpMyAdminConf => crate::phpmyadmin_conf::PMA_CONF_MARKER,
        ManagedFile::PhpMyAdminConfigInc => "RAMPP — generated config.inc.php",
    }
}

pub fn is_rampp_owned(file: ManagedFile, content: &str) -> bool {
    // Pre-1.5.0 wrote a disabled phpmyadmin.conf as a zero-length file, and only
    // RAMPP ever did that — treat it as ours so upgrades are not blocked.
    if file == ManagedFile::PhpMyAdminConf && content.is_empty() {
        return true;
    }
    content.contains(marker(file))
}

/// Port a service will actually bind: the ledger's assignment, or its configured
/// port before anything has been allocated.
fn port_for(cfg: &RampConfig, ports: &PortState, svc: Service) -> u16 {
    ports.assigned(svc).unwrap_or(match svc {
        Service::Apache => cfg.apache.port,
        Service::Mysql => cfg.mysql.port,
        Service::Php => cfg.php.port,
    })
}

/// Content every managed file should have, rendered from the ledger.
///
/// `PhpMyAdminConfigInc` is omitted when `pma_dir_exists` is false — RAMPP never
/// creates a config file for an install that is not there.
pub fn desired_configs(
    cfg: &RampConfig,
    ports: &PortState,
    pma_enabled: bool,
    pma_dir_exists: bool,
    secret: &str,
) -> Vec<Desired> {
    let apache_port = port_for(cfg, ports, Service::Apache);
    let mysql_port = port_for(cfg, ports, Service::Mysql);
    let php_port = port_for(cfg, ports, Service::Php);

    let mut out = vec![
        Desired {
            file: ManagedFile::HttpdConf,
            path: cfg.apache.conf.clone(),
            content: crate::apache_conf::generate_httpd_conf_with_ports(cfg, apache_port, php_port),
        },
        Desired {
            file: ManagedFile::MyIni,
            path: cfg.mysql.ini.clone(),
            content: crate::mysql_conf::generate_my_ini_with_port(cfg, mysql_port),
        },
        Desired {
            file: ManagedFile::PhpIni,
            path: cfg.php.ini.clone(),
            content: crate::php_conf::generate_php_ini_with_port(cfg, mysql_port),
        },
        Desired {
            file: ManagedFile::PhpMyAdminConf,
            path: cfg
                .install_dir
                .join("apache")
                .join("conf")
                .join("phpmyadmin.conf"),
            content: if pma_enabled && pma_dir_exists {
                crate::phpmyadmin_conf::generate_phpmyadmin_apache_conf(&cfg.install_dir, php_port)
            } else {
                crate::phpmyadmin_conf::generate_phpmyadmin_conf_disabled()
            },
        },
    ];

    if pma_dir_exists {
        out.push(Desired {
            file: ManagedFile::PhpMyAdminConfigInc,
            path: cfg.install_dir.join("phpmyadmin").join("config.inc.php"),
            content: crate::phpmyadmin_conf::generate_config_inc_php(
                &cfg.install_dir,
                mysql_port,
                &cfg.phpmyadmin.mysql_user,
                &cfg.phpmyadmin.mysql_password,
                secret,
            ),
        });
    }

    out
}

/// On a genuinely fresh install (nothing has ever provisioned it — main.rs uses
/// `rampp.state`'s absence as that signal, not `rampp.toml`'s: see the comment
/// there), any pre-existing but unmarked `HttpdConf` can only be vendor-shipped
/// stock content — e.g. Apache Lounge's own `httpd.conf`, which every new user's
/// installation steps have them extract to exactly this path — since nothing
/// else has had a chance to run yet. Back it up next to itself with a
/// `.vendor.bak` suffix (following the same naming convention `reconcile`
/// already uses for its own hand-edit backups) and remove it, so `reconcile`'s
/// ordinary "genuinely absent" branch then writes RAMPP's own marked version
/// through its normal, already-tested path. No writing logic is duplicated here.
///
/// A no-op if the file does not exist (`reconcile` will just create it fresh),
/// already carries the RAMPP marker (nothing to adopt), or a `.vendor.bak` from
/// a prior adoption already exists next to it (see the in-function comment —
/// refuses rather than risks clobbering that backup).
///
/// MUST NOT be called outside the fresh-install startup path. On every later
/// launch an unmarked file might be the user's own deliberate replacement, and
/// the ordinary marker-gated protection in `reconcile` must apply unchanged —
/// this function bypasses that protection by design, which is only safe at the
/// single moment nothing but a vendor ZIP extraction could have written it.
pub fn adopt_vendor_httpd_conf(path: &std::path::Path) -> Result<(), String> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    if is_rampp_owned(ManagedFile::HttpdConf, &content) {
        return Ok(());
    }
    let backup = path.with_extension(
        path.extension()
            .map(|e| format!("{}.vendor.bak", e.to_string_lossy()))
            .unwrap_or_else(|| "vendor.bak".to_string()),
    );

    // A backup already existing means a prior launch already adopted a vendor
    // file at this path — this is only reachable at all on what main.rs believes
    // is a fresh install, so in the ordinary case there should be nothing here
    // yet. Refuse rather than guess: overwriting it would destroy the one copy
    // of whatever vendor content that first adoption preserved, with no way to
    // recover it. This is not a failure — the vendor file is simply left in
    // place for the ordinary marker-gated `reconcile` path to pick up as
    // `user_owned`, exactly as it would on any non-fresh launch.
    if backup.exists() {
        log::warn!(
            "{} already exists — a vendor httpd.conf was already adopted here once; \
             leaving {} untouched rather than risk overwriting that backup",
            backup.display(),
            path.display()
        );
        return Ok(());
    }

    std::fs::write(&backup, content.as_bytes())
        .map_err(|e| format!("cannot back up vendor {}: {e}", path.display()))?;
    std::fs::remove_file(path)
        .map_err(|e| format!("cannot remove vendor {}: {e}", path.display()))?;
    log::warn!(
        "adopted vendor {} on fresh install — backed up to {} and removed so RAMPP can write its own generated version",
        path.display(),
        backup.display()
    );
    Ok(())
}

/// Bring every managed file in line with its desired content.
///
/// Idempotent by construction: a file is written only when its content actually
/// differs, so a second pass reports nothing changed. That property is also what
/// keeps the restart rule in the reducer from looping.
pub fn reconcile(desired: &[Desired]) -> ReconcileReport {
    let mut report = ReconcileReport::default();

    for item in desired {
        match std::fs::read_to_string(&item.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Genuinely absent — write it fresh.
                match crate::config::atomic_write(&item.path, item.content.as_bytes()) {
                    Ok(()) => report.changed.push(item.file),
                    Err(e) => report.errors.push((item.file, e)),
                }
            }
            Err(e) => {
                // The file exists but could not be read as UTF-8 text — e.g.
                // permission-denied, or content in some other encoding. Either
                // way we cannot check for the ownership marker, so we must not
                // guess: treating this the same as "missing" would let RAMPP
                // clobber a file it never proved it owns. Surface it instead
                // and leave it untouched.
                report.errors.push((
                    item.file,
                    format!("cannot read {}: {e}", item.path.display()),
                ));
            }
            Ok(existing) => {
                if !is_rampp_owned(item.file, &existing) {
                    report.user_owned.push(item.file);
                    continue;
                }
                if existing == item.content {
                    continue;
                }
                // Keep a copy so a hand edit that left the marker in place is
                // recoverable. Single slot — overwritten on each change.
                let backup = item.path.with_extension(
                    item.path
                        .extension()
                        .map(|e| format!("{}.bak", e.to_string_lossy()))
                        .unwrap_or_else(|| "bak".to_string()),
                );
                if let Err(e) = std::fs::write(&backup, existing.as_bytes()) {
                    log::warn!("could not back up {}: {e}", item.path.display());
                }
                match crate::config::atomic_write(&item.path, item.content.as_bytes()) {
                    Ok(()) => report.changed.push(item.file),
                    Err(e) => report.errors.push((item.file, e)),
                }
            }
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ApacheConfig, MysqlConfig, PhpConfig, PhpMyAdminConfig};
    use tempfile::TempDir;

    fn test_cfg(dir: &std::path::Path) -> RampConfig {
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
    fn desired_uses_assigned_ports_not_configured_ones() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let mut ports = PortState::default();
        ports.assign(Service::Apache, 8081);
        ports.assign(Service::Php, 9001);
        ports.assign(Service::Mysql, 3307);
        let desired = desired_configs(&cfg, &ports, true, true, "s".repeat(32).as_str());

        let httpd = desired
            .iter()
            .find(|d| d.file == ManagedFile::HttpdConf)
            .unwrap();
        assert!(httpd.content.contains("Listen 127.0.0.1:8081"));
        assert!(httpd.content.contains("fcgi://127.0.0.1:9001"));

        let pma = desired
            .iter()
            .find(|d| d.file == ManagedFile::PhpMyAdminConf)
            .unwrap();
        assert!(
            pma.content.contains("9001"),
            "phpmyadmin.conf must follow PHP's real port"
        );

        let inc = desired
            .iter()
            .find(|d| d.file == ManagedFile::PhpMyAdminConfigInc)
            .unwrap();
        assert!(
            inc.content.contains("3307"),
            "config.inc.php must follow MySQL's real port"
        );

        let php_ini = desired
            .iter()
            .find(|d| d.file == ManagedFile::PhpIni)
            .unwrap();
        assert!(php_ini.content.contains("mysqli.default_port = 3307"));
    }

    #[test]
    fn desired_falls_back_to_configured_ports_before_any_assignment() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let desired = desired_configs(&cfg, &PortState::default(), false, false, "s");
        let httpd = desired
            .iter()
            .find(|d| d.file == ManagedFile::HttpdConf)
            .unwrap();
        assert!(httpd.content.contains("Listen 127.0.0.1:8080"));
    }

    #[test]
    fn desired_omits_config_inc_when_phpmyadmin_is_not_installed() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let desired = desired_configs(&cfg, &PortState::default(), true, false, "s");
        assert!(!desired
            .iter()
            .any(|d| d.file == ManagedFile::PhpMyAdminConfigInc));
    }

    #[test]
    fn disabled_phpmyadmin_conf_is_still_rampp_owned() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let desired = desired_configs(&cfg, &PortState::default(), false, true, "s");
        let pma = desired
            .iter()
            .find(|d| d.file == ManagedFile::PhpMyAdminConf)
            .unwrap();
        assert!(is_rampp_owned(ManagedFile::PhpMyAdminConf, &pma.content));
        assert!(!pma.content.contains("Alias /phpmyadmin"));
    }

    #[test]
    fn zero_length_phpmyadmin_conf_counts_as_rampp_owned() {
        // Pre-1.5.0 installs wrote this file empty; only RAMPP ever did that.
        assert!(is_rampp_owned(ManagedFile::PhpMyAdminConf, ""));
    }

    #[test]
    fn a_file_without_a_marker_is_not_rampp_owned() {
        assert!(!is_rampp_owned(
            ManagedFile::HttpdConf,
            "ServerRoot \"C:/mine\"\n"
        ));
        assert!(!is_rampp_owned(
            ManagedFile::MyIni,
            "[mysqld]\nport = 3306\n"
        ));
        assert!(!is_rampp_owned(ManagedFile::PhpIni, "[PHP]\n"));
    }

    // ── adopt_vendor_httpd_conf (Finding 2) ────────────────────────────────

    #[test]
    fn adopt_vendor_httpd_conf_is_a_noop_when_absent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("httpd.conf");
        assert!(adopt_vendor_httpd_conf(&path).is_ok());
        assert!(!path.exists(), "must not create the file itself");
    }

    #[test]
    fn adopt_vendor_httpd_conf_leaves_an_already_marked_file_alone() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("httpd.conf");
        let marked = format!(
            "{}\nListen 127.0.0.1:8080\n",
            marker(ManagedFile::HttpdConf)
        );
        std::fs::write(&path, &marked).unwrap();

        adopt_vendor_httpd_conf(&path).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            marked,
            "an already-owned file must not be touched"
        );
        let backup = path.with_extension("conf.vendor.bak");
        assert!(!backup.exists(), "nothing to adopt — no backup expected");
    }

    #[test]
    fn adopt_vendor_httpd_conf_backs_up_and_removes_an_unmarked_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("httpd.conf");
        let vendor = "# Apache Lounge httpd.conf\nServerRoot \"C:/rampp/apache\"\nListen 80\n";
        std::fs::write(&path, vendor).unwrap();

        adopt_vendor_httpd_conf(&path).unwrap();

        assert!(
            !path.exists(),
            "the vendor file must be cleared so reconcile sees it as genuinely absent"
        );
        let backup = path.with_extension("conf.vendor.bak");
        assert_eq!(std::fs::read_to_string(backup).unwrap(), vendor);
    }

    #[test]
    fn adopt_vendor_httpd_conf_refuses_to_clobber_an_existing_backup() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("httpd.conf");
        let backup = path.with_extension("conf.vendor.bak");
        let vendor = "# Apache Lounge httpd.conf (second occurrence)\nListen 80\n";
        let existing_backup = "# Apache Lounge httpd.conf (already adopted once)\nListen 80\n";
        std::fs::write(&path, vendor).unwrap();
        std::fs::write(&backup, existing_backup).unwrap();

        let result = adopt_vendor_httpd_conf(&path);

        assert!(
            result.is_ok(),
            "an existing backup is a refusal, not a failure: {result:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            vendor,
            "the vendor file must be left completely untouched"
        );
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            existing_backup,
            "the pre-existing backup must not be overwritten"
        );
    }
}
