use rampp::provision::{adopt_vendor_httpd_conf, desired_configs, is_rampp_owned, reconcile};
use rampp::state::{
    ApacheConfig, ManagedFile, MysqlConfig, PhpConfig, PhpMyAdminConfig, PortState, RampConfig,
    Service,
};
use tempfile::TempDir;

fn cfg_for(dir: &std::path::Path) -> RampConfig {
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
fn first_pass_writes_everything_second_pass_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let cfg = cfg_for(tmp.path());
    let ports = PortState::default();

    let first = reconcile(&desired_configs(&cfg, &ports, false, false, "s"));
    assert!(!first.changed.is_empty(), "first pass must create files");
    assert!(
        first.errors.is_empty(),
        "unexpected errors: {:?}",
        first.errors
    );

    let second = reconcile(&desired_configs(&cfg, &ports, false, false, "s"));
    assert!(
        second.changed.is_empty(),
        "reconcile must be idempotent, but rewrote {:?}",
        second.changed
    );
}

#[test]
fn idempotent_pass_creates_no_backup_files() {
    let tmp = TempDir::new().unwrap();
    let cfg = cfg_for(tmp.path());
    let ports = PortState::default();
    reconcile(&desired_configs(&cfg, &ports, false, false, "s"));
    reconcile(&desired_configs(&cfg, &ports, false, false, "s"));

    let backups: Vec<_> = walk(tmp.path())
        .into_iter()
        .filter(|p| p.to_string_lossy().ends_with(".bak"))
        .collect();
    assert!(
        backups.is_empty(),
        "no-op pass created backups: {backups:?}"
    );
}

#[test]
fn user_owned_files_are_never_written() {
    let tmp = TempDir::new().unwrap();
    let cfg = cfg_for(tmp.path());
    std::fs::create_dir_all(cfg.apache.conf.parent().unwrap()).unwrap();
    let mine = b"# my own apache config, no RAMPP marker\nServerRoot \"C:/x\"\n";
    std::fs::write(&cfg.apache.conf, mine).unwrap();

    let report = reconcile(&desired_configs(
        &cfg,
        &PortState::default(),
        false,
        false,
        "s",
    ));

    assert!(report.user_owned.contains(&ManagedFile::HttpdConf));
    assert!(!report.changed.contains(&ManagedFile::HttpdConf));
    assert_eq!(std::fs::read(&cfg.apache.conf).unwrap(), mine);
}

/// A file that exists but cannot be decoded as UTF-8 cannot be checked for the
/// RAMPP marker at all. Silently treating that the same as "missing" would let
/// reconcile overwrite a file it never actually proved it owns — so this must
/// surface as an error, not a write, and the file must be left untouched.
#[test]
fn unreadable_existing_file_is_reported_as_an_error_and_left_untouched() {
    let tmp = TempDir::new().unwrap();
    let cfg = cfg_for(tmp.path());
    std::fs::create_dir_all(cfg.apache.conf.parent().unwrap()).unwrap();
    let invalid_utf8: &[u8] = &[0xFF, 0xFE, 0xFD, b'\n'];
    std::fs::write(&cfg.apache.conf, invalid_utf8).unwrap();

    let report = reconcile(&desired_configs(
        &cfg,
        &PortState::default(),
        false,
        false,
        "s",
    ));

    assert!(
        report
            .errors
            .iter()
            .any(|(f, _)| *f == ManagedFile::HttpdConf),
        "unreadable file must be reported as an error: {:?}",
        report.errors
    );
    assert!(!report.changed.contains(&ManagedFile::HttpdConf));
    assert!(!report.user_owned.contains(&ManagedFile::HttpdConf));
    assert_eq!(
        std::fs::read(&cfg.apache.conf).unwrap(),
        invalid_utf8,
        "an unreadable file must never be overwritten"
    );
}

#[test]
fn changing_a_marked_file_backs_up_the_previous_content() {
    let tmp = TempDir::new().unwrap();
    let cfg = cfg_for(tmp.path());
    let mut ports = PortState::default();
    reconcile(&desired_configs(&cfg, &ports, false, false, "s"));
    let before = std::fs::read_to_string(&cfg.apache.conf).unwrap();

    // Move Apache to a different port so the rendered content differs.
    ports.assign(Service::Apache, 8081);
    let report = reconcile(&desired_configs(&cfg, &ports, false, false, "s"));

    assert!(report.changed.contains(&ManagedFile::HttpdConf));
    let backup = cfg.apache.conf.with_extension("conf.bak");
    assert_eq!(std::fs::read_to_string(backup).unwrap(), before);
    assert!(std::fs::read_to_string(&cfg.apache.conf)
        .unwrap()
        .contains("Listen 127.0.0.1:8081"));
}

/// Finding 2 (final whole-branch review), regression test #1: an ONGOING
/// (non-fresh-install) reconcile must keep refusing to touch an unmarked,
/// non-empty httpd.conf exactly as before — this is the deferred-minor #24
/// from Task 10, upgraded to must-fix. `reconcile` itself is untouched by the
/// Finding 2 fix (only `main.rs`'s startup path gained a pre-step), so this
/// pins that the ordinary protection still holds when that pre-step never runs.
#[test]
fn unmarked_non_empty_httpd_conf_blocks_a_non_fresh_reconcile() {
    let tmp = TempDir::new().unwrap();
    let cfg = cfg_for(tmp.path());
    std::fs::create_dir_all(cfg.apache.conf.parent().unwrap()).unwrap();
    let vendor = b"# Apache Lounge httpd.conf\nServerRoot \"C:/rampp/apache\"\nListen 80\n";
    std::fs::write(&cfg.apache.conf, vendor).unwrap();

    // No adoption step run — this is the ordinary, non-fresh-install path.
    let report = reconcile(&desired_configs(
        &cfg,
        &PortState::default(),
        false,
        false,
        "s",
    ));

    assert!(report.user_owned.contains(&ManagedFile::HttpdConf));
    assert!(!report.changed.contains(&ManagedFile::HttpdConf));
    assert_eq!(std::fs::read(&cfg.apache.conf).unwrap(), vendor);
}

/// Finding 2 (final whole-branch review), regression test #2: a genuinely fresh
/// install (i.e. `rampp.toml` was just created — simulated here by calling
/// `adopt_vendor_httpd_conf` before `reconcile`, exactly as `main.rs`'s startup
/// path does) must adopt a stock, unmarked `httpd.conf` — backing up the vendor
/// content and letting `reconcile`'s normal NotFound branch write RAMPP's own
/// marked version, rather than permanently classifying it user-owned and never
/// writing RAMPP's port, health endpoint, or PHP proxy configuration.
#[test]
fn fresh_install_adopts_a_stock_unmarked_httpd_conf() {
    let tmp = TempDir::new().unwrap();
    let cfg = cfg_for(tmp.path());
    std::fs::create_dir_all(cfg.apache.conf.parent().unwrap()).unwrap();
    let vendor = b"# Apache Lounge httpd.conf\nServerRoot \"C:/rampp/apache\"\nListen 80\n";
    std::fs::write(&cfg.apache.conf, vendor).unwrap();

    adopt_vendor_httpd_conf(&cfg.apache.conf).expect("adoption must succeed");

    let report = reconcile(&desired_configs(
        &cfg,
        &PortState::default(),
        false,
        false,
        "s",
    ));

    assert!(
        report.changed.contains(&ManagedFile::HttpdConf),
        "must be written fresh through reconcile's normal NotFound path"
    );
    assert!(!report.user_owned.contains(&ManagedFile::HttpdConf));
    let content = std::fs::read_to_string(&cfg.apache.conf).unwrap();
    assert!(
        is_rampp_owned(ManagedFile::HttpdConf, &content),
        "the adopted file must carry RAMPP's marker"
    );

    let vendor_backup = cfg.apache.conf.with_extension("conf.vendor.bak");
    assert_eq!(
        std::fs::read(vendor_backup).unwrap(),
        vendor,
        "the original vendor content must be preserved as a backup"
    );
}

fn walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}
