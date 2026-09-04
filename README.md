# RAMPP

<div align="center">

[![Build](https://img.shields.io/github/actions/workflow/status/rbenzing/RAMPP/release.yml?style=for-the-badge)](https://github.com/rbenzing/RAMPP/actions/workflows/release.yml)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg?style=for-the-badge)](https://www.gnu.org/licenses/gpl-3.0)
[![Platform](https://img.shields.io/badge/platform-Windows%20x64-lightgrey?style=for-the-badge)](https://github.com/rbenzing/RAMPP/releases)
[![Release](https://img.shields.io/github/v/release/rbenzing/RAMPP?style=for-the-badge)](https://github.com/rbenzing/RAMPP/releases/latest)
[![Rust](https://img.shields.io/badge/rust-2021%20edition-orange?style=for-the-badge&logo=rust&logoColor=white)](https://www.rust-lang.org)
[![Buy Me A Coffee](https://img.shields.io/badge/Buy%20Me%20A%20Coffee-FFDD00?style=for-the-badge&logo=buymeacoffee&logoColor=black)](https://buymeacoffee.com/russellbenzing)

**A deterministic local development stack manager for Windows x64**

⚙️ **Deterministic State Machine** • 🧱 **Apache · MySQL · PHP** • 🛡️ **Job-Object Isolation** • 🦀 **Rust**

[Features](#-features) • [Installation](#-installation) • [Architecture](#-architecture) • [License](#-license)

</div>

---

**RAMPP** is a deterministic local development stack manager for Windows x64. It orchestrates Apache, MySQL, and PHP through a formally defined state machine — no race conditions, no orphaned processes, no partial config writes.

> Replace XAMPP or Laragon with something you can read, audit, and trust.

---

## ✨ Features

- **Deterministic** — every state transition is explicit: `STATE + EVENT → NEW STATE + SIDE EFFECTS`
- **Safe** — every service runs inside a Windows Job Object; killing RAMPP kills the entire process tree, no zombies
- **Observable** — all transitions logged; events are replayable for debugging
- **Fast** — sub-second UI, Apache ready in ≤ 3 s, MySQL ready in ≤ 5 s
- **Self-provisioning** — generates `httpd.conf`, `my.ini`, `php.ini`, `phpmyadmin.conf` and `config.inc.php`, and initialises the MySQL data directory on first run; marker-gated so hand-edited files are never clobbered
- **phpMyAdmin integration** — optional one-click admin toggle, wired automatically to the running MySQL + PHP instances
- **System tray** — lives quietly in the tray; full egui status window on demand
- **Crash recovery** — automatic restart with exponential backoff (1 s → 2 s → 4 s → 8 s → Error)
- **Quick access** — open localhost in the browser or jump straight to a service config file from each row
- **Live uptime** — each running service shows elapsed uptime; startup progress shown during Starting
- **Log tools** — copy the full log to clipboard or clear it with one click; error badges are dismissible

---

## 📋 Requirements

| Requirement | Notes |
|---|---|
| Windows 10/11 x64 | Only supported platform |
| [Apache HTTP Server 2.4 (Win64, VS17)](https://www.apachelounge.com/download/) | From Apache Lounge |
| [MySQL 9.x Community (ZIP)](https://dev.mysql.com/downloads/mysql/) | ZIP archive, not installer |
| [PHP 8.x Thread Safe (ZIP)](https://windows.php.net/download/) | TS x64 ZIP — required for PHP-CGI FastCGI |
| [phpMyAdmin (ZIP)](https://www.phpmyadmin.net/downloads/) | Optional — enables the built-in admin toggle |
| [Visual C++ Redistributable 2022 x64](https://aka.ms/vs/17/release/vc_redist.x64.exe) | Required by Apache Lounge builds |

---

## 📦 Installation

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

Optionally, extract [phpMyAdmin](https://www.phpmyadmin.net/downloads/) so that `index.php` ends up at:

```
C:\rampp\phpmyadmin\index.php
```

phpMyAdmin is not required for Apache, MySQL or PHP to run — it only unlocks the
admin toggle in the status window (see [Configuration ownership](#configuration-ownership)
below for the files RAMPP generates for it).

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
  phpmyadmin\        (optional)
    index.php
    ...
```

### 3. Install the Visual C++ Redistributable

Run `vc_redist.x64.exe` if you haven't already.

### 4. Run RAMPP

Double-click `rampp.exe`. On first launch RAMPP will:

1. Generate `rampp.toml` (ports 80 + 3306 + 9000)
2. Create `apache\conf\httpd.conf`, `apache\htdocs\index.php`, `mysql\my.ini`, `php\php.ini`, and (if `phpmyadmin\` exists) `apache\conf\phpmyadmin.conf` + `phpmyadmin\config.inc.php`
3. Run `mysqld --initialize-insecure` (~10–20 s, once only), then create a `127.0.0.1` account so RAMPP's own TCP connections (health checks, phpMyAdmin) can authenticate — see the note below
4. Show the system tray icon and status window

Click **Start All** to bring up all three services. Apache will be at `http://127.0.0.1/`, MySQL at `127.0.0.1:3306` (root, no password), and PHP-CGI will be listening on `127.0.0.1:9000` (FastCGI, proxied from Apache).

> **Why a second MySQL account?** `mysqld --initialize-insecure` only ever creates
> `root@localhost`. RAMPP's own connections — health checks, phpMyAdmin, anything
> in `rampp.toml`'s `[phpmyadmin]` section — are TCP to `127.0.0.1` (the
> loopback-only security constraint), which is a distinct grant from `localhost`
> and would otherwise be refused outright. First-run initialization also grants
> `<mysql_user>@'127.0.0.1'` (default user `root`, empty password) full
> privileges — but not `WITH GRANT OPTION` — so this works out of the box.

### Configuration ownership

RAMPP generates `httpd.conf`, `my.ini`, `php.ini`, `phpmyadmin.conf` and
phpMyAdmin's `config.inc.php`, and each one carries a marker line on its first
line identifying it as generated.

- **Keep the marker** and RAMPP keeps the file in sync — ports, document root and
  phpMyAdmin wiring all follow your settings automatically. Your previous content
  is saved beside it as `.bak` whenever RAMPP changes it.
- **Remove the marker** and the file becomes yours. RAMPP will never write it
  again. If a port then needs to move because something else holds it, the
  affected service reports an error naming the file instead of silently
  misconfiguring itself.

**Upgrading to 1.5.0:** before 1.5.0, `php.ini` and `my.ini` were never
regenerated once created. If you edited either of them while leaving the marker
line intact, your changes will be replaced on first run and saved to
`php.ini.bak` / `my.ini.bak`. Remove the marker line first if you want RAMPP to
leave the file alone permanently.

**phpMyAdmin during a service crash:** phpMyAdmin now stays enabled if MySQL or
PHP crashes, rather than switching itself off and requiring a manual re-toggle.
If PHP crashes, Apache's `mod_proxy_fcgi` can't reach the PHP-CGI backend and
returns 503 until auto-retry restarts it; if MySQL crashes while Apache/PHP
stay up, phpMyAdmin's own PHP code hits the failed connection instead, which
surfaces as an in-page connection error, not an Apache-level 503.

---

## ⚙️ Configuration

Edit `C:\rampp\rampp.toml` to change ports:

```toml
install_dir = "C:\\ramppp"

[apache]
port = 80

[mysql]
port = 3306

[php]
port = 9000

[phpmyadmin]
mysql_user = "root"
mysql_password = ""
```

RAMPP validates the entire file before accepting it — an invalid config is rejected completely and the last valid config is preserved. After editing, restart the affected service from the UI.

`[phpmyadmin]` sets the credentials RAMPP uses for its own MySQL connections —
health checks and phpMyAdmin's `config.inc.php` — and the account that first-run
initialization grants at `127.0.0.1`, described above. It does not need to match
any account you create yourself for other tools.

See [Configuration ownership](#configuration-ownership) above for which files
RAMPP regenerates and which it leaves alone.

---

## 🏗️ Architecture

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
| Config | `config.rs` | `rampp.toml` load/validate + atomic write |
| Paths | `paths.rs` | Install-dir contract, traversal rejection, symlink rejection |
| Log | `logger.rs` | Bounded ring buffer (1 000 lines) |
| Apache conf | `apache_conf.rs` | Generate `httpd.conf` with PHP FastCGI proxy |
| MySQL conf | `mysql_conf.rs` | Generate `my.ini`, initialize data dir, grant the `127.0.0.1` account |
| PHP conf | `php_conf.rs` | Generate `php.ini` for PHP-CGI |
| phpMyAdmin conf | `phpmyadmin_conf.rs` | Generate `phpmyadmin.conf` and `config.inc.php` |
| Provisioning | `provision.rs` | Marker-gated, diff-driven reconciler for every managed config file |
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

## 🔨 Building from source

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

## 🔒 Security model

- Services bind to `127.0.0.1` only — no external exposure by default
- All binary paths are absolute and validated against the install directory — no PATH-based execution
- Environment variables are sanitised before spawning child processes
- Config writes are atomic (`temp → fsync → rename`) — a crash during write cannot corrupt the config
- Symlinks are rejected for the config directory, binaries, and data directory
- MySQL is initialised with `--initialize-insecure` (no root password); first run also grants `<mysql_user>@'127.0.0.1'` full privileges (no `WITH GRANT OPTION`) so RAMPP's own loopback connections authenticate — **suitable for local development only**

---

## 📄 License

RAMPP is free software: you can redistribute it and/or modify it under the terms of the [GNU General Public License v3.0](LICENSE).

---

## 👤 About the Author

Built by **Russell Benzing**. RAMPP is a deterministic, auditable alternative to XAMPP/Laragon for local Windows development.

---

## 🆘 Support

- **Issues**: [GitHub Issues](https://github.com/rbenzing/RAMPP/issues)
- **Releases**: [github.com/rbenzing/RAMPP/releases](https://github.com/rbenzing/RAMPP/releases)

If RAMPP is useful to you, you can support the work:

<div align="center">

[![Buy Me A Coffee](https://img.shields.io/badge/Buy%20Me%20A%20Coffee-FFDD00?style=for-the-badge&logo=buymeacoffee&logoColor=black)](https://buymeacoffee.com/russellbenzing)

</div>
