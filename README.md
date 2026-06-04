# RAMPP

[![Build](https://github.com/rbenzing/RAMPP/actions/workflows/release.yml/badge.svg)](https://github.com/rbenzing/RAMPP/actions/workflows/release.yml)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)
[![Platform](https://img.shields.io/badge/platform-Windows%20x64-lightgrey)](https://github.com/rbenzing/RAMPP/releases)
[![Release](https://img.shields.io/github/v/release/rbenzing/RAMPP)](https://github.com/rbenzing/RAMPP/releases/latest)
[![Rust](https://img.shields.io/badge/rust-2021%20edition-orange)](https://www.rust-lang.org)

**RAMPP** is a deterministic local development stack manager for Windows x64. It orchestrates Apache, MySQL, and PHP through a formally defined state machine — no race conditions, no orphaned processes, no partial config writes.

> Replace XAMPP or Laragon with something you can read, audit, and trust.

---

## Features

- **Deterministic** — every state transition is explicit: `STATE + EVENT → NEW STATE + SIDE EFFECTS`
- **Safe** — every service runs inside a Windows Job Object; killing RAMPP kills the entire process tree, no zombies
- **Observable** — all transitions logged; events are replayable for debugging
- **Fast** — sub-second UI, Apache ready in ≤ 3 s, MySQL ready in ≤ 5 s
- **Self-provisioning** — generates `httpd.conf`, `my.ini`, `php.ini`, and initialises the MySQL data directory on first run
- **System tray** — lives quietly in the tray; full egui status window on demand
- **Crash recovery** — automatic restart with exponential backoff (1 s → 2 s → 4 s → 8 s → Error)
- **Quick access** — open localhost in the browser or jump straight to a service config file from each row
- **Live uptime** — each running service shows elapsed uptime; startup progress shown during Starting
- **Log tools** — copy the full log to clipboard or clear it with one click; error badges are dismissible

---

## Requirements

| Requirement | Notes |
|---|---|
| Windows 10/11 x64 | Only supported platform |
| [Apache HTTP Server 2.4 (Win64, VS17)](https://www.apachelounge.com/download/) | From Apache Lounge |
| [MySQL 9.x Community (ZIP)](https://dev.mysql.com/downloads/mysql/) | ZIP archive, not installer |
| [PHP 8.x Thread Safe (ZIP)](https://windows.php.net/download/) | TS x64 ZIP — required for PHP-CGI FastCGI |
| [Visual C++ Redistributable 2022 x64](https://aka.ms/vs/17/release/vc_redist.x64.exe) | Required by Apache Lounge builds |

---

## Installation

### 1. Download the release binary

Grab `rampp.exe` from the [latest release](https://github.com/rbenzing/RAMPP/releases/latest) and place it at:

```
C:\rampp\rampp.exe
```

### 2. Extract Apache and MySQL

Extract the Apache ZIP so that `httpd.exe` ends up at:

```
C:\rampp\apache\bin\httpd.exe
```

Extract the MySQL ZIP so that `mysqld.exe` ends up at:

```
C:\rampp\mysql\bin\mysqld.exe
```

Extract the PHP ZIP so that `php-cgi.exe` ends up at:

```
C:\rampp\php\php-cgi.exe
```

The final layout should look like:

```
C:\rampp\
  rampp.exe
  apache\
    bin\httpd.exe
    modules\
    ...
  mysql\
    bin\mysqld.exe
    lib\
    share\
    ...
  php\
    php-cgi.exe
    ext\
    ...
```

### 3. Install the Visual C++ Redistributable

Run `vc_redist.x64.exe` if you haven't already.

### 4. Run RAMPP

Double-click `rampp.exe`. On first launch RAMPP will:

1. Generate `ramp.toml` (ports 80 + 3306 + 9000)
2. Create `apache\conf\httpd.conf`, `apache\htdocs\index.php`, `mysql\my.ini`, `php\php.ini`
3. Run `mysqld --initialize-insecure` (~10–20 s, once only)
4. Show the system tray icon and status window

Click **Start All** to bring up all three services. Apache will be at `http://127.0.0.1/`, MySQL at `127.0.0.1:3306` (root, no password), and PHP-CGI will be listening on `127.0.0.1:9000` (FastCGI, proxied from Apache).

---

## Configuration

Edit `C:\rampp\ramp.toml` to change ports:

```toml
install_dir = "C:\\rampp"

[apache]
port = 80

[mysql]
port = 3306

[php]
port = 9000
```

RAMPP validates the entire file before accepting it — an invalid config is rejected completely and the last valid config is preserved. After editing, restart the affected service from the UI.

> `httpd.conf`, `my.ini`, and `php.ini` are generated once and never overwritten — edit them freely for custom virtual hosts, extensions, etc.

---

## Architecture

RAMPP is a single binary built around a pure reducer:

```
STATE + EVENT → NEW STATE + SIDE EFFECTS
```

| Layer | File | Role |
|---|---|---|
| Types | `state.rs` | `AppState`, `ServiceState` machine, all constants |
| Events | `events.rs` | `Event` enum (13 variants) + `SideEffect` enum |
| Logic | `reducer.rs` | Pure function — no I/O, fully unit-tested |
| I/O | `executor.rs` | Translates `SideEffect`s into process ops and threads |
| Processes | `process.rs` | Windows Job Object spawn/kill |
| Health | `health.rs` | Apache HTTP + MySQL TCP + PHP TCP readiness and health checks |
| Config | `config.rs` | `ramp.toml` load/validate + atomic write |
| Paths | `paths.rs` | Install-dir contract, traversal rejection, symlink rejection |
| Log | `logger.rs` | Bounded ring buffer (1 000 lines) |
| Apache conf | `apache_conf.rs` | Generate `httpd.conf` with PHP FastCGI proxy |
| MySQL conf | `mysql_conf.rs` | Generate `my.ini`, initialize data dir |
| PHP conf | `php_conf.rs` | Generate `php.ini` for PHP-CGI |
| Tray | `tray.rs` | Windows system tray |
| UI | `ui.rs` | egui status window |

### Service state machine

```
Stopped ──START──► Starting ──PROCESS_READY──► Running
                      │                           │
               PROCESS_EXIT                 PROCESS_EXIT
                      │                           │
                      └──────────► Crashed ◄──────┘
                                     │
                               AUTO_RETRY (×4, backoff)
                                     │
                                  Starting
                                     │
                             (max retries exceeded)
                                     │
                                   Error

Any state ──FATAL_ERROR──► Error
Running   ──STOP──► Stopping ──PROCESS_EXIT──► Stopped
```

### Process isolation

Every service spawns inside a dedicated **Windows Job Object** with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Dropping the job handle terminates the entire process tree — Apache child workers, MySQL threads, everything — with no orphans.

If `AssignProcessToJobObject` fails the service transitions directly to `Error` and never starts.

### Health checks

| Service | Check | Pass condition | Interval | Fail threshold |
|---|---|---|---|---|
| Apache | HTTP GET `127.0.0.1:port/` | 200–399 + `Server: Apache` | 2 s | 3 consecutive |
| MySQL | TCP connect + 4-byte greeting | Handshake starts | 2 s | 3 consecutive |
| PHP | TCP connect to FastCGI port | Connection succeeds | 2 s | 3 consecutive |

Three consecutive failures trigger `HEALTH_CHECK_FAIL`, which kills the service and schedules a retry.

---

## Building from source

```bash
# Prerequisites: Rust stable toolchain (rustup.rs), MSVC build tools
git clone https://github.com/rbenzing/RAMPP.git
cd rampp
cargo build --release
# Binary at target\release\rampp.exe
```

Run the test suite:

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt -- --check
```

---

## Security model

- Services bind to `127.0.0.1` only — no external exposure by default
- All binary paths are absolute and validated against the install directory — no PATH-based execution
- Environment variables are sanitised before spawning child processes
- Config writes are atomic (`temp → fsync → rename`) — a crash during write cannot corrupt the config
- Symlinks are rejected for the config directory, binaries, and data directory
- MySQL is initialised with `--initialize-insecure` (no root password) — **suitable for local development only**

---

## License

RAMPP is free software: you can redistribute it and/or modify it under the terms of the [GNU General Public License v3.0](LICENSE).
