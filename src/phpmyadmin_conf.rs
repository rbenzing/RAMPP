/// Marker identifying a RAMPP-generated phpmyadmin.conf. Present in both the
/// enabled and disabled forms so ownership detection works either way.
pub const PMA_CONF_MARKER: &str = "# RAMPP — phpMyAdmin";

/// Escape a string for embedding inside a PHP single-quoted literal.
/// Only `\` and `'` are special there — order matters, backslash first.
pub fn php_single_quoted(s: &str) -> String {
    s.replace('\\', r"\\").replace('\'', r"\'")
}

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
    let temp_dir = php_single_quoted(
        &install_dir
            .join("logs")
            .join("phpmyadmin")
            .display()
            .to_string()
            .replace('\\', "/"),
    );
    let mysql_user = php_single_quoted(mysql_user);
    let mysql_password = php_single_quoted(mysql_password);
    let blowfish_secret = php_single_quoted(blowfish_secret);
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
// No outbound call to phpmyadmin.net on every dashboard load — RAMPP is loopback-only
// and the request stalls the page when offline.
$cfg['VersionCheck'] = false;
// Windows ACLs make phpMyAdmin's config-permission check a false positive.
$cfg['CheckConfigurationPermissions'] = false;
"#
    )
}

fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// 32 hex characters for phpMyAdmin's `blowfish_secret`.
///
/// The previous implementation produced 16 hex chars and mirrored them, so the
/// second half was fully determined by the first. This mixes an OS-seeded
/// `RandomState` hash, the epoch nanos, the pid, and a stack address (ASLR)
/// through splitmix64, and derives the low half from the high half plus
/// independent inputs so the two are not equal.
pub fn generate_blowfish_secret() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let os_seed = RandomState::new().build_hasher().finish();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = u64::from(std::process::id());
    let stack_addr = &os_seed as *const u64 as u64;

    let hi = splitmix64(os_seed ^ nanos);
    let lo = splitmix64(pid.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ stack_addr ^ hi);
    format!("{hi:016x}{lo:016x}")
}

/// Content of `phpmyadmin.conf` when phpMyAdmin is disabled. A marker-only file
/// rather than an empty one, so ownership detection still recognizes it as ours.
pub fn generate_phpmyadmin_conf_disabled() -> String {
    format!("{PMA_CONF_MARKER} disabled (do not remove this line)\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
    fn blowfish_secret_is_32_chars() {
        let secret = generate_blowfish_secret();
        assert_eq!(secret.len(), 32, "blowfish_secret must be exactly 32 chars");
    }

    #[test]
    fn blowfish_secret_is_alphanumeric() {
        let secret = generate_blowfish_secret();
        assert!(
            secret.chars().all(|c| c.is_ascii_hexdigit()),
            "blowfish_secret must be hex"
        );
    }

    #[test]
    fn php_single_quoted_escapes_quote_and_backslash() {
        assert_eq!(php_single_quoted("plain"), "plain");
        assert_eq!(php_single_quoted("it's"), r"it\'s");
        assert_eq!(php_single_quoted(r"back\slash"), r"back\\slash");
        assert_eq!(php_single_quoted(r"both\'s"), r"both\\\'s");
    }

    #[test]
    fn config_inc_php_escapes_password_with_quote() {
        let tmp = TempDir::new().unwrap();
        let php = generate_config_inc_php(
            tmp.path(),
            3306,
            "root",
            r"pa'ss\word",
            "secret12345678901234567890123456",
        );
        // The raw sequence must never appear unescaped — it would end the PHP literal.
        assert!(
            php.contains(r"pa\'ss\\word"),
            "password must be escaped: {php}"
        );
    }

    #[test]
    fn config_inc_php_disables_version_check_and_permission_check() {
        let tmp = TempDir::new().unwrap();
        let php = generate_config_inc_php(
            tmp.path(),
            3306,
            "root",
            "",
            "secret12345678901234567890123456",
        );
        assert!(php.contains("$cfg['VersionCheck'] = false;"));
        assert!(php.contains("$cfg['CheckConfigurationPermissions'] = false;"));
    }

    #[test]
    fn blowfish_secret_halves_are_independent() {
        let s = generate_blowfish_secret();
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(&s[..16], &s[16..], "halves must not be mirrored");
    }

    #[test]
    fn blowfish_secret_differs_between_calls() {
        let a = generate_blowfish_secret();
        let b = generate_blowfish_secret();
        assert_ne!(a, b);
    }

    #[test]
    fn disabled_conf_carries_the_ownership_marker() {
        let conf = generate_phpmyadmin_conf_disabled();
        assert!(conf.contains(PMA_CONF_MARKER));
        assert!(!conf.contains("Alias /phpmyadmin"));
    }

    #[test]
    fn enabled_conf_carries_the_ownership_marker() {
        let tmp = TempDir::new().unwrap();
        let conf = generate_phpmyadmin_apache_conf(tmp.path(), 9000);
        assert!(conf.contains(PMA_CONF_MARKER));
    }
}
