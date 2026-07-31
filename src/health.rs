use crate::events::Event;
use crate::state::{
    Service, APACHE_READY_TIMEOUT, HEALTH_CHECK_INTERVAL, HEALTH_ENDPOINT_PATH,
    HEALTH_PROBE_TIMEOUT, MYSQL_READY_TIMEOUT, PHP_READY_TIMEOUT,
};
use crossbeam_channel::Sender;
use std::io::Read;
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// Check if Apache is ready.
///
/// Probes `HEALTH_ENDPOINT_PATH`, which the generated httpd.conf aliases to a
/// RAMP-owned static file outside the DocumentRoot (see `apache_conf`). That keeps
/// the probe a plain file read: it never reaches `mod_proxy_fcgi`, so it does not
/// hang when PHP-CGI is down and does not depend on the user's application booting.
///
/// Redirects are deliberately NOT followed. Whatever answers the probe, the only
/// question is whether Apache is serving, and the redirect response already answers
/// it via the Server header. Following a `Location` would hand control of the
/// probe's latency and destination to whatever the redirect points at.
///
/// Any HTTP response identifying as Apache via the Server header counts as ready.
pub fn check_apache_ready(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}{HEALTH_ENDPOINT_PATH}");
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(HEALTH_PROBE_TIMEOUT)
        .build();
    match agent.get(&url).call() {
        Ok(resp) => server_is_apache(resp.header("Server").unwrap_or("")),
        // 4xx/5xx responses come back as Err::Status with the underlying response —
        // a 404 from Apache still proves Apache is up and answering.
        Err(ureq::Error::Status(_, resp)) => server_is_apache(resp.header("Server").unwrap_or("")),
        Err(_) => false,
    }
}

fn server_is_apache(header: &str) -> bool {
    header.to_lowercase().contains("apache")
}

/// Check if MySQL is ready: TCP connect + reads MySQL greeting packet prefix.
pub fn check_mysql_ready(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
        Ok(mut stream) => {
            // MySQL server sends a greeting; wait for at least 4 bytes.
            // set_read_timeout must succeed — if it fails, read_exact would block
            // indefinitely and hang the health check thread permanently.
            if stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .is_err()
            {
                return false;
            }
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).is_ok()
        }
        Err(_) => false,
    }
}

/// Check if PHP-CGI is ready: TCP connect to its FastCGI port succeeds.
pub fn check_php_ready(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok()
}

/// Poll for service readiness up to the spec-defined timeout.
/// Emits ProcessReady on success or ProcessExit{exit_code: None} on timeout.
pub fn poll_until_ready(svc: Service, port: u16, tx: Sender<Event>) {
    let timeout = match svc {
        Service::Apache => APACHE_READY_TIMEOUT,
        Service::Mysql => MYSQL_READY_TIMEOUT,
        Service::Php => PHP_READY_TIMEOUT,
    };
    poll_until_ready_with_timeout(svc, port, tx, timeout);
}

/// Poll for service readiness with an explicit timeout — used directly by integration tests
/// to avoid waiting for the full spec timeout (3–5s) in a test suite.
pub fn poll_until_ready_with_timeout(
    svc: Service,
    port: u16,
    tx: Sender<Event>,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    let poll_interval = Duration::from_millis(200);

    while Instant::now() < deadline {
        let ready = match svc {
            Service::Apache => check_apache_ready(port),
            Service::Mysql => check_mysql_ready(port),
            Service::Php => check_php_ready(port),
        };
        if ready {
            let _ = tx.send(Event::ProcessReady(svc));
            return;
        }
        std::thread::sleep(poll_interval);
    }

    // Timed out — treat as process exit so the reducer handles it
    let _ = tx.send(Event::ProcessExit {
        service: svc,
        exit_code: None,
    });
}

/// Runs health checks on a TICK interval. Returns when stopped (channel dropped).
pub fn run_health_checker(
    svc: Service,
    port: u16,
    tx: Sender<Event>,
    stop: crossbeam_channel::Receiver<()>,
) {
    loop {
        crossbeam_channel::select! {
            recv(stop) -> _ => break,
            default(HEALTH_CHECK_INTERVAL) => {
                let ok = match svc {
                    Service::Apache => check_apache_ready(port),
                    Service::Mysql => check_mysql_ready(port),
                    Service::Php => check_php_ready(port),
                };
                let event = if ok {
                    Event::HealthCheckPass(svc)
                } else {
                    Event::HealthCheckFail(svc)
                };
                if tx.send(event).is_err() {
                    break;
                }
            }
        }
    }
}
