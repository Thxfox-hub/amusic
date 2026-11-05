//! Мост к Strudel (JS) через Node.js subprocess. Протокол: JSON по stdin/stdout.
//! Запуск: node scripts/strudel-runner.mjs из директории strudel.

use crate::settings;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct StrudelEvent {
    pub label: String,
    pub begin: f64,
    pub end: f64,
    pub color: String,
    /// Sample name (s("bd") → "bd"). For waveform lookup.
    pub s: Option<String>,
    /// Bank name (bank("crate") → Some("crate")). For waveform lookup.
    pub bank: Option<String>,
    /// Variant index (bd_0, bd_1 → Some(0), Some(1)). For waveform lookup.
    pub n: Option<f64>,
    /// Source id / code block label (value.id), чтобы группировать события по месту в коде.
    pub source: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(tag = "cmd", rename_all = "lowercase")]
enum NodeCommand {
    Eval {
        code: String,
        autostart: bool,
    },
    Start,
    Pause {
        position: f64,
    },
    Stop,
    Now,
    QueryArc {
        from: f64,
        to: f64,
        #[serde(skip)]
        generation: u64,
    },
    #[serde(rename = "loadsamples")]
    LoadSamples,
    #[serde(rename = "canceldownloads")]
    CancelDownloads,
    AudioWindow {
        from: f64,
        to: f64,
        #[serde(skip)]
        resp: Option<mpsc::Sender<Result<AudioWindowResult, String>>>,
    },
    Seek {
        position: f64,
    },
    #[serde(rename = "setbpm")]
    SetBpm {
        bpm: f64,
    },
    #[serde(rename = "getbpm")]
    GetBpm,
}

#[derive(Deserialize)]
struct NodeResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    #[serde(rename = "errorLine")]
    error_line: Option<usize>,
    #[serde(default)]
    #[serde(rename = "errorColumn")]
    error_column: Option<usize>,
    value: Option<f64>,
    events: Option<Vec<SerdeEvent>>,
    #[serde(default)]
    #[serde(rename = "audioEvents")]
    audio_events: Option<Vec<SerdeAudioEvent>>,
    #[serde(default)]
    cps: Option<f64>,
}

#[derive(Deserialize)]
struct SerdeEvent {
    label: String,
    begin: f64,
    end: f64,
    color: String,
    #[serde(default)]
    s: Option<String>,
    #[serde(default)]
    bank: Option<String>,
    #[serde(default)]
    n: Option<f64>,
    #[serde(default)]
    source: Option<String>,
}

#[derive(Deserialize)]
struct SerdeAudioValue {
    #[serde(default)]
    s: Option<String>,
    #[serde(default)]
    bank: Option<String>,
    #[serde(default)]
    n: Option<f64>,
    #[serde(default)]
    midi: Option<f64>,
    #[serde(default)]
    gain: Option<f64>,
    #[serde(default)]
    pan: Option<f64>,
    #[serde(default)]
    cps: Option<f64>,
    #[serde(default)]
    cut: Option<f64>,
    #[serde(default)]
    room: Option<f64>,
    #[serde(default)]
    shape: Option<f64>,
    #[serde(default)]
    attack: Option<f64>,
    #[serde(default)]
    decay: Option<f64>,
    #[serde(default)]
    sustain: Option<f64>,
    #[serde(default)]
    release: Option<f64>,
    #[serde(default)]
    speed: Option<f64>,
    #[serde(default)]
    cutoff: Option<f64>,
    #[serde(default)]
    hcutoff: Option<f64>,
    #[serde(default)]
    coarse: Option<f64>,
    #[serde(default)]
    crush: Option<f64>,
    #[serde(default)]
    compressor: Option<f64>,
    #[serde(default)]
    compressorRatio: Option<f64>,
    #[serde(default)]
    penv: Option<f64>,
    #[serde(default)]
    pattack: Option<f64>,
    #[serde(default)]
    prelease: Option<f64>,
}

#[derive(Deserialize)]
struct SerdeAudioEvent {
    begin: f64,
    end: f64,
    #[serde(default)]
    value: Option<SerdeAudioValue>,
}

#[derive(Debug, Clone)]
pub struct StrudelAudioEvent {
    pub begin: f64,
    pub end: f64,
    pub s: Option<String>,
    pub bank: Option<String>,
    pub n: Option<f64>,
    pub midi: Option<f64>,
    pub gain: Option<f64>,
    pub pan: Option<f64>,
    pub cps: Option<f64>,
    pub cut: Option<f64>,
    pub room: Option<f64>,
    pub shape: Option<f64>,
    pub attack: Option<f64>,
    pub decay: Option<f64>,
    pub sustain: Option<f64>,
    pub release: Option<f64>,
    pub speed: Option<f64>,
    pub cutoff: Option<f64>,
    pub hcutoff: Option<f64>,
    pub coarse: Option<f64>,
    pub crush: Option<f64>,
    /// Seuil du compresseur en dB (`.compressor(-32)`), None = pas de compression.
    pub compressor: Option<f64>,
    /// Ratio du compresseur (`.compressorRatio(10)`), None = pas de compression.
    pub compressor_ratio: Option<f64>,
    /// Enveloppe de pitch en demi-tons (`.penv(3)`).
    pub penv: Option<f64>,
    /// Durée de montée du pitch en secondes (`.pattack(0.05)`).
    pub pattack: Option<f64>,
    /// Durée de retombée du pitch en secondes (`.prelease(0.1)`).
    pub prelease: Option<f64>,
    /// Génération d'evaluation : incrémentée à chaque `evaluate()` ; permet au
    /// moteur audio de remplacer les voix d'un ancien code au lieu de les laisser
    /// jouer leurs anciens paramètres (les options semblaient alors « mortes »).
    pub epoch: u64,
}

pub struct AudioWindowResult {
    pub cps: f64,
    pub events: Vec<StrudelAudioEvent>,
}

/// Progress info during sample pack download (from runner stdout).
#[derive(Debug, Clone, Default)]
pub struct SampleProgress {
    pub pack: String,
    pub bank: String,
    pub file: String,
    pub path: String,
    pub n: u64,
    pub total_keys: Option<u64>,
    pub total_banks: Option<u64>,
}

/// Прогресс скачивания семплов: пак, текущее число файлов, всего файлов.
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub pack: String,
    pub current: u64,
    pub total: u64,
}

struct BridgeState {
    last_now: Option<f64>,
    last_events: Vec<StrudelEvent>,
    query_generation: u64,
    last_error: Option<String>,
    last_error_line: Option<usize>,
    last_error_column: Option<usize>,
    sample_progress: Option<SampleProgress>,
    /// Прогресс скачивания (pack, current, total) из runner stdout.
    download_progress: Option<DownloadProgress>,
    loadsamples_completed: bool,
    /// Génération d'evaluation — tamponnée sur chaque événement audio.
    eval_epoch: u64,
}

impl Default for BridgeState {
    fn default() -> Self {
        Self {
            last_now: None,
            last_events: Vec::new(),
            query_generation: 0,
            last_error: None,
            last_error_line: None,
            last_error_column: None,
            sample_progress: None,
            download_progress: None,
            loadsamples_completed: false,
            eval_epoch: 0,
        }
    }
}

/// Максимум строк в буфере терминала (логи runner stderr).
const TERMINAL_LOG_MAX: usize = 2000;

/// Handle clonable vers le canal de commandes du bridge — permet à un thread
/// worker d'émettre des requêtes `AudioWindow` sans toucher `StrudelBridge`.
#[derive(Clone)]
pub struct BridgeCommandSender(mpsc::Sender<NodeCommand>);

impl BridgeCommandSender {
    /// Requête audio_window bloquante — à appeler depuis un thread worker, pas l'UI.
    pub fn audio_window(
        &self,
        from: f64,
        to: f64,
        wait: Duration,
    ) -> Result<AudioWindowResult, String> {
        let (tx_resp, rx_resp) = mpsc::channel();
        self.0
            .send(NodeCommand::AudioWindow {
                from,
                to,
                resp: Some(tx_resp),
            })
            .map_err(|e| e.to_string())?;
        rx_resp
            .recv_timeout(wait)
            .map_err(|e| format!("audio_window timeout: {e}"))?
    }
}

pub struct StrudelBridge {
    tx: Option<mpsc::Sender<NodeCommand>>,
    state: Arc<Mutex<BridgeState>>,
    /// Логи stderr процесса runner (для вкладки «Терминал»).
    terminal_log: Arc<Mutex<Vec<String>>>,
}

#[derive(Debug, Clone, Copy)]
enum TransportMode {
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone)]
struct RunnerStateSnapshot {
    active_code: Option<String>,
    bpm: Option<f64>,
    position: f64,
    transport: TransportMode,
}

impl Default for RunnerStateSnapshot {
    fn default() -> Self {
        Self {
            active_code: None,
            bpm: None,
            position: 0.0,
            transport: TransportMode::Stopped,
        }
    }
}

impl Default for StrudelBridge {
    fn default() -> Self {
        Self::with_state(BridgeState::default())
    }
}

impl StrudelBridge {
    fn with_state(initial_state: BridgeState) -> Self {
        Self::with_state_and_tx(initial_state, true)
    }

    fn with_state_and_tx(initial_state: BridgeState, start_worker: bool) -> Self {
        let state = Arc::new(Mutex::new(initial_state));
        if !start_worker {
            return Self {
                tx: None,
                state,
                terminal_log: Arc::new(Mutex::new(Vec::new())),
            };
        }
        let terminal_log = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel();
        let state_clone = Arc::clone(&state);
        let state_err = Arc::clone(&state);
        let terminal_log_worker = Arc::clone(&terminal_log);
        thread::spawn(move || {
            if let Err(e) = run_worker(rx, state_clone, terminal_log_worker) {
                if let Ok(mut s) = state_err.lock() {
                    s.last_error = Some(e);
                    s.last_error_line = None;
                    s.last_error_column = None;
                }
            }
        });
        Self {
            tx: Some(tx),
            state,
            terminal_log,
        }
    }
}

/// Tue et récolte le processus runner quel que soit le chemin de sortie.
/// Sans ça, `Child` n'est pas tué au drop et chaque retour d'erreur laissait
/// un processus node orphelin ; un `wait()` seul pouvait aussi bloquer à
/// jamais si le fils était encore vivant.
struct ChildGuard(Child);

impl ChildGuard {
    fn reap(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.reap();
    }
}

fn run_worker(
    rx: mpsc::Receiver<NodeCommand>,
    state: Arc<Mutex<BridgeState>>,
    terminal_log: Arc<Mutex<Vec<String>>>,
) -> Result<(), String> {
    let mut pending: Option<NodeCommand> = None;
    let mut snapshot = RunnerStateSnapshot::default();

    loop {
        let (child, mut stdin_opt, stdout_opt, stderr_opt) = spawn_node()?;
        let mut child = ChildGuard(child);
        let mut stdin = stdin_opt.take().ok_or("no stdin")?;
        let stdout = stdout_opt.ok_or("no stdout")?;
        if let Some(stderr) = stderr_opt {
            let log = Arc::clone(&terminal_log);
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    if let Ok(l) = line {
                        if let Ok(mut buf) = log.lock() {
                            buf.push(l);
                            let n = buf.len();
                            if n > TERMINAL_LOG_MAX {
                                buf.drain(0..n - TERMINAL_LOG_MAX);
                            }
                        }
                    }
                }
            });
        }
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let mut tick = 0u32;

        if let Err(e) = replay_state(&mut stdin, &mut reader, &mut line, &snapshot) {
            if is_disconnect_error(&e) {
                child.reap();
                thread::sleep(Duration::from_millis(200));
                continue;
            }
            return Err(e);
        }

        'session: loop {
            let cmd = match pending.take() {
                Some(c) => Some(c),
                None => match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(c) => Some(c),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    // L'émetteur est mort (bridge relâché) : on sort au lieu de
                    // respawner le runner indéfiniment. Le ChildGuard tue node.
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                },
            };
            let cmd = match cmd {
                Some(c) => c,
                None => {
                    tick = tick.wrapping_add(1);
                    if tick % 2 == 0 {
                        NodeCommand::Now
                    } else {
                        continue;
                    }
                }
            };

            update_snapshot(&mut snapshot, &cmd);
            let retry_cmd = if matches!(cmd, NodeCommand::Now) {
                None
            } else {
                Some(cmd.clone())
            };

            match &cmd {
                NodeCommand::AudioWindow { resp, .. } => {
                    if let Err(e) = send_cmd(&mut stdin, &cmd) {
                        if is_disconnect_error(&e) {
                            pending = retry_cmd;
                            break 'session;
                        }
                        return Err(e);
                    }
                    loop {
                        line.clear();
                        let n = match reader.read_line(&mut line) {
                            Err(e) if e.kind() == ErrorKind::BrokenPipe => {
                                pending = retry_cmd;
                                break 'session;
                            }
                            Err(e) => return Err(e.to_string()),
                            Ok(n) => n,
                        };
                        if n == 0 {
                            pending = retry_cmd;
                            break 'session;
                        }
                        let trimmed = line.trim();
                        if trimmed.is_empty() || !trimmed.starts_with('{') {
                            continue;
                        }
                        match serde_json::from_str::<NodeResponse>(trimmed) {
                            Ok(resp_msg) => {
                                if !resp_msg.ok {
                                    let err = resp_msg
                                        .error
                                        .unwrap_or_else(|| "audioWindow error".to_string());
                                    if let Some(tx) = resp {
                                        let _ = tx.send(Err(err));
                                    } else if let Ok(mut s) = state.lock() {
                                        s.last_error = Some("audioWindow error".to_string());
                                        s.last_error_line = None;
                                        s.last_error_column = None;
                                    }
                                } else {
                                    let cps = resp_msg.cps.unwrap_or(1.0);
                                    let epoch = state
                                        .lock()
                                        .map(|s| s.eval_epoch)
                                        .unwrap_or(0);
                                    let events = resp_msg
                                        .audio_events
                                        .unwrap_or_default()
                                        .into_iter()
                                        .map(|e| {
                                            let v = e.value.unwrap_or(SerdeAudioValue {
                                                s: None,
                                                bank: None,
                                                n: None,
                                                midi: None,
                                                gain: None,
                                                pan: None,
                                                cps: None,
                                                cut: None,
                                                room: None,
                                                shape: None,
                                                attack: None,
                                                decay: None,
                                                sustain: None,
                                                release: None,
                                                speed: None,
                                                cutoff: None,
                                                hcutoff: None,
                                                coarse: None,
                                                crush: None,
                                                compressor: None,
                                                compressorRatio: None,
                                                penv: None,
                                                pattack: None,
                                                prelease: None,
                                            });
                                            StrudelAudioEvent {
                                                begin: e.begin,
                                                end: e.end,
                                                s: v.s,
                                                bank: v.bank,
                                                n: v.n,
                                                midi: v.midi,
                                                gain: v.gain,
                                                pan: v.pan,
                                                cps: v.cps,
                                                cut: v.cut,
                                                room: v.room,
                                                shape: v.shape,
                                                attack: v.attack,
                                                decay: v.decay,
                                                sustain: v.sustain,
                                                release: v.release,
                                                speed: v.speed,
                                                cutoff: v.cutoff,
                                                hcutoff: v.hcutoff,
                                                coarse: v.coarse,
                                                crush: v.crush,
                                                compressor: v.compressor,
                                                compressor_ratio: v.compressorRatio,
                                                penv: v.penv,
                                                pattack: v.pattack,
                                                prelease: v.prelease,
                                                epoch,
                                            }
                                        })
                                        .collect();
                                    if let Some(tx) = resp {
                                        let _ = tx.send(Ok(AudioWindowResult { cps, events }));
                                    } else if let Ok(mut s) = state.lock() {
                                        s.last_error = None;
                                        s.last_error_line = None;
                                        s.last_error_column = None;
                                    }
                                }
                                break;
                            }
                            Err(_) => continue,
                        }
                    }
                }
                NodeCommand::LoadSamples => {
                    if let Err(e) = send_cmd(&mut stdin, &cmd) {
                        if is_disconnect_error(&e) {
                            pending = retry_cmd;
                            break 'session;
                        }
                        return Err(e);
                    }
                    loop {
                        line.clear();
                        let n = match reader.read_line(&mut line) {
                            Err(e) if e.kind() == ErrorKind::BrokenPipe => {
                                pending = retry_cmd;
                                break 'session;
                            }
                            Err(e) => return Err(e.to_string()),
                            Ok(n) => n,
                        };
                        if n == 0 {
                            pending = retry_cmd;
                            break 'session;
                        }
                        let trimmed = line.trim();
                        if trimmed.is_empty() || !trimmed.starts_with('{') {
                            continue;
                        }
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                            if v.get("type").and_then(|t| t.as_str()) == Some("download_progress") {
                                let pack = v
                                    .get("pack")
                                    .and_then(|p| p.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let current =
                                    v.get("current").and_then(|c| c.as_u64()).unwrap_or(0);
                                let total = v.get("total").and_then(|t| t.as_u64()).unwrap_or(0);
                                if let Ok(mut s) = state.lock() {
                                    s.download_progress = Some(DownloadProgress {
                                        pack,
                                        current,
                                        total,
                                    });
                                }
                                continue;
                            }
                            if v.get("type").and_then(|t| t.as_str()) == Some("sample_progress") {
                                let prog = SampleProgress {
                                    pack: v
                                        .get("pack")
                                        .and_then(|p| p.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    bank: v
                                        .get("bank")
                                        .and_then(|b| b.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    file: v
                                        .get("file")
                                        .and_then(|f| f.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    path: v
                                        .get("path")
                                        .and_then(|p| p.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    n: v.get("n").and_then(|n| n.as_u64()).unwrap_or(0),
                                    total_keys: v.get("totalKeys").and_then(|t| t.as_u64()),
                                    total_banks: v.get("totalBanks").and_then(|t| t.as_u64()),
                                };
                                if let Ok(mut s) = state.lock() {
                                    s.sample_progress = Some(prog);
                                }
                                continue;
                            }
                            if v.get("ok").and_then(|o| o.as_bool()) == Some(true) {
                                if let Ok(mut s) = state.lock() {
                                    s.loadsamples_completed = true;
                                }
                                break;
                            }
                        }
                    }
                }
                NodeCommand::CancelDownloads => {
                    if let Err(e) = send_cmd(&mut stdin, &cmd) {
                        if is_disconnect_error(&e) {
                            pending = retry_cmd;
                            break 'session;
                        }
                        return Err(e);
                    }
                }
                _ => {
                    if let Err(e) = send_cmd(&mut stdin, &cmd) {
                        if is_disconnect_error(&e) {
                            pending = retry_cmd;
                            break 'session;
                        }
                        return Err(e);
                    }
                    loop {
                        line.clear();
                        let n = match reader.read_line(&mut line) {
                            Err(e) if e.kind() == ErrorKind::BrokenPipe => {
                                pending = retry_cmd;
                                break 'session;
                            }
                            Err(e) => return Err(e.to_string()),
                            Ok(n) => n,
                        };
                        if n == 0 {
                            pending = retry_cmd;
                            break 'session;
                        }
                        let trimmed = line.trim();
                        if trimmed.is_empty() || !trimmed.starts_with('{') {
                            continue;
                        }
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                            if v.get("type").and_then(|t| t.as_str()) == Some("download_progress") {
                                let pack = v
                                    .get("pack")
                                    .and_then(|p| p.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let current =
                                    v.get("current").and_then(|c| c.as_u64()).unwrap_or(0);
                                let total = v.get("total").and_then(|t| t.as_u64()).unwrap_or(0);
                                if let Ok(mut s) = state.lock() {
                                    s.download_progress = Some(DownloadProgress {
                                        pack,
                                        current,
                                        total,
                                    });
                                }
                                continue;
                            }
                        }
                        match serde_json::from_str::<NodeResponse>(trimmed) {
                            Ok(resp) => {
                                if !resp.ok {
                                    if let Some(ref e) = resp.error {
                                        if let Ok(mut s) = state.lock() {
                                            s.last_error = Some(e.clone());
                                            s.last_error_line = resp.error_line;
                                            s.last_error_column = resp.error_column;
                                        }
                                    }
                                } else if let Ok(mut s) = state.lock() {
                                    s.last_error = None;
                                    s.last_error_line = None;
                                    s.last_error_column = None;
                                    if let Some(v) = resp.value {
                                        if matches!(cmd, NodeCommand::Stop) {
                                            s.last_now = None;
                                        } else {
                                            s.last_now = Some(v);
                                        }
                                    }
                                    if matches!(
                                        cmd,
                                        NodeCommand::Stop
                                            | NodeCommand::Eval { .. }
                                            | NodeCommand::Seek { .. }
                                    ) {
                                        invalidate_events_cache(&mut s);
                                    }
                                    if let Some(evs) = resp.events {
                                        if let NodeCommand::QueryArc { generation, .. } = cmd {
                                            let _ = apply_query_events_if_current(
                                                &mut s, generation, evs,
                                            );
                                        }
                                    }
                                }
                                break;
                            }
                            Err(_) => continue,
                        }
                    }
                }
            }
        }

        if let Ok(mut s) = state.lock() {
            s.last_error = Some("runner disconnected, reconnecting".to_string());
            s.last_error_line = None;
            s.last_error_column = None;
        }
        child.reap();
        thread::sleep(Duration::from_millis(200));
    }
}

fn spawn_node() -> Result<
    (
        Child,
        Option<ChildStdin>,
        Option<ChildStdout>,
        Option<ChildStderr>,
    ),
    String,
> {
    let strudel_dir = crate::paths::ensure_strudel_runtime()?;
    let runner = strudel_dir.join("scripts").join("strudel-runner.mjs");
    if !runner.exists() {
        return Err(format!(
            "Runner not found: {} (set STRUDEL_HOME or reinstall struidel)",
            runner.display()
        ));
    }
    // Папка для локальных файлов/кэша Strudel берётся из настроек IDE (sample_folder),
    // чтобы пользователь мог менять её через UI.
    let ide_settings = settings::load_settings();
    let strudel_files = std::path::PathBuf::from(&ide_settings.sample_folder);
    if let Err(e) = std::fs::create_dir_all(&strudel_files) {
        eprintln!(
            "[StrudelBridge] failed to create STRUDEL_FILES dir {:?}: {}",
            strudel_files, e
        );
    }
    let strudel_files_str = strudel_files.to_string_lossy().to_string();
    let mut child = Command::new("node")
        .arg(&runner)
        .current_dir(&strudel_dir)
        .env("STRUDEL_FILES", &strudel_files_str)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn node: {}", e))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    Ok((child, stdin, stdout, stderr))
}

const RUNNER_DISCONNECTED: &str = "runner disconnected";

fn send_cmd(stdin: &mut ChildStdin, cmd: &NodeCommand) -> Result<(), String> {
    let line = serde_json::to_string(cmd).map_err(|e| e.to_string())?;
    let write = |e: std::io::Error| {
        if e.kind() == ErrorKind::BrokenPipe {
            RUNNER_DISCONNECTED.to_string()
        } else {
            e.to_string()
        }
    };
    stdin.write_all(line.as_bytes()).map_err(write)?;
    stdin.write_all(b"\n").map_err(write)?;
    stdin.flush().map_err(write)?;
    Ok(())
}

fn is_disconnect_error(error: &str) -> bool {
    error == RUNNER_DISCONNECTED || error.contains("Broken pipe")
}

fn invalidate_events_cache(state: &mut BridgeState) {
    state.query_generation = state.query_generation.wrapping_add(1);
    state.last_events.clear();
    state.last_now = None;
}

fn map_events(evs: Vec<SerdeEvent>) -> Vec<StrudelEvent> {
    evs.into_iter()
        .map(|e| StrudelEvent {
            label: e.label,
            begin: e.begin,
            end: e.end,
            color: e.color,
            s: e.s,
            bank: e.bank,
            n: e.n,
            source: e.source,
        })
        .collect()
}

fn apply_query_events_if_current(
    state: &mut BridgeState,
    response_generation: u64,
    events: Vec<SerdeEvent>,
) -> bool {
    if state.query_generation != response_generation {
        return false;
    }
    state.last_events = map_events(events);
    true
}

fn update_snapshot(snapshot: &mut RunnerStateSnapshot, cmd: &NodeCommand) {
    match cmd {
        NodeCommand::Eval { code, autostart } => {
            snapshot.active_code = Some(code.clone());
            snapshot.transport = if *autostart {
                TransportMode::Playing
            } else {
                TransportMode::Paused
            };
        }
        NodeCommand::Start => snapshot.transport = TransportMode::Playing,
        NodeCommand::Pause { position } => {
            snapshot.position = *position;
            snapshot.transport = TransportMode::Paused;
        }
        NodeCommand::Stop => {
            snapshot.position = 0.0;
            snapshot.transport = TransportMode::Stopped;
        }
        NodeCommand::Seek { position } => snapshot.position = *position,
        NodeCommand::SetBpm { bpm } => snapshot.bpm = Some(*bpm),
        _ => {}
    }
}

fn replay_state(
    stdin: &mut ChildStdin,
    reader: &mut BufReader<ChildStdout>,
    line: &mut String,
    snapshot: &RunnerStateSnapshot,
) -> Result<(), String> {
    if let Some(bpm) = snapshot.bpm {
        send_cmd(stdin, &NodeCommand::SetBpm { bpm })?;
        consume_response(reader, line)?;
    }
    if let Some(code) = &snapshot.active_code {
        send_cmd(
            stdin,
            &NodeCommand::Eval {
                code: code.clone(),
                autostart: false,
            },
        )?;
        consume_response(reader, line)?;
        send_cmd(
            stdin,
            &NodeCommand::Seek {
                position: snapshot.position,
            },
        )?;
        consume_response(reader, line)?;
        match snapshot.transport {
            TransportMode::Stopped => {
                send_cmd(stdin, &NodeCommand::Stop)?;
                consume_response(reader, line)?;
            }
            TransportMode::Playing => {
                send_cmd(stdin, &NodeCommand::Start)?;
                consume_response(reader, line)?;
            }
            TransportMode::Paused => {
                send_cmd(
                    stdin,
                    &NodeCommand::Pause {
                        position: snapshot.position,
                    },
                )?;
                consume_response(reader, line)?;
            }
        }
    }
    Ok(())
}

fn consume_response(reader: &mut BufReader<ChildStdout>, line: &mut String) -> Result<(), String> {
    loop {
        line.clear();
        let n = reader.read_line(line).map_err(|e| {
            if e.kind() == ErrorKind::BrokenPipe {
                RUNNER_DISCONNECTED.to_string()
            } else {
                e.to_string()
            }
        })?;
        if n == 0 {
            return Err(RUNNER_DISCONNECTED.to_string());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }
        if serde_json::from_str::<NodeResponse>(trimmed).is_ok() {
            return Ok(());
        }
    }
}

impl StrudelBridge {
    pub fn start(&mut self) -> Result<(), String> {
        if let Some(ref tx) = self.tx {
            tx.send(NodeCommand::Start).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn pause(&mut self, position: f64) -> Result<(), String> {
        if let Some(ref tx) = self.tx {
            tx.send(NodeCommand::Pause { position })
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), String> {
        self.clear_events_cache();
        if let Some(ref tx) = self.tx {
            tx.send(NodeCommand::Stop).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn now(&self) -> Option<f64> {
        self.state.lock().ok().and_then(|s| s.last_now)
    }

    pub fn clear_events_cache(&self) {
        if let Ok(mut s) = self.state.lock() {
            invalidate_events_cache(&mut s);
        }
    }

    pub fn query_arc(&self, _from: f64, _to: f64) -> Vec<StrudelEvent> {
        self.state
            .lock()
            .map(|s| s.last_events.clone())
            .unwrap_or_default()
    }

    /// Запрашивает события для указанного диапазона циклов (для скролла таймлайна).
    pub fn request_query(&self, from: f64, to: f64) {
        let generation = self
            .state
            .lock()
            .map(|s| s.query_generation)
            .unwrap_or_default();
        if let Some(ref tx) = self.tx {
            let _ = tx.send(NodeCommand::QueryArc {
                from,
                to,
                generation,
            });
        }
    }

    pub fn evaluate(&mut self, code: &str) -> Result<(), String> {
        self.clear_events_cache();
        if let Ok(mut s) = self.state.lock() {
            s.eval_epoch = s.eval_epoch.wrapping_add(1);
        }
        if let Some(ref tx) = self.tx {
            tx.send(NodeCommand::Eval {
                code: code.to_string(),
                autostart: true,
            })
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Ask the runner to load default sample packs (crate, piano, Dirt-Samples, vcsl, etc.) into the cache.
    pub fn load_samples(&self) -> Result<(), String> {
        if let Some(ref tx) = self.tx {
            tx.send(NodeCommand::LoadSamples)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

        /// Clone du canal de commandes node — pour le scheduler hors-UI (`sched-tick`),
    /// qui continue le scheduling audio quand le thread UI est gelé (fenêtre occluse).
    pub fn command_sender(&self) -> Option<BridgeCommandSender> {
        self.tx.clone().map(BridgeCommandSender)
    }

    /// Блокирующий запрос (может подвисать UI).
    pub fn audio_window(&mut self, from: f64, to: f64) -> Result<AudioWindowResult, String> {
        let rx = self.start_audio_window_request(from, to)?;
        rx.recv_timeout(Duration::from_millis(500))
            .map_err(|e| format!("audio_window timeout: {e}"))?
    }

    /// Запускает запрос аудио-окна без блокировки. Ответ нужно забирать через try_recv на полученном приёмнике.
    pub fn start_audio_window_request(
        &mut self,
        from: f64,
        to: f64,
    ) -> Result<mpsc::Receiver<Result<AudioWindowResult, String>>, String> {
        let (tx_resp, rx_resp) = mpsc::channel();
        if let Some(ref tx) = self.tx {
            tx.send(NodeCommand::AudioWindow {
                from,
                to,
                resp: Some(tx_resp),
            })
            .map_err(|e| e.to_string())?;
            Ok(rx_resp)
        } else {
            Err("Strudel bridge is not initialized".to_string())
        }
    }

    pub fn last_error(&self) -> Option<String> {
        self.state.lock().ok().and_then(|s| s.last_error.clone())
    }

    pub fn last_error_location(&self) -> Option<(usize, usize)> {
        self.state.lock().ok().and_then(|s| {
            if s.last_error_line.is_none() && s.last_error_column.is_none() {
                None
            } else {
                Some((
                    s.last_error_line.unwrap_or(0),
                    s.last_error_column.unwrap_or(0),
                ))
            }
        })
    }

    /// Текущий прогресс загрузки семплов (если идёт).
    pub fn sample_progress(&self) -> Option<SampleProgress> {
        self.state
            .lock()
            .ok()
            .and_then(|s| s.sample_progress.clone())
    }

    /// Прогресс скачивания (пак, текущее, всего) для полосы в UI.
    pub fn download_progress(&self) -> Option<DownloadProgress> {
        self.state
            .lock()
            .ok()
            .and_then(|s| s.download_progress.clone())
    }

    /// Отправить команду отмены скачивания семплов.
    pub fn cancel_loadsamples(&self) -> Result<(), String> {
        if let Some(ref tx) = self.tx {
            tx.send(NodeCommand::CancelDownloads)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Перемотать паттерн на указанную позицию (в циклах).
    pub fn seek(&self, position: f64) -> Result<(), String> {
        self.clear_events_cache();
        if let Some(ref tx) = self.tx {
            tx.send(NodeCommand::Seek { position })
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Установить темп в BPM (CPS = BPM/60).
    pub fn set_bpm(&self, bpm: f64) -> Result<(), String> {
        if let Some(ref tx) = self.tx {
            tx.send(NodeCommand::SetBpm { bpm })
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Забрать флаг завершения loadsamples и сбросить его.
    pub fn take_loadsamples_completed(&self) -> bool {
        if let Ok(mut s) = self.state.lock() {
            let v = s.loadsamples_completed;
            s.loadsamples_completed = false;
            return v;
        }
        false
    }

    /// Содержимое лога терминала (stderr runner) для вкладки «Терминал».
    pub fn terminal_lines(&self) -> Vec<String> {
        self.terminal_log
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_serde_event(sample: &str) -> SerdeEvent {
        SerdeEvent {
            label: sample.to_string(),
            begin: 0.0,
            end: 1.0,
            color: "#fff".to_string(),
            s: Some(sample.to_string()),
            bank: None,
            n: None,
            source: None,
        }
    }

    fn test_strudel_event(sample: &str) -> StrudelEvent {
        StrudelEvent {
            label: sample.to_string(),
            begin: 0.0,
            end: 1.0,
            color: "#fff".to_string(),
            s: Some(sample.to_string()),
            bank: None,
            n: None,
            source: None,
        }
    }

    #[test]
    fn clear_events_cache_clears_events_now_and_advances_generation() {
        let mut state = BridgeState::default();
        state.last_now = Some(1.25);
        state.last_events.push(test_strudel_event("bd"));
        let bridge = StrudelBridge::with_state_and_tx(state, false);
        let before = bridge.state.lock().unwrap().query_generation;
        bridge.clear_events_cache();
        let after = bridge.state.lock().unwrap().query_generation;
        assert_eq!(after, before.wrapping_add(1));
        assert!(bridge.query_arc(0.0, 0.0).is_empty());
        assert_eq!(bridge.now(), None);
    }

    #[test]
    fn stale_query_generation_does_not_populate_last_events() {
        let mut state = BridgeState::default();
        let request_generation = state.query_generation;
        invalidate_events_cache(&mut state);
        assert!(!apply_query_events_if_current(
            &mut state,
            request_generation,
            vec![test_serde_event("bd")],
        ));
        assert!(state.last_events.is_empty());
    }

    #[test]
    fn current_query_generation_populates_last_events() {
        let mut state = BridgeState::default();
        let request_generation = state.query_generation;
        assert!(apply_query_events_if_current(
            &mut state,
            request_generation,
            vec![test_serde_event("bd")],
        ));
        assert_eq!(state.last_events.len(), 1);
        assert_eq!(state.last_events[0].s.as_deref(), Some("bd"));
    }
}
