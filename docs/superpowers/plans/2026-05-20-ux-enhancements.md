# UX Enhancements Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add six UX improvements to the RAMP egui UI: open config files in editor, open localhost in browser, uptime display, log copy/clear, error badge dismissal, and full-width log panel.

**Architecture:** All changes are pure UI-side (`src/ui.rs`) except: (a) uptime requires keeping `started_at` set after `Starting→Running` transition in `src/reducer.rs`; (b) log clear requires a new `ClearLog` event and ring-buffer clear method in `src/logger.rs`; (c) browser open and editor open are fire-and-forget `std::process::Command` calls made directly from the egui update loop (no new `SideEffect` needed — these are read-only shell-outs with no state impact). Error badge dismiss sends a new `DismissError(Service)` event handled in `src/reducer.rs`.

**Tech Stack:** Rust, egui/eframe, crossbeam-channel, `std::process::Command` (Windows `cmd /c start`)

---

## File Map

| File | Changes |
|------|---------|
| `src/ui.rs` | All rendering changes: config buttons, open-localhost button, uptime display, log copy/clear, error dismiss button, full-width log |
| `src/events.rs` | Add `ClearLog` and `DismissError(Service)` events |
| `src/reducer.rs` | Handle `DismissError` (clear `last_error`); keep `started_at` alive through `Running` state |
| `src/logger.rs` | Add `clear()` method to the shared ring buffer |
| `src/state.rs` | No changes needed — `started_at` and `last_error` fields already exist |

---

### Task 1: Full-Width Log Panel

This is the simplest change — no new events or state. The log panel currently lives inside the default `CentralPanel` but uses no explicit width constraint. The `ScrollArea` has `max_height` set but no `min_rect` fill. We fix this by removing any width cap and ensuring the `ScrollArea` fills available width.

**Files:**
- Modify: `src/ui.rs`

- [ ] **Step 1: Read the current log rendering block**

Open `src/ui.rs` lines 139–151. The relevant section is:

```rust
ui.separator();
ui.label("Log");

let lines = self.log.tail(100);
egui::ScrollArea::vertical()
    .stick_to_bottom(true)
    .max_height(300.0)
    .show(ui, |ui| {
        for line in &lines {
            ui.monospace(line);
        }
    });
```

- [ ] **Step 2: Replace with full-width version**

In `src/ui.rs`, replace the log block (lines 139–151) with:

```rust
ui.separator();
ui.label("Log");

let lines = self.log.tail(100);
egui::ScrollArea::vertical()
    .stick_to_bottom(true)
    .max_height(300.0)
    .auto_shrink([false, false])
    .show(ui, |ui| {
        ui.set_min_width(ui.available_width());
        for line in &lines {
            ui.monospace(line);
        }
    });
```

`auto_shrink([false, false])` prevents the scroll area from shrinking horizontally. `ui.set_min_width(ui.available_width())` forces the inner layout to span the full panel width so long lines push the scroll area wide rather than wrapping.

- [ ] **Step 3: Build and check**

```
cargo build 2>&1
```

Expected: compiles without errors or warnings.

- [ ] **Step 4: Commit**

```
git add src/ui.rs
git commit -m "feat: full-width log scroll area"
```

---

### Task 2: Add ClearLog and DismissError Events

Add the two new event variants needed by later tasks. Do this first so the compiler helps catch all required match arms.

**Files:**
- Modify: `src/events.rs`
- Modify: `src/reducer.rs` (add match arms — minimal stubs that compile)

- [ ] **Step 1: Add events to the enum**

In `src/events.rs`, in the `Event` enum, add after the `OpenPhpMyAdmin` line:

```rust
    // UI actions
    ClearLog,
    DismissError(Service),
```

- [ ] **Step 2: Add stub match arms in the reducer**

Open `src/reducer.rs` and find the top-level `match event` block. Add two arms anywhere in the match (before the wildcard/catch-all if one exists):

```rust
Event::ClearLog => {
    // handled in executor / UI — reducer has no state to change
    (state, vec![])
}
Event::DismissError(svc) => {
    state.service_mut(svc).last_error = None;
    (state, vec![SideEffect::LogEvent(format!("{svc} error dismissed"))])
}
```

- [ ] **Step 3: Build**

```
cargo build 2>&1
```

Expected: compiles. If the compiler reports non-exhaustive match arms in any other match on `Event`, add `Event::ClearLog | Event::DismissError(_) => {}` stubs there too.

- [ ] **Step 4: Run existing reducer tests**

```
cargo test reducer::tests 2>&1
```

Expected: all pass.

- [ ] **Step 5: Commit**

```
git add src/events.rs src/reducer.rs
git commit -m "feat: add ClearLog and DismissError events"
```

---

### Task 3: Reducer — Keep started_at Through Running State

Right now `started_at` is cleared when a service transitions from `Starting` to `Running` (via `clear_started_at`). We need it to persist so the UI can show uptime. It should only be cleared when the service stops, crashes, or errors.

**Files:**
- Modify: `src/reducer.rs`

- [ ] **Step 1: Find where started_at is cleared**

```
cargo grep "clear_started_at" src/reducer.rs
```

Or open `src/reducer.rs` and search for `clear_started_at`. Note every call site.

- [ ] **Step 2: Write the failing test**

In `src/reducer.rs`, in the `#[cfg(test)]` module, add:

```rust
#[test]
fn started_at_survives_running_transition() {
    let state = make_state();
    // Transition to Starting
    let (state, _) = reducer(state, Event::StartService(Service::Apache));
    assert!(state.apache.started_at.is_some(), "started_at must be set on Starting");
    // Simulate ProcessReady — should move to Running without clearing started_at
    let (state, _) = reducer(state, Event::ProcessReady(Service::Apache));
    assert!(state.apache.started_at.is_some(), "started_at must survive Running transition");
    // Simulate stop — now it should clear
    let (state, _) = reducer(state, Event::StopService(Service::Apache));
    // Transition through Stopping → Stopped via ProcessExit
    let (state, _) = reducer(state, Event::ProcessExit { service: Service::Apache, exit_code: Some(0) });
    assert!(state.apache.started_at.is_none(), "started_at must clear on Stopped");
}
```

- [ ] **Step 3: Run to verify it fails**

```
cargo test reducer::tests::started_at_survives_running_transition 2>&1
```

Expected: FAIL — `started_at` is `None` after `ProcessReady`.

- [ ] **Step 4: Fix the reducer**

In `src/reducer.rs`, find the `ProcessReady(svc)` arm. Remove any `state.clear_started_at(svc)` call inside it (or in `AppState::clear_started_at` called from it).

Then find all `clear_started_at` calls for the non-running terminal states — `Stopped`, `Error`, `Crashed` — and confirm they remain. If `clear_started_at` was only called on `Running`, just delete that one call. If it's called in a shared helper, split the logic so it only clears in terminal states.

The key invariant: `started_at` is `Some` during `Starting` and `Running`, `None` in `Stopped`/`Error`/`Crashed`.

- [ ] **Step 5: Run the new test**

```
cargo test reducer::tests::started_at_survives_running_transition 2>&1
```

Expected: PASS.

- [ ] **Step 6: Run full reducer suite**

```
cargo test reducer::tests 2>&1
```

Expected: all pass.

- [ ] **Step 7: Commit**

```
git add src/reducer.rs
git commit -m "feat: preserve started_at through Running state for uptime display"
```

---

### Task 4: Logger — Add clear() Method

**Files:**
- Modify: `src/logger.rs`

- [ ] **Step 1: Read the logger**

Open `src/logger.rs` in full. Identify the struct holding the ring buffer (likely a `Mutex<VecDeque<String>>` or similar) and the `SharedLog` type alias.

- [ ] **Step 2: Write a failing test**

In `src/logger.rs` tests, add:

```rust
#[test]
fn clear_empties_the_log() {
    let log = SharedLog::new(100);
    log.push("line 1".to_string());
    log.push("line 2".to_string());
    assert_eq!(log.tail(10).len(), 2);
    log.clear();
    assert_eq!(log.tail(10).len(), 0, "log must be empty after clear()");
}
```

- [ ] **Step 3: Run to verify it fails**

```
cargo test 2>&1 | grep clear_empties
```

Expected: compile error — `clear` method doesn't exist yet.

- [ ] **Step 4: Implement clear()**

In `src/logger.rs`, on the `SharedLog` type (or its impl block), add:

```rust
pub fn clear(&self) {
    if let Ok(mut buf) = self.0.lock() {
        buf.clear();
    }
}
```

Adjust `self.0` to match however the inner mutex/buffer is accessed in this file (check the existing `push` and `tail` methods for the pattern).

- [ ] **Step 5: Run the test**

```
cargo test 2>&1 | grep clear_empties
```

Expected: PASS.

- [ ] **Step 6: Commit**

```
git add src/logger.rs
git commit -m "feat: add SharedLog::clear() for log panel clear action"
```

---

### Task 5: UI — Uptime Display

Show how long each Running service has been up, e.g. `2h 14m` or `45s`. `started_at` is now `Some` for both `Starting` and `Running` states. We already show elapsed startup time during `Starting` — this extends it to `Running` with a different format.

**Files:**
- Modify: `src/ui.rs`

- [ ] **Step 1: Add a helper to format uptime**

At the bottom of `src/ui.rs`, add:

```rust
fn format_uptime(elapsed_secs: u64) -> String {
    if elapsed_secs < 60 {
        format!("{elapsed_secs}s")
    } else if elapsed_secs < 3600 {
        format!("{}m {}s", elapsed_secs / 60, elapsed_secs % 60)
    } else {
        format!("{}h {}m", elapsed_secs / 3600, (elapsed_secs % 3600) / 60)
    }
}
```

- [ ] **Step 2: Wire it into service_row**

In `src/ui.rs`, in `service_row`, find the existing startup elapsed block:

```rust
// Show elapsed startup time
if status.state == ServiceState::Starting {
    if let Some(start) = status.started_at {
        ui.label(format!("({}s)", start.elapsed().as_secs()));
    }
}
```

Replace it with:

```rust
match status.state {
    ServiceState::Starting => {
        if let Some(start) = status.started_at {
            ui.label(format!("({}s)", start.elapsed().as_secs()));
        }
    }
    ServiceState::Running => {
        if let Some(start) = status.started_at {
            let secs = start.elapsed().as_secs();
            ui.colored_label(egui::Color32::DARK_GRAY, format!("up {}", format_uptime(secs)));
        }
    }
    _ => {}
}
```

- [ ] **Step 3: Build**

```
cargo build 2>&1
```

Expected: clean compile.

- [ ] **Step 4: Commit**

```
git add src/ui.rs
git commit -m "feat: show service uptime in UI when Running"
```

---

### Task 6: UI — Log Copy and Clear Buttons

Add two small buttons above the log area. "Copy" copies all visible log lines to the clipboard using egui's built-in clipboard support. "Clear" sends `Event::ClearLog` and immediately clears the `SharedLog` ring buffer.

**Files:**
- Modify: `src/ui.rs`

Note: egui provides clipboard access via `ui.ctx().copy_text(string)` (egui 0.27+) or `ctx.output_mut(|o| o.copied_text = ...)`. Check the egui version in `Cargo.toml` to pick the right call.

- [ ] **Step 1: Check egui version**

```
cargo metadata --format-version 1 | python -c "import json,sys; deps=json.load(sys.stdin)['packages']; print(next(p['version'] for p in deps if p['name']=='egui'))"
```

If Python isn't available: `grep egui Cargo.toml`

- [ ] **Step 2: Add log action buttons**

In `src/ui.rs`, in the `update` method, replace:

```rust
ui.separator();
ui.label("Log");
```

with:

```rust
ui.separator();
ui.horizontal(|ui| {
    ui.label("Log");
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if ui.small_button("✕ Clear").clicked() {
            self.log.clear();
            let _ = self.tx.send(Event::ClearLog);
        }
        if ui.small_button("⎘ Copy").clicked() {
            let text = self.log.tail(100).join("\n");
            ctx.copy_text(text);
        }
    });
});
```

If `ctx.copy_text` doesn't exist in your egui version, use:
```rust
ctx.output_mut(|o| o.copied_text = text);
```

- [ ] **Step 3: Build**

```
cargo build 2>&1
```

Expected: clean compile. If `copy_text` / `copied_text` errors occur, adjust per the egui version found in Step 1.

- [ ] **Step 4: Commit**

```
git add src/ui.rs
git commit -m "feat: add log copy and clear buttons"
```

---

### Task 7: UI — Error Badge Dismiss Button

Add a small `✕` button next to the inline error label. Clicking it sends `DismissError(svc)` which the reducer handles by clearing `last_error` (wired in Task 2).

**Files:**
- Modify: `src/ui.rs`

- [ ] **Step 1: Locate the error rendering in service_row**

In `src/ui.rs`, find this block in `service_row` (around line 211):

```rust
// Show last error and recovery hint — truncated inline, full text on hover
if status.state == ServiceState::Error {
    if let Some(err) = &status.last_error {
        let short = truncate_error(err);
        ui.colored_label(egui::Color32::RED, format!("⚠ {short}"))
            .on_hover_text(err.as_str());
    }
    ui.colored_label(egui::Color32::GRAY, "(click ▶ to retry)");
} else if let Some(err) = &status.last_error {
    let short = truncate_error(err);
    ui.colored_label(egui::Color32::RED, format!("⚠ {short}"))
        .on_hover_text(err.as_str());
}
```

- [ ] **Step 2: Replace with dismiss button**

Replace the block with:

```rust
if status.state == ServiceState::Error {
    if let Some(err) = &status.last_error {
        let short = truncate_error(err);
        ui.colored_label(egui::Color32::RED, format!("⚠ {short}"))
            .on_hover_text(err.as_str());
        if ui.small_button("✕").on_hover_text("Dismiss error").clicked() {
            let _ = tx.send(Event::DismissError(svc));
        }
    }
    ui.colored_label(egui::Color32::GRAY, "(click ▶ to retry)");
} else if let Some(err) = &status.last_error {
    let short = truncate_error(err);
    ui.colored_label(egui::Color32::RED, format!("⚠ {short}"))
        .on_hover_text(err.as_str());
    if ui.small_button("✕").on_hover_text("Dismiss error").clicked() {
        let _ = tx.send(Event::DismissError(svc));
    }
}
```

- [ ] **Step 3: Build**

```
cargo build 2>&1
```

Expected: clean compile.

- [ ] **Step 4: Write a reducer test for DismissError**

In `src/reducer.rs` tests, add:

```rust
#[test]
fn dismiss_error_clears_last_error() {
    let mut state = make_state();
    state.apache.last_error = Some("something broke".into());
    let (state, effects) = reducer(state, Event::DismissError(Service::Apache));
    assert!(state.apache.last_error.is_none(), "last_error must be cleared after DismissError");
    assert!(
        effects.iter().any(|e| matches!(e, SideEffect::LogEvent(_))),
        "should emit a log event"
    );
}
```

- [ ] **Step 5: Run the test**

```
cargo test reducer::tests::dismiss_error_clears_last_error 2>&1
```

Expected: PASS.

- [ ] **Step 6: Commit**

```
git add src/ui.rs src/reducer.rs
git commit -m "feat: error badge dismiss button"
```

---

### Task 8: UI — Open Config File in Editor

Add an "Edit Config" button on Apache, MySQL, and PHP rows that opens the respective config file in the user's default text editor (using Windows `cmd /c start ""`). Config paths already live in `RampConfig`:
- Apache: `config.apache.conf` → `apache/conf/httpd.conf`
- MySQL: `config.mysql.ini` → `mysql/my.ini`
- PHP: `config.php.ini` → `php/php.ini`

This is a fire-and-forget shell-out — no event, no state change. Done directly in the egui click handler.

**Files:**
- Modify: `src/ui.rs`

- [ ] **Step 1: Add a helper to open a file in the default editor**

At the bottom of `src/ui.rs`, add:

```rust
fn open_in_editor(path: &std::path::Path) {
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", &path.to_string_lossy()])
        .spawn();
}
```

`cmd /c start "" <path>` opens the file with its registered default application on Windows. The leading `""` is the window title argument that `start` requires when a path with spaces follows.

- [ ] **Step 2: Add config_path parameter to service_row**

Update the `service_row` signature to accept the config file path:

```rust
fn service_row(
    ui: &mut egui::Ui,
    tx: &Sender<Event>,
    svc: Service,
    status: &crate::state::ServiceStatus,
    configured_port: u16,
    phpmyadmin_enabled: bool,
    phpmyadmin_dir_exists: bool,
    show_admin: bool,
    mysql_running: bool,
    php_running: bool,
    apache_running: bool,
    config_path: &std::path::Path,   // ← new
) {
```

- [ ] **Step 3: Update the three call sites in update()**

In the `update` method, update all three `service_row(...)` calls to pass the config path. Use `state.config.*` fields:

```rust
service_row(
    ui, &self.tx, Service::Apache, &state.apache,
    state.config.apache.port,
    false, false, false,
    mysql_running, php_running, apache_running,
    &state.config.apache.conf,   // ← new
);
service_row(
    ui, &self.tx, Service::Mysql, &state.mysql,
    state.config.mysql.port,
    state.phpmyadmin_enabled, state.phpmyadmin_dir_exists, true,
    mysql_running, php_running, apache_running,
    &state.config.mysql.ini,     // ← new
);
service_row(
    ui, &self.tx, Service::Php, &state.php,
    state.config.php.port,
    false, false, false,
    mysql_running, php_running, apache_running,
    &state.config.php.ini,       // ← new
);
```

- [ ] **Step 4: Add the Edit Config button inside service_row**

Inside `service_row`, in the `ui.with_layout(right_to_left...)` block, add the button after the Restart button:

```rust
if ui.button("✎ Edit Config").clicked() {
    open_in_editor(config_path);
}
```

Place it before the Restart button (since layout is right-to-left, items added later appear further left):

```rust
ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
    // Start/Stop toggle
    ...
    // Restart
    ...
    // Edit Config ← add here (renders left of Restart in RTL layout)
    if ui.button("✎ Edit Config").clicked() {
        open_in_editor(config_path);
    }
    // Admin controls (MySQL row only)
    ...
});
```

- [ ] **Step 5: Build**

```
cargo build 2>&1
```

Expected: clean compile.

- [ ] **Step 6: Commit**

```
git add src/ui.rs
git commit -m "feat: open config file in default editor from service row"
```

---

### Task 9: UI — Open Localhost in Browser (Apache Row)

Add an "↗ Open" button on the Apache row that opens `http://localhost:{port}` in the default browser when Apache is Running. Mirrors the phpMyAdmin open button on the MySQL row.

**Files:**
- Modify: `src/ui.rs`

- [ ] **Step 1: Add a helper to open a URL in the default browser**

At the bottom of `src/ui.rs`, add:

```rust
fn open_in_browser(url: &str) {
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", url])
        .spawn();
}
```

- [ ] **Step 2: Add the Open button to the Apache row**

In `service_row`, the button should only appear for Apache. We can key off `svc == Service::Apache` (the `svc` parameter is already in scope). Add this inside the `right_to_left` layout block, after the Restart button:

```rust
// Open in browser — Apache only
if svc == Service::Apache {
    let port = status.effective_port.unwrap_or(configured_port);
    let open_btn = ui.add_enabled(
        status.state == ServiceState::Running,
        egui::Button::new("↗ Open"),
    );
    if !matches!(status.state, ServiceState::Running) {
        open_btn.on_disabled_hover_text("Apache must be Running");
    }
    if open_btn.clicked() {
        open_in_browser(&format!("http://localhost:{port}"));
    }
}
```

- [ ] **Step 3: Build**

```
cargo build 2>&1
```

Expected: clean compile.

- [ ] **Step 4: Run full test suite and lint**

```
cargo clippy -- -D warnings 2>&1
cargo fmt -- --check 2>&1
cargo test 2>&1
```

Expected: all pass, no warnings.

- [ ] **Step 5: Commit**

```
git add src/ui.rs
git commit -m "feat: open localhost in browser from Apache row"
```

---

### Task 10: Final Build and Lint Pass

- [ ] **Step 1: Full suite**

```
cargo clippy -- -D warnings && cargo fmt -- --check && cargo test 2>&1
```

Expected: all green. Fix any warnings before proceeding.

- [ ] **Step 2: Release build**

```
cargo build --release 2>&1
```

Expected: successful build producing `target/release/ramp.exe`.

- [ ] **Step 3: Commit if any fmt fixes were needed**

If `cargo fmt` changed files:

```
cargo fmt
git add -u
git commit -m "style: fmt cleanup after UX enhancement tasks"
```

---

## Self-Review

**Spec coverage:**
- ✅ Open config files in editor → Task 8
- ✅ Open localhost in browser → Task 9
- ✅ Uptime display → Tasks 3 + 5
- ✅ Log copy/clear → Tasks 4 + 6
- ✅ Error badge dismiss → Tasks 2 + 7
- ✅ Full-width log → Task 1

**Placeholder scan:** None found — all steps contain actual code.

**Type consistency:**
- `format_uptime(u64) → String` — used only in Task 5, defined in Task 5. ✅
- `open_in_editor(&Path)` — defined Task 8 Step 1, called Task 8 Step 4. ✅
- `open_in_browser(&str)` — defined Task 9 Step 1, called Task 9 Step 2. ✅
- `SharedLog::clear()` — defined Task 4, called in UI Task 6. ✅
- `Event::ClearLog` / `Event::DismissError(Service)` — defined Task 2, used Tasks 6 and 7. ✅
- `service_row` signature extended in Task 8 Step 2; all three call sites updated in Task 8 Step 3. ✅
