//! Native tests for the Rust handoff-server SDK, against a tiny `std::net` HTTP
//! stub (plan v0.5 Task 2 Step 1).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use hearth_handoff_server::{ServerError, ServerReport, Session};
use serde_json::json;

/// One request the stub received.
#[derive(Clone, Debug)]
struct Recorded {
    request_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Recorded {
    fn path(&self) -> &str {
        self.request_line.split(' ').nth(1).unwrap_or_default()
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// A one-request-at-a-time HTTP server on an ephemeral port.
struct Stub {
    url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Stub {
    fn start(respond: impl Fn(&Recorded) -> (u16, String) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stub");
        let port = listener.local_addr().expect("local address").port();
        listener
            .set_nonblocking(true)
            .expect("non-blocking listener");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let requests_for_thread = Arc::clone(&requests);
        let stop_for_thread = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stop_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if let Some(recorded) = read_request(&mut stream) {
                            let (status, body) = respond(&recorded);
                            requests_for_thread
                                .lock()
                                .expect("requests lock")
                                .push(recorded);
                            write_response(&mut stream, status, &body);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}"),
            requests,
            stop,
            handle: Some(handle),
        }
    }

    fn session(&self) -> Session {
        Session::new(
            "hs_test",
            "session-secret",
            format!("{}/handoff/hs_test", self.url),
        )
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Option<Recorded> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        let Some(header_end) = find(&buffer, b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let mut lines = head.split("\r\n");
        let request_line = lines.next()?.to_string();
        let mut headers = Vec::new();
        let mut content_length = 0usize;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((name, value));
        }
        let mut body = buffer[header_end + 4..].to_vec();
        while body.len() < content_length {
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read]);
        }
        body.truncate(content_length);
        return Some(Recorded {
            request_line,
            headers,
            body: String::from_utf8_lossy(&body).to_string(),
        });
    }
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        403 => "Forbidden",
        404 => "Not Found",
        410 => "Gone",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[test]
fn from_env_reads_vars() {
    std::env::set_var("HEARTH_SID", "hs_env");
    std::env::set_var("HEARTH_SESSION_SECRET", "env-secret");
    std::env::set_var("HEARTH_REPORT_URL", "http://127.0.0.1:9/handoff/hs_env");
    let session = Session::from_env().expect("the environment contract is set");
    assert_eq!(session.sid, "hs_env");
    assert_eq!(session.secret, "env-secret");
    assert_eq!(session.report_url, "http://127.0.0.1:9/handoff/hs_env");
    std::env::remove_var("HEARTH_SID");
    std::env::remove_var("HEARTH_SESSION_SECRET");
    std::env::remove_var("HEARTH_REPORT_URL");
}

#[test]
fn redeem_maps_403_to_forbidden() {
    let stub = Stub::start(|_| (403, "{}".to_owned()));
    assert_eq!(stub.session().redeem("ticket"), Err(ServerError::Forbidden));
    let request = &stub.requests()[0];
    assert_eq!(request.path(), "/handoff/hs_test/redeem");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&request.body).expect("redeem JSON"),
        json!({ "ticket": "ticket" })
    );
}

#[test]
fn redeem_maps_410_to_gone() {
    let stub = Stub::start(|_| (410, "{}".to_owned()));
    assert_eq!(stub.session().redeem("ticket"), Err(ServerError::Gone));
}

#[test]
fn report_sends_secret_header() {
    let stub = Stub::start(|_| (204, String::new()));
    let session = stub.session();
    session
        .report(&ServerReport::Started)
        .expect("the report lands");

    let requests = stub.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.path(), "/handoff/hs_test/report");
    assert_eq!(
        request.header("X-Hearth-Session-Secret"),
        Some("session-secret")
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&request.body).expect("report JSON"),
        json!({ "type": "started" })
    );
}

#[test]
fn heartbeat_stops_on_gone() {
    let stub = Stub::start(|_| (410, "{}".to_owned()));
    let handle = stub.session().spawn_heartbeat(Duration::from_millis(10));
    let deadline = Instant::now() + Duration::from_secs(2);
    while !handle.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        handle.is_finished(),
        "the heartbeat stops when the room says Gone"
    );
    handle.join().expect("heartbeat thread");
    assert!(
        !stub.requests().is_empty(),
        "it sent at least one heartbeat"
    );
}
