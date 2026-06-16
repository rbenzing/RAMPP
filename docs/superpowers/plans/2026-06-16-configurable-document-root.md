# Configurable Apache DocumentRoot Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the user pick any directory on disk as the Apache DocumentRoot via an in-app folder picker, persisted to `ramp.toml` and applied by auto-restarting Apache.

**Architecture:** Add a `document_root` field to `ApacheConfig`, resolved from an optional `ramp.toml` key (defaulting to `<install_dir>/apache/htdocs`). The generated `httpd.conf` reads it. A new `Event::SetDocumentRoot` updates state (pure reducer) and emits a `SideEffect::PersistConfig` plus the standard Apache-restart effects when Apache is running. The executor's `PersistConfig` handler refreshes its own config copy, writes `ramp.toml`, and ensures the folder exists — so the subsequent respawn regenerates `httpd.conf` with the new root.

**Tech Stack:** Rust, egui/eframe (UI), `rfd` (native folder picker), `toml`/`serde` (config).

**Reference spec:** `docs/superpowers/specs/2026-06-16-configurable-document-root-design.md`

---

## File Structure

| File | Change |
|------|--------|
| `src/state.rs` | Add `document_root: PathBuf` to `ApacheConfig` |
| `src/paths.rs` | New `validate_document_root` (absolute + exists + dir + not symlink) |
| `src/config.rs` | Optional `document_root` TOML key; resolve/validate; new `write_config` |
| `src/apache_conf.rs` | Generator reads `document_root`; `ensure_htdocs` → `ensure_document_root` (seed-if-empty) |
| `src/events.rs` | `Event::SetDocumentRoot(PathBuf)`, `SideEffect::PersistConfig` |
| `src/reducer.rs` | Handle `SetDocumentRoot` |
| `src/executor.rs` | Handle `PersistConfig` (refresh config + write toml + ensure folder) |
| `src/main.rs` | Update `ensure_htdocs` call site |
| `src/ui.rs` | DocumentRoot row + folder-picker button |
| `Cargo.toml` | Add `rfd` dependency |
| `tests/reducer_props.rs`, `tests/integration_spawn.rs` | Add `document_root` to `ApacheConfig` literals |

---

## Task 1: Add `document_root` field to `ApacheConfig` and restore compilation

Adding the field breaks every `ApacheConfig` constructor. This task adds the field and fixes all constructors so the workspace compiles again. No behavior change yet — `config.rs` uses a hardcoded default.

**Files:**
- Modify: `src/state.rs:97-102`
- Modify: `src/config.rs:126-132`
- Modify: `src/reducer.rs:423-427`
- Modify: `src/apache_conf.rs:167-171`
- Modify: `src/php_conf.rs:190-194`
- Modify: `src/mysql_conf.rs:161-165`
- Modify: `src/phpmyadmin_conf.rs:139-143`
- Modify: `tests/reducer_props.rs:21-25`
- Modify: `tests/integration_spawn.rs:17-21`

- [ ] **Step 1: Add the field to the struct**

In `src/state.rs`, change the `ApacheConfig` struct:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApacheConfig {
    pub port: u16,
    pub bin: PathBuf,
    pub conf: PathBuf,
    pub document_root: PathBuf,
}
```

- [ ] **Step 2: Update the real builder in `src/config.rs`**

In `validate_and_build`, change the `apache` field of the returned `RampConfig` (lines 128-132):

```rust
        apache: ApacheConfig {
            port: doc.apache.port,
            bin: paths.apache_bin,
            conf: paths.apache_conf,
            document_root: install_dir.join("apache").join("htdocs"),
        },
```

- [ ] **Step 3: Update the test constructor in `src/reducer.rs`**

In `make_state` (lines 423-427), add the field:

```rust
            apache: ApacheConfig {
                port: 80,
                bin: std::path::PathBuf::from("C:\\ramp\\apache\\bin\\httpd.exe"),
                conf: std::path::PathBuf::from("C:\\ramp\\apache\\conf\\httpd.conf"),
                document_root: std::path::PathBuf::from("C:\\ramp\\apache\\htdocs"),
            },
```

- [ ] **Step 4: Update the test constructor in `src/apache_conf.rs`**

In `test_cfg` (lines 167-171):

```rust
            apache: ApacheConfig {
                port: 8080,
                bin: dir.join("apache").join("bin").join("httpd.exe"),
                conf: dir.join("apache").join("conf").join("httpd.conf"),
                document_root: dir.join("apache").join("htdocs"),
            },
```

- [ ] **Step 5: Update the test constructor in `src/php_conf.rs`**

In `test_cfg` (lines 190-194):

```rust
            apache: ApacheConfig {
                port: 80,
                bin: dir.join("apache").join("bin").join("httpd.exe"),
                conf: dir.join("apache").join("conf").join("httpd.conf"),
                document_root: dir.join("apache").join("htdocs"),
            },
```

- [ ] **Step 6: Update the test constructor in `src/mysql_conf.rs`**

In `test_cfg` (lines 161-165):

```rust
            apache: ApacheConfig {
                port: 80,
                bin: dir.join("apache").join("bin").join("httpd.exe"),
                conf: dir.join("apache").join("conf").join("httpd.conf"),
                document_root: dir.join("apache").join("htdocs"),
            },
```

- [ ] **Step 7: Update the test constructor in `src/phpmyadmin_conf.rs`**

In `test_cfg` (lines 139-143):

```rust
            apache: ApacheConfig {
                port: 8080,
                bin: dir.join("apache").join("bin").join("httpd.exe"),
                conf: dir.join("apache").join("conf").join("httpd.conf"),
                document_root: dir.join("apache").join("htdocs"),
            },
```

- [ ] **Step 8: Update the test constructor in `tests/reducer_props.rs`**

At lines 21-25:

```rust
        apache: ApacheConfig {
            port: 8080,
            bin: PathBuf::from("C:\\ramp\\apache\\bin\\httpd.exe"),
            conf: PathBuf::from("C:\\ramp\\apache\\conf\\httpd.conf"),
            document_root: PathBuf::from("C:\\ramp\\apache\\htdocs"),
        },
```

- [ ] **Step 9: Update the test constructor in `tests/integration_spawn.rs`**

At lines 17-21 (`install_dir` is the local var used by this helper):

```rust
        apache: ApacheConfig {
            port: 18080,
            bin,
            conf: install_dir.join("httpd.conf"),
            document_root: install_dir.join("htdocs"),
        },
```

- [ ] **Step 10: Build to confirm everything compiles**

Run: `cargo build`
Expected: builds successfully (warnings about unused are acceptable; no errors).

- [ ] **Step 11: Run the full test suite to confirm nothing regressed**

Run: `cargo test`
Expected: all existing tests PASS.

- [ ] **Step 12: Commit**

```bash
git add src/ tests/
git commit -m "feat: add document_root field to ApacheConfig with htdocs default"
```

---

## Task 2: Add `validate_document_root` to `src/paths.rs`

**Files:**
- Modify: `src/paths.rs` (add function + tests)

- [ ] **Step 1: Write the failing tests**

Add these tests inside the existing `#[cfg(test)] mod tests` block in `src/paths.rs`:

```rust
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
        assert!(err.contains("directory"), "expected directory error, got: {err}");
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib paths::tests::document_root`
Expected: FAIL — `cannot find function validate_document_root`.

- [ ] **Step 3: Implement `validate_document_root`**

Add this public function to `src/paths.rs` (after `validate_critical_path`):

```rust
/// Validate a user-chosen Apache DocumentRoot. Unlike `validate_critical_path`,
/// the path is NOT confined to `install_dir` — the user may point anywhere on disk.
/// Requirements: absolute, exists, is a directory, and is not a symlink (consistent
/// with RAMPP's no-symlink-following stance).
pub fn validate_document_root(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("document root must be absolute: {}", path.display()));
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib paths::tests::document_root`
Expected: PASS (the symlink test may early-return on CI without symlink privilege — still counts as pass).

- [ ] **Step 5: Commit**

```bash
git add src/paths.rs
git commit -m "feat: add validate_document_root path validator"
```

---

## Task 3: Read & validate `document_root` from `ramp.toml`; add `write_config`

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write the failing tests**

Add these tests inside the `#[cfg(test)] mod tests` block in `src/config.rs`:

```rust
    #[test]
    fn document_root_defaults_to_htdocs_when_absent() {
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
        assert_eq!(cfg.apache.document_root, dir.join("apache").join("htdocs"));
    }

    #[test]
    fn document_root_reads_custom_value() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let custom = dir.join("my_site");
        std::fs::create_dir(&custom).unwrap();
        write_toml(
            dir,
            &format!(
                r#"install_dir = "{}"
[apache]
port = 8080
document_root = "{}"
[mysql]
port = 3306
[php]
port = 9000
"#,
                dir.display().to_string().replace('\\', "\\\\"),
                custom.display().to_string().replace('\\', "\\\\")
            ),
        );
        let cfg = load_config(dir).unwrap();
        assert_eq!(cfg.apache.document_root, custom);
    }

    #[test]
    fn rejects_document_root_that_is_not_a_directory() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let missing = dir.join("nope");
        write_toml(
            dir,
            &format!(
                r#"install_dir = "{}"
[apache]
port = 8080
document_root = "{}"
[mysql]
port = 3306
[php]
port = 9000
"#,
                dir.display().to_string().replace('\\', "\\\\"),
                missing.display().to_string().replace('\\', "\\\\")
            ),
        );
        assert!(load_config(dir).is_err());
    }

    #[test]
    fn write_config_round_trips_document_root() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let custom = dir.join("site2");
        std::fs::create_dir(&custom).unwrap();
        // Start from a default config on disk
        write_default_config(dir).unwrap();
        let mut cfg = load_config(dir).unwrap();
        cfg.apache.document_root = custom.clone();
        write_config(&cfg).unwrap();
        // Reload and confirm the new value persisted
        let reloaded = load_config(dir).unwrap();
        assert_eq!(reloaded.apache.document_root, custom);
        assert_eq!(reloaded.apache.port, cfg.apache.port);
        assert_eq!(reloaded.php.port, cfg.php.port);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib config::tests::document_root config::tests::rejects_document_root config::tests::write_config_round_trips`
Expected: FAIL — `document_root` field unknown / `write_config` not found.

- [ ] **Step 3: Add the optional TOML field**

In `src/config.rs`, update `TomlApache` (lines 19-22) and add `PathBuf` usage (already imported):

```rust
#[derive(Debug, Serialize, Deserialize)]
struct TomlApache {
    port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    document_root: Option<PathBuf>,
}
```

- [ ] **Step 4: Resolve & validate `document_root` in `validate_and_build`**

In `validate_and_build`, after the port-clash checks and before the `Ok(RampConfig { ... })` (around line 125), insert:

```rust
    let document_root = match &doc.apache.document_root {
        Some(p) => {
            crate::paths::validate_document_root(p)
                .map_err(|e| format!("invalid apache.document_root: {e}"))?;
            p.clone()
        }
        None => install_dir.join("apache").join("htdocs"),
    };
```

Then change the `apache` field of the returned struct (from Task 1's edit) to use it:

```rust
        apache: ApacheConfig {
            port: doc.apache.port,
            bin: paths.apache_bin,
            conf: paths.apache_conf,
            document_root,
        },
```

- [ ] **Step 5: Add the `write_config` function**

Add this public function to `src/config.rs` (after `write_default_config`):

```rust
/// Serialize the current config back to ramp.toml (atomic write). Preserves all
/// known fields (ports, phpMyAdmin credentials) and writes the document_root.
pub fn write_config(cfg: &RampConfig) -> Result<(), String> {
    let paths = InstallPaths::from_install_dir(&cfg.install_dir)?;
    let doc = TomlRoot {
        install_dir: cfg.install_dir.clone(),
        apache: TomlApache {
            port: cfg.apache.port,
            document_root: Some(cfg.apache.document_root.clone()),
        },
        mysql: TomlMysql {
            port: cfg.mysql.port,
        },
        php: TomlPhp {
            port: cfg.php.port,
        },
        phpmyadmin: TomlPhpMyAdmin {
            mysql_user: cfg.phpmyadmin.mysql_user.clone(),
            mysql_password: cfg.phpmyadmin.mysql_password.clone(),
        },
    };
    let serialized =
        toml::to_string_pretty(&doc).map_err(|e| format!("serialize config failed: {e}"))?;
    atomic_write(&paths.config, serialized.as_bytes())
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib config::tests`
Expected: all `config::tests` PASS, including the four new ones.

- [ ] **Step 7: Commit**

```bash
git add src/config.rs
git commit -m "feat: read/validate document_root from ramp.toml and add write_config"
```

---

## Task 4: Use `document_root` in generated `httpd.conf`; seed-if-empty

**Files:**
- Modify: `src/apache_conf.rs`
- Modify: `src/main.rs:68`

- [ ] **Step 1: Write the failing tests**

Add these tests inside the `#[cfg(test)] mod tests` block in `src/apache_conf.rs`:

```rust
    #[test]
    fn document_root_reflects_config_value() {
        let tmp = TempDir::new().unwrap();
        let mut cfg = test_cfg(tmp.path());
        cfg.apache.document_root = tmp.path().join("custom_site");
        let conf = generate_httpd_conf(&cfg);
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
        assert!(index.exists(), "empty document root should be seeded with index.php");
    }

    #[test]
    fn ensure_document_root_leaves_nonempty_folder_untouched() {
        let tmp = TempDir::new().unwrap();
        let cfg = test_cfg(tmp.path());
        std::fs::create_dir_all(&cfg.apache.document_root).unwrap();
        let existing = cfg.apache.document_root.join("app.php");
        std::fs::write(&existing, b"<?php // user file").unwrap();
        ensure_document_root(&cfg).unwrap();
        // No index.php seeded into a non-empty folder
        assert!(!cfg.apache.document_root.join("index.php").exists());
        // User file untouched
        assert_eq!(std::fs::read(&existing).unwrap(), b"<?php // user file");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib apache_conf::tests::document_root apache_conf::tests::ensure_document_root`
Expected: FAIL — `ensure_document_root` not found; `DocumentRoot` assertion fails (still hardcoded to htdocs).

- [ ] **Step 3: Use `document_root` in the generator**

In `src/apache_conf.rs`, inside `generate_httpd_conf_with_ports`, add a normalized doc-root string near the top (after the `logs_dir` lines, ~line 15):

```rust
    let doc_root = cfg
        .apache
        .document_root
        .display()
        .to_string()
        .replace('\\', "/");
```

Then change the `DocumentRoot`/`<Directory>` lines in the format string (currently lines 58-59) from:

```
DocumentRoot "{apache_dir}/htdocs"
<Directory "{apache_dir}/htdocs">
```

to:

```
DocumentRoot "{doc_root}"
<Directory "{doc_root}">
```

(`doc_root` is captured automatically by the inline `format!` named-argument capture, the same way `apache_dir` already is.)

- [ ] **Step 4: Replace `ensure_htdocs` with `ensure_document_root`**

In `src/apache_conf.rs`, replace the whole `ensure_htdocs` function (lines 143-155) with:

```rust
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
```

- [ ] **Step 5: Update the caller in `src/main.rs`**

Change line 68 from `apache_conf::ensure_htdocs(&config)` to:

```rust
    if let Err(e) = apache_conf::ensure_document_root(&config) {
        log::warn!("cannot create document root: {e}");
    }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --lib apache_conf::tests`
Expected: all `apache_conf::tests` PASS, including the three new ones. (No existing test referenced `ensure_htdocs` — only `src/main.rs:68` called it, updated in Step 5.)

- [ ] **Step 7: Build the whole workspace**

Run: `cargo build`
Expected: builds — no remaining references to `ensure_htdocs`.

- [ ] **Step 8: Commit**

```bash
git add src/apache_conf.rs src/main.rs
git commit -m "feat: generate DocumentRoot from config and seed only empty folders"
```

---

## Task 5: Add `Event::SetDocumentRoot` and `SideEffect::PersistConfig`

**Files:**
- Modify: `src/events.rs`

- [ ] **Step 1: Add the event variant**

In `src/events.rs`, add to the `Event` enum under the `// UI actions` section (after `DismissError(Service)`):

```rust
    /// User picked a new Apache DocumentRoot (already validated by the UI).
    SetDocumentRoot(std::path::PathBuf),
```

- [ ] **Step 2: Add the side-effect variant**

In `src/events.rs`, add to the `SideEffect` enum (after `PersistDesiredState`):

```rust
    /// Refresh the executor's config copy from state and persist ramp.toml.
    PersistConfig,
```

- [ ] **Step 3: Build to confirm the enums compile**

Run: `cargo build`
Expected: builds, but with a non-exhaustive-match error in `src/executor.rs` for `PersistConfig` IF the executor match is exhaustive. If `cargo build` reports a missing match arm, that is expected and fixed in Task 7. To keep this task self-contained, only confirm `src/events.rs` itself has no syntax error:

Run: `cargo build 2>&1 | head -20`
Expected: the only errors (if any) are `non-exhaustive patterns: ... PersistConfig not covered` in `executor.rs`. No errors originating in `events.rs`.

- [ ] **Step 4: Commit**

```bash
git add src/events.rs
git commit -m "feat: add SetDocumentRoot event and PersistConfig side effect"
```

---

## Task 6: Handle `SetDocumentRoot` in the reducer

**Files:**
- Modify: `src/reducer.rs`

- [ ] **Step 1: Write the failing tests**

Add these tests inside the `#[cfg(test)] mod tests` block in `src/reducer.rs` (near the RestartService tests):

```rust
    #[test]
    fn set_document_root_persists_and_restarts_when_apache_running() {
        let mut state = make_state();
        set_state(&mut state, Service::Apache, ServiceState::Running);
        let new_root = std::path::PathBuf::from("C:\\sites\\myapp");
        let (new_state, effects) =
            reducer(state, Event::SetDocumentRoot(new_root.clone()));
        // Config updated
        assert_eq!(new_state.config.apache.document_root, new_root);
        // Apache restarting
        assert_eq!(new_state.apache.state, ServiceState::Stopping);
        assert_eq!(new_state.apache.desired, DesiredServiceState::Running);
        // Persisted config + Apache kill emitted
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::PersistConfig)));
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::KillService(Service::Apache))));
    }

    #[test]
    fn set_document_root_persist_only_when_apache_stopped() {
        let state = make_state(); // apache Stopped
        let new_root = std::path::PathBuf::from("C:\\sites\\other");
        let (new_state, effects) =
            reducer(state, Event::SetDocumentRoot(new_root.clone()));
        assert_eq!(new_state.config.apache.document_root, new_root);
        // Stays Stopped — no restart
        assert_eq!(new_state.apache.state, ServiceState::Stopped);
        assert!(effects
            .iter()
            .any(|e| matches!(e, SideEffect::PersistConfig)));
        assert!(!effects
            .iter()
            .any(|e| matches!(e, SideEffect::KillService(Service::Apache))));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib reducer::tests::set_document_root`
Expected: FAIL — `SetDocumentRoot` arm missing (compile error: non-exhaustive match) or assertion failure.

- [ ] **Step 3: Add the reducer arm**

In `src/reducer.rs`, inside the `match event { ... }`, add a new arm in the `// UI actions` area (after the `Event::DismissError` arm, around line 369):

```rust
        Event::SetDocumentRoot(path) => {
            state.config.apache.document_root = path;
            effects.push(SideEffect::PersistConfig);
            effects.push(SideEffect::LogEvent(format!(
                "document root set to {}",
                state.config.apache.document_root.display()
            )));
            // Apache reads DocumentRoot only at startup. If it's running, restart it so
            // the change goes live; otherwise it applies on next start. Mirrors the
            // RestartService(Apache) Running/Starting branch.
            match state.apache.state {
                ServiceState::Running | ServiceState::Starting => {
                    state.apache.state = ServiceState::Stopping;
                    state.clear_started_at(Service::Apache);
                    state.apache.desired = DesiredServiceState::Running;
                    effects.push(SideEffect::StopHealthCheck(Service::Apache));
                    effects.push(SideEffect::KillService(Service::Apache));
                    effects.push(SideEffect::LogEvent(
                        "Apache: restarting to apply new document root".to_string(),
                    ));
                }
                _ => {}
            }
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib reducer::tests::set_document_root`
Expected: PASS (both tests).

- [ ] **Step 5: Run the full reducer test module to confirm no regressions**

Run: `cargo test --lib reducer::tests`
Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add src/reducer.rs
git commit -m "feat: handle SetDocumentRoot in reducer with persist + conditional restart"
```

---

## Task 7: Handle `PersistConfig` in the executor

The executor's `self.config` is set once at construction and never refreshed. The `PersistConfig` handler must refresh it from `state` (so the subsequent Apache respawn regenerates `httpd.conf` with the new root), write `ramp.toml`, and ensure the folder exists.

**Files:**
- Modify: `src/executor.rs`

- [ ] **Step 1: Add the match arm in `execute`**

In `src/executor.rs`, in the `match effect` block inside `execute` (after the `SideEffect::PersistDesiredState` arm, ~line 72), add:

```rust
                SideEffect::PersistConfig => self.do_persist_config(state),
```

- [ ] **Step 2: Implement `do_persist_config`**

Add this method to the `impl Executor` block (after `do_persist`, ~line 254). Note it takes `&mut self` because it refreshes `self.config`:

```rust
    fn do_persist_config(&mut self, state: &AppState) {
        // Refresh the executor's config copy so a subsequent respawn regenerates
        // httpd.conf with the new document root. The executor otherwise keeps the
        // config it was constructed with.
        self.config = state.config.clone();

        if let Err(e) = crate::config::write_config(&self.config) {
            log::error!("config persist failed: {e}");
            self.log.push(format!("ERROR: config persist failed — {e}"));
            return;
        }
        // Ensure the new document root exists (seed index.php only if empty).
        if let Err(e) = crate::apache_conf::ensure_document_root(&self.config) {
            self.log
                .push(format!("warn: could not prepare document root — {e}"));
        }
        self.log.push(format!(
            "document root saved: {}",
            self.config.apache.document_root.display()
        ));
    }
```

- [ ] **Step 3: Confirm `execute` takes `&mut self`**

`execute` is already `pub fn execute(&mut self, ...)` (line 58), so calling a `&mut self` method is fine. No signature change needed.

- [ ] **Step 4: Build to verify the match is now exhaustive**

Run: `cargo build`
Expected: builds with no errors.

- [ ] **Step 5: Run the full test suite**

Run: `cargo test`
Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add src/executor.rs
git commit -m "feat: persist config and refresh executor config on PersistConfig"
```

---

## Task 8: Add the `rfd` folder picker and DocumentRoot UI

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/ui.rs`

- [ ] **Step 1: Add the `rfd` dependency**

In `Cargo.toml`, under `[dependencies]` (after `tray-item = "0.9"`), add:

```toml
rfd = "0.14"
```

- [ ] **Step 2: Fetch and build to confirm the dependency resolves**

Run: `cargo build`
Expected: `rfd` downloads and the workspace builds.

- [ ] **Step 3: Add the DocumentRoot row to the UI**

In `src/ui.rs`, inside `impl eframe::App for RampApp`'s `update`, locate the `ui.horizontal(|ui| { ... })` block that holds the "Start All / Reload Config" buttons (ends at the closing of that block, ~line 140). Immediately AFTER that `ui.horizontal` block and BEFORE the `ui.separator();` that precedes the Log section (~line 142), insert:

```rust
            ui.separator();
            ui.horizontal(|ui| {
                ui.label("Document Root:");
                ui.monospace(state.config.apache.document_root.display().to_string());
                if ui.button("📁 Change…").clicked() {
                    if let Some(folder) = rfd::FileDialog::new()
                        .set_title("Select Apache Document Root")
                        .pick_folder()
                    {
                        match crate::paths::validate_document_root(&folder) {
                            Ok(()) => {
                                let _ = self.tx.send(Event::SetDocumentRoot(folder));
                            }
                            Err(e) => {
                                log::error!("invalid document root: {e}");
                                self.log.push(format!("ERROR: invalid document root — {e}"));
                            }
                        }
                    }
                }
            });
```

- [ ] **Step 4: Build and run clippy**

Run: `cargo build && cargo clippy -- -D warnings`
Expected: builds with no clippy warnings. (If clippy flags `state.config.apache.document_root.display().to_string()`, it is acceptable as written; egui `monospace` needs an owned/`&str`.)

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/ui.rs
git commit -m "feat: add document root folder picker to the UI"
```

---

## Task 9: Final verification

**Files:** none (verification only)

- [ ] **Step 1: Format check**

Run: `cargo fmt -- --check`
Expected: no output (all formatted). If it fails, run `cargo fmt` and commit the formatting.

- [ ] **Step 2: Lint**

Run: `cargo clippy -- -D warnings`
Expected: no warnings, exit 0.

- [ ] **Step 3: Full test suite**

Run: `cargo test`
Expected: all tests PASS.

- [ ] **Step 4: Manual smoke check (optional, if a built stack is available)**

Run: `cargo run`
Then: with Apache running, click "📁 Change…", pick an empty folder, confirm Apache restarts and `http://localhost:<port>` serves `phpinfo()` from the new folder; confirm `ramp.toml` now contains a `document_root` key under `[apache]`.

- [ ] **Step 5: Final commit (only if Step 1 produced formatting changes)**

```bash
git add -A
git commit -m "chore: formatting for configurable document root"
```

---

## Notes / Known limitations

- The in-app picker is the supported way to change the DocumentRoot. Changing it by hand-editing `ramp.toml` + "Reload Config" updates the persisted value but, like all config fields today, does not refresh the executor's in-memory config — the change applies on the next process launch. This matches existing behavior for ports and is out of scope here.
- phpMyAdmin is served via its own alias in `conf/phpmyadmin.conf` and is unaffected by the DocumentRoot.
