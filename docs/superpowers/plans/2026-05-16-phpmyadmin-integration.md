# phpMyAdmin Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an optional "Admin" button to the MySQL UI row that configures phpMyAdmin into the existing Apache instance and opens it in the browser, with full state persistence and graceful edge-case handling.

**Architecture:** phpMyAdmin is a config toggle, not a managed service. RAMP writes/empties `apache/conf/phpmyadmin.conf` and generates `phpmyadmin/config.inc.php`, then restarts Apache via the existing event loop. State ownership remains in the reducer via new `TogglePhpMyAdmin` / `PhpMyAdminToggled(bool)` events.

**Tech Stack:** Rust, egui 0.27, crossbeam-channel, serde_json, existing RAMP modules.

---

## File Map

| Action | File | What changes |
|--------|------|-------------|
| Create | `src/phpmyadmin_conf.rs` | All phpMyAdmin file generation logic |
| Modify | `src/state.rs` | Add `phpmyadmin_enabled`, `phpmyadmin_dir_exists` to `AppState`; add `PhpMyAdminConfig` to `RampConfig`; add fields to `PersistedState` |
| Modify | `src/events.rs` | Add `TogglePhpMyAdmin`, `PhpMyAdminToggled(bool)` events; add `TogglePhpMyAdmin(bool)` side effect |
| Modify | `src/paths.rs` | Add `phpmyadmin_dir`, `phpmyadmin_config`, `phpmyadmin_apache_conf` to `InstallPaths` |
| Modify | `src/config.rs` | Add `[phpmyadmin]` toml parsing; populate `PhpMyAdminConfig` in `RampConfig` |
| Modify | `src/apache_conf.rs` | Append `Include "conf/phpmyadmin.conf"` to generated `httpd.conf` |
| Modify | `src/reducer.rs` | Handle `TogglePhpMyAdmin` and `PhpMyAdminToggled(bool)` events |
| Modify | `src/executor.rs` | Handle `SideEffect::TogglePhpMyAdmin(bool)` — write files, emit follow-up events, open browser |
| Modify | `src/ui.rs` | Add "Admin" button to MySQL row; pass full state reference |
| Modify | `src/main.rs` | Startup reconciliation; load `phpmyadmin_enabled` from `PersistedState`; ensure `phpmyadmin.conf` exists; check `phpmyadmin_dir_exists` |

---

## Task 1: Add `phpmyadmin_conf.rs` — file generation (TDD)

**Files:**
- Create: `src/phpmyadmin_conf.rs`

- [ ] **Step 1.1: Write failing tests first**

Create `src/phpmyadmin_conf.rs` with only the test module and stubs:

```rust
use crate::state::RampConfig;

pub fn generate_phpmyadmin_apache_conf(
    install_dir: &std::path::Path,
    php_port: u16,
) -> String {
    todo!()
}

pub fn generate_config_inc_php(
    mysql_port: u16,
    mysql_user: &str,
    mysql_password: &str,
    blowfish_secret: &str,
) -> String {
    todo!()
}

pub fn is_ramp_owned_config(path: &std::path::Path) -> bool {
    todo!()
}

pub fn generate_blowfish_secret(install_dir: &std::path::Path) -> String {
    todo!()
}

pub fn write_phpmyadmin_apache_conf_enabled(
    cfg: &RampConfig,
    php_port: u16,
) -> Result<(), String> {
    todo!()
}

pub fn write_phpmyadmin_apache_conf_disabled(cfg: &RampConfig) -> Result<(), String> {
    todo!()
}

pub fn ensure_phpmyadmin_apache_conf(cfg: &RampConfig, enabled: bool, php_port: u16) -> Result<(), String> {
    todo!()
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
    fn apache_conf_contains_alias() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let conf = generate_phpmyadmin_apache_conf(tmp.path(), 9000);
        assert!(conf.contains("Alias /phpmyadmin"));
        assert!(conf.contains("phpmyadmin"));
    }

    #[test]
    fn apache_conf_contains_proxy_pass_match_for_php() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        let conf = generate_phpmyadmin_apache_conf(tmp.path(), 9000);
        assert!(conf.contains("ProxyPassMatch"));
        assert!(conf.contains("fcgi://127.0.0.1:9000"));
    }

    #[test]
    fn apache_conf_requires_local() {
        let tmp = TempDir::new().unwrap();
        let conf = generate_phpmyadmin_apache_conf(tmp.path(), 9000);
        assert!(conf.contains("Require local"));
    }

    #[test]
    fn config_inc_php_contains_marker() {
        let php = generate_config_inc_php(3306, "root", "", "secret12345678901234567890123456");
        assert!(php.contains("RAMP — generated config.inc.php"));
    }

    #[test]
    fn config_inc_php_contains_credentials() {
        let php = generate_config_inc_php(3306, "admin", "pass123", "secret12345678901234567890123456");
        assert!(php.contains("'admin'"));
        assert!(php.contains("'pass123'"));
        assert!(php.contains("3306"));
    }

    #[test]
    fn config_inc_php_contains_blowfish_secret() {
        let secret = "abcdefghij1234567890abcdefghij12";
        let php = generate_config_inc_php(3306, "root", "", secret);
        assert!(php.contains(secret));
    }

    #[test]
    fn is_ramp_owned_config_true_when_marker_present() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.inc.php");
        std::fs::write(&path, "<?php\n// RAMP — generated config.inc.php (do not remove this line — RAMP uses it to detect generated configs)\n").unwrap();
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
        assert!(secret.chars().all(|c| c.is_ascii_hexdigit()), "blowfish_secret must be hex");
    }

    #[test]
    fn write_enabled_creates_populated_conf() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        std::fs::create_dir_all(tmp.path().join("apache").join("conf")).unwrap();
        write_phpmyadmin_apache_conf_enabled(&cfg, 9000).unwrap();
        let content = std::fs::read_to_string(
            tmp.path().join("apache").join("conf").join("phpmyadmin.conf")
        ).unwrap();
        assert!(content.contains("Alias /phpmyadmin"));
    }

    #[test]
    fn write_disabled_creates_empty_conf() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        std::fs::create_dir_all(tmp.path().join("apache").join("conf")).unwrap();
        write_phpmyadmin_apache_conf_disabled(&cfg).unwrap();
        let content = std::fs::read_to_string(
            tmp.path().join("apache").join("conf").join("phpmyadmin.conf")
        ).unwrap();
        assert!(content.is_empty());
    }
}
```

- [ ] **Step 1.2: Add `phpmyadmin_conf` to `src/lib.rs`** (needed for tests to compile)

Open `src/lib.rs` and add:
```rust
pub mod phpmyadmin_conf;
```

- [ ] **Step 1.3: Add stub `PhpMyAdminConfig` to `src/state.rs`** (needed for test_cfg to compile)

In `src/state.rs`, after `PhpConfig`:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhpMyAdminConfig {
    pub mysql_user: String,
    pub mysql_password: String,
}
```

And add to `RampConfig`:
```rust
pub struct RampConfig {
    pub install_dir: PathBuf,
    pub apache: ApacheConfig,
    pub mysql: MysqlConfig,
    pub php: PhpConfig,
    pub phpmyadmin: PhpMyAdminConfig,  // ADD THIS
}
```

- [ ] **Step 1.4: Run tests to verify they fail (not compile error)**

```
cargo test phpmyadmin_conf
```
Expected: compile errors on `todo!()` panics at runtime, or `PhpMyAdminConfig` not found — fix compile errors only, do not implement yet. The tests should compile and panic at `todo!()`.

- [ ] **Step 1.5: Implement `generate_phpmyadmin_apache_conf`**

```rust
pub fn generate_phpmyadmin_apache_conf(
    install_dir: &std::path::Path,
    php_port: u16,
) -> String {
    let pma_dir = install_dir.join("phpmyadmin");
    let pma_dir_s = pma_dir.display().to_string().replace('\\', "/");

    format!(
        r#"# RAMP — phpMyAdmin enabled (do not remove this line)
Alias /phpmyadmin "{pma_dir_s}"
<Directory "{pma_dir_s}">
    Options None
    AllowOverride None
    Require local
</Directory>

# Route .php files under /phpmyadmin through PHP-CGI FastCGI.
# Uses ProxyPassMatch (not FilesMatch+SetHandler) to avoid the Windows
# mod_proxy_fcgi drive-letter URL parsing bug (same workaround as httpd.conf).
ProxyFCGIBackendType GENERIC
ProxyPassMatch "^/phpmyadmin/(.+\.php(/.*)?)$" "fcgi://127.0.0.1:{php_port}/phpmyadmin/$1"
"#
    )
}
```

- [ ] **Step 1.6: Implement `generate_config_inc_php`**

```rust
pub fn generate_config_inc_php(
    mysql_port: u16,
    mysql_user: &str,
    mysql_password: &str,
    blowfish_secret: &str,
) -> String {
    format!(
        r#"<?php
// RAMP — generated config.inc.php (do not remove this line — RAMP uses it to detect generated configs)
$cfg['blowfish_secret'] = '{blowfish_secret}';
$cfg['Servers'][1]['auth_type'] = 'config';
$cfg['Servers'][1]['host'] = '127.0.0.1';
$cfg['Servers'][1]['port'] = {mysql_port};
$cfg['Servers'][1]['user'] = '{mysql_user}';
$cfg['Servers'][1]['password'] = '{mysql_password}';
$cfg['Servers'][1]['AllowNoPassword'] = true;
$cfg['UploadDir'] = '';
$cfg['SaveDir'] = '';
"#
    )
}
```

- [ ] **Step 1.7: Implement `is_ramp_owned_config`**

```rust
pub fn is_ramp_owned_config(path: &std::path::Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    content.contains("RAMP — generated config.inc.php")
}
```

- [ ] **Step 1.8: Implement `generate_blowfish_secret`**

```rust
pub fn generate_blowfish_secret(install_dir: &std::path::Path) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Combine time nanos with process ID for local-dev-grade uniqueness.
    // blowfish_secret only protects session cookie encryption for a local tool.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0xdeadbeef);
    let pid = std::process::id();
    // XOR with install_dir path hash for additional variance
    let path_hash: u64 = install_dir
        .display()
        .to_string()
        .bytes()
        .enumerate()
        .fold(0u64, |acc, (i, b)| acc.wrapping_add((b as u64).wrapping_mul(i as u64 + 1)));
    let combined = (nanos as u64).wrapping_mul(0x9e3779b97f4a7c15)
        ^ (pid as u64).wrapping_mul(0x6c62272e07bb0142)
        ^ path_hash;
    // Format as 16-char hex, duplicated to get 32 chars
    let half = format!("{combined:016x}");
    format!("{half}{half}")
        .chars()
        .take(32)
        .collect()
}
```

- [ ] **Step 1.9: Implement `write_phpmyadmin_apache_conf_enabled`, `write_phpmyadmin_apache_conf_disabled`, `ensure_phpmyadmin_apache_conf`**

```rust
pub fn write_phpmyadmin_apache_conf_enabled(
    cfg: &RampConfig,
    php_port: u16,
) -> Result<(), String> {
    let conf_path = cfg.install_dir
        .join("apache")
        .join("conf")
        .join("phpmyadmin.conf");
    let dir = conf_path.parent().ok_or("phpmyadmin.conf has no parent")?;
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("cannot create apache/conf dir: {e}"))?;
    let content = generate_phpmyadmin_apache_conf(&cfg.install_dir, php_port);
    crate::config::atomic_write(&conf_path, content.as_bytes())
        .map_err(|e| format!("cannot write phpmyadmin.conf: {e}"))
}

pub fn write_phpmyadmin_apache_conf_disabled(cfg: &RampConfig) -> Result<(), String> {
    let conf_path = cfg.install_dir
        .join("apache")
        .join("conf")
        .join("phpmyadmin.conf");
    let dir = conf_path.parent().ok_or("phpmyadmin.conf has no parent")?;
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("cannot create apache/conf dir: {e}"))?;
    crate::config::atomic_write(&conf_path, b"")
        .map_err(|e| format!("cannot write phpmyadmin.conf: {e}"))
}

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
```

- [ ] **Step 1.10: Run tests — all must pass**

```
cargo test phpmyadmin_conf
```
Expected: all tests in `phpmyadmin_conf::tests` pass.

- [ ] **Step 1.11: Run lint**

```
cargo clippy -- -D warnings
```
Fix any warnings before proceeding.

- [ ] **Step 1.12: Commit**

```
git add src/phpmyadmin_conf.rs src/state.rs src/lib.rs
git commit -m "feat: add phpmyadmin_conf module with file generation"
```

---

## Task 2: Extend `paths.rs`, `state.rs`, `events.rs` with new fields

**Files:**
- Modify: `src/paths.rs`
- Modify: `src/state.rs`
- Modify: `src/events.rs`

- [ ] **Step 2.1: Write failing tests for new path fields**

Add to the `paths::tests` module in `src/paths.rs`:

```rust
#[test]
fn install_paths_includes_phpmyadmin_paths() {
    use tempfile::TempDir;
    let tmp = TempDir::new().unwrap();
    let paths = InstallPaths::from_install_dir(tmp.path()).unwrap();
    assert!(paths.phpmyadmin_dir.ends_with("phpmyadmin"));
    assert!(paths.phpmyadmin_config.ends_with("config.inc.php"));
    assert!(paths.phpmyadmin_apache_conf.ends_with("phpmyadmin.conf"));
}
```

Run `cargo test paths::tests::install_paths_includes_phpmyadmin_paths` — expected: compile error (`phpmyadmin_dir` doesn't exist).

- [ ] **Step 2.2: Add phpmyadmin paths to `InstallPaths`**

In `src/paths.rs`, extend `InstallPaths`:
```rust
pub struct InstallPaths {
    pub root: PathBuf,
    pub config: PathBuf,
    pub state_file: PathBuf,
    pub log_file: PathBuf,
    pub apache_bin: PathBuf,
    pub apache_conf: PathBuf,
    pub apache_logs: PathBuf,
    pub mysql_bin: PathBuf,
    pub mysql_data: PathBuf,
    pub mysql_ini: PathBuf,
    pub php_bin: PathBuf,
    pub php_ini: PathBuf,
    pub php_logs: PathBuf,
    pub phpmyadmin_dir: PathBuf,         // <install>/phpmyadmin/
    pub phpmyadmin_config: PathBuf,      // <install>/phpmyadmin/config.inc.php
    pub phpmyadmin_apache_conf: PathBuf, // <install>/apache/conf/phpmyadmin.conf
}
```

In `InstallPaths::from_install_dir`, add to the `Ok(Self { ... })` block:
```rust
phpmyadmin_dir: root.join("phpmyadmin"),
phpmyadmin_config: root.join("phpmyadmin").join("config.inc.php"),
phpmyadmin_apache_conf: root.join("apache").join("conf").join("phpmyadmin.conf"),
```

- [ ] **Step 2.3: Run path tests**

```
cargo test paths::tests
```
Expected: all pass including the new test.

- [ ] **Step 2.4: Write failing tests for `PersistedState` new fields**

Add to `config::tests` in `src/config.rs`:

```rust
#[test]
fn persisted_state_defaults_phpmyadmin_enabled_to_false() {
    // Old ramp.state without phpmyadmin_enabled must deserialize cleanly
    let json = r#"{"apache_desired":"Stopped","mysql_desired":"Stopped","php_desired":"Stopped"}"#;
    let persisted: crate::state::PersistedState = serde_json::from_str(json).unwrap();
    assert!(!persisted.phpmyadmin_enabled);
    assert!(persisted.phpmyadmin_blowfish_secret.is_none());
}
```

Run `cargo test config::tests::persisted_state_defaults_phpmyadmin_enabled_to_false` — expected: compile error.

- [ ] **Step 2.5: Add fields to `PersistedState` in `src/state.rs`**

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedState {
    pub apache_desired: DesiredServiceState,
    pub mysql_desired: DesiredServiceState,
    #[serde(default = "DesiredServiceState::default_stopped")]
    pub php_desired: DesiredServiceState,
    #[serde(default)]
    pub phpmyadmin_enabled: bool,
    #[serde(default)]
    pub phpmyadmin_blowfish_secret: Option<String>,
}
```

Also update `PersistedState::default_stopped`:
```rust
pub fn default_stopped() -> Self {
    Self {
        apache_desired: DesiredServiceState::Stopped,
        mysql_desired: DesiredServiceState::Stopped,
        php_desired: DesiredServiceState::Stopped,
        phpmyadmin_enabled: false,
        phpmyadmin_blowfish_secret: None,
    }
}
```

- [ ] **Step 2.6: Add `phpmyadmin_enabled` and `phpmyadmin_dir_exists` to `AppState`**

In `src/state.rs`, extend `AppState`:
```rust
pub struct AppState {
    pub apache: ServiceStatus,
    pub mysql: ServiceStatus,
    pub php: ServiceStatus,
    pub config: RampConfig,
    pub ports: PortState,
    pub phpmyadmin_enabled: bool,
    pub phpmyadmin_dir_exists: bool,
}
```

Update `AppState::new`:
```rust
pub fn new(config: RampConfig) -> Self {
    Self {
        apache: ServiceStatus::new(),
        mysql: ServiceStatus::new(),
        php: ServiceStatus::new(),
        config,
        ports: PortState::new(),
        phpmyadmin_enabled: false,
        phpmyadmin_dir_exists: false,
    }
}
```

- [ ] **Step 2.7: Add new events and side effect to `src/events.rs`**

```rust
pub enum Event {
    // ... existing variants ...
    TogglePhpMyAdmin,
    PhpMyAdminToggled(bool),
}

pub enum SideEffect {
    // ... existing variants ...
    TogglePhpMyAdmin(bool),
}
```

- [ ] **Step 2.8: Run all tests**

```
cargo test
```
Expected: all existing tests pass; new config test passes.

- [ ] **Step 2.9: Commit**

```
git add src/paths.rs src/state.rs src/events.rs src/config.rs
git commit -m "feat: add phpmyadmin state fields, paths, and events"
```

---

## Task 3: Extend `config.rs` — parse `[phpmyadmin]` from `ramp.toml`

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 3.1: Write failing tests**

Add to `config::tests` in `src/config.rs`:

```rust
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
```

Run `cargo test config::tests::load_config_defaults_phpmyadmin_when_section_absent` — expected: compile error or test failure.

- [ ] **Step 3.2: Add `TomlPhpMyAdmin` and update parsing in `src/config.rs`**

Add after `TomlPhp`:
```rust
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
```

Update `TomlRoot`:
```rust
struct TomlRoot {
    install_dir: PathBuf,
    apache: TomlApache,
    mysql: TomlMysql,
    #[serde(default)]
    php: TomlPhp,
    #[serde(default)]
    phpmyadmin: TomlPhpMyAdmin,
}
```

Update `validate_and_build` to populate `phpmyadmin`:
```rust
Ok(RampConfig {
    install_dir: install_dir.to_path_buf(),
    apache: ApacheConfig { ... },
    mysql: MysqlConfig { ... },
    php: PhpConfig { ... },
    phpmyadmin: crate::state::PhpMyAdminConfig {
        mysql_user: doc.phpmyadmin.mysql_user,
        mysql_password: doc.phpmyadmin.mysql_password,
    },
})
```

Also update `write_default_config` — the default toml does NOT include `[phpmyadmin]` (it's optional). No change needed there.

- [ ] **Step 3.3: Run config tests**

```
cargo test config::tests
```
Expected: all pass including two new tests.

- [ ] **Step 3.4: Commit**

```
git add src/config.rs
git commit -m "feat: parse optional [phpmyadmin] credentials from ramp.toml"
```

---

## Task 4: Extend `apache_conf.rs` — add `Include` directive

**Files:**
- Modify: `src/apache_conf.rs`

- [ ] **Step 4.1: Write failing test**

Add to `apache_conf::tests` in `src/apache_conf.rs`:

```rust
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
```

Note: `test_cfg` in `apache_conf::tests` needs updating to include `phpmyadmin` field. Update it:
```rust
fn test_cfg(dir: &Path) -> RampConfig {
    RampConfig {
        install_dir: dir.to_path_buf(),
        apache: ApacheConfig { ... },
        mysql: MysqlConfig { ... },
        php: PhpConfig { ... },
        phpmyadmin: crate::state::PhpMyAdminConfig {
            mysql_user: "root".to_string(),
            mysql_password: String::new(),
        },
    }
}
```

Run `cargo test apache_conf::tests::generates_conf_with_phpmyadmin_include` — expected: test fails (include not present).

- [ ] **Step 4.2: Add Include directive to `generate_httpd_conf_with_ports`**

At the end of the format string in `generate_httpd_conf_with_ports`, add:

```rust
// After the last </IfModule> block, append:
r#"
# phpMyAdmin — managed by RAMP (do not remove this line)
Include "conf/phpmyadmin.conf"
"#
```

The full end of the format string should be:

```
<IfModule mime_module>
    TypesConfig conf/mime.types
    AddType application/x-compress .Z
    AddType application/x-gzip .gz .tgz
</IfModule>

# phpMyAdmin — managed by RAMP (do not remove this line)
Include "conf/phpmyadmin.conf"
```

- [ ] **Step 4.3: Fix all existing `test_cfg` usages in other test modules**

Search for all test helper functions named `test_cfg` across all `_conf.rs` files and add `phpmyadmin: crate::state::PhpMyAdminConfig { mysql_user: "root".to_string(), mysql_password: String::new() }` to each one.

Files to check: `src/apache_conf.rs`, `src/php_conf.rs`, `src/mysql_conf.rs`, `src/reducer.rs`.

In `src/reducer.rs`, update `make_state()`:
```rust
fn make_state() -> AppState {
    let config = RampConfig {
        install_dir: std::path::PathBuf::from("C:\\ramp"),
        apache: ApacheConfig { ... },
        mysql: MysqlConfig { ... },
        php: PhpConfig { ... },
        phpmyadmin: crate::state::PhpMyAdminConfig {
            mysql_user: "root".to_string(),
            mysql_password: String::new(),
        },
    };
    AppState::new(config)
}
```

- [ ] **Step 4.4: Run all tests**

```
cargo test
```
Expected: all pass.

- [ ] **Step 4.5: Commit**

```
git add src/apache_conf.rs src/reducer.rs src/php_conf.rs src/mysql_conf.rs
git commit -m "feat: add phpmyadmin Include directive to generated httpd.conf"
```

---

## Task 5: Extend `reducer.rs` — handle phpMyAdmin events

**Files:**
- Modify: `src/reducer.rs`

- [ ] **Step 5.1: Write failing reducer tests**

Add to `reducer::tests` in `src/reducer.rs`:

```rust
// ── phpMyAdmin toggle ─────────────────────────────────────────────────

fn make_state_all_running() -> AppState {
    let mut state = make_state();
    state.apache.state = ServiceState::Running;
    state.mysql.state = ServiceState::Running;
    state.php.state = ServiceState::Running;
    state.phpmyadmin_dir_exists = true;
    state
}

#[test]
fn toggle_phpmyadmin_on_when_services_running_emits_side_effect() {
    let state = make_state_all_running();
    let (new_state, effects) = reducer(state, Event::TogglePhpMyAdmin);
    assert!(effects.iter().any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(true))));
    // State unchanged until PhpMyAdminToggled arrives
    assert!(!new_state.phpmyadmin_enabled);
}

#[test]
fn toggle_phpmyadmin_off_when_enabled_emits_side_effect() {
    let mut state = make_state_all_running();
    state.phpmyadmin_enabled = true;
    let (_, effects) = reducer(state, Event::TogglePhpMyAdmin);
    assert!(effects.iter().any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(false))));
}

#[test]
fn toggle_phpmyadmin_ignored_when_mysql_not_running() {
    let mut state = make_state_all_running();
    state.mysql.state = ServiceState::Stopped;
    let (new_state, effects) = reducer(state, Event::TogglePhpMyAdmin);
    assert!(!effects.iter().any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(_))));
    assert!(effects.iter().any(|e| matches!(e, SideEffect::LogEvent(_))));
    assert!(!new_state.phpmyadmin_enabled);
}

#[test]
fn toggle_phpmyadmin_ignored_when_php_not_running() {
    let mut state = make_state_all_running();
    state.php.state = ServiceState::Stopped;
    let (_, effects) = reducer(state, Event::TogglePhpMyAdmin);
    assert!(!effects.iter().any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(_))));
}

#[test]
fn toggle_phpmyadmin_ignored_when_apache_not_running() {
    let mut state = make_state_all_running();
    state.apache.state = ServiceState::Stopped;
    let (_, effects) = reducer(state, Event::TogglePhpMyAdmin);
    assert!(!effects.iter().any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(_))));
}

#[test]
fn toggle_phpmyadmin_ignored_when_dir_missing() {
    let mut state = make_state_all_running();
    state.phpmyadmin_dir_exists = false;
    let (_, effects) = reducer(state, Event::TogglePhpMyAdmin);
    assert!(!effects.iter().any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(_))));
}

#[test]
fn phpmyadmin_toggled_true_sets_enabled_and_persists() {
    let state = make_state();
    let (new_state, effects) = reducer(state, Event::PhpMyAdminToggled(true));
    assert!(new_state.phpmyadmin_enabled);
    assert!(effects.iter().any(|e| matches!(e, SideEffect::PersistDesiredState)));
}

#[test]
fn phpmyadmin_toggled_false_clears_enabled_and_persists() {
    let mut state = make_state();
    state.phpmyadmin_enabled = true;
    let (new_state, effects) = reducer(state, Event::PhpMyAdminToggled(false));
    assert!(!new_state.phpmyadmin_enabled);
    assert!(effects.iter().any(|e| matches!(e, SideEffect::PersistDesiredState)));
}

#[test]
fn mysql_process_exit_while_phpmyadmin_enabled_emits_toggle_off() {
    let mut state = make_state_all_running();
    state.phpmyadmin_enabled = true;
    let (_, effects) = reducer(
        state,
        Event::ProcessExit { service: Service::Mysql, exit_code: Some(1) },
    );
    assert!(effects.iter().any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(false))));
}

#[test]
fn php_process_exit_while_phpmyadmin_enabled_emits_toggle_off() {
    let mut state = make_state_all_running();
    state.phpmyadmin_enabled = true;
    let (_, effects) = reducer(
        state,
        Event::ProcessExit { service: Service::Php, exit_code: Some(1) },
    );
    assert!(effects.iter().any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(false))));
}

#[test]
fn mysql_process_exit_while_phpmyadmin_disabled_does_not_emit_toggle() {
    let mut state = make_state_all_running();
    state.phpmyadmin_enabled = false;
    let (_, effects) = reducer(
        state,
        Event::ProcessExit { service: Service::Mysql, exit_code: Some(1) },
    );
    assert!(!effects.iter().any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(_))));
}
```

Run `cargo test reducer::tests::toggle_phpmyadmin` — expected: compile error (events not yet handled).

- [ ] **Step 5.2: Add `TogglePhpMyAdmin` handler to reducer**

In `src/reducer.rs`, inside the `match event { ... }` block, add before the closing `}`:

```rust
// ── phpMyAdmin toggle ─────────────────────────────────────────────────
Event::TogglePhpMyAdmin => {
    let all_running = state.apache.state == ServiceState::Running
        && state.mysql.state == ServiceState::Running
        && state.php.state == ServiceState::Running;

    if !state.phpmyadmin_dir_exists {
        effects.push(SideEffect::LogEvent(
            "phpMyAdmin: directory not found — cannot toggle".to_string(),
        ));
    } else if !all_running {
        effects.push(SideEffect::LogEvent(
            "phpMyAdmin: MySQL, PHP, and Apache must all be running".to_string(),
        ));
    } else {
        let target = !state.phpmyadmin_enabled;
        effects.push(SideEffect::TogglePhpMyAdmin(target));
    }
}

Event::PhpMyAdminToggled(enabled) => {
    state.phpmyadmin_enabled = enabled;
    effects.push(SideEffect::PersistDesiredState);
    effects.push(SideEffect::LogEvent(format!(
        "phpMyAdmin: {}",
        if enabled { "enabled" } else { "disabled" }
    )));
}
```

- [ ] **Step 5.3: Update `ConfigReloaded` handler to re-check `phpmyadmin_dir_exists`**

The spec requires `phpmyadmin_dir_exists` to be rechecked on config reload (install_dir may change). In the existing `Event::ConfigReloaded` handler in `src/reducer.rs`, after updating `state.config`:

```rust
Event::ConfigReloaded(new_config) => {
    state.config = *new_config;
    // Re-check whether the phpmyadmin dir still exists under the (possibly new) install_dir
    let pma_dir = state.config.install_dir.join("phpmyadmin");
    state.phpmyadmin_dir_exists = pma_dir.exists() && pma_dir.is_dir();
    // If phpmyadmin was enabled but dir no longer exists, auto-disable
    if state.phpmyadmin_enabled && !state.phpmyadmin_dir_exists {
        effects.push(SideEffect::TogglePhpMyAdmin(false));
    }
    effects.push(SideEffect::LogEvent(
        "config reloaded — restart services to apply changes".to_string(),
    ));
}
```

Note: this is an I/O call (`exists()`) inside the reducer, which is normally pure. However, `phpmyadmin_dir_exists` is a cache of filesystem state — this is the designated refresh point, consistent with how `AppState::phpmyadmin_dir_exists` is documented as "checked at startup + ConfigReloaded". The call is read-only and never panics.

Add a reducer test for this:
```rust
#[test]
fn config_reloaded_rechecks_phpmyadmin_dir_exists() {
    let mut state = make_state();
    state.phpmyadmin_enabled = true;
    state.phpmyadmin_dir_exists = true;
    // Reload config with an install_dir that has no phpmyadmin subdir
    let mut new_config = state.config.clone();
    new_config.install_dir = std::path::PathBuf::from("C:\\nonexistent_ramp_test_dir_12345");
    let (new_state, effects) = reducer(state, Event::ConfigReloaded(Box::new(new_config)));
    assert!(!new_state.phpmyadmin_dir_exists);
    // Should auto-disable since dir is gone
    assert!(effects.iter().any(|e| matches!(e, SideEffect::TogglePhpMyAdmin(false))));
}
```

- [ ] **Step 5.5: Add auto-disable on MySQL/PHP exit**

In the existing `Event::ProcessExit` handler, after the match arm for `ServiceState::Starting | ServiceState::Running` (the crash arm), add:

```rust
// Auto-disable phpMyAdmin if MySQL or PHP crashes/stops
if state.phpmyadmin_enabled
    && matches!(svc, Service::Mysql | Service::Php)
{
    effects.push(SideEffect::TogglePhpMyAdmin(false));
}
```

Place this at the end of the `Event::ProcessExit` match — outside the inner `match state.service(svc).state` block, after the state has been updated.

The full `ProcessExit` handler ends like this:
```rust
        other => {
            effects.push(SideEffect::LogEvent(format!(
                "{svc}: ProcessExit ignored in state {other}"
            )));
        }
    }
    // Auto-disable phpMyAdmin if a required dependency crashes
    if state.phpmyadmin_enabled && matches!(svc, Service::Mysql | Service::Php) {
        effects.push(SideEffect::TogglePhpMyAdmin(false));
    }
}
```

- [ ] **Step 5.6: Run reducer tests**

```
cargo test reducer::tests
```
Expected: all pass including new phpmyadmin tests.

- [ ] **Step 5.7: Commit**

```
git add src/reducer.rs
git commit -m "feat: handle TogglePhpMyAdmin and PhpMyAdminToggled events in reducer"
```

---

## Task 6: Extend `executor.rs` — handle `SideEffect::TogglePhpMyAdmin`

**Files:**
- Modify: `src/executor.rs`

- [ ] **Step 6.1: Add `do_toggle_phpmyadmin` to `Executor`**

In `src/executor.rs`, add the `do_toggle_phpmyadmin` method to `impl Executor`:

```rust
fn do_toggle_phpmyadmin(&mut self, enable: bool, state: &AppState) {
    let pma_dir = self.config.install_dir.join("phpmyadmin");

    if enable && !pma_dir.exists() {
        log::error!("phpMyAdmin: directory not found at {}", pma_dir.display());
        self.log.push(format!(
            "ERROR: phpMyAdmin directory not found at {}",
            pma_dir.display()
        ));
        // Emit toggled(false) so reducer knows the attempt failed
        let _ = self.tx.send(Event::PhpMyAdminToggled(false));
        return;
    }

    let php_port = self.effective_port(Service::Php);

    if enable {
        // Generate/update config.inc.php if RAMP owns it (or it doesn't exist yet)
        let config_path = pma_dir.join("config.inc.php");
        let should_write = !config_path.exists()
            || crate::phpmyadmin_conf::is_ramp_owned_config(&config_path);

        if should_write {
            // Get or generate blowfish_secret from persisted state
            // (persisted state is read by executor indirectly via the state snapshot)
            // We generate a new one if the state doesn't have one yet — it'll be
            // persisted on the next PersistDesiredState cycle.
            // For now, load from the state file directly.
            let blowfish_secret = self.load_or_generate_blowfish_secret();

            let mysql_port = self.effective_port(Service::Mysql);
            let content = crate::phpmyadmin_conf::generate_config_inc_php(
                mysql_port,
                &self.config.phpmyadmin.mysql_user,
                &self.config.phpmyadmin.mysql_password,
                &blowfish_secret,
            );
            if let Err(e) = crate::config::atomic_write(&config_path, content.as_bytes()) {
                log::error!("phpMyAdmin: cannot write config.inc.php: {e}");
                self.log.push(format!("ERROR: phpMyAdmin config.inc.php write failed: {e}"));
                let _ = self.tx.send(Event::PhpMyAdminToggled(false));
                return;
            }
        }
    }

    // Write phpmyadmin.conf (enabled or empty)
    let result = if enable {
        crate::phpmyadmin_conf::write_phpmyadmin_apache_conf_enabled(&self.config, php_port)
    } else {
        crate::phpmyadmin_conf::write_phpmyadmin_apache_conf_disabled(&self.config)
    };

    if let Err(e) = result {
        log::error!("phpMyAdmin: cannot write phpmyadmin.conf: {e}");
        self.log.push(format!("ERROR: phpMyAdmin conf write failed: {e}"));
        let _ = self.tx.send(Event::PhpMyAdminToggled(!enable)); // revert
        return;
    }

    // Notify reducer that toggle completed
    let _ = self.tx.send(Event::PhpMyAdminToggled(enable));

    // Restart Apache to pick up the new include content
    let _ = self.tx.send(Event::RestartService(Service::Apache));

    // Open browser on enable only
    if enable {
        let apache_port = self.effective_port(Service::Apache);
        let url = format!("http://127.0.0.1:{apache_port}/phpmyadmin/");
        log::info!("phpMyAdmin: opening {url}");
        self.log.push(format!("phpMyAdmin: opening {url}"));
        // Spawn detached — failure is non-fatal (user can open manually)
        if let Err(e) = std::process::Command::new("cmd")
            .args(["/c", "start", "", &url])
            .spawn()
        {
            log::warn!("phpMyAdmin: could not open browser: {e}");
        }
    }
}

fn load_or_generate_blowfish_secret(&self) -> String {
    let state_path = self.config.install_dir.join("ramp.state");
    if let Ok(data) = std::fs::read(&state_path) {
        if let Ok(persisted) = serde_json::from_slice::<crate::state::PersistedState>(&data) {
            if let Some(secret) = persisted.phpmyadmin_blowfish_secret {
                return secret;
            }
        }
    }
    // Generate and immediately persist the new secret
    let secret = crate::phpmyadmin_conf::generate_blowfish_secret(&self.config.install_dir);
    // Persist it by reading current state, updating, and writing back
    let state_path = self.config.install_dir.join("ramp.state");
    if let Ok(data) = std::fs::read(&state_path) {
        if let Ok(mut persisted) = serde_json::from_slice::<crate::state::PersistedState>(&data) {
            persisted.phpmyadmin_blowfish_secret = Some(secret.clone());
            if let Ok(json) = serde_json::to_vec_pretty(&persisted) {
                let _ = crate::config::atomic_write(&state_path, &json);
            }
        }
    }
    secret
}
```

- [ ] **Step 6.2: Wire `SideEffect::TogglePhpMyAdmin` into `execute()`**

In the `execute` method's match block, add:

```rust
SideEffect::TogglePhpMyAdmin(enable) => self.do_toggle_phpmyadmin(enable, state),
```

- [ ] **Step 6.3: Add `phpmyadmin_conf` to `use` imports in `executor.rs`** (already available via `crate::`)

No import needed — accessed as `crate::phpmyadmin_conf::*`.

- [ ] **Step 6.4: Run all tests**

```
cargo test
```
Expected: all pass.

- [ ] **Step 6.5: Run lint**

```
cargo clippy -- -D warnings
```

- [ ] **Step 6.6: Commit**

```
git add src/executor.rs
git commit -m "feat: executor handles SideEffect::TogglePhpMyAdmin — writes configs and opens browser"
```

---

## Task 7: Extend `ui.rs` — "Admin" button on MySQL row

**Files:**
- Modify: `src/ui.rs`

- [ ] **Step 7.1: Update `service_row` signature to accept full `AppState`**

Change `service_row` to accept state fields needed for cross-service checks:

```rust
fn service_row(
    ui: &mut egui::Ui,
    tx: &Sender<Event>,
    svc: Service,
    status: &crate::state::ServiceStatus,
    configured_port: u16,
    // New params for MySQL Admin button:
    phpmyadmin_enabled: bool,
    phpmyadmin_dir_exists: bool,
    mysql_running: bool,
    php_running: bool,
    apache_running: bool,
)
```

- [ ] **Step 7.2: Update all three `service_row` call sites in `update()`**

Replace:
```rust
service_row(ui, &self.tx, Service::Apache, &state.apache, state.config.apache.port);
service_row(ui, &self.tx, Service::Mysql, &state.mysql, state.config.mysql.port);
service_row(ui, &self.tx, Service::Php, &state.php, state.config.php.port);
```

With:
```rust
let mysql_running = state.mysql.state == ServiceState::Running;
let php_running = state.php.state == ServiceState::Running;
let apache_running = state.apache.state == ServiceState::Running;

service_row(
    ui, &self.tx, Service::Apache, &state.apache, state.config.apache.port,
    false, false, mysql_running, php_running, apache_running,
);
service_row(
    ui, &self.tx, Service::Mysql, &state.mysql, state.config.mysql.port,
    state.phpmyadmin_enabled, state.phpmyadmin_dir_exists,
    mysql_running, php_running, apache_running,
);
service_row(
    ui, &self.tx, Service::Php, &state.php, state.config.php.port,
    false, false, mysql_running, php_running, apache_running,
);
```

- [ ] **Step 7.3: Add Admin button to `service_row` for MySQL**

Inside `service_row`, within the `ui.with_layout(right_to_left)` block, add BEFORE the Stop button (renders leftmost in RTL):

```rust
// Admin button — only rendered for MySQL row
if svc == Service::Mysql {
    let all_up = mysql_running && php_running && apache_running;
    let can_admin = all_up && phpmyadmin_dir_exists;

    let btn_label = if phpmyadmin_enabled { "Admin ■" } else { "Admin ▶" };

    let btn = egui::Button::new(btn_label);
    let response = ui.add_enabled(can_admin, btn);

    // Hover tooltip explains why it's disabled
    if !can_admin {
        let tooltip = if !phpmyadmin_dir_exists {
            "phpMyAdmin not found in install directory".to_string()
        } else {
            "MySQL, PHP, and Apache must all be running".to_string()
        };
        response.on_disabled_hover_text(tooltip);
    }

    if response.clicked() {
        let _ = tx.send(Event::TogglePhpMyAdmin);
    }
}
```

- [ ] **Step 7.4: Build to check for compile errors**

```
cargo build
```
Fix any compile errors (signature mismatches, missing fields).

- [ ] **Step 7.5: Run all tests**

```
cargo test
```
Expected: all pass.

- [ ] **Step 7.6: Commit**

```
git add src/ui.rs
git commit -m "feat: add Admin button to MySQL service row in UI"
```

---

## Task 8: Extend `main.rs` — startup reconciliation

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 8.1: Declare `phpmyadmin_conf` module**

At the top of `src/main.rs`, add:
```rust
mod phpmyadmin_conf;
```

- [ ] **Step 8.2: Load `phpmyadmin_enabled` and `phpmyadmin_blowfish_secret` from `PersistedState`**

After the existing persisted state loading:
```rust
app_state.apache.desired = persisted.apache_desired;
app_state.mysql.desired = persisted.mysql_desired;
app_state.php.desired = persisted.php_desired;
```

Add:
```rust
// phpMyAdmin startup reconciliation
let phpmyadmin_dir = config.install_dir.join("phpmyadmin");
app_state.phpmyadmin_dir_exists = phpmyadmin_dir.exists() && phpmyadmin_dir.is_dir();

if persisted.phpmyadmin_enabled && !app_state.phpmyadmin_dir_exists {
    log::warn!(
        "phpMyAdmin was enabled but directory not found at {} — disabling",
        phpmyadmin_dir.display()
    );
    // Force disable: write empty conf and persist
    if let Err(e) = phpmyadmin_conf::write_phpmyadmin_apache_conf_disabled(&config) {
        log::warn!("phpMyAdmin: could not write empty phpmyadmin.conf: {e}");
    }
    // Persist the forced-disabled state
    let mut updated_persisted = persisted.clone();
    updated_persisted.phpmyadmin_enabled = false;
    let state_path = config.install_dir.join("ramp.state");
    if let Ok(data) = serde_json::to_vec_pretty(&updated_persisted) {
        let _ = config::atomic_write(&state_path, &data);
    }
    app_state.phpmyadmin_enabled = false;
} else if persisted.phpmyadmin_enabled && app_state.phpmyadmin_dir_exists {
    app_state.phpmyadmin_enabled = true;
    // Pre-write enabled conf so Apache picks it up on first start
    let php_port = config.php.port; // effective port not known yet; use configured
    if let Err(e) = phpmyadmin_conf::write_phpmyadmin_apache_conf_enabled(&config, php_port) {
        log::warn!("phpMyAdmin: could not write enabled phpmyadmin.conf at startup: {e}");
    }
    // Regenerate config.inc.php if RAMP owns it
    let config_path = phpmyadmin_dir.join("config.inc.php");
    let should_write = !config_path.exists()
        || phpmyadmin_conf::is_ramp_owned_config(&config_path);
    if should_write {
        let secret = persisted
            .phpmyadmin_blowfish_secret
            .clone()
            .unwrap_or_else(|| phpmyadmin_conf::generate_blowfish_secret(&config.install_dir));
        let content = phpmyadmin_conf::generate_config_inc_php(
            config.mysql.port,
            &config.phpmyadmin.mysql_user,
            &config.phpmyadmin.mysql_password,
            &secret,
        );
        if let Err(e) = config::atomic_write(&config_path, content.as_bytes()) {
            log::warn!("phpMyAdmin: could not write config.inc.php at startup: {e}");
        }
    }
} else {
    // phpmyadmin disabled — ensure empty conf exists so Apache Include doesn't fail
    app_state.phpmyadmin_enabled = false;
    if let Err(e) = phpmyadmin_conf::write_phpmyadmin_apache_conf_disabled(&config) {
        log::warn!("phpMyAdmin: could not write empty phpmyadmin.conf: {e}");
    }
}
```

- [ ] **Step 8.3: Update `do_persist` in `executor.rs` to include `phpmyadmin_enabled`**

In `src/executor.rs`, update `do_persist`:

```rust
fn do_persist(&self, state: &AppState) {
    let state_path = self.config.install_dir.join("ramp.state");
    // Preserve blowfish_secret from the existing state file
    let existing_secret = std::fs::read(&state_path)
        .ok()
        .and_then(|data| serde_json::from_slice::<crate::state::PersistedState>(&data).ok())
        .and_then(|p| p.phpmyadmin_blowfish_secret);

    let persisted = crate::state::PersistedState {
        apache_desired: state.apache.desired,
        mysql_desired: state.mysql.desired,
        php_desired: state.php.desired,
        phpmyadmin_enabled: state.phpmyadmin_enabled,
        phpmyadmin_blowfish_secret: existing_secret,
    };
    let result = serde_json::to_vec_pretty(&persisted)
        .map_err(|e| format!("serialize state failed: {e}"))
        .and_then(|data| atomic_write(&state_path, &data));

    if let Err(e) = result {
        log::error!("PERSIST FAILED — desired service state will not survive restart: {e}");
        let msg =
            format!("ERROR: state persist failed — restart may not restore services: {e}");
        self.log.push(msg);
    }
}
```

- [ ] **Step 8.4: Build and run all tests**

```
cargo build && cargo test
```
Expected: all pass.

- [ ] **Step 8.5: Run lint and format check**

```
cargo clippy -- -D warnings && cargo fmt -- --check
```

Fix any issues with `cargo fmt` if needed.

- [ ] **Step 8.6: Commit**

```
git add src/main.rs src/executor.rs
git commit -m "feat: startup reconciliation for phpMyAdmin state and persist phpmyadmin_enabled"
```

---

## Task 9: Final verification

- [ ] **Step 9.1: Run full test suite**

```
cargo test
```
Expected: all tests pass. Zero failures.

- [ ] **Step 9.2: Run lint**

```
cargo clippy -- -D warnings
```
Expected: zero warnings.

- [ ] **Step 9.3: Run format check**

```
cargo fmt -- --check
```
Expected: no diff.

- [ ] **Step 9.4: Release build**

```
cargo build --release
```
Expected: builds cleanly, no errors.

- [ ] **Step 9.5: Final commit**

```
git add -A
git commit -m "feat: phpMyAdmin integration complete — Admin button on MySQL row"
```

---

## Edge Case Reference

| Scenario | Handled in |
|----------|-----------|
| phpMyAdmin dir missing at toggle | Reducer rejects, LogEvent |
| phpMyAdmin dir missing at startup with enabled state | `main.rs` reconciliation: force disable + persist |
| MySQL/PHP crash while Admin enabled | Reducer `ProcessExit` → `SideEffect::TogglePhpMyAdmin(false)` |
| `config.inc.php` write fails | Executor aborts toggle, sends `PhpMyAdminToggled(false)` |
| `phpmyadmin.conf` write fails | Executor aborts toggle, reverts |
| User-owned `config.inc.php` | `is_ramp_owned_config()` returns false → skip |
| Old `ramp.state` without `phpmyadmin_enabled` | `#[serde(default)]` → false |
| Apache restart after toggle fails | State already set; next Apache start picks up conf |
| `blowfish_secret` missing from state | Generated fresh in executor, persisted on next write |
| Browser open fails | Non-fatal warning, user can open manually |
| Apache not running when button pressed | Button disabled in UI (precondition check) |
