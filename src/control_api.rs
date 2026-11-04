//! API de contrôle locale : petit serveur HTTP JSON sur 127.0.0.1:9878.
//! Le serveur MCP `struidel` (et `curl`) s'y branchent pour lire l'état,
//! piloter le transport, évaluer du code et régler les paramètres live sans
//! passer par l'interface graphique.
//!
//! Architecture : le thread HTTP ne touche jamais l'état de l'app — il met les
//! requêtes en file ; le thread UI les dépile à chaque frame dans
//! `StruidelApp::poll_control_api` et y dépose les réponses. Le statut courant
//! est republié par l'app (~4 Hz) dans `status`.

use std::collections::{HashMap, VecDeque};
use std::io::{Cursor, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tiny_http::{Method, Response, Server, StatusCode};

pub const CONTROL_PORT: u16 = 9878;
const RESPONSE_WAIT: Duration = Duration::from_secs(8);

/// Une requête d'un client HTTP, exécutée par le thread UI.
pub struct ControlRequest {
    pub id: u64,
    /// Corps JSON de la requête (`{"action": "...", ...}`).
    pub body: Value,
}

struct Shared {
    queue: VecDeque<ControlRequest>,
    responses: HashMap<u64, Value>,
    status: Value,
    started_at: Instant,
    /// Contexte egui pour réveiller le thread UI quand une requête arrive :
    /// sans repaint, `update()` n'est jamais appelée et la file resterait pleine.
    wakeup: Option<egui::Context>,
}

/// Handle clonable détenu par l'app : dépile les requêtes, publie le statut.
#[derive(Clone)]
pub struct ControlApi {
    shared: Arc<Mutex<Shared>>,
    next_id: Arc<AtomicU64>,
}

impl ControlApi {
    /// Démarre le serveur. Un échec de bind (port pris) désactive l'API sans
    /// empêcher l'app de tourner.
    pub fn start() -> Self {
        let api = Self {
            shared: Arc::new(Mutex::new(Shared {
                queue: VecDeque::new(),
                responses: HashMap::new(),
                status: json!({"ok": false, "error": "status not published yet"}),
                started_at: Instant::now(),
                wakeup: None,
            })),
            next_id: Arc::new(AtomicU64::new(1)),
        };
        match Server::http(("127.0.0.1", CONTROL_PORT)) {
            Ok(server) => {
                let shared = Arc::clone(&api.shared);
                let next_id = Arc::clone(&api.next_id);
                thread::Builder::new()
                    .name("control-api".to_string())
                    .spawn(move || serve(server, shared, next_id))
                    .ok();
                eprintln!("[ControlApi] écoute sur http://127.0.0.1:{CONTROL_PORT}");
            }
            Err(e) => {
                eprintln!("[ControlApi] port {CONTROL_PORT} indisponible, API désactivée : {e}");
            }
        }
        api
    }

    /// Vide la file des requêtes en attente (appelé par le thread UI).
    pub fn take_requests(&self) -> Vec<ControlRequest> {
        match self.shared.lock() {
            Ok(mut s) => s.queue.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Dépose la réponse d'une requête traitée.
    pub fn respond(&self, id: u64, value: Value) {
        if let Ok(mut s) = self.shared.lock() {
            s.responses.insert(id, value);
        }
    }

    /// Snapshot du statut courant, lu par `GET /api/status`.
    pub fn publish_status(&self, status: Value) {
        if let Ok(mut s) = self.shared.lock() {
            s.status = status;
        }
    }

    /// Donne le contexte egui utilisé pour réveiller l'UI (appelé à chaque frame).
    pub fn set_wakeup(&self, ctx: egui::Context) {
        if let Ok(mut s) = self.shared.lock() {
            s.wakeup = Some(ctx);
        }
    }
}

fn serve(server: Server, shared: Arc<Mutex<Shared>>, next_id: Arc<AtomicU64>) {
    for mut request in server.incoming_requests() {
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("/").to_string();
        let method = request.method().clone();
        let response = match (method, path.as_str()) {
            (Method::Get, "/api/health") => json_ok(json!({
                "ok": true,
                "app": "struidel",
                "uptime_s": shared
                    .lock()
                    .map(|s| s.started_at.elapsed().as_secs())
                    .unwrap_or(0),
            })),
            (Method::Get, "/api/status") => {
                let status = shared
                    .lock()
                    .map(|s| s.status.clone())
                    .unwrap_or_else(|_| json!({"ok": false, "error": "lock poisoned"}));
                json_ok(status)
            }
            (Method::Post, "/api/action") => {
                let mut body = String::new();
                match request.as_reader().read_to_string(&mut body) {
                    Ok(_) => match serde_json::from_str::<Value>(&body) {
                        Ok(v) if v.get("action").and_then(|a| a.as_str()).is_some() => {
                            dispatch(v, &shared, &next_id)
                        }
                        _ => json_err("invalid JSON or missing \"action\""),
                    },
                    Err(_) => json_err("failed to read body"),
                }
            }
            _ => Response::from_string("{\"ok\":false,\"error\":\"not found\"}")
                .with_status_code(StatusCode(404)),
        };
        let _ = request.respond(response);
    }
}

/// Met la requête en file et attend que le thread UI produise la réponse.
fn dispatch(body: Value, shared: &Arc<Mutex<Shared>>, next_id: &Arc<AtomicU64>) -> Response<Cursor<Vec<u8>>> {
    let id = next_id.fetch_add(1, Ordering::Relaxed);
    {
        let Ok(mut s) = shared.lock() else {
            return json_err("control queue lock poisoned");
        };
        s.queue.push_back(ControlRequest { id, body });
        // Réveille l'UI : sinon update() n'est pas rappelée et la requête attend.
        if let Some(ctx) = &s.wakeup {
            ctx.request_repaint();
        }
    }
    let deadline = Instant::now() + RESPONSE_WAIT;
    loop {
        {
            if let Ok(mut s) = shared.lock() {
                if let Some(resp) = s.responses.remove(&id) {
                    return json_ok(resp);
                }
            }
        }
        if Instant::now() >= deadline {
            return json_err("timeout: l'application n'a pas répondu");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn json_ok(v: Value) -> Response<Cursor<Vec<u8>>> {
    Response::from_string(v.to_string()).with_header(
        tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap(),
    )
}

fn json_err(msg: &str) -> Response<Cursor<Vec<u8>>> {
    Response::from_string(
        json!({"ok": false, "error": msg}).to_string(),
    )
    .with_status_code(StatusCode(400))
    .with_header(tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap())
}
