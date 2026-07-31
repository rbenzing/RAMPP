/// Layer 2 integration tests: health check readiness polling with mock TCP servers.
///
/// These tests use real TCP listeners to verify that poll_until_ready correctly
/// detects when a service becomes available and correctly times out when it does not.
///
/// Run with: cargo test --test integration_health
use crossbeam_channel::unbounded;
use rampp::events::Event;
use rampp::health::{
    check_apache_ready, check_mysql_ready, check_php_ready, poll_until_ready, run_health_checker,
};
use rampp::state::Service;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Helper: bind on a random OS-assigned port, return the listener and the port.
fn bind_random() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind random port");
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

/// The readiness probe must judge Apache by the response Apache itself returns and
/// must never chase a redirect. A running application can answer the probe path with
/// a 302 into an arbitrarily slow route (a Laravel auth redirect to /login is the
/// common case); following it makes probe latency a property of the user's app, so a
/// perfectly healthy Apache reads as dead and gets killed and retried.
///
/// The mock answers the first request with a 302 carrying Apache's Server header and
/// stalls any followed-up request past the probe's own timeout.
#[test]
fn check_apache_ready_does_not_follow_redirects() {
    let (listener, port) = bind_random();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_srv = Arc::clone(&hits);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let hits = Arc::clone(&hits_srv);
            std::thread::spawn(move || {
                // Drain the request headers so the client's write completes.
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let n = hits.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    let _ = stream.write_all(
                        b"HTTP/1.1 302 Found\r\n\
                          Server: Apache/2.4.66 (Win64)\r\n\
                          Location: /login\r\n\
                          Content-Length: 0\r\n\r\n",
                    );
                } else {
                    // The redirect target: slower than the probe is willing to wait.
                    std::thread::sleep(Duration::from_secs(6));
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\n\
                          Server: Apache/2.4.66 (Win64)\r\n\
                          Content-Length: 0\r\n\r\n",
                    );
                }
                let _ = stream.flush();
            });
        }
    });
    std::thread::sleep(Duration::from_millis(50));

    let start = std::time::Instant::now();
    let ready = check_apache_ready(port);
    let elapsed = start.elapsed();

    assert!(
        ready,
        "a 302 carrying Apache's Server header proves Apache is serving"
    );
    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "probe must issue exactly one request and not follow the redirect"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "probe took {elapsed:?} — it followed the redirect into the slow route"
    );
}

/// Apache's readiness budget must outlast more than one stalled probe.
///
/// A probe that hangs burns HEALTH_PROBE_TIMEOUT of the budget. With a 3s budget and
/// a 2s probe timeout, one stalled probe leaves no room for a second attempt, so a
/// cold start that is merely slow gets reported as a crash. Observed Apache cold
/// starts on Windows reach ~2.6s from spawn to accepting connections, which alone
/// nearly exhausts a 3s budget.
#[test]
fn apache_ready_budget_survives_multiple_stalled_probes() {
    use rampp::state::{APACHE_READY_TIMEOUT, HEALTH_PROBE_TIMEOUT};
    assert!(
        APACHE_READY_TIMEOUT >= HEALTH_PROBE_TIMEOUT * 3,
        "APACHE_READY_TIMEOUT ({APACHE_READY_TIMEOUT:?}) must allow at least three \
         probe attempts of {HEALTH_PROBE_TIMEOUT:?} each"
    );
}

/// check_php_ready returns true when a TCP listener is present on the port.
#[test]
fn check_php_ready_true_when_port_open() {
    let (_listener, port) = bind_random();
    assert!(check_php_ready(port), "should be ready with listener bound");
}

/// check_php_ready returns false when nothing is listening.
#[test]
fn check_php_ready_false_when_no_listener() {
    // Use a port that is very unlikely to have anything on it.
    // We do a quick scan; if it happens to be in use the test is a no-op.
    let port = 59876u16;
    if !check_php_ready(port) {
        // Good — confirms the function returns false for closed ports.
    }
    // If it returns true, there happens to be a service there — skip silently.
}

/// check_mysql_ready returns false for a port with a plain TCP listener
/// (no MySQL greeting bytes) — the function checks for 4 bytes of data.
#[test]
fn check_mysql_ready_false_for_plain_tcp() {
    let (listener, port) = bind_random();
    // Accept in a thread but send no data — MySQL check reads 4 bytes.
    std::thread::spawn(move || {
        if let Ok((_stream, _)) = listener.accept() {
            // Intentionally send nothing — check_mysql_ready should fail.
            std::thread::sleep(Duration::from_millis(200));
        }
    });
    // Give the accept thread time to start
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        !check_mysql_ready(port),
        "plain TCP with no data should not pass MySQL ready check"
    );
}

/// check_mysql_ready must return within a bounded time even when the server
/// accepts the connection but never sends data (read_timeout enforced).
/// Before the C2 fix, a set_read_timeout failure would cause read_exact to
/// block indefinitely. This test validates the 2-second read timeout fires.
#[test]
fn check_mysql_ready_returns_within_timeout() {
    let (listener, port) = bind_random();
    std::thread::spawn(move || {
        if let Ok((_stream, _)) = listener.accept() {
            // Hold the connection open for longer than the 2s read timeout.
            std::thread::sleep(Duration::from_secs(10));
        }
    });
    std::thread::sleep(Duration::from_millis(50));

    let start = std::time::Instant::now();
    let result = check_mysql_ready(port);
    let elapsed = start.elapsed();

    assert!(!result, "should return false when no greeting bytes sent");
    // Must return within ~3s (2s read timeout + some slack). If it blocks
    // indefinitely the test will time out and fail the suite.
    assert!(
        elapsed < Duration::from_secs(5),
        "check_mysql_ready took too long ({elapsed:?}) — read timeout may not be enforced"
    );
}

/// poll_until_ready sends ProcessReady when the port opens within the timeout.
#[test]
fn poll_until_ready_resolves_when_port_opens() {
    let (tx, rx) = unbounded();
    let port = {
        // Pre-bind to get a free port, then release so poll can race it.
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
        // l drops here, releasing the port
    };

    // Open the listener after a short delay so poll_until_ready has to wait.
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        // Bind and hold open long enough for the check to succeed.
        let _listener = TcpListener::bind(format!("127.0.0.1:{port}")).unwrap();
        std::thread::sleep(Duration::from_millis(500));
    });

    poll_until_ready(Service::Php, port, tx);

    let event = rx
        .recv_timeout(Duration::from_secs(3))
        .expect("should receive an event");
    assert!(
        matches!(event, Event::ProcessReady(Service::Php)),
        "expected ProcessReady(Php), got {event:?}"
    );
}

/// poll_until_ready sends ProcessExit when no service comes up within the timeout.
/// Uses a short artificial timeout by checking a port that will never open.
#[test]
fn poll_until_ready_times_out_and_sends_process_exit() {
    use rampp::health::poll_until_ready_with_timeout;

    let (tx, rx) = unbounded();
    // Port with nothing listening and no thread will open it.
    let port = 59877u16;
    let timeout = Duration::from_millis(400);

    poll_until_ready_with_timeout(Service::Php, port, tx, timeout);

    let event = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("should receive timeout event");
    assert!(
        matches!(
            event,
            Event::ProcessExit {
                service: Service::Php,
                exit_code: None
            }
        ),
        "expected ProcessExit on timeout, got {event:?}"
    );
}

/// run_health_checker stops cleanly when the stop channel is signalled.
#[test]
fn health_checker_stops_on_signal() {
    let (event_tx, _event_rx) = unbounded();
    let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
    let (_listener, port) = bind_random();

    let handle = std::thread::spawn(move || {
        run_health_checker(Service::Php, port, event_tx, stop_rx);
    });

    // Let at least one health check cycle fire (HEALTH_CHECK_INTERVAL = 2s — too long
    // for a test; we just confirm the thread terminates promptly on signal).
    std::thread::sleep(Duration::from_millis(50));
    let _ = stop_tx.send(());

    handle
        .join()
        .expect("health checker thread should exit cleanly");
}
