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
const RESPONSE_WAIT: Duration = Duration::from_secs(3);
/// Au-delà de ce délai sans `update()`, l'UI est considérée gelée — en pratique :
/// fenêtre occluse (autre workspace) → Wayland suspend les frame callbacks →
/// la boucle eframe se fige. Les actions écriture sont alors rejetées vite au
/// lieu d'attendre un timeout, et les lectures répondent depuis le snapshot.
const UI_STALE_MS: u64 = 2500;

/// Une requête d'un client HTTP, exécutée par le thread UI.
pub struct ControlRequest {
    pub id: u64,
    /// Corps JSON de la requête (`{"action": "...", ...}`).
    pub body: Value,
    /// Instant de mise en file : une requête trop vieille est jetée sans être
    /// exécutée — sinon un gel UI long déclencherait une rafale d'actions
    /// périmées au dégel.
    pub queued_at: Instant,
}

struct Shared {
    queue: VecDeque<ControlRequest>,
    responses: HashMap<u64, Value>,
    status: Value,
    started_at: Instant,
    /// Contexte egui pour réveiller le thread UI quand une requête arrive :
    /// sans repaint, `update()` n'est jamais appelée et la file resterait pleine.
    wakeup: Option<egui::Context>,
    /// Heartbeat du thread UI (epoch millis, alimenté à chaque `update()`).
    heartbeat: Option<Arc<AtomicU64>>,
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
                heartbeat: None,
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

    /// Branche le heartbeat du watchdog UI (epoch millis à chaque `update()`).
    pub fn set_heartbeat(&self, heartbeat: Arc<AtomicU64>) {
        if let Ok(mut s) = self.shared.lock() {
            s.heartbeat = Some(heartbeat);
        }
    }

    /// Vide la file des requêtes en attente (appelé par le thread UI).
    /// Les requêtes plus vieilles que `UI_STALE_MS` sont jetées : leur client a
    /// déjà eu un timeout, et les exécuter déclencherait des actions périmées.
    pub fn take_requests(&self) -> Vec<ControlRequest> {
        match self.shared.lock() {
            Ok(mut s) => {
                let now = Instant::now();
                s.queue
                    .retain(|r| now.duration_since(r.queued_at) < Duration::from_millis(UI_STALE_MS));
                s.queue.drain(..).collect()
            }
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

/// Âge du heartbeat UI en millisecondes (None = heartbeat non branché → sain).
fn ui_stale_ms(shared: &Arc<Mutex<Shared>>) -> Option<u64> {
    let s = shared.lock().ok()?;
    let beat = s.heartbeat.as_ref()?.load(Ordering::Relaxed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Some(now.saturating_sub(beat))
}

/// Met la requête en file et attend que le thread UI produise la réponse.
/// Deux courts-circuits : les lectures (`ping`, `get_status`) sont servies
/// directement depuis le snapshot — elles répondent même si la boucle UI est
/// figée (fenêtre occluse) — et les écritures sont rejetées vite quand l'UI
/// est figée, au lieu de s'empiler pour rien jusqu'au timeout.
fn dispatch(body: Value, shared: &Arc<Mutex<Shared>>, next_id: &Arc<AtomicU64>) -> Response<Cursor<Vec<u8>>> {
    let action = body.get("action").and_then(|a| a.as_str()).unwrap_or("");
    let stale_ms = ui_stale_ms(shared);
    let ui_paused = stale_ms.is_some_and(|ms| ms > UI_STALE_MS);
    match action {
        "ping" => {
            return json_ok(json!({
                "ok": true, "app": "struidel",
                "ui_paused": ui_paused,
                "ui_stale_ms": stale_ms,
            }));
        }
        "get_status" => {
            let mut status = shared
                .lock()
                .map(|s| s.status.clone())
                .unwrap_or_else(|_| json!({"ok": false}));
            if let Some(obj) = status.as_object_mut() {
                obj.insert("ui_paused".into(), json!(ui_paused));
                if let Some(ms) = stale_ms {
                    obj.insert("ui_stale_ms".into(), json!(ms));
                }
                if ui_paused {
                    obj.insert(
                        "hint".into(),
                        json!("interface en pause — la fenêtre struIDEl est occluse (autre workspace) ; affiche-la ou épingle-la"),
                    );
                }
            }
            return json_ok(status);
        }
        _ => {}
    }
    let id = next_id.fetch_add(1, Ordering::Relaxed);
    {
        let Ok(mut s) = shared.lock() else {
            return json_err("control queue lock poisoned");
        };
        s.queue.push_back(ControlRequest {
            id,
            body,
            queued_at: Instant::now(),
        });
        // Réveille l'UI : sinon update() n'est pas rappelée et la requête attend.
        if let Some(ctx) = &s.wakeup {
            ctx.request_repaint();
        }
    }
    let start = Instant::now();
    let deadline = start + RESPONSE_WAIT;
    loop {
        {
            if let Ok(mut s) = shared.lock() {
                if let Some(resp) = s.responses.remove(&id) {
                    return json_ok(resp);
                }
            }
        }
        // Heartbeat gelé pendant l'attente → la boucle UI ne tourne pas
        // (fenêtre occluse) : répondre honnêtement au lieu d'attendre.
        // Délai de grâce 600 ms : une app idle visible est parquée (heartbeat
        // périmé) mais le wakeup la réveille en ~10 ms — ne pas la condamner
        // avant qu'elle ait eu sa chance.
        if Instant::now().duration_since(start) > Duration::from_millis(600)
            && ui_stale_ms(shared).is_some_and(|ms| ms > UI_STALE_MS)
        {
            return json_err(
                "interface en pause : la fenêtre struIDEl est occluse — affiche-la (workspace) ou épingle-la pour réactiver l'API",
            );
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
