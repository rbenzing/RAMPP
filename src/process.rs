use crate::state::{RampConfig, Service};
use crossbeam_channel::Sender;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{
    CreateProcessW, ResumeThread, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW,
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
};

use crate::events::Event;
use crate::paths::validate_critical_path;

/// RAII wrapper: dropping closes the Job Object handle, which (with KILL_ON_JOB_CLOSE)
/// terminates the entire process tree including all children.
pub struct JobHandle(pub HANDLE);

impl Drop for JobHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

// SAFETY: Job handles are only sent across threads within this module under controlled ownership.
unsafe impl Send for JobHandle {}

/// A running service process. Fields are pub(crate) so the executor's watcher can access them.
pub struct ServiceProcess {
    pub job_handle: JobHandle,
    /// Raw process handle (owned — we close it on drop via `CloseHandle`).
    proc_handle: HANDLE,
    /// Raw thread handle (owned — closed on drop).
    thread_handle: HANDLE,
    pub service: Service,
}

impl ServiceProcess {
    /// Force-kill by closing the Job Object (kills entire process tree), then wait for cleanup.
    pub fn kill(mut self) {
        // Invalidate the job handle so Drop doesn't double-close it; close it here first
        // so KILL_ON_JOB_CLOSE fires before we wait on proc_handle.
        let job = std::mem::take(&mut self.job_handle.0);
        if !job.is_invalid() {
            unsafe {
                let _ = CloseHandle(job);
            }
        }
        // Wait for the process to actually terminate before returning.
        // This prevents the caller from assuming the process is gone when it isn't.
        unsafe {
            WaitForSingleObject(self.proc_handle, INFINITE);
        }
        // Drop runs here: closes proc_handle and thread_handle
    }

    /// Non-blocking: has the process exited? Returns exit code if so.
    pub fn try_wait(&self) -> Option<u32> {
        unsafe {
            // WaitForSingleObject with 0ms timeout — returns WAIT_OBJECT_0 (0) if done.
            if WaitForSingleObject(self.proc_handle, 0).0 == 0 {
                let mut code: u32 = 0;
                let _ = windows::Win32::System::Threading::GetExitCodeProcess(
                    self.proc_handle,
                    &mut code,
                );
                Some(code)
            } else {
                None
            }
        }
    }
}

impl Drop for ServiceProcess {
    fn drop(&mut self) {
        unsafe {
            if !self.proc_handle.is_invalid() {
                let _ = CloseHandle(self.proc_handle);
            }
            if !self.thread_handle.is_invalid() {
                let _ = CloseHandle(self.thread_handle);
            }
        }
    }
}

// SAFETY: ServiceProcess is only moved between threads under controlled ownership (watcher).
unsafe impl Send for ServiceProcess {}

/// Build a null-terminated UTF-16 command line string for `CreateProcessW`.
/// Arguments are quoted with the MSVC quoting rules (backslash-escape before `"`).
fn build_command_line(bin: &std::path::Path, args: &[String]) -> Vec<u16> {
    fn quote_arg(s: &str) -> String {
        if !s.contains([' ', '\t', '"']) {
            return s.to_owned();
        }
        let mut out = String::from('"');
        let mut backslashes: usize = 0;
        for ch in s.chars() {
            match ch {
                '\\' => backslashes += 1,
                '"' => {
                    // Double all preceding backslashes, then escape the quote.
                    for _ in 0..backslashes {
                        out.push('\\');
                    }
                    out.push('\\');
                    out.push('"');
                    backslashes = 0;
                }
                _ => {
                    for _ in 0..backslashes {
                        out.push('\\');
                    }
                    out.push(ch);
                    backslashes = 0;
                }
            }
        }
        // Double trailing backslashes before closing quote.
        for _ in 0..backslashes {
            out.push('\\');
        }
        out.push('"');
        out
    }

    let mut cmd = quote_arg(&bin.to_string_lossy());
    for arg in args {
        cmd.push(' ');
        cmd.push_str(&quote_arg(arg));
    }
    OsStr::new(&cmd).encode_wide().chain(Some(0)).collect()
}

/// Build a null-terminated UTF-16 environment block for `CreateProcessW`.
/// Format: KEY=VALUE\0KEY=VALUE\0\0
fn build_env_block(vars: &[(String, String)]) -> Vec<u16> {
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in vars {
        let entry = format!("{k}={v}");
        block.extend(OsStr::new(&entry).encode_wide());
        block.push(0);
    }
    block.push(0); // double-null terminator
    block
}

/// Validate binary, spawn process suspended, attach to Windows Job Object, then resume.
///
/// `effective_port` is the port the service should actually bind to. For PHP this is
/// passed as a CLI argument; for Apache/MySQL the port is baked into their config
/// files, which the caller must regenerate before calling this if it differs from
/// the configured port.
///
/// Using CREATE_SUSPENDED closes the race window where grandchildren could be spawned
/// before Job Object assignment: the process cannot execute any user code until we call
/// ResumeThread after AssignProcessToJobObject succeeds.
///
/// Returns Err if any step fails — caller MUST NOT start the service in that case.
pub fn spawn_service(
    svc: Service,
    cfg: &RampConfig,
    effective_port: u16,
    _tx: Sender<Event>,
) -> Result<ServiceProcess, String> {
    let (bin, args, work_dir) = service_params(svc, cfg, effective_port);

    validate_critical_path(&bin, &cfg.install_dir, false)
        .map_err(|e| format!("binary validation: {e}"))?;

    if !bin.exists() {
        return Err(format!("binary not found: {}", bin.display()));
    }

    // Create Job Object
    let job_raw =
        unsafe { CreateJobObjectW(None, None) }.map_err(|e| format!("CreateJobObjectW: {e}"))?;

    // Configure: kill all processes when the job handle is closed.
    // SAFETY: size_of never exceeds u32::MAX for any realistic struct.
    let info_size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>();
    assert!(
        info_size <= u32::MAX as usize,
        "JOBOBJECT_EXTENDED_LIMIT_INFORMATION size overflows u32"
    );
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    unsafe {
        SetInformationJobObject(
            job_raw,
            JobObjectExtendedLimitInformation,
            &raw const info as *const _,
            info_size as u32,
        )
        .map_err(|e| {
            let _ = CloseHandle(job_raw);
            format!("SetInformationJobObject: {e}")
        })?;
    }

    // Build sanitized environment block
    let system_root = crate::paths::system_root();
    let temp = crate::paths::temp_dir();
    let mut env_vars: Vec<(String, String)> =
        vec![("SystemRoot".into(), system_root), ("TEMP".into(), temp)];
    if svc == Service::Php {
        env_vars.extend(php_env(cfg));
    }

    let cmd_line = build_command_line(&bin, &args);
    let env_block = build_env_block(&env_vars);

    // Convert working directory to wide string
    let work_dir_wide: Vec<u16> = OsStr::new(work_dir.as_os_str())
        .encode_wide()
        .chain(Some(0))
        .collect();

    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();

    // Spawn suspended: no user code runs until ResumeThread.
    // This guarantees Job Object assignment happens before any grandchildren can be spawned.
    let created = unsafe {
        CreateProcessW(
            None,
            windows::core::PWSTR(cmd_line.as_ptr() as *mut u16),
            None,
            None,
            false,
            CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            Some(env_block.as_ptr() as *const _),
            windows::core::PCWSTR(work_dir_wide.as_ptr()),
            &si,
            &mut pi,
        )
    };

    if let Err(e) = created {
        unsafe {
            let _ = CloseHandle(job_raw);
        }
        return Err(format!("CreateProcessW: {e}"));
    }

    // Assign to Job Object before resuming — the process is still suspended.
    let assign = unsafe { AssignProcessToJobObject(job_raw, pi.hProcess) };
    if let Err(e) = assign {
        unsafe {
            // Terminate the suspended process, close all handles.
            let _ = TerminateProcess(pi.hProcess, 1);
            let _ = CloseHandle(pi.hProcess);
            let _ = CloseHandle(pi.hThread);
            let _ = CloseHandle(job_raw);
        }
        return Err(format!(
            "AssignProcessToJobObject: {e} — service must not start"
        ));
    }

    // Now it is safe to let the process run.
    unsafe {
        ResumeThread(pi.hThread);
    }

    Ok(ServiceProcess {
        job_handle: JobHandle(job_raw),
        proc_handle: pi.hProcess,
        thread_handle: pi.hThread,
        service: svc,
    })
}

/// Pre-check whether a port is available.
///
/// Tries to connect to 127.0.0.1:port — if connect succeeds, a real listener owns
/// the port (conflict). If connect fails with ConnectionRefused, the port has no
/// listener and is free to bind. We avoid TcpListener::bind here because on Windows
/// it returns WSAEADDRINUSE for sockets in TIME_WAIT state, producing false positives
/// after a crash loop even though no live process owns the port.
pub fn check_port_available(port: u16) -> bool {
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    // Ok(_) = a listener accepted us → port in use
    // Err(_) = refused, timed out, or no route → no listener, port is free
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err()
}

/// Scan upward from `start` for the first free port, up to `start + range` inclusive.
/// Returns None if every port in the range is occupied.
pub fn find_available_port(start: u16, range: u16) -> Option<u16> {
    for offset in 0..=range {
        let port = start.checked_add(offset)?;
        if check_port_available(port) {
            return Some(port);
        }
    }
    None
}

fn service_params(
    svc: Service,
    cfg: &RampConfig,
    effective_port: u16,
) -> (PathBuf, Vec<String>, PathBuf) {
    match svc {
        Service::Apache => {
            // Apache's port is baked into httpd.conf; the caller is responsible for
            // regenerating that file when effective_port differs from the configured port.
            let bin = cfg.apache.bin.clone();
            let work_dir = cfg.install_dir.join("apache");
            let args = vec![
                "-f".into(),
                cfg.apache.conf.display().to_string(),
                "-DFOREGROUND".into(),
            ];
            (bin, args, work_dir)
        }
        Service::Mysql => {
            // MySQL's port is baked into my.ini; the caller is responsible for
            // regenerating it when effective_port differs from the configured port.
            let bin = cfg.mysql.bin.clone();
            let work_dir = cfg.install_dir.join("mysql");
            let args = vec![
                format!("--defaults-file={}", cfg.mysql.ini.to_string_lossy()),
                "--console".into(),
            ];
            (bin, args, work_dir)
        }
        Service::Php => {
            let bin = cfg.php.bin.clone();
            let work_dir = cfg.install_dir.join("php");
            // PHP-CGI in FastCGI mode: bind to loopback on the resolved port.
            let args = vec!["-b".into(), format!("127.0.0.1:{effective_port}")];
            (bin, args, work_dir)
        }
    }
}

/// Extra environment variables needed by PHP-CGI.
fn php_env(cfg: &RampConfig) -> Vec<(String, String)> {
    vec![
        (
            "PHPRC".into(),
            cfg.php
                .ini
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        ),
        // Prevent PHP from forking its own child workers — let Apache manage concurrency.
        ("PHP_FCGI_CHILDREN".into(), "0".into()),
        ("PHP_FCGI_MAX_REQUESTS".into(), "500".into()),
    ]
}

/// What a service's error log says about why it exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // wired up in task 12
pub enum ExitDiagnosis {
    /// The service could not bind its port. `reserved` means Windows refused the
    /// bind outright (WSAEACCES), which usually means the port sits inside an
    /// excluded port range reserved by Hyper-V, WSL2 or Docker Desktop — those
    /// are invisible to a connect-based availability check.
    PortBindFailure {
        reserved: bool,
    },
    Unknown,
}

/// Classify a service exit from the tail of its error log. Pure — the caller is
/// responsible for reading only the bytes this run produced.
#[allow(dead_code)] // wired up in task 12
pub fn diagnose_exit(svc: Service, log_tail: &str) -> ExitDiagnosis {
    let lower = log_tail.to_lowercase();

    let bind_failed = match svc {
        Service::Apache => lower.contains("ah00072") || lower.contains("make_sock: could not bind"),
        Service::Mysql => lower.contains("can't start server: bind on tcp/ip port"),
        // PHP-CGI writes bind errors to stderr, not php_errors.log, so there is
        // no reliable signature to match. Fall through to Unknown.
        Service::Php => false,
    };

    if !bind_failed {
        return ExitDiagnosis::Unknown;
    }

    let reserved = lower.contains("10013")
        || lower.contains("wsaeacces")
        || lower.contains("forbidden by its access permissions");

    ExitDiagnosis::PortBindFailure { reserved }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apache_bind_failure_is_recognized() {
        let tail = "[crit] (OS 10048)Only one usage of each socket address \
                    is normally permitted.  : AH00072: make_sock: could not bind to \
                    address 127.0.0.1:8080";
        assert!(matches!(
            diagnose_exit(Service::Apache, tail),
            ExitDiagnosis::PortBindFailure { reserved: false }
        ));
    }

    #[test]
    fn apache_reserved_range_failure_is_flagged() {
        let tail = "(OS 10013)An attempt was made to access a socket in a way \
                    forbidden by its access permissions.  : AH00072: make_sock: \
                    could not bind to address 127.0.0.1:8080";
        assert!(matches!(
            diagnose_exit(Service::Apache, tail),
            ExitDiagnosis::PortBindFailure { reserved: true }
        ));
    }

    #[test]
    fn mysql_bind_failure_is_recognized() {
        let tail = "[ERROR] [MY-010262] [Server] Can't start server: Bind on TCP/IP \
                    port: An attempt was made to access a socket in a way forbidden \
                    by its access permissions.";
        assert!(matches!(
            diagnose_exit(Service::Mysql, tail),
            ExitDiagnosis::PortBindFailure { reserved: true }
        ));
    }

    #[test]
    fn unrelated_crash_is_not_a_bind_failure() {
        let tail = "[ERROR] [MY-012574] [InnoDB] Unable to lock ./ibdata1 error: 11";
        assert!(matches!(
            diagnose_exit(Service::Mysql, tail),
            ExitDiagnosis::Unknown
        ));
    }

    #[test]
    fn empty_log_is_not_a_bind_failure() {
        assert!(matches!(
            diagnose_exit(Service::Apache, ""),
            ExitDiagnosis::Unknown
        ));
    }
}
