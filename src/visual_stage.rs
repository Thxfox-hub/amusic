//! Local Visual Stage: HTTP static files + WebSocket music-event broadcast.
//! Browser opens hydra-synth stage; IDE pushes hit events over WS.

use std::fs;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use tiny_http::{Header, Method, Response, Server, StatusCode};
use tungstenite::protocol::Message;
use tungstenite::{accept, WebSocket};

const DEFAULT_HTTP_PORT: u16 = 9876;
const DEFAULT_WS_PORT: u16 = 9877;
const MIN_BROADCAST_INTERVAL: Duration = Duration::from_millis(16); // ~60 Hz cap

const EMBEDDED_INDEX: &str = include_str!("../visual-stage/index.html");
const EMBEDDED_STAGE_JS: &str = include_str!("../visual-stage/stage.js");

static STAGE_RX: Mutex<Option<Receiver<String>>> = Mutex::new(None);

#[derive(Clone)]
pub struct VisualStage {
    inner: Arc<VisualStageInner>,
}

struct VisualStageInner {
    running: AtomicBool,
    http_port: u16,
    ws_port: u16,
    client_count: AtomicUsize,
    tx: Sender<String>,
    last_broadcast: Mutex<Instant>,
    sent_in_window: Mutex<(Instant, u32)>,
}

struct ClientSlot {
    ws: WebSocket<TcpStream>,
}

impl VisualStage {
    pub fn new() -> Self {
        let (tx, rx) = unbounded::<String>();
        *STAGE_RX.lock().unwrap() = Some(rx);
        Self {
            inner: Arc::new(VisualStageInner {
                running: AtomicBool::new(false),
                http_port: DEFAULT_HTTP_PORT,
                ws_port: DEFAULT_WS_PORT,
                client_count: AtomicUsize::new(0),
                tx,
                last_broadcast: Mutex::new(Instant::now() - Duration::from_secs(1)),
                sent_in_window: Mutex::new((Instant::now(), 0)),
            }),
        }
    }

    pub fn http_port(&self) -> u16 {
        self.inner.http_port
    }

    pub fn ws_port(&self) -> u16 {
        self.inner.ws_port
    }

    pub fn client_count(&self) -> usize {
        self.inner.client_count.load(Ordering::Relaxed)
    }

    pub fn is_running(&self) -> bool {
        self.inner.running.load(Ordering::Relaxed)
    }

    pub fn stage_url(&self) -> String {
        format!(
            "http://127.0.0.1:{}?ws={}",
            self.inner.http_port, self.inner.ws_port
        )
    }

    /// Start HTTP + WS listeners if not already running.
    pub fn ensure_running(&self) {
        if self.inner.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let http_port = self.inner.http_port;
        let ws_port = self.inner.ws_port;
        let asset_dir = resolve_asset_dir();

        thread::Builder::new()
            .name("visual-stage-http".into())
            .spawn(move || {
                if let Err(e) = run_http(http_port, asset_dir) {
                    eprintln!("[VisualStage] HTTP error: {e}");
                }
            })
            .ok();

        let rx = STAGE_RX.lock().unwrap().take();
        let Some(rx) = rx else {
            eprintln!("[VisualStage] WS receiver already taken");
            return;
        };

        let clients = Arc::new(Mutex::new(Vec::<ClientSlot>::new()));
        let count_accept = Arc::clone(&self.inner);
        let clients_accept = Arc::clone(&clients);
        thread::Builder::new()
            .name("visual-stage-ws-accept".into())
            .spawn(move || {
                if let Err(e) = run_ws_accept(ws_port, clients_accept, count_accept) {
                    eprintln!("[VisualStage] WS accept error: {e}");
                }
            })
            .ok();

        let count_hub = Arc::clone(&self.inner);
        thread::Builder::new()
            .name("visual-stage-ws-hub".into())
            .spawn(move || {
                run_ws_hub(rx, clients, count_hub);
            })
            .ok();
        eprintln!(
            "[VisualStage] listening http://127.0.0.1:{http_port}  ws://127.0.0.1:{ws_port}"
        );
    }

    pub fn open_in_browser(&self) {
        self.ensure_running();
        thread::sleep(Duration::from_millis(100));
        let url = self.stage_url();
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
    }

    /// Broadcast a batch of hits in ONE websocket message (no per-hit rate kill).
    /// Any Strudel pattern → same bus mapping on whatever Hydra look is active.
    pub fn broadcast_hits(&self, hits: &[serde_json::Value]) {
        if hits.is_empty() {
            return;
        }
        self.ensure_running();
        let msg = serde_json::json!({
            "type": "hits",
            "n": hits.len(),
            "events": hits,
        });
        let _ = self.inner.tx.send(msg.to_string());
    }

    /// Rate-limited single hit (legacy / scene cues).
    pub fn broadcast_hit(&self, json: &str) {
        self.ensure_running();
        {
            let mut last = self.inner.last_broadcast.lock().unwrap();
            let now = Instant::now();
            if now.duration_since(*last) < MIN_BROADCAST_INTERVAL {
                return;
            }
            *last = now;
        }
        {
            let mut win = self.inner.sent_in_window.lock().unwrap();
            let now = Instant::now();
            if now.duration_since(win.0) > Duration::from_secs(1) {
                *win = (now, 0);
            }
            if win.1 >= 120 {
                return;
            }
            win.1 += 1;
        }
        let _ = self.inner.tx.send(json.to_string());
    }

    pub fn broadcast_json(&self, value: &serde_json::Value) {
        self.broadcast_hit(&value.to_string());
    }
}

impl Default for VisualStage {
    fn default() -> Self {
        Self::new()
    }
}

fn resolve_asset_dir() -> Option<PathBuf> {
    let mut candidates = vec![
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("visual-stage"),
        PathBuf::from("/usr/share/struidel/visual-stage"),
    ];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("visual-stage"));
            candidates.push(parent.join("../share/struidel/visual-stage"));
            candidates.push(parent.join("../visual-stage"));
            candidates.push(parent.join("../../ide-native/visual-stage"));
        }
    }
    candidates.push(PathBuf::from("ide-native/visual-stage"));
    candidates.push(PathBuf::from("visual-stage"));
    candidates
        .into_iter()
        .find(|p| p.join("index.html").is_file())
}

fn content_type(path: &str) -> &'static str {
    if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

fn run_http(
    port: u16,
    asset_dir: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr = format!("127.0.0.1:{port}");
    let server = Server::http(&addr).map_err(|e| format!("bind HTTP {addr}: {e}"))?;
    eprintln!("[VisualStage] HTTP on http://{addr}");
    for request in server.incoming_requests() {
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("/");
        if request.method() != &Method::Get && request.method() != &Method::Head {
            let _ = request.respond(Response::empty(StatusCode(405)));
            continue;
        }
        let (body, ctype) = match path {
            "/" | "/index.html" => {
                let body = asset_dir
                    .as_ref()
                    .and_then(|d| fs::read(d.join("index.html")).ok())
                    .unwrap_or_else(|| EMBEDDED_INDEX.as_bytes().to_vec());
                (body, "text/html; charset=utf-8")
            }
            "/stage.js" => {
                let body = asset_dir
                    .as_ref()
                    .and_then(|d| fs::read(d.join("stage.js")).ok())
                    .unwrap_or_else(|| EMBEDDED_STAGE_JS.as_bytes().to_vec());
                (body, "application/javascript; charset=utf-8")
            }
            other => {
                if let Some(dir) = &asset_dir {
                    let rel = other.trim_start_matches('/');
                    let file = dir.join(rel);
                    if file.is_file() {
                        match fs::read(&file) {
                            Ok(b) => (b, content_type(rel)),
                            Err(_) => {
                                let _ = request
                                    .respond(Response::from_string("not found").with_status_code(404));
                                continue;
                            }
                        }
                    } else {
                        let _ =
                            request.respond(Response::from_string("not found").with_status_code(404));
                        continue;
                    }
                } else {
                    let _ =
                        request.respond(Response::from_string("not found").with_status_code(404));
                    continue;
                }
            }
        };
        let mut response = Response::from_data(body);
        if let Ok(h) = Header::from_bytes("Content-Type", ctype) {
            response.add_header(h);
        }
        if let Ok(h) = Header::from_bytes("Cache-Control", "no-store") {
            response.add_header(h);
        }
        let _ = request.respond(response);
    }
    Ok(())
}

fn run_ws_accept(
    port: u16,
    clients: Arc<Mutex<Vec<ClientSlot>>>,
    inner: Arc<VisualStageInner>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;
    let listener = TcpListener::bind(addr)?;
    eprintln!("[VisualStage] WS on ws://127.0.0.1:{port}");
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        stream.set_nodelay(true).ok();
        stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .ok();
        match accept(stream) {
            Ok(ws) => {
                let mut guard = clients.lock().unwrap();
                guard.push(ClientSlot { ws });
                inner.client_count.store(guard.len(), Ordering::Relaxed);
            }
            Err(e) => {
                eprintln!("[VisualStage] WS handshake failed: {e}");
            }
        }
    }
    Ok(())
}

fn run_ws_hub(
    rx: Receiver<String>,
    clients: Arc<Mutex<Vec<ClientSlot>>>,
    inner: Arc<VisualStageInner>,
) {
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(msg) => {
                let mut guard = clients.lock().unwrap();
                guard.retain_mut(|slot| match slot.ws.send(Message::Text(msg.clone().into())) {
                    Ok(()) => true,
                    Err(_) => false,
                });
                inner.client_count.store(guard.len(), Ordering::Relaxed);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                let mut guard = clients.lock().unwrap();
                guard.retain_mut(|slot| match slot.ws.send(Message::Ping(vec![].into())) {
                    Ok(()) => {
                        loop {
                            match slot.ws.read() {
                                Ok(Message::Close(_)) => return false,
                                Ok(_) => continue,
                                Err(tungstenite::Error::Io(ref e))
                                    if e.kind() == std::io::ErrorKind::WouldBlock
                                        || e.kind() == std::io::ErrorKind::TimedOut =>
                                {
                                    break;
                                }
                                Err(_) => return false,
                            }
                        }
                        true
                    }
                    Err(_) => false,
                });
                inner.client_count.store(guard.len(), Ordering::Relaxed);
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Build a compact hit JSON for the stage.
pub fn hit_json(
    t: f64,
    s: Option<&str>,
    g: Option<f64>,
    n: Option<f64>,
    midi: Option<f64>,
) -> serde_json::Value {
    serde_json::json!({
        "type": "hit",
        "t": t,
        "s": s,
        "g": g.unwrap_or(0.7),
        "n": n,
        "midi": midi,
    })
}

pub fn hit_json_full(
    t: f64,
    s: Option<&str>,
    bank: Option<&str>,
    g: Option<f64>,
    n: Option<f64>,
    midi: Option<f64>,
) -> serde_json::Value {
    serde_json::json!({
        "type": "hit",
        "t": t,
        "s": s,
        "bank": bank,
        "g": g.unwrap_or(0.7),
        "n": n,
        "midi": midi,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpStream;
    use std::time::Duration;

    #[test]
    fn http_serves_index() {
        let stage = VisualStage::new();
        stage.ensure_running();
        thread::sleep(Duration::from_millis(200));
        let mut stream =
            TcpStream::connect(format!("127.0.0.1:{}", stage.http_port())).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .ok();
        let req = b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
        use std::io::Write;
        stream.write_all(req).unwrap();
        let mut buf = String::new();
        stream.read_to_string(&mut buf).ok();
        assert!(
            buf.contains("Visual Stage") || buf.contains("hydra"),
            "unexpected body: {}",
            &buf[..buf.len().min(200)]
        );
    }
}
