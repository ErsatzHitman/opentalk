//! Generic WebSocket streaming session: one I/O thread per session, driven by a
//! provider-specific [`Protocol`].
//!
//! - `push()` only enqueues PCM on a channel (never blocks the capture thread). Audio pushed
//!   before the socket is usable is capped at 3 s and flushed as 50 ms frames once it is.
//! - The thread connects (TCP, TLS, WebSocket upgrade), waits for the provider to be ready,
//!   then alternates between sending frames and reading server events with a short socket
//!   timeout, so an idle session costs ~50 wakeups/s and no busy loop.
//! - `finish()` flushes the tail, sends the provider's finalize messages and waits until the
//!   text has settled: the provider says it is done, or 250 ms pass without news and no
//!   partial is outstanding, or the hard ceiling (2 s, 3 s for Gemini) expires.
//! - `cancel()` shuts the socket down from the caller's thread, so it takes effect at once.
use super::util;
use crate::stt::{StreamingSession, SttError, SttErrorCode, Transcript};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::{HeaderName, HeaderValue};
use tungstenite::{Message, WebSocket};

/// Input sample rate of every session.
pub const SAMPLE_RATE: usize = 16_000;
/// 50 ms of 16 kHz audio.
pub const FRAME_SAMPLES: usize = 800;
/// Audio accepted before the socket is usable (3 s).
pub const PRE_OPEN_CAP_SAMPLES: usize = 3 * SAMPLE_RATE;
/// Silence after the last relevant server message that counts as "settled".
pub const QUIET: Duration = Duration::from_millis(250);
/// Default hard ceiling for the post-finalize wait.
pub const CEILING: Duration = Duration::from_millis(2000);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(6);
const READY_TIMEOUT: Duration = Duration::from_secs(6);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_WRITE_TIMEOUT: Duration = Duration::from_millis(500);
/// While recording the thread sleeps on its command channel (woken by audio), so an idle or
/// streaming session costs a handful of wakeups per second instead of a polling loop.
const IDLE_WAIT: Duration = Duration::from_millis(250);
/// While recording, server messages are read whenever the socket is readable just before a
/// write (see [`Loop::read_if_readable`]), and otherwise at least this often (partials are not
/// surfaced, so this only has to catch errors in a stalled session).
const DRAIN_EVERY: Duration = Duration::from_millis(250);
/// Socket read timeout for those drains.
const DRAIN_POLL: Duration = Duration::from_millis(2);
/// Socket read timeout while waiting for the provider to become ready.
const POLL_HANDSHAKE: Duration = Duration::from_millis(10);
/// Socket read timeout while waiting for the final text.
const POLL_SETTLING: Duration = Duration::from_millis(8);
/// `push()` hands audio to the I/O thread in chunks of at least this many samples (100 ms).
const STAGE_SAMPLES: usize = 2 * FRAME_SAMPLES;
/// How long `finish()` waits for the I/O thread before giving up (it is bounded anyway).
const FINISH_GUARD: Duration = Duration::from_secs(30);

/// Where and how to connect.
pub struct ConnectSpec {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// Provider-specific half of a session. All methods run on the session's I/O thread.
pub trait Protocol: Send + 'static {
    /// Display name for errors ("Deepgram").
    fn name(&self) -> &'static str;
    /// Resolve the URL and headers. May block (AssemblyAI mints a token over HTTP).
    fn connect_spec(&mut self) -> Result<ConnectSpec, SttError>;
    /// Credentials that must never reach an error text, beyond those the session finds in the
    /// [`ConnectSpec`] (an `Authorization` value, a `key=` / `token=` query value). AssemblyAI
    /// lists its account API key, which only the token mint request carries.
    fn credentials(&self) -> Vec<String> {
        Vec::new()
    }
    /// Messages to send as soon as the WebSocket is up.
    fn on_open(&mut self) -> Vec<Message> {
        Vec::new()
    }
    /// Audio may be sent (after the provider's own handshake events).
    fn ready(&self) -> bool;
    /// Encode one frame of 16 kHz mono PCM.
    fn encode(&mut self, frame: &[i16]) -> Message;
    /// Pad the final short frame with silence to a full 50 ms frame.
    fn pad_tail(&self) -> bool {
        false
    }
    /// Handle one server message (JSON text). Push replies onto `out`; return true when the
    /// message changed what the session is waiting for (new text, a turn boundary, ...).
    fn on_message(&mut self, text: &str, out: &mut Vec<Message>) -> bool;
    /// The server closed the socket with this code.
    fn on_close(&mut self, _code: u16, _reason: &str) {}
    /// Messages that make the server flush its pending transcript.
    fn finalize(&mut self) -> Vec<Message>;
    /// The server confirmed the final text; no need to wait for the quiet period.
    fn done(&self) -> bool {
        false
    }
    /// A partial result is outstanding, so its final is still in flight.
    fn partial_pending(&self) -> bool;
    /// Messages sent after the text has settled, before the socket is closed.
    fn closing(&mut self) -> Vec<Message> {
        Vec::new()
    }
    /// Send `message` when nothing has been sent for `interval`.
    fn keepalive(&self) -> Option<(Duration, Message)> {
        None
    }
    fn ceiling(&self) -> Duration {
        CEILING
    }
    /// Final segments joined by spaces, else the last partial.
    fn text(&self) -> String;
    /// A provider-reported failure, used when no text was produced.
    fn error(&self) -> Option<SttError>;
}

enum Cmd {
    Audio(Vec<i16>),
    Finish,
    Cancel,
}

/// State shared between the caller's thread and the I/O thread.
pub struct Shared {
    cancelled: AtomicBool,
    ready: AtomicBool,
    pre_ready: AtomicUsize,
    sock: Mutex<Option<TcpStream>>,
}

impl Shared {
    fn new() -> Self {
        Self { cancelled: AtomicBool::new(false), ready: AtomicBool::new(false), pre_ready: AtomicUsize::new(0), sock: Mutex::new(None) }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    fn sock(&self) -> std::sync::MutexGuard<'_, Option<TcpStream>> {
        self.sock.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn abort(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(s) = self.sock().take() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

/// Handle returned to the daemon.
pub struct WsSession {
    tx: mpsc::Sender<Cmd>,
    result: mpsc::Receiver<Result<Transcript, SttError>>,
    shared: Arc<Shared>,
    /// Audio not yet handed to the I/O thread (see `STAGE_SAMPLES`).
    stage: Vec<i16>,
    consumed: bool,
}

/// Start the I/O thread for `proto`.
pub fn spawn(proto: Box<dyn Protocol>) -> Result<Box<dyn StreamingSession>, SttError> {
    let (tx, rx) = mpsc::channel();
    let (rtx, result) = mpsc::sync_channel(1);
    let shared = Arc::new(Shared::new());
    let thread_shared = shared.clone();
    std::thread::Builder::new()
        .name("stt-stream".into())
        .stack_size(1024 * 1024)
        .spawn(move || {
            let mut proto = proto;
            let out = run(&mut *proto, &rx, &thread_shared);
            thread_shared.sock().take();
            let _ = rtx.send(out);
        })
        .map_err(|e| util::err(SttErrorCode::Other, "Could not start the streaming thread.", e.to_string()))?;
    Ok(Box::new(WsSession { tx, result, shared, stage: Vec::new(), consumed: false }))
}

impl StreamingSession for WsSession {
    fn push(&mut self, pcm: &[i16]) {
        if pcm.is_empty() || self.shared.is_cancelled() {
            return;
        }
        let mut take = pcm.len();
        if !self.shared.ready.load(Ordering::Acquire) {
            let used = self.shared.pre_ready.fetch_add(take, Ordering::Relaxed);
            if used >= PRE_OPEN_CAP_SAMPLES {
                return;
            }
            take = take.min(PRE_OPEN_CAP_SAMPLES - used);
        }
        self.stage.extend_from_slice(&pcm[..take]);
        if self.stage.len() >= STAGE_SAMPLES {
            self.flush_stage();
        }
    }

    fn finish(mut self: Box<Self>) -> Result<Transcript, SttError> {
        self.consumed = true;
        self.flush_stage();
        let _ = self.tx.send(Cmd::Finish);
        match self.result.recv_timeout(FINISH_GUARD) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.shared.abort();
                Err(util::err(SttErrorCode::Timeout, "Streaming transcription timed out.", "I/O thread unresponsive"))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(util::err(SttErrorCode::Other, "Streaming transcription stopped unexpectedly.", "I/O thread exited"))
            }
        }
    }

    fn cancel(mut self: Box<Self>) {
        self.consumed = true;
        self.shared.abort();
        let _ = self.tx.send(Cmd::Cancel);
    }
}

impl WsSession {
    fn flush_stage(&mut self) {
        if !self.stage.is_empty() {
            let _ = self.tx.send(Cmd::Audio(std::mem::take(&mut self.stage)));
        }
    }
}

impl Drop for WsSession {
    fn drop(&mut self) {
        if !self.consumed {
            self.shared.abort();
            let _ = self.tx.send(Cmd::Cancel);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// transport

enum Stream {
    Plain(TcpStream),
    Tls(Box<native_tls::TlsStream<TcpStream>>),
}

impl Stream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Plain(s) => s,
            Stream::Tls(s) => s.get_ref(),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

/// Tests may point sessions at a local TLS server with a self-signed certificate.
#[cfg(test)]
pub static TEST_INSECURE_TLS: AtomicBool = AtomicBool::new(false);

fn tls_connector() -> Result<native_tls::TlsConnector, native_tls::Error> {
    #[cfg(test)]
    if TEST_INSECURE_TLS.load(Ordering::Relaxed) {
        return native_tls::TlsConnector::builder().danger_accept_invalid_certs(true).danger_accept_invalid_hostnames(true).build();
    }
    native_tls::TlsConnector::new()
}

fn open(name: &str, spec: &ConnectSpec, shared: &Shared, secrets: &[String]) -> Result<WebSocket<Stream>, SttError> {
    let bad = |what: &str| util::err(SttErrorCode::Other, format!("{name} streaming URL is invalid."), util::scrub(what, secrets));
    let mut request = spec.url.as_str().into_client_request().map_err(|e| bad(&e.to_string()))?;
    for (k, v) in &spec.headers {
        let k = HeaderName::from_bytes(k.as_bytes()).map_err(|_| bad("bad header name"))?;
        let v = HeaderValue::from_str(v).map_err(|_| bad("bad header value"))?;
        request.headers_mut().insert(k, v);
    }
    let uri = request.uri().clone();
    let tls = match uri.scheme_str() {
        Some("wss") => true,
        Some("ws") => false,
        _ => return Err(bad("scheme must be ws or wss")),
    };
    let host = uri.host().ok_or_else(|| bad("missing host"))?.trim_matches(['[', ']']).to_string();
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });

    let unreachable = |e: &dyn std::fmt::Display| util::err(SttErrorCode::Unreachable, format!("Could not reach {name}."), util::scrub(&e.to_string(), secrets));
    let addrs: Vec<_> = (host.as_str(), port).to_socket_addrs().map_err(|e| unreachable(&e))?.collect();
    let mut tcp = None;
    let mut last_err = None;
    // IPv4 first: a v6 route that black-holes would otherwise eat the whole connect budget.
    for a in addrs.iter().filter(|a| a.is_ipv4()).chain(addrs.iter().filter(|a| a.is_ipv6())) {
        if shared.is_cancelled() {
            return Err(util::cancelled());
        }
        match TcpStream::connect_timeout(a, CONNECT_TIMEOUT) {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let tcp = match tcp {
        Some(t) => t,
        None => {
            let e = last_err.map(|e| unreachable(&e)).unwrap_or_else(|| unreachable(&"no address"));
            return Err(if shared.is_cancelled() { util::cancelled() } else { e });
        }
    };
    let _ = tcp.set_nodelay(true);
    let _ = tcp.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
    let _ = tcp.set_write_timeout(Some(WRITE_TIMEOUT));
    *shared.sock() = tcp.try_clone().ok();
    if shared.is_cancelled() {
        shared.abort();
        return Err(util::cancelled());
    }

    let stream = if tls {
        let connector = tls_connector().map_err(|e| unreachable(&e))?;
        match connector.connect(&host, tcp) {
            Ok(s) => Stream::Tls(Box::new(s)),
            Err(e) => return Err(if shared.is_cancelled() { util::cancelled() } else { unreachable(&e) }),
        }
    } else {
        Stream::Plain(tcp)
    };

    match tungstenite::client(request, stream) {
        Ok((ws, _resp)) => Ok(ws),
        Err(tungstenite::HandshakeError::Failure(e)) => {
            Err(if shared.is_cancelled() { util::cancelled() } else { util::ws_error(name, &e, secrets) })
        }
        Err(tungstenite::HandshakeError::Interrupted(_)) => {
            Err(util::err(SttErrorCode::Timeout, format!("{name} did not answer the connection request."), "handshake interrupted"))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// I/O thread

fn run(proto: &mut dyn Protocol, rx: &mpsc::Receiver<Cmd>, shared: &Shared) -> Result<Transcript, SttError> {
    let t0 = Instant::now();
    let spec = proto.connect_spec()?;
    if shared.is_cancelled() {
        return Err(util::cancelled());
    }
    let mut secrets = spec_secrets(&spec);
    for c in proto.credentials() {
        push_secret(&mut secrets, c);
    }
    let mut ws = open(proto.name(), &spec, shared, &secrets)?;
    crate::log_debug!("{} stream: socket up after {:.0} ms", proto.name(), t0.elapsed().as_secs_f64() * 1000.0);

    let mut sess = Loop { ws: &mut ws, proto, shared, rx, secrets, pending: Vec::new(), finish: false, lost: false, conn_err: None };
    let r = sess.drive();
    sess.shutdown();
    r
}

fn push_secret(into: &mut Vec<String>, s: String) {
    if !s.trim().is_empty() && !into.contains(&s) {
        into.push(s);
    }
}

/// The credential-bearing parts of the spec, for scrubbing error details: the raw and decoded
/// forms of every `key=` / `token=` query value, and the credential of every auth-like header
/// (the whole value and its last word, so `Bearer abc` yields `abc`). Percent-encoded variants
/// of each are added by the redactor itself.
fn spec_secrets(spec: &ConnectSpec) -> Vec<String> {
    let mut out = Vec::new();
    for (k, v) in &spec.headers {
        let k = k.to_ascii_lowercase();
        if k.contains("auth") || k.contains("key") || k.contains("token") {
            push_secret(&mut out, v.rsplit(' ').next().unwrap_or(v).to_string());
            push_secret(&mut out, v.clone());
        }
    }
    let query = spec.url.split_once('?').map_or("", |(_, q)| q);
    for kv in query.split('&') {
        if let Some(raw) = kv.strip_prefix("key=").or_else(|| kv.strip_prefix("token=")) {
            // The URL carries it percent-encoded; the server echoes the decoded credential.
            push_secret(&mut out, util::percent_decode(raw));
            push_secret(&mut out, raw.to_string());
        }
    }
    out
}

struct Loop<'a> {
    ws: &'a mut WebSocket<Stream>,
    proto: &'a mut dyn Protocol,
    shared: &'a Shared,
    rx: &'a mpsc::Receiver<Cmd>,
    /// Credentials to scrub from error texts (see [`spec_secrets`], [`Protocol::credentials`]).
    secrets: Vec<String>,
    pending: Vec<i16>,
    finish: bool,
    lost: bool,
    conn_err: Option<SttError>,
}

impl Loop<'_> {
    fn set_poll(&self, d: Duration) {
        let _ = self.ws.get_ref().tcp().set_read_timeout(Some(d));
    }

    fn send_all(&mut self, msgs: Vec<Message>) {
        if msgs.is_empty() || self.lost {
            return;
        }
        for m in msgs {
            if let Err(e) = self.ws.write(m) {
                self.fail(e);
                return;
            }
        }
        if let Err(e) = self.ws.flush() {
            self.fail(e);
        }
    }

    fn fail(&mut self, e: tungstenite::Error) {
        if !matches!(e, tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) {
            self.conn_err.get_or_insert_with(|| util::ws_error(self.proto.name(), &e, &self.secrets));
        }
        self.lost = true;
    }

    fn apply(&mut self, cmd: Cmd) -> Result<(), SttError> {
        match cmd {
            Cmd::Audio(v) => self.pending.extend_from_slice(&v),
            Cmd::Finish => self.finish = true,
            Cmd::Cancel => return Err(util::cancelled()),
        }
        Ok(())
    }

    /// Move queued commands into local state. `Err` = cancelled.
    fn drain(&mut self) -> Result<(), SttError> {
        loop {
            match self.rx.try_recv() {
                Ok(cmd) => self.apply(cmd)?,
                Err(mpsc::TryRecvError::Disconnected) => return Err(util::cancelled()),
                Err(mpsc::TryRecvError::Empty) => break,
            }
        }
        if self.shared.is_cancelled() { Err(util::cancelled()) } else { Ok(()) }
    }

    /// Read at most one server message (bounded by the socket timeout). `Some(activity)` when
    /// a message arrived, where `activity` says whether it mattered to the settle logic.
    fn read_one(&mut self) -> Option<bool> {
        match self.ws.read() {
            Ok(Message::Text(t)) => Some(self.handle_text(t.as_str())),
            Ok(Message::Binary(b)) => Some(match std::str::from_utf8(&b) {
                Ok(t) => self.handle_text(t),
                Err(_) => false,
            }),
            Ok(Message::Close(frame)) => {
                if let Some(f) = frame {
                    self.proto.on_close(u16::from(f.code), f.reason.as_str());
                }
                self.lost = true;
                None
            }
            Ok(_) => Some(false),
            Err(tungstenite::Error::Io(e)) if util::is_timeout(&e) => None,
            Err(e) => {
                self.fail(e);
                None
            }
        }
    }

    /// True when the socket has bytes (or an EOF/error) waiting, without consuming anything.
    /// A non-blocking `peek`: no read timeout to wait out, no extra thread.
    fn socket_readable(&self) -> bool {
        let tcp = self.ws.get_ref().tcp();
        if tcp.set_nonblocking(true).is_err() {
            return true; // cannot tell: read normally
        }
        let mut probe = [0u8; 1];
        let readable = match tcp.peek(&mut probe) {
            Ok(_) => true,
            Err(e) => !util::is_timeout(&e),
        };
        let _ = tcp.set_nonblocking(false);
        readable
    }

    /// Read what the server already sent, if anything, before the next write. A peer that has
    /// closed answers our next write with an RST, and Windows then throws away whatever it had
    /// sent that we had not read yet, so the text received so far must be read first. Returns
    /// whether anything was handled.
    fn read_if_readable(&mut self, poll: &mut Duration) -> bool {
        if self.lost || !self.socket_readable() {
            return false;
        }
        if *poll != DRAIN_POLL {
            self.set_poll(DRAIN_POLL);
            *poll = DRAIN_POLL;
        }
        // Bounded so a chatty server cannot starve the audio path.
        for _ in 0..64 {
            if self.lost || self.read_one().is_none() {
                break;
            }
        }
        true
    }

    fn handle_text(&mut self, text: &str) -> bool {
        let mut out = Vec::new();
        let activity = self.proto.on_message(text, &mut out);
        self.send_all(out);
        activity
    }

    fn send_frames(&mut self, last_send: &mut Instant, sent: &mut usize) {
        let mut offset = 0;
        let mut msgs = Vec::new();
        while self.pending.len() - offset >= FRAME_SAMPLES {
            msgs.push(self.proto.encode(&self.pending[offset..offset + FRAME_SAMPLES]));
            offset += FRAME_SAMPLES;
        }
        if offset > 0 {
            self.pending.drain(..offset);
            *sent += offset;
            *last_send = Instant::now();
            self.send_all(msgs);
        }
    }

    fn drive(&mut self) -> Result<Transcript, SttError> {
        let open_msgs = self.proto.on_open();
        self.send_all(open_msgs);

        let ready_deadline = Instant::now() + READY_TIMEOUT;
        let mut was_ready = false;
        let mut sent = 0usize; // samples framed and sent
        let mut last_send = Instant::now();
        let mut last_read = Instant::now();
        let mut finalizing: Option<Instant> = None;
        let mut last_activity = Instant::now();
        let mut poll = Duration::ZERO; // read timeout currently applied to the socket

        loop {
            // Recording: sleep until audio (or a command) arrives; the socket is only looked at
            // every DRAIN_EVERY. Starting up and settling: the socket is the thing to watch.
            if was_ready && finalizing.is_none() {
                match self.rx.recv_timeout(IDLE_WAIT) {
                    Ok(cmd) => self.apply(cmd)?,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Err(util::cancelled()),
                }
            }
            self.drain()?;

            if !was_ready && self.proto.ready() {
                was_ready = true;
                self.shared.ready.store(true, Ordering::Release);
                crate::log_debug!("{} stream: ready, {} samples queued", self.proto.name(), self.pending.len());
            }
            if was_ready && finalizing.is_none() && self.read_if_readable(&mut poll) {
                last_read = Instant::now();
            }
            if self.lost {
                break;
            }
            if !was_ready && Instant::now() >= ready_deadline {
                return Err(self.conn_err.take().or_else(|| self.proto.error()).unwrap_or_else(|| {
                    util::err(SttErrorCode::Timeout, format!("{} did not become ready in time.", self.proto.name()), "ready timeout")
                }));
            }

            if was_ready && finalizing.is_none() {
                self.send_frames(&mut last_send, &mut sent);
                if let Some((interval, msg)) = self.proto.keepalive()
                    && last_send.elapsed() >= interval
                {
                    last_send = Instant::now();
                    self.send_all(vec![msg]);
                }
                if self.finish {
                    if sent == 0 && self.pending.is_empty() {
                        // Nothing was ever recorded: nothing to wait for.
                        break;
                    }
                    if !self.pending.is_empty() {
                        if self.proto.pad_tail() {
                            self.pending.resize(FRAME_SAMPLES, 0);
                        }
                        let tail = std::mem::take(&mut self.pending);
                        let m = self.proto.encode(&tail);
                        self.send_all(vec![m]);
                    }
                    let fin = self.proto.finalize();
                    self.send_all(fin);
                    let now = Instant::now();
                    finalizing = Some(now);
                    last_activity = now;
                    crate::log_debug!("{} stream: finalize sent", self.proto.name());
                }
            }

            if let Some(t_fin) = finalizing {
                let now = Instant::now();
                if self.proto.done() {
                    crate::log_debug!("{} stream: done after {:.0} ms", self.proto.name(), (now - t_fin).as_secs_f64() * 1000.0);
                    break;
                }
                if now - t_fin >= self.proto.ceiling() {
                    crate::log_debug!("{} stream: ceiling reached", self.proto.name());
                    break;
                }
                if now - last_activity >= QUIET && !self.proto.partial_pending() {
                    crate::log_debug!("{} stream: quiet after {:.0} ms", self.proto.name(), (now - t_fin).as_secs_f64() * 1000.0);
                    break;
                }
            }

            // Server messages.
            let watching = finalizing.is_some() || !was_ready;
            if watching || last_read.elapsed() >= DRAIN_EVERY {
                let want = if finalizing.is_some() {
                    POLL_SETTLING
                } else if !was_ready {
                    POLL_HANDSHAKE
                } else {
                    DRAIN_POLL
                };
                if poll != want {
                    self.set_poll(want);
                    poll = want;
                }
                if watching {
                    if self.read_one() == Some(true) {
                        last_activity = Instant::now();
                    }
                } else {
                    // Bounded so a chatty server cannot starve the audio path.
                    for _ in 0..64 {
                        if self.lost || self.read_one().is_none() {
                            break;
                        }
                    }
                    last_read = Instant::now();
                }
            }
        }

        let text = self.proto.text().trim().to_string();
        if text.is_empty() {
            if let Some(e) = self.proto.error().or_else(|| self.conn_err.take()) {
                return Err(e);
            }
            if self.lost {
                return Err(util::err(SttErrorCode::Unreachable, format!("{} closed the connection.", self.proto.name()), "closed before any text"));
            }
        }
        Ok(Transcript { text, detected_language: None })
    }

    /// Send the provider's goodbye, close the WebSocket and drop the connection.
    fn shutdown(&mut self) {
        if !self.lost && !self.shared.is_cancelled() {
            let _ = self.ws.get_ref().tcp().set_write_timeout(Some(CLOSE_WRITE_TIMEOUT));
            let bye = self.proto.closing();
            self.send_all(bye);
            let _ = self.ws.close(None);
            let _ = self.ws.flush();
        }
        let _ = self.ws.get_ref().tcp().shutdown(Shutdown::Both);
    }
}

#[cfg(test)]
mod secret_tests {
    use super::*;

    #[test]
    fn spec_secrets_cover_query_and_header_credentials() {
        let q = ConnectSpec { url: "wss://h/ws?sample_rate=16000&token=tok%2B%2F%3D".into(), headers: vec![] };
        assert_eq!(spec_secrets(&q), vec!["tok+/=".to_string(), "tok%2B%2F%3D".to_string()]);
        let h = ConnectSpec { url: "wss://h/ws".into(), headers: vec![("Authorization".into(), "Token abc+def".into()), ("User-Agent".into(), "x".into())] };
        assert_eq!(spec_secrets(&h), vec!["abc+def".to_string(), "Token abc+def".to_string()]);
        let g = ConnectSpec { url: "wss://h/ws?key=k%20k".into(), headers: vec![("x-goog-api-key".into(), "gk".into())] };
        assert!(spec_secrets(&g).contains(&"k k".to_string()) && spec_secrets(&g).contains(&"gk".to_string()));
    }
}
