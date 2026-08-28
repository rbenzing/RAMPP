use crate::state::{HEALTH_ENDPOINT_DIR, HEALTH_ENDPOINT_FILE};
use std::path::{Path, PathBuf};

/// Validates and resolves all install-relative paths.
/// Enforces: absolute paths only, no symlinks for critical paths, no traversal.
#[allow(dead_code)]
pub struct InstallPaths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub state_file: PathBuf,
    pub log_file: PathBuf,
    pub apache_bin: PathBuf,
    pub apache_conf: PathBuf,
    pub apache_logs: PathBuf,
    /// RAMPP-owned directory Apache serves the readiness probe from, kept outside
    /// the user's DocumentRoot so no project `.htaccess` can capture the probe.
    pub apache_health_dir: PathBuf,
    pub apache_health_file: PathBuf,
    pub mysql_bin: PathBuf,
    pub mysqladmin_bin: PathBuf,
    pub mysql_data: PathBuf,
    pub mysql_ini: PathBuf,
    pub php_bin: PathBuf,
    pub php_ini: PathBuf,
    pub php_logs: PathBuf,
    pub phpmyadmin_dir: PathBuf,
    pub phpmyadmin_config: PathBuf,
    pub phpmyadmin_apache_conf: PathBuf,
}

impl InstallPaths {
    pub fn from_install_dir(install_dir: &Path) -> Result<Self, String> {
        let root = install_dir.to_path_buf();
        if !root.is_absolute() {
            return Err(format!(
                "install_dir must be absolute, got: {}",
                root.display()
            ));
        }

        // Ensure the install_dir itself (and every ancestor component we can check)
        // is not a symlink. An attacker controlling a symlink in the base path could
        // redirect all derived paths to an arbitrary location.
        validate_no_symlink_in_path(&root)?;

        Ok(Self {
            config: root.join("rampp.toml"),
            state_file: root.join("rampp.state"),
            log_file: root.join("logs").join("rampp.log"),
            apache_bin: root.join("apache").join("bin").join("httpd.exe"),
            apache_conf: root.join("apache").join("conf").join("httpd.conf"),
            apache_logs: root.join("logs"),
            apache_health_dir: root.join("apache").join(HEALTH_ENDPOINT_DIR),
            apache_health_file: root
                .join("apache")
                .join(HEALTH_ENDPOINT_DIR)
                .join(HEALTH_ENDPOINT_FILE),
            mysql_bin: root.join("mysql").join("bin").join("mysqld.exe"),
            mysqladmin_bin: root.join("mysql").join("bin").join("mysqladmin.exe"),
            mysql_data: root.join("mysql").join("data"),
            mysql_ini: root.join("mysql").join("my.ini"),
            php_bin: root.join("php").join("php-cgi.exe"),
            php_ini: root.join("php").join("php.ini"),
            php_logs: root.join("logs"),
            phpmyadmin_dir: root.join("phpmyadmin"),
            phpmyadmin_config: root.join("phpmyadmin").join("config.inc.php"),
            phpmyadmin_apache_conf: root.join("apache").join("conf").join("phpmyadmin.conf"),
            root,
        })
    }
}

/// Walk every existing ancestor of `path` and reject if any component is a symlink.
/// This prevents an attacker from placing a symlink in the install_dir itself to
/// redirect all derived paths (binaries, config, data) to an arbitrary location.
fn validate_no_symlink_in_path(path: &Path) -> Result<(), String> {
    // Collect all components from root down to (and including) path.
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "symlink detected in critical base path: {}",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Path doesn't exist yet — stop checking (remaining components also won't exist).
                break;
            }
            Err(e) => {
                return Err(format!(
                    "cannot verify symlink status of {}: {e}",
                    current.display()
                ));
            }
        }
    }
    Ok(())
}

/// Validate that a path is:
/// - absolute
/// - does not escape install_dir
/// - does not traverse with ".."
/// - is not a symlink (for critical paths)
pub fn validate_critical_path(
    path: &Path,
    install_dir: &Path,
    allow_symlink: bool,
) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("path must be absolute: {}", path.display()));
    }

    // Reject any component that is ".."
    for component in path.components() {
        use std::path::Component;
        if matches!(component, Component::ParentDir) {
            return Err(format!("path traversal rejected: {}", path.display()));
        }
    }

    // Must be inside install_dir
    if !path.starts_with(install_dir) {
        return Err(format!(
            "path {} is outside install_dir {}",
            path.display(),
            install_dir.display()
        ));
    }

    // No symlinks for critical paths.
    // If symlink_metadata fails for any reason other than the path not existing,
    // we treat it as a validation failure — never silently skip the check.
    if !allow_symlink {
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(format!(
                        "symlink not allowed for critical path: {}",
                        path.display()
                    ));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Path doesn't exist yet (e.g. conf generated at first run) — allow.
            }
            Err(e) => {
                return Err(format!(
                    "cannot verify symlink status of {}: {e}",
                    path.display()
                ));
            }
        }
    }

    Ok(())
}

/// Validate a user-chosen Apache DocumentRoot. Unlike `validate_critical_path`,
/// the path is NOT confined to `install_dir` — the user may point anywhere on disk.
/// Requirements: absolute, exists, is a directory, and is not a symlink (consistent
/// with RAMPP's no-symlink-following stance).
pub fn validate_document_root(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!(
            "document root must be absolute: {}",
            path.display()
        ));
    }
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("cannot access document root {}: {e}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "symlink not allowed for document root: {}",
            path.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!(
            "document root must be a directory: {}",
            path.display()
        ));
    }
    Ok(())
}

/// Resolve the Windows system root directory, robust to non-`C:` installations.
/// Resolution order: `%SystemRoot%` → `%windir%` → `<%SystemDrive%>\Windows` →
/// `C:\Windows`. The literal `C:` is only ever a last resort when no environment
/// variable identifies the system drive.
pub fn system_root() -> String {
    resolve_system_root(
        std::env::var("SystemRoot").ok(),
        std::env::var("windir").ok(),
        std::env::var("SystemDrive").ok(),
    )
}

/// Pure resolution logic for `system_root`, separated for deterministic testing
/// without mutating process environment variables.
fn resolve_system_root(
    system_root: Option<String>,
    windir: Option<String>,
    system_drive: Option<String>,
) -> String {
    let nonempty = |s: Option<String>| s.filter(|v| !v.is_empty());
    nonempty(system_root)
        .or_else(|| nonempty(windir))
        .unwrap_or_else(|| {
            let drive = nonempty(system_drive).unwrap_or_else(|| "C:".to_string());
            format!("{drive}\\Windows")
        })
}

/// Resolve a temporary directory without hardcoding a drive letter. Uses the OS
/// temp path (honors `%TMP%`/`%TEMP%`, and the OS itself falls back to the Windows
/// directory on the correct drive), so it is inherently drive-letter agnostic.
pub fn temp_dir() -> String {
    std::env::temp_dir().display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_root_prefers_system_root_var() {
        assert_eq!(
            resolve_system_root(
                Some("D:\\Windows".into()),
                Some("X:\\win".into()),
                Some("E:".into())
            ),
            "D:\\Windows"
        );
    }

    #[test]
    fn system_root_falls_back_to_windir() {
        assert_eq!(
            resolve_system_root(None, Some("D:\\Windows".into()), Some("E:".into())),
            "D:\\Windows"
        );
    }

    #[test]
    fn system_root_falls_back_to_system_drive() {
        assert_eq!(
            resolve_system_root(None, None, Some("D:".into())),
            "D:\\Windows"
        );
    }

    #[test]
    fn system_root_defaults_to_c_when_nothing_set() {
        assert_eq!(resolve_system_root(None, None, None), "C:\\Windows");
    }

    #[test]
    fn system_root_treats_empty_strings_as_unset() {
        assert_eq!(
            resolve_system_root(
                Some(String::new()),
                Some(String::new()),
                Some(String::new())
            ),
            "C:\\Windows"
        );
    }

    #[test]
    fn system_root_from_env_is_nonempty() {
        // The live resolver must always yield a usable, non-empty path.
        assert!(!system_root().is_empty());
        assert!(!temp_dir().is_empty());
    }

    #[test]
    fn rejects_relative_install_dir() {
        assert!(InstallPaths::from_install_dir(Path::new("relative/path")).is_err());
    }

    #[test]
    fn rejects_traversal() {
        let base = Path::new("C:\\rampp");
        let bad = Path::new("C:\\rampp\\..\\windows\\system32\\evil.exe");
        assert!(validate_critical_path(bad, base, false).is_err());
    }

    #[test]
    fn rejects_path_outside_install_dir() {
        let base = Path::new("C:\\rampp");
        let outside = Path::new("C:\\windows\\system32\\httpd.exe");
        assert!(validate_critical_path(outside, base, false).is_err());
    }

    #[test]
    fn accepts_valid_path() {
        let base = Path::new("C:\\rampp");
        let ok = Path::new("C:\\rampp\\apache\\bin\\httpd.exe");
        assert!(validate_critical_path(ok, base, true).is_ok());
    }

    #[test]
    fn rejects_symlink_for_critical_path() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("real.txt");
        let link = tmp.path().join("link.txt");
        std::fs::write(&target, b"data").unwrap();
        // Creating symlinks on Windows requires SeCreateSymbolicLinkPrivilege
        // or Developer Mode. Skip if unavailable (common in CI without elevation).
        match std::os::windows::fs::symlink_file(&target, &link) {
            Err(e) if e.raw_os_error() == Some(1314) => return, // ERROR_PRIVILEGE_NOT_HELD
            Err(e) => panic!("unexpected symlink error: {e}"),
            Ok(()) => {}
        }

        // validate_critical_path with allow_symlink=false must reject the symlink
        let result = validate_critical_path(&link, tmp.path(), false);
        assert!(
            result.is_err(),
            "symlink must be rejected when allow_symlink=false"
        );
        assert!(
            result.unwrap_err().contains("symlink"),
            "error message must mention symlink"
        );
    }

    #[test]
    fn allows_symlink_when_flag_set() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("real.txt");
        let link = tmp.path().join("link.txt");
        std::fs::write(&target, b"data").unwrap();
        match std::os::windows::fs::symlink_file(&target, &link) {
            Err(e) if e.raw_os_error() == Some(1314) => return, // ERROR_PRIVILEGE_NOT_HELD
            Err(e) => panic!("unexpected symlink error: {e}"),
            Ok(()) => {}
        }

        // With allow_symlink=true, the same path must pass
        assert!(
            validate_critical_path(&link, tmp.path(), true).is_ok(),
            "symlink should be allowed when allow_symlink=true"
        );
    }

    #[test]
    fn rejects_nonexistent_but_out_of_bounds_path() {
        // Path that doesn't exist yet but is outside install_dir
        let base = Path::new("C:\\rampp");
        let outside = Path::new("C:\\other\\file.txt");
        assert!(validate_critical_path(outside, base, false).is_err());
    }

    #[test]
    fn accepts_nonexistent_path_inside_install_dir() {
        // Paths that don't exist yet (e.g. conf files) must be accepted
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let nonexistent = tmp.path().join("subdir").join("file.conf");
        // File doesn't exist — validate_critical_path must accept it (no symlink check fails)
        assert!(validate_critical_path(&nonexistent, tmp.path(), false).is_ok());
    }

    #[test]
    fn install_paths_from_existing_dir_succeeds() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let paths = InstallPaths::from_install_dir(tmp.path());
        assert!(paths.is_ok());
        let p = paths.unwrap();
        assert!(p.config.starts_with(tmp.path()));
        assert!(p.apache_bin.starts_with(tmp.path()));
        assert!(p.mysql_bin.starts_with(tmp.path()));
    }

    /// The health endpoint is a RAMPP-owned artifact inside the install dir, so the
    /// path contract must name it — and it must satisfy the same critical-path rules
    /// as every other RAMPP-owned file.
    #[test]
    fn install_paths_includes_health_endpoint_paths() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let paths = InstallPaths::from_install_dir(tmp.path()).unwrap();
        assert!(paths.apache_health_dir.ends_with("rampp-health"));
        assert!(paths.apache_health_file.ends_with("health.txt"));
        assert!(
            validate_critical_path(&paths.apache_health_file, tmp.path(), false).is_ok(),
            "health endpoint file must be a valid critical path inside install_dir"
        );
    }

    #[test]
    fn install_paths_includes_phpmyadmin_paths() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let paths = InstallPaths::from_install_dir(tmp.path()).unwrap();
        assert!(paths.phpmyadmin_dir.ends_with("phpmyadmin"));
        assert!(paths.phpmyadmin_config.ends_with("config.inc.php"));
        assert!(paths.phpmyadmin_apache_conf.ends_with("phpmyadmin.conf"));
    }

    #[test]
    fn document_root_accepts_existing_dir() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        assert!(validate_document_root(tmp.path()).is_ok());
    }

    #[test]
    fn document_root_rejects_relative() {
        assert!(validate_document_root(Path::new("relative\\dir")).is_err());
    }

    #[test]
    fn document_root_rejects_missing() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(validate_document_root(&missing).is_err());
    }

    #[test]
    fn document_root_rejects_file() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("index.php");
        std::fs::write(&file, b"<?php").unwrap();
        let err = validate_document_root(&file).unwrap_err();
        assert!(
            err.contains("directory"),
            "expected directory error, got: {err}"
        );
    }

    #[test]
    fn document_root_rejects_symlink() {
        use tempfile::TempDir;
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("real_dir");
        std::fs::create_dir(&target).unwrap();
        let link = tmp.path().join("link_dir");
        // Creating directory symlinks on Windows needs privilege/Developer Mode.
        match std::os::windows::fs::symlink_dir(&target, &link) {
            Err(e) if e.raw_os_error() == Some(1314) => return, // ERROR_PRIVILEGE_NOT_HELD
            Err(e) => panic!("unexpected symlink error: {e}"),
            Ok(()) => {}
        }
        let result = validate_document_root(&link);
        assert!(result.is_err(), "symlink document_root must be rejected");
        assert!(result.unwrap_err().contains("symlink"));
    }

    #[test]
    fn install_paths_include_mysqladmin() {
        let paths = InstallPaths::from_install_dir(Path::new("C:\\rampp")).unwrap();
        assert!(paths.mysqladmin_bin.ends_with("mysqladmin.exe"));
    }
}
