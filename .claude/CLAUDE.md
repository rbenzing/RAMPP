# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Status

RAMPP is a **Rust-based Windows x86-64 local development stack manager** (Apache + MySQL + PHP). Implementation is complete. Read `BRIEF.md`, and `AGENTS.md` before making changes.

## Commands

- **Build:** `cargo build --release`
- **Test (all):** `cargo test`
- **Test (single):** `cargo test <test_name>` (e.g. `cargo test reducer::tests::stopped_start`)
- **Lint:** `cargo clippy -- -D warnings`
- **Format check:** `cargo fmt -- --check`
- **Format fix:** `cargo fmt`
- **Run:** `cargo run`

All three must pass before committing: `cargo clippy -- -D warnings && cargo fmt -- --check && cargo test`

Prefer single-test runs during iteration (`cargo test reducer::tests::test_name`). Full suite is for final verification.

## Architecture

The system is a **deterministic event-driven state machine**:

```
STATE + EVENT → NEW STATE + SIDE EFFECTS
```

**Core architectural law:** State is owned exclusively by the reducer. No code outside the reducer may mutate state. All external inputs (UI, OS signals, timers) must be converted to events and queued.

### Event Loop

Single-threaded reducer consuming a multi-producer FIFO event queue. Processing cycle:
1. Dequeue event
2. `reducer(state, event)` → new state + side effects
3. Execute side effects (spawn/kill processes, file I/O, network probes)
4. Side effects emit follow-up events — they never mutate state directly

### Service State Machine

States: `Stopped → Starting → Running → Stopping → Stopped` (with `Crashed` and `Error` branches)

Key transitions:
- `Starting → PROCESS_READY → Running`
- `Running → PROCESS_EXIT → Crashed → AUTO_RETRY → Starting`
- `Any → FATAL_ERROR → Error`
- Invalid transitions must be rejected without mutating state

### Process Management (Windows-specific)

Every service process **must** be attached to a Windows Job Object at spawn time. If Job Object assignment fails, the service must not start (emit `PROCESS_SPAWN_FAILED`, transition to `Error`). PID tracking is advisory only — kill at the Job level, not PID level.

### Configuration System

`rampp.toml` is the source of truth. All writes must follow: write temp file → `fsync` → atomic rename. Invalid configs are rejected entirely; the last valid config is always preserved.

### Readiness Contracts

- **Apache READY:** TCP port bound + HTTP 200–399 + Apache signature in response (timeout: 3s)
- **MySQL READY:** TCP connect success + MySQL protocol handshake (timeout: 5s)
- **PHP READY:** TCP connect to FastCGI port succeeds (timeout: 5s)

Health checks run every 2s (TICK). Three consecutive failures → `HEALTH_CHECK_FAIL`.

### Retry Policy

Backoff: `1s → 2s → 4s → 8s → STOP`. Max 4 retries, then `Error` state. No infinite loops.

### Security Constraints

- Absolute binary paths only — no PATH-based execution
- Services bind to `127.0.0.1` only
- All file writes atomic; no in-place config modification
- No symlink following for config dir, binaries, or data directory

## Testing Strategy (4-Layer Pyramid)

1. **Pure state tests** — reducer logic, all transitions, invariants (no I/O)
2. **Integration tests** — process spawn/kill, Job Object enforcement, readiness detection (use mock binaries)
3. **System tests** — full Apache + MySQL execution, port conflicts, crash recovery
4. **Property-based tests** — randomized event sequences asserting invariants never break

All events must be loggable and replayable to support deterministic regression testing.

## Invariants (Must Always Hold)

- One process tree per service
- Running state implies a valid Job Object
- No shared ports between services
- No silent state transitions
- Config always valid or rejected entirely
- No orphaned processes

## Key Allowed Events

`START_SERVICE`, `STOP_SERVICE`, `RESTART_SERVICE`, `PROCESS_EXIT`, `PROCESS_READY`, `HEALTH_CHECK_PASS`, `HEALTH_CHECK_FAIL`, `PORT_CONFLICT_DETECTED`, `CONFIG_RELOADED`, `FATAL_ERROR`, `TICK`

## Global State Shape

```
services.apache: ServiceState
services.mysql: ServiceState
services.php: ServiceState
config: RampConfig
ports: PortState
ui: UiState
last_error: Error | null
desired_state.apache: DesiredServiceState
desired_state.mysql: DesiredServiceState
desired_state.php: DesiredServiceState
```

`desired_state` is persisted; runtime state is ephemeral. On startup, restore desired state then reconcile with actual system state.

## Source Layout

| File | Role |
|------|------|
| `src/state.rs` | All types: `AppState`, `ServiceState`, `RampConfig`, constants |
| `src/events.rs` | `Event` and `SideEffect` enums |
| `src/reducer.rs` | Pure reducer function + all Layer 1 unit tests |
| `src/executor.rs` | Translates `SideEffect`s into I/O; owns process/thread handles |
| `src/process.rs` | Windows Job Object spawn/kill |
| `src/health.rs` | Apache HTTP + MySQL TCP + PHP TCP readiness/health checks |
| `src/php_conf.rs` | `php.ini` generation for PHP-CGI |
| `src/config.rs` | `rampp.toml` load/validate + atomic write helper |
| `src/paths.rs` | Install directory contract + path validation |
| `src/logger.rs` | Bounded ring buffer for log lines |
| `src/tray.rs` | Windows system tray (tray-item crate) |
| `src/ui.rs` | egui window (service status, start/stop, log tail) |
| `src/main.rs` | Entrypoint: wires event loop thread, tray thread, egui main thread |
| `assets/icon.ico` | System tray icon |
