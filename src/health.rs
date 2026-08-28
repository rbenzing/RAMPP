use crate::events::Event;
use crate::state::{
    Service, APACHE_READY_TIMEOUT, HEALTH_CHECK_INTERVAL, HEALTH_ENDPOINT_PATH,
    HEALTH_PROBE_TIMEOUT, MYSQL_READY_TIMEOUT, PHP_READY_TIMEOUT,
};
use crossbeam_channel::Sender;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// Check if Apache is ready.
///
/// Probes `HEALTH_ENDPOINT_PATH`, which the generated httpd.conf aliases to a
/// RAMPP-owned static file outside the DocumentRoot (see `apache_conf`). That keeps
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

/// Largest handshake payload we will read. A real greeting is well under this;
/// the cap stops a garbage or hostile server from making us allocate.
const MYSQL_MAX_HANDSHAKE: usize = 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
pub enum MysqlProbe {
    Ready,
    NotListening,
    Unhealthy(String),
}

/// Connect, read the greeting, verify it is really the MySQL protocol, then
/// disconnect cleanly with COM_QUIT.
///
/// The clean disconnect matters: dropping the socket mid-handshake every 2
/// seconds made mysqld log `Aborted connection` continuously. Checking the
/// protocol version byte is what makes this an actual handshake check — the
/// previous "any 4 bytes" test passed for any TCP server at all.
pub fn probe_mysql(port: u16) -> MysqlProbe {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = match TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) {
        Ok(s) => s,
        Err(_) => return MysqlProbe::NotListening,
    };
    // set_read_timeout/set_write_timeout must succeed — if either fails, a
    // subsequent read/write could block indefinitely and hang the health
    // check thread permanently.
    if stream.set_read_timeout(Some(PROBE_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(PROBE_TIMEOUT)).is_err()
    {
        return MysqlProbe::NotListening;
    }

    let mut header = [0u8; 4];
    if stream.read_exact(&mut header).is_err() {
        return MysqlProbe::NotListening;
    }
    let len = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
    if len == 0 || len > MYSQL_MAX_HANDSHAKE {
        return MysqlProbe::Unhealthy(format!("implausible handshake length {len}"));
    }
    let mut payload = vec![0u8; len];
    if stream.read_exact(&mut payload).is_err() {
        return MysqlProbe::Unhealthy("truncated handshake".to_string());
    }

    match payload[0] {
        // Protocol version 10 — a real MySQL server greeting.
        0x0a => {
            // COM_QUIT: payload length 1, sequence 1, command 0x01.
            let _ = stream.write_all(&[0x01, 0x00, 0x00, 0x01, 0x01]);
            let _ = stream.flush();
            MysqlProbe::Ready
        }
        0xFF => {
            let msg = String::from_utf8_lossy(&payload[1..]).trim().to_string();
            MysqlProbe::Unhealthy(format!("server returned an error packet: {msg}"))
        }
        other => MysqlProbe::Unhealthy(format!("unexpected protocol version 0x{other:02x}")),
    }
}

/// Boolean wrapper for call sites that only need readiness.
pub fn check_mysql_ready(port: u16) -> bool {
    matches!(probe_mysql(port), MysqlProbe::Ready)
}

/// Check PHP-CGI by speaking FastCGI rather than opening and dropping a socket.
///
/// `php-cgi.exe` on Windows serves requests serially, so a bare connect competes
/// with real traffic. FCGI_GET_VALUES is a management record: the responder
/// answers it without running a script. FCGI_UNKNOWN_TYPE is an equally good
/// answer — it still proves a FastCGI responder is on the other end.
pub fn check_php_ready(port: u16) -> bool {
    const FCGI_GET_VALUES: u8 = 9;
    const FCGI_GET_VALUES_RESULT: u8 = 10;
    const FCGI_UNKNOWN_TYPE: u8 = 11;
    const QUERY: &[u8] = b"FCGI_MPXS_CONNS";

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, PROBE_TIMEOUT) else {
        return false;
    };
    if stream.set_read_timeout(Some(PROBE_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(PROBE_TIMEOUT)).is_err()
    {
        return false;
    }

    // Body is one name-value pair with an empty value: nameLen, valueLen, name.
    let content_len = 2 + QUERY.len();
    let mut record = vec![
        1,                        // version
        FCGI_GET_VALUES,          // type
        0,                        // requestId hi — 0 = management record
        0,                        // requestId lo
        (content_len >> 8) as u8, // contentLength hi
        content_len as u8,        // contentLength lo
        0,                        // paddingLength
        0,                        // reserved
        QUERY.len() as u8,        // nameLength
        0,                        // valueLength
    ];
    record.extend_from_slice(QUERY);

    if stream.write_all(&record).is_err() || stream.flush().is_err() {
        return false;
    }

    let mut header = [0u8; 8];
    if stream.read_exact(&mut header).is_err() {
        return false;
    }
    header[0] == 1 && matches!(header[1], FCGI_GET_VALUES_RESULT | FCGI_UNKNOWN_TYPE)
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
                    Service::Mysql => match probe_mysql(port) {
                        MysqlProbe::Ready => true,
                        MysqlProbe::NotListening => false,
                        MysqlProbe::Unhealthy(msg) => {
                            let _ = tx.send(Event::DiagnosticLog(format!("MySQL: {msg}")));
                            false
                        }
                    },
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    /// Serve one connection with `greeting`, then hand back everything the client
    /// sent so the test can assert on a clean COM_QUIT.
    fn fake_server(greeting: Vec<u8>) -> (u16, std::sync::mpsc::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let _ = sock.write_all(&greeting);
                let _ = sock.flush();
                let mut got = Vec::new();
                let mut buf = [0u8; 64];
                sock.set_read_timeout(Some(Duration::from_millis(500))).ok();
                if let Ok(n) = sock.read(&mut buf) {
                    got.extend_from_slice(&buf[..n]);
                }
                let _ = tx.send(got);
            }
        });
        (port, rx)
    }

    /// Minimal MySQL greeting: 3-byte LE length, 1-byte sequence, then payload
    /// whose first byte is the protocol version.
    fn mysql_greeting(protocol_version: u8) -> Vec<u8> {
        let payload = vec![protocol_version, b'9', b'.', b'7', 0];
        let len = payload.len();
        let mut pkt = vec![len as u8, (len >> 8) as u8, (len >> 16) as u8, 0];
        pkt.extend_from_slice(&payload);
        pkt
    }

    #[test]
    fn mysql_probe_ready_on_protocol_version_10() {
        let (port, _rx) = fake_server(mysql_greeting(10));
        assert!(matches!(probe_mysql(port), MysqlProbe::Ready));
    }

    #[test]
    fn mysql_probe_sends_com_quit_before_closing() {
        let (port, rx) = fake_server(mysql_greeting(10));
        assert!(matches!(probe_mysql(port), MysqlProbe::Ready));
        let sent = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // COM_QUIT: payload len 1, sequence 1, command 0x01.
        assert_eq!(sent, vec![0x01, 0x00, 0x00, 0x01, 0x01]);
    }

    #[test]
    fn mysql_probe_reports_err_packet_as_unhealthy() {
        let (port, _rx) = fake_server(mysql_greeting(0xFF));
        assert!(matches!(probe_mysql(port), MysqlProbe::Unhealthy(_)));
    }

    #[test]
    fn mysql_probe_rejects_a_server_that_is_not_mysql() {
        // Four arbitrary bytes used to pass the old check.
        let (port, _rx) = fake_server(vec![b'H', b'T', b'T', b'P']);
        assert!(!check_mysql_ready(port));
    }

    #[test]
    fn mysql_probe_not_listening_when_nothing_is_bound() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(matches!(probe_mysql(port), MysqlProbe::NotListening));
    }

    /// FastCGI response header: version 1, the given type, request id 0.
    fn fcgi_header(record_type: u8) -> Vec<u8> {
        vec![1, record_type, 0, 0, 0, 0, 0, 0]
    }

    #[test]
    fn php_probe_ready_on_get_values_result() {
        let (port, _rx) = fake_server(fcgi_header(10));
        assert!(check_php_ready(port));
    }

    #[test]
    fn php_probe_ready_on_unknown_type() {
        // FCGI_UNKNOWN_TYPE still proves a FastCGI responder is alive.
        let (port, _rx) = fake_server(fcgi_header(11));
        assert!(check_php_ready(port));
    }

    #[test]
    fn php_probe_rejects_a_non_fastcgi_listener() {
        let (port, _rx) = fake_server(b"HTTP/1.1 200 OK\r\n\r\n".to_vec());
        assert!(!check_php_ready(port));
    }
}
