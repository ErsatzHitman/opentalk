//! In-process mock server for the streaming tests: plain `ws://` on 127.0.0.1 (tungstenite
//! server side) that also answers ordinary HTTP requests (AssemblyAI's token mint).
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::{Message, WebSocket};

/// A request the server saw (WebSocket upgrade or plain HTTP).
#[derive(Clone, Debug)]
pub struct Req {
    pub is_ws: bool,
    /// Path and query as sent.
    pub target: String,
    pub headers: Vec<(String, String)>,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Server side of one accepted WebSocket.
pub struct Conn {
    pub ws: WebSocket<TcpStream>,
    pub req: Req,
}

impl Conn {
    /// Next client data message (Text/Binary), skipping control frames. `None` on timeout/close.
    pub fn recv(&mut self, timeout: Duration) -> Option<Message> {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let _ = self.ws.get_ref().set_read_timeout(Some(left));
            match self.ws.read() {
                Ok(m @ (Message::Text(_) | Message::Binary(_))) => return Some(m),
                Ok(Message::Close(_)) => return None,
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }

    /// Next text message, panicking with context if something else arrives.
    pub fn recv_text(&mut self) -> String {
        match self.recv(Duration::from_secs(5)) {
            Some(Message::Text(t)) => t.as_str().to_string(),
            other => panic!("expected a text message, got {other:?}"),
        }
    }

    pub fn recv_json(&mut self) -> serde_json::Value {
        serde_json::from_str(&self.recv_text()).expect("client sent valid JSON")
    }

    pub fn send(&mut self, text: &str) {
        let _ = self.ws.send(Message::text(text));
    }

    pub fn send_binary(&mut self, text: &str) {
        let _ = self.ws.send(Message::binary(text.as_bytes().to_vec()));
    }

    /// Write raw bytes to the socket (for frames split across TCP segments).
    pub fn raw(&mut self, bytes: &[u8]) {
        let s = self.ws.get_mut();
        let _ = s.write_all(bytes);
        let _ = s.flush();
    }

    /// True once the client has closed or dropped the connection.
    pub fn closed_within(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            let _ = self.ws.get_ref().set_read_timeout(Some(left));
            match self.ws.read() {
                Ok(Message::Close(_)) | Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => return true,
                Ok(_) => {}
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => return true,
            }
        }
    }
}

/// Unmasked server-to-client text frame with a short payload.
pub fn text_frame(payload: &str) -> Vec<u8> {
    assert!(payload.len() < 126);
    let mut f = vec![0x81, payload.len() as u8];
    f.extend_from_slice(payload.as_bytes());
    f
}

pub struct Mock {
    pub addr: SocketAddr,
    /// Every request seen, in arrival order.
    pub requests: Arc<Mutex<Vec<Req>>>,
    stop: Arc<AtomicBool>,
}

impl Mock {
    pub fn ws_url(&self) -> String {
        format!("ws://{}", self.addr)
    }
    pub fn requests(&self) -> Vec<Req> {
        self.requests.lock().unwrap().clone()
    }
    pub fn ws_requests(&self) -> Vec<Req> {
        self.requests().into_iter().filter(|r| r.is_ws).collect()
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
    }
}

#[derive(Clone)]
pub struct Options {
    /// Sleep before answering the WebSocket upgrade (slow handshake).
    pub accept_delay: Duration,
    /// Refuse the upgrade with this HTTP status and body.
    pub reject: Option<(u16, &'static str)>,
    /// Response to plain HTTP requests.
    pub http: (u16, String),
}

impl Default for Options {
    fn default() -> Self {
        Self { accept_delay: Duration::ZERO, reject: None, http: (200, "{}".into()) }
    }
}

/// Start a mock server; `script` runs once per accepted WebSocket connection.
pub fn serve(opts: Options, script: impl Fn(&mut Conn) + Send + Sync + 'static) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let script = Arc::new(script);
    {
        let (requests, stop) = (requests.clone(), stop.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let Ok(stream) = stream else { continue };
                let (requests, script, opts) = (requests.clone(), script.clone(), opts.clone());
                std::thread::spawn(move || handle(stream, requests, script, opts));
            }
        });
    }
    Mock { addr, requests, stop }
}

fn parse_head(head: &str, is_ws: bool) -> Req {
    let mut lines = head.split("\r\n");
    let target = lines.next().unwrap_or("").split(' ').nth(1).unwrap_or("").to_string();
    let headers = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    Req { is_ws, target, headers }
}

fn handle(mut stream: TcpStream, requests: Arc<Mutex<Vec<Req>>>, script: Arc<dyn Fn(&mut Conn) + Send + Sync>, opts: Options) {
    // Wait for the request head without consuming it, so tungstenite can still parse it.
    let mut buf = vec![0u8; 8192];
    let deadline = Instant::now() + Duration::from_secs(5);
    let n = loop {
        match stream.peek(&mut buf) {
            Ok(0) => return,
            Ok(n) if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") => break n,
            Ok(_) => std::thread::sleep(Duration::from_millis(2)),
            Err(_) => return,
        }
        if Instant::now() > deadline {
            return;
        }
    };
    let head = String::from_utf8_lossy(&buf[..n]).to_string();
    let head = head.split("\r\n\r\n").next().unwrap_or("").to_string();
    let is_ws = head.to_ascii_lowercase().contains("upgrade: websocket");

    if !is_ws {
        let mut sink = vec![0u8; head.len() + 4];
        let _ = stream.read_exact(&mut sink);
        requests.lock().unwrap().push(parse_head(&head, false));
        let (status, body) = &opts.http;
        let resp = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let _ = stream.write_all(resp.as_bytes());
        return;
    }

    std::thread::sleep(opts.accept_delay);
    let req = parse_head(&head, true);
    requests.lock().unwrap().push(req.clone());
    let reject = opts.reject;
    let callback = move |_: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        match reject {
            Some((status, body)) => Err(tungstenite::http::Response::builder().status(status).body(Some(body.to_string())).unwrap()),
            None => Ok(resp),
        }
    };
    if let Ok(ws) = tungstenite::accept_hdr(stream, callback) {
        let mut conn = Conn { ws, req };
        script(&mut conn);
    }
}
