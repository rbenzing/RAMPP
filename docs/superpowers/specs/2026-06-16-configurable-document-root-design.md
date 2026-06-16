# Configurable Apache DocumentRoot — Design

**Date:** 2026-06-16
**Status:** Approved for planning

## Problem

The folder PHP serves from — the Apache `DocumentRoot` — is currently hardcoded to
`<install_dir>\apache\htdocs` in two places:

- `src/apache_conf.rs` (`generate_httpd_conf_with_ports`): emits
  `DocumentRoot "{apache_dir}/htdocs"` and the matching `<Directory>` block.
- `src/apache_conf.rs` (`ensure_htdocs`): creates `<install_dir>/apache/htdocs` and
  seeds a default `index.php` (`phpinfo()`) on first run.

There is no setting for it. Users cannot point RAMPP at their own project folder the way
they can with XAMPP/WAMP. PHP's own `php.ini` keeps `doc_root =` empty deliberately (for
phpMyAdmin compatibility), so the script path comes from Apache — meaning the Apache
DocumentRoot is the effective "active folder PHP uses."

## Goal

Let the user choose any directory on disk as the active DocumentRoot, via an in-app
folder picker, persisted to `ramp.toml`, applied automatically on Apache (re)start.

## Decisions (confirmed with user)

| Question | Decision |
|----------|----------|
| What folder | The Apache DocumentRoot (web root) |
| Location scope | Anywhere on disk (not confined to `install_dir`) |
| How to set | In-app native folder picker + persisted setting |
| Apply timing | Auto-restart Apache if it is running |
| Seeding `index.php` | Seed only if the chosen folder is empty |
| Validation strictness | Absolute + exists + is a directory + not a symlink |
| Folder picker impl | Add the `rfd` crate |

## Design

### 1. Config & state

**`src/state.rs`** — add a field to `ApacheConfig`:

```rust
pub struct ApacheConfig {
    pub port: u16,
    pub bin: PathBuf,
    pub conf: PathBuf,
    pub document_root: PathBuf, // NEW
}
```

**`src/config.rs`** — add an optional TOML field (backward compatible):

```rust
struct TomlApache {
    port: u16,
    #[serde(default)]
    document_root: Option<PathBuf>, // NEW — absent in existing configs
}
```

In `validate_and_build`:
- If `document_root` is `None`, default to `<install_dir>/apache/htdocs`.
- If `Some`, run `validate_document_root` (see §2). A failure rejects the entire config,
  upholding the "config always valid or rejected entirely" invariant.

`write_default_config` is unchanged (omitting `document_root` yields the default).

Add a config-write path so a changed DocumentRoot can be persisted:
- A function that serializes the current config back to `ramp.toml` via `atomic_write`
  (read existing TOML → set `apache.document_root` → atomic write). Round-tripping through
  the existing `TomlRoot` struct preserves all known fields (ports, phpMyAdmin creds).

### 2. Validation — `src/paths.rs`

New function:

```rust
pub fn validate_document_root(path: &Path) -> Result<(), String>
```

Rules:
- Must be **absolute**.
- Must **exist** and be a **directory**.
- Must **not be a symlink** (consistent with the existing no-symlink-following stance).
- **Not** confined to `install_dir` — the user opted into "anywhere on disk."

This is intentionally distinct from `validate_critical_path`, which enforces
`install_dir` confinement and is used for binaries/config/data.

### 3. Apache config generation — `src/apache_conf.rs`

- `generate_httpd_conf_with_ports` emits `DocumentRoot` and the `<Directory "...">` block
  from `cfg.apache.document_root` (forward-slash normalized, same as `apache_dir`), instead
  of the hardcoded `{apache_dir}/htdocs`.
- Rename `ensure_htdocs` → `ensure_document_root`:
  - Create the configured folder if missing (`create_dir_all`).
  - Seed `index.php` (`<?php phpinfo();`) **only when the folder is empty**, so existing
    project folders are never modified.
- The FastCGI `<FilesMatch>` proxy block and the `Include "conf/phpmyadmin.conf"` line are
  unchanged. phpMyAdmin is served through its own alias and is unaffected by the DocumentRoot.

Because `src/executor.rs` already force-rewrites `httpd.conf` via
`rewrite_httpd_conf_with_ports` on **every** Apache start, a new DocumentRoot baked into the
generator is picked up automatically on (re)start — no extra wiring needed for the apply path.

### 4. Events & reducer

**`src/events.rs`**
- New `Event::SetDocumentRoot(PathBuf)`.
- New `SideEffect::PersistConfig` (executor writes `ramp.toml` atomically).

**`src/reducer.rs`** (pure — no filesystem access)
- On `SetDocumentRoot(path)`:
  - Update `state.config.apache.document_root = path`.
  - Emit `SideEffect::PersistConfig`.
  - If Apache is currently `Running`, also emit the standard Apache restart side effects
    (same sequence as `RestartService(Apache)`) so the change is applied immediately.
  - If Apache is stopped, persist only — the change applies on next start.

**Filesystem validation placement:** `validate_document_root` runs in the UI thread
**before** `Event::SetDocumentRoot` is sent (the folder picker already guarantees the path
exists; this enforces symlink/dir/absolute and surfaces a clear error). On failure the UI
logs an error and sends nothing, so the reducer never commits an invalid root. `load_config`
re-validates on the next reload as a backstop. This keeps the reducer pure and respects the
"state owned exclusively by the reducer; side effects do I/O" architectural law.

**`src/executor.rs`**
- Handle `SideEffect::PersistConfig` by writing the current config to `ramp.toml` via the
  atomic-write helper.

### 5. UI & dependency

**`Cargo.toml`** — add `rfd` (native file/folder dialogs).

**`src/ui.rs`**
- Add a "Document Root" line in the central panel showing the current
  `state.config.apache.document_root`, with a **"📁 Change…"** button.
- On click: `rfd::FileDialog::new().pick_folder()`. If a folder is returned, run
  `validate_document_root`; on success send `Event::SetDocumentRoot(path)`, on failure push
  an error line to the log.

## Testing

1. **`config`** — defaults `document_root` to `apache/htdocs` when absent; reads a custom
   value; rejects a config whose `document_root` is missing/relative/not a directory.
2. **`paths`** — `validate_document_root` accepts an existing dir; rejects relative paths,
   nonexistent paths, files, and symlinks (skipping the symlink case when the OS denies
   symlink creation, matching existing tests).
3. **`apache_conf`** — generated `DocumentRoot`/`<Directory>` reflect the configured root;
   `ensure_document_root` creates + seeds `index.php` in an empty folder and leaves a
   non-empty folder untouched.
4. **`reducer`** — `SetDocumentRoot` updates `config.apache.document_root` and emits the
   restart effects when Apache is Running; persist-only when Apache is Stopped.

All three gates must pass before commit:
`cargo clippy -- -D warnings && cargo fmt -- --check && cargo test`.

## Out of scope

- Multiple/named site profiles (single DocumentRoot only).
- Per-vhost configuration.
- Making the PHP binary or `php.ini` location selectable.
