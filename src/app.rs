//! Главное состояние приложения и разметка окна.

use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use crate::doc_json;
use crate::settings::{self, IdeSettings, RecentItem};
use crate::strudel_bridge::StrudelBridge;
use crate::strudel_parser::parse_pattern_document;
use crate::sync_engine::{SyncEngine, SyncResult};
use crate::ui::{self, ActiveSampleEvent, EditorTab, ExamplePlaybackState, FileTree};
use crate::visual_stage::{self, VisualStage};
use crate::workspace_layout::{ContentId, LayoutNode, PanelData};

use crate::arrangement::PatternArrangement;
use crate::arrangement_ir::{stable_clip_id, ArrangementEdit};
use crate::audio::{AudioEngine, AudioEvent};
use crate::audio_clip_engine::{write_wav_with_options, ClipRenderEngine, RenderConfig};
use crate::audio_recording::{RecordingController, RecordingFinalize, RecordingRequest};
use crate::offline_export;
use crate::pattern_ir::{PatternDocument, PatternIr};
use crate::project_audio::{
    AudioClipId, AudioSource, AudioTrackId, Fade, FadeCurve, FadeEdge, FxSlot,
    ProjectAudioState, RecordingSessionInfo,
};
use crate::sample_loader::SampleKey;
use crate::waveform_cache::WaveformPeak;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const REVERSE_SEEK_BATCH_WINDOW_CYCLES: f64 = 0.12;
const REVERSE_SEEK_MIN_INTERVAL: Duration = Duration::from_millis(90);

/// État miroir écrit par le thread UI et lu par `sched-tick` — le worker qui
/// continue le scheduling audio quand l'UI est gelée (fenêtre occluse).
/// Les f64 transitent en bits dans des atomics (monotonicité non garantie
/// sous concurrence, mais les deux écrivains sont mutuellement exclusifs en
/// pratique : le worker n'agit que quand le heartbeat UI est mort).
struct SchedShared {
    playing: AtomicBool,
    paused: AtomicBool,
    reverse: AtomicBool,
    audio_enabled: AtomicBool,
    /// Dernier `current_time` vu par l'UI (cycles) + instant de cette écriture.
    current_time: AtomicU64,
    current_time_at: std::sync::Mutex<Instant>,
    /// CPS courant (cycles/seconde) pour avancer le temps pendant le gel.
    cps: AtomicU64,
    /// settings.audio_gain — appliqué aux voix schedulées par le worker.
    audio_gain: AtomicU64,
    session_start: AtomicU64,
    /// Borne déjà schedulée (cycles) — source de vérité unique UI+worker.
    last_scheduled_to: AtomicU64,
    /// Moteur audio courant (peut être remplacé lors d'une reconstruction async).
    engine: std::sync::Mutex<Option<Arc<AudioEngine>>>,
}

impl SchedShared {
    fn new() -> Self {
        Self {
            playing: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            reverse: AtomicBool::new(false),
            audio_enabled: AtomicBool::new(true),
            current_time: AtomicU64::new(0),
            current_time_at: std::sync::Mutex::new(Instant::now()),
            cps: AtomicU64::new(1.0f64.to_bits()),
            audio_gain: AtomicU64::new(1.0f64.to_bits()),
            session_start: AtomicU64::new(0),
            last_scheduled_to: AtomicU64::new((-1.0f64).to_bits()),
            engine: std::sync::Mutex::new(None),
        }
    }
    fn get_f64(a: &AtomicU64) -> f64 {
        f64::from_bits(a.load(Ordering::Relaxed))
    }
    fn set_f64(a: &AtomicU64, v: f64) {
        a.store(v.to_bits(), Ordering::Relaxed);
    }
    fn scheduled_to(&self) -> f64 {
        Self::get_f64(&self.last_scheduled_to)
    }
    fn set_scheduled_to(&self, v: f64) {
        Self::set_f64(&self.last_scheduled_to, v);
    }
    /// Temps courant estimé : le dernier vu par l'UI + dérive au cps courant.
    /// Correct tant que l'UI écrivait avant de geler (fenêtre occluse).
    fn current_estimate(&self) -> f64 {
        let base = Self::get_f64(&self.current_time);
        let cps = Self::get_f64(&self.cps).max(0.01);
        let elapsed = self
            .current_time_at
            .lock()
            .map(|i| i.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        base + elapsed * cps
    }
}

pub struct StruidelApp {
    bridge: StrudelBridge,
    /// Дерево разметки: сплиты и панели с вкладками (Рабочее пространство, Таймлайн, Документация, Настройки).
    root_layout: LayoutNode,
    editor_tabs: Vec<EditorTab>,
    active_editor_index: usize,
    file_tree: FileTree,
    /// Дерево для папки с семплами (sample_folder).
    samples_file_tree: FileTree,
    root_folder: Option<PathBuf>,
    recent: Vec<RecentItem>,
    settings: IdeSettings,
    /// Документация из strudel/doc.json для подсказок и вкладки «Документация»
    doc_store: Option<doc_json::DocStore>,
    /// Поиск по имени функции во вкладке «Документация»
    doc_lookup: String,
    current_time: f64,
    playing: bool,
    paused: bool,
    /// Какое верхнее меню открыто (File, Edit, Run, More). Без Selection, View, Go.
    open_top_menu: Option<TopMenuKind>,
    /// Позиция выпадающего меню (левый верхний угол под пунктом меню).
    menu_dropdown_anchor: Option<egui::Rect>,
    save_as_requested: bool,
    audio: Option<Arc<AudioEngine>>,
    /// Init audio en cours (l'énumération ALSA + open du device peut prendre des
    /// secondes — jamais sur le thread UI). Receveur rempli par un thread dédié.
    pending_audio_engine: Option<mpsc::Receiver<Result<Arc<AudioEngine>, String>>>,
    /// Heartbeat du thread UI (millis epoch SystemTime) + « l'UI est censée
    /// ticker » (lecture/enregistrement/init). Un thread `ui-watchdog` relaie
    /// un blocage prolongé vers les notifications freedesktop (panneau Quickshell).
    ui_heartbeat: Arc<AtomicU64>,
    ui_expect_ticks: Arc<AtomicBool>,
    /// Vrai quand l'UI est occupée à fond (lecture active etc.) — utilisé par le
    /// watchdog pour détailler la notif.
    ui_busy_label: Arc<std::sync::Mutex<String>>,
    /// One-shot de diagnostic : ms de sommeil à consommer dans update() pour
    /// simuler un gel UI (env STRUIDEL_DEBUG_STALL_MS).
    debug_ui_stall_ms: Arc<AtomicU64>,
    /// Rendus FX de clip en cours (bitcrush, gate, comp…) — chaque receveur
    /// rend une source dérivée installée comme `fx_source_file_id`.
    pending_clip_fx_renders: Vec<ClipFxRenderRx>,
    /// Révision par clip : un rendu fini plus vieux que la révision courante
    /// est jeté (l'utilisateur a continué de bouger les sliders).
    clip_fx_rev: HashMap<AudioClipId, u64>,
    /// API de contrôle locale (MCP struidel) : requêtes HTTP dépilées par frame.
    control: crate::control_api::ControlApi,
    /// Dernière publication du statut de contrôle (~4 Hz).
    control_status_at: Instant,
    /// État de scheduling partagé avec le thread `sched-tick` : quand le thread
    /// UI est gelé (fenêtre occluse — Wayland suspend les frame callbacks), ce
    /// worker prend le relais pour que l'audio et l'API continuent.
    sched: Arc<SchedShared>,
    /// CPS из последнего audio_window — для интерполяции времени при воспроизведении.
    last_cps: f64,
    /// Время из последнего ответа bridge.now() и момент получения — для плавной интерполяции.
    last_bridge_now: f64,
    last_bridge_instant: Instant,
    /// Сглаженное время для отрисовки таймлайна (курсор не дёргается).
    timeline_display_time: f64,
    /// После eval перезагрузить сэмплы (подхватить кэш от samples('github:...')).
    reload_samples_after_eval: bool,
    /// Время последней перезагрузки сэмплов (чтобы не спамить).
    last_samples_reload: Option<Instant>,
    /// Запланировать перезагрузку сэмплов после "Load samples" в это время.
    reload_samples_at: Option<Instant>,
    /// Неблокирующий запрос аудио: (from, to, reverse, приёмник). Ответ забираем в update().
    pending_audio: Option<(
        f64,
        f64,
        bool,
        mpsc::Receiver<Result<crate::strudel_bridge::AudioWindowResult, String>>,
    )>,
    /// После свежего запуска не двигать current_time, пока не получим первый ответ аудио-окна.
    /// Это устраняет «пустой первый цикл» и дрожание: пока node.js делает eval, время не бежит.
    awaiting_first_audio_after_start: bool,
    /// Состояние вида таймлайна (зум).
    timeline_view: ui::TimelineViewState,
    /// Последний запрос QueryArc для таймлайна (from, to, когда отправлен) — для троттлинга.
    last_timeline_query: Option<(f64, f64, Instant)>,
    /// Последний запрос QueryArc из редактора — для троттлинга (не чаще ~100 ms).
    last_editor_query: Option<Instant>,
    /// Отложенные действия по разметке (drop вкладки), применяются в начале отрисовки.
    pending_layout_actions: Vec<LayoutAction>,
    /// Плавная анимация подсветки зоны dnd (0..1).
    dnd_preview_alpha: f32,
    /// Текущая панель/тип зоны, для которой рисуем dnd-превью (для перезапуска анимации при смене зоны).
    dnd_preview_kind: Option<(Vec<usize>, DndPreviewKind)>,
    /// Текущий анимированный прямоугольник подсветки dnd (гладкое перетекание между зонами).
    dnd_preview_rect: Option<egui::Rect>,
    /// Путь панели, в которой сейчас идёт анимация dnd-превью.
    dnd_preview_panel: Option<Vec<usize>>,
    /// Какая вкладка сейчас перетаскивается (для dnd payload).
    dragging_tab: Option<ContentId>,
    /// Прямоугольник пункта "Open Recent" для отрисовки подменю справа при наведении.
    open_recent_submenu_rect: Option<egui::Rect>,
    /// Активный корень во вкладке «Samples»: 0 — основная папка, дальше — дополнительные.
    active_samples_root: usize,
    /// Идёт ли сейчас фоновая загрузка стандартных семпл-паков.
    samples_loading: bool,
    /// Время запуска последней загрузки семплов (для индикации прогресса).
    samples_loading_started: Option<Instant>,
    /// Классификация семплов по категориям (ударные, клавиши и т.д.) для таймлайна и браузера.
    sample_categories: crate::sample_categories::SampleCategories,
    /// Текст для вставки в редактор при следующем кадре (из браузера семплов: двойной клик / перетаскивание).
    pending_sample_insert: Option<String>,
    /// Échantillon disque dont la suppression est armée dans la palette Sons (1er clic).
    pending_sample_delete: Option<SampleKey>,
    /// Aperçus en cours dans la palette Sons : code du son → instant de fin.
    sound_previews: HashMap<String, Instant>,
    /// Panneau « Paramètres live » : appel `s(…)` choisi, paramètre choisi, appliquer à tous les sons.
    live_sound_sel: usize,
    live_param_sel: usize,
    live_scope: crate::live_params::LiveScope,
    live_type_sel: usize,
    /// Lancer le code de l'éditeur dès qu'un son inséré depuis la palette est appliqué.
    run_after_insert: bool,
    /// Destination d'un son ajouté depuis la palette : au curseur ou nouvelle partie (stack).
    sound_insert_mode: SoundInsertMode,
    sound_insert_controls: bool,
    /// Выбранная категория в браузере семплов (ключ категории или "all").
    sample_browser_category: String,
    /// Строка поиска в браузере семплов.
    sample_browser_search: String,
    /// Кэш списка семплов (полный список из движка), чтобы не держать lock и не пересчитывать каждый кадр.
    sample_browser_entries_cache: Option<Vec<(SampleKey, usize)>>,
    /// Отложенная загрузка кэша на следующий кадр (чтобы не блокировать при переключении на вкладку).
    sample_browser_cache_pending: bool,
    /// Кэш отфильтрованного/отсортированного списка (пересчитывается только при смене поиска, категории или кэша записей).
    sample_browser_filtered_rows: Option<Vec<(SampleKey, usize)>>,
    /// Пара (поиск, категория), для которой построен sample_browser_filtered_rows.
    sample_browser_filter_key: (String, String),
    /// Recherche dans la palette « Sons » (à côté de Mixer).
    sound_palette_search: String,
    /// Import de sons depuis un repo GitHub : URL saisie, état UI, canal thread.
    github_import_url: String,
    github_import_rx: Option<mpsc::Receiver<crate::github_samples::GithubImportEvent>>,
    github_import_state: GithubImportUi,
    /// Состояние отладчика (брейкпоинты, вотчпоинты, кэтчпоинты).
    debugger: ui::DebuggerState,
    /// Состояние транспорта (BPM, шаг, реверс, петля).
    transport_state: ui::TransportState,
    /// Последняя вкладка, с которой пользователь взаимодействовал (для auto playback source).
    playback_focus_content: ContentId,
    /// Централизованное состояние play/stop для examples в completion/doc popup.
    example_playback: ExamplePlaybackState,
    /// Снимок позиции основной шкалы до запуска example — для восстановления после остановки.
    pre_example_main_time: Option<f64>,
    /// Отдельное время воспроизведения example (не влияет на основную шкалу таймлайна).
    example_display_time: f64,
    /// DAW audio tracks, clips, takes, pool and edit history.
    audio_project: ProjectAudioState,
    /// Code-first synchronization between Strudel source, arrangement IR and DAW projection.
    sync_engine: SyncEngine,
    /// Parsed pattern document for the active `.strudel` editor tab.
    pattern_document: Option<PatternDocument>,
    /// Monotonic editor-buffer revision used to discard stale sync worker results.
    last_code_revision: u64,
    /// Input recording pipeline and writer thread.
    audio_recording: RecordingController,
    /// Cached waveform peaks for audio pool sources (avoid rereading WAV every frame).
    waveform_peaks: HashMap<u64, WaveformPeak>,
    /// Background finalize after Stop Rec (writer join + peak build).
    pending_recording_finalize: Option<(
        mpsc::Receiver<Result<RecordingFinalize, String>>,
        RecordingStopContext,
    )>,
    /// Export « FX voix » d'une prise vers la banque de samples (thread de fond).
    pending_voice_export: Option<mpsc::Receiver<Result<PathBuf, String>>>,
    /// Profil Bluetooth à restaurer après l'enregistrement (carte, profil d'origine).
    pending_bt_profile_restore: Option<(String, String)>,
    /// Editable audio timeline viewport.
    audio_timeline_view: ui::AudioTimelineViewState,
    /// Non-destructive clip renderer used by consolidate/bounce.
    clip_renderer: ClipRenderEngine,
    /// Реверсивное воспроизведение: двигаемся назад.
    reverse: bool,
    /// Предыдущее значение current_time (для детектирования пересечения брейкпоинтов).
    prev_time: f64,
    /// Количество кадров, в течение которых игнорировать ответ bridge.now() после seek.
    seek_ignore_now_frames: u32,
    /// Момент последнего reverse seek (чтобы не дергать JS-планировщик каждый кадр).
    last_reverse_seek_at: Instant,
    /// Последний beat метронома, по которому уже был сыгран клик.
    last_metronome_beat: Option<i64>,
    /// Фильтры и поиск для вкладки Problems.
    problems_show_errors: bool,
    problems_show_warnings: bool,
    problems_show_info: bool,
    problems_search: String,
    /// Диалог офлайн-экспорта в WAV.
    export_dialog: ExportDialogState,
    /// Линейная аранжировка паттернов.
    arrangement: PatternArrangement,
    /// Вид панели аранжировки.
    arrangement_view: ui::ArrangementViewState,
    /// Код, который был eval-нут из аранжировки в последний раз (для отображения).
    arrangement_active_code: String,
    /// Предыдущий set активных блоков (row_id+block_id), чтобы не eval каждый кадр.
    arrangement_last_active: Option<String>,
    /// Нативный движок для простых s("...")/sound("...") паттернов —
    /// синтезирует audio_window прямо в Rust и обходит node.js bridge,
    /// устраняя задержку первого цикла. Partagé avec le thread sched-tick.
    native_engine: Arc<std::sync::Mutex<crate::native_engine::NativeEngine>>,
    /// Visual Stage (browser Hydra) + WS event fan-out.
    visual_stage: VisualStage,
    /// Chain Seed panel state (NFT / on-chain → Strudel).
    chain_seed: crate::chain_seed::ChainSeedState,
    /// Background RPC fetch + generate progress/result.
    chain_seed_fetch_rx: Option<mpsc::Receiver<crate::chain_seed::ChainSeedEvent>>,
    /// Standalone Hydra sketch editor.
    hydra_studio: ui::HydraStudioState,
    /// Local Ollama → Strudel (separate from Chain Seed).
    ai_pattern: ui::AiPatternState,
    /// Onglet « Vue » : rythme / onde / spectre du son.
    sound_view: ui::SoundViewState,
    /// Background AI generate result.
    ai_pattern_rx: Option<mpsc::Receiver<Result<(String, u32, String), String>>>,
}

#[derive(Clone, Copy)]
struct RecordingStopContext {
    track_id: AudioTrackId,
    started_at_cycle: f64,
    /// Cycle where the user pressed Stop — frozen so Saving UI and clip length don't follow the playhead.
    frozen_end_cycle: f64,
}

#[derive(Default)]
struct ExportDialogState {
    open: bool,
    /// Длительность экспорта в циклах (строка из поля ввода).
    duration_cycles: String,
    /// Путь к выходному файлу.
    output_path: String,
    /// Текущий статус.
    status: ExportStatus,
    /// Приёмник результата из фонового потока.
    rx: Option<mpsc::Receiver<Result<(), String>>>,
}

#[derive(Default, PartialEq)]
enum ExportStatus {
    #[default]
    Idle,
    Running,
    Done,
    Error(String),
}

impl ExportDialogState {
    fn is_running(&self) -> bool {
        self.status == ExportStatus::Running
    }
}

/// Пункт верхнего меню (как в VS Code; без Selection, View, Go).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum TopMenuKind {
    File,
    Edit,
    Run,
    More,
}

/// Où atterrit un son ajouté depuis la palette « Sons ».
#[derive(Clone, Copy, PartialEq, Eq)]
enum SoundInsertMode {
    /// Insère `s("…")` à la position du curseur (ou fin de fichier).
    AtCursor,
    /// Ajoute le son comme nouvelle partie jouée en parallèle (`stack(…)`).
    NewPart,
}

/// État affiché de l'import GitHub dans l'onglet Sons.
#[derive(Clone, Debug, Default)]
enum GithubImportUi {
    #[default]
    Idle,
    Running { files: usize, bytes: u64 },
    Done { files: usize, bytes: u64 },
    Error(String),
}

/// Ondes de synthèse et synthés composés jouables par le moteur natif.
const SYNTH_SOUNDS: &[&str] = &[
    "sine", "sawtooth", "saw", "square", "triangle", "tri", "cosine", "pink", "white",
    "brown", "crackle",
    // Basses / leads / nappes techno
    "supersaw", "hoover", "pulse", "subbass", "fmsine",
];
/// Percussions synthétisées (type 808/909) — accordage interne, `.speed()` pour altérer.
const DRUM_SOUNDS: &[&str] =
    &["synthbd", "synthsd", "synthhh", "synthoh", "synthcp"];

/// Receveur d'un rendu FX de clip : (clip_id, révision, résultat WAV).
type ClipFxRenderRx =
    mpsc::Receiver<(AudioClipId, u64, Result<(PathBuf, u32, u16, u64), String>)>;

/// Millisecondes depuis l'epoch Unix (heartbeat du watchdog UI).
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Envoie une notification freedesktop — reçue par le NotificationServer de
/// Quickshell et affichée dans le panneau notifs de la sidebar droite.
/// Fire-and-forget : un helper thread récolte l'enfant pour éviter les zombies.
fn ui_watchdog_notify(urgency: &str, summary: &str, body: &str) {
    let spawn = std::process::Command::new("notify-send")
        .args([
            "-a",
            "struIDEl",
            "-u",
            urgency,
            "-i",
            "dialog-warning",
            summary,
            body,
        ])
        .spawn();
    if let Ok(mut child) = spawn {
        let _ = std::thread::Builder::new()
            .name("notify-reap".into())
            .spawn(move || {
                let _ = child.wait();
            });
    }
}

/// Suffixe de contrôles inline ajouté à un son inséré quand « contrôles » est
/// coché : widgets live directement dans le code — `slider(v,min,max)` donne
/// une piste dans l'éditeur, `toggler(v)` une case on/off (mute via `.mask`).
fn sound_controls_suffix(
    name: &str,
    bank: Option<&str>,
    categories: &crate::sample_categories::SampleCategories,
) -> String {
    let cat = bank
        .map(|b| categories.category_for_bank(b).key)
        .unwrap_or_default();
    let n = name.to_ascii_lowercase();
    let gain = if cat == "vocal" {
        ".gain(slider(1,0,3))"
    } else {
        ".gain(slider(0.8,0,1.5))"
    };
    let mid = if n.contains("bass") || n.starts_with("sub") || cat == "bass" {
        ".lpf(slider(900,80,4000))"
    } else if cat == "drums" || cat == "percussion" || DRUM_SOUNDS.contains(&name) {
        ".speed(slider(1,0.5,2))"
    } else if n.starts_with("gm_")
        || SYNTH_SOUNDS.contains(&name)
        || cat == "synth"
        || cat == "keys"
        || cat == "orchestral"
    {
        ".lpf(slider(3000,200,12000))"
    } else {
        ""
    };
    format!("{gain}{mid}.mask(toggler(1))")
}

/// Construit le snippet `s("nom")[.bank("b")][+ contrôles]` inséré par la palette.
fn insert_snippet(
    name: &str,
    bank: Option<&str>,
    controls_on: bool,
    categories: &crate::sample_categories::SampleCategories,
) -> String {
    let mut s = format!("s(\"{name}\")");
    if let Some(b) = bank {
        s.push_str(&format!(".bank(\"{b}\")"));
    }
    if controls_on {
        s.push_str(&sound_controls_suffix(name, bank, categories));
    }
    s
}

/// Index de la `)` qui ferme un `stack(…)` recouvrant tout le code, sinon None.
/// Ignore les parenthèses à l'intérieur des chaînes "…".
fn whole_stack_call(code: &str) -> Option<usize> {
    let t = code.trim();
    let after = t.strip_prefix("stack")?.trim_start();
    let open = t.len() - after.len();
    if after.as_bytes().first() != Some(&b'(') {
        return None;
    }
    let bytes = t.as_bytes();
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    for (i, &b) in bytes.iter().enumerate().skip(open) {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return t[i + 1..].trim().is_empty().then_some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// `expr` devient une nouvelle partie jouée en parallèle : ajoutée au `stack(…)`
/// existant, ou tout le code est emballé dans un `stack(…)` sinon.
fn extend_stack(code: &str, expr: &str) -> String {
    let t = code.trim();
    if t.is_empty() {
        return expr.to_string();
    }
    if let Some(close) = whole_stack_call(t) {
        format!("{},\n  {})", t[..close].trim_end(), expr)
    } else {
        let indented = t
            .lines()
            .map(|l| format!("  {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("stack(\n{},\n  {}\n)", indented, expr)
    }
}

/// Тип подсветки dnd-зоны внутри панели.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DndPreviewKind {
    Center,
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone)]
enum LayoutAction {
    /// Добавить вкладку в панель по пути.
    AddTabToPanel {
        content_id: ContentId,
        path: Vec<usize>,
    },
    /// Создать сплит на месте панели по пути.
    SplitPanel {
        path: Vec<usize>,
        horizontal: bool,
        content_id: ContentId,
        new_panel_first: bool,
    },
}

impl StruidelApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut visuals = egui::Visuals::dark();
        // Окно и панели заливаем одним и тем же базовым тёмным цветом,
        // чтобы панели полностью перекрывали друг друга и не «просвечивали».
        let base = visuals.extreme_bg_color;
        visuals.window_fill = base;
        visuals.panel_fill = base;
        // Лейблы, заголовки и прочие неинтерактивные элементы — без отдельной серой подложки.
        visuals.widgets.noninteractive.bg_fill = egui::Color32::TRANSPARENT;
        visuals.widgets.inactive.bg_fill = egui::Color32::TRANSPARENT;
        cc.egui_ctx.set_visuals(visuals);
        // Полоса прокрутки: тонкая, всегда слегка видимая и без изменения толщины при наведении.
        // Используем плавающий стиль с небольшой выделенной шириной и ненулевой
        // непрозрачностью dormant/active, чтобы скроллбар был заметен даже без ховера.
        cc.egui_ctx.all_styles_mut(|style| {
            let mut scroll = style.spacing.scroll;
            scroll.floating = true;
            scroll.bar_width = 6.0;
            scroll.floating_allocated_width = 4.0;
            scroll.dormant_background_opacity = 0.5;
            scroll.dormant_handle_opacity = 0.5;
            scroll.active_background_opacity = 0.8;
            scroll.active_handle_opacity = 0.8;
            scroll.interact_background_opacity = 1.0;
            scroll.interact_handle_opacity = 1.0;
            style.spacing.scroll = scroll;
        });
        let settings = settings::load_settings();
        let eth_rpc_url = settings.eth_rpc_url.clone();
        let ollama_url = settings.ollama_url.clone();
        let ollama_model = settings.ollama_model.clone();
        let recent = settings::load_recent();
        if let Err(e) = crate::paths::ensure_strudel_runtime() {
            eprintln!("[StruidelApp] Strudel runtime: {e}");
        }
        let doc_store = doc_json::default_doc_json_path().and_then(|p| {
            #[cfg(debug_assertions)]
            eprintln!("[StruidelApp] default_doc_json_path = {:?}", p);
            doc_json::DocStore::load(&p).ok()
        });
        #[cfg(debug_assertions)]
        {
            if let Some(ref store) = doc_store {
                eprintln!(
                    "[StruidelApp] DocStore loaded with {} labels",
                    store.all_labels().len()
                );
            } else {
                eprintln!("[StruidelApp] DocStore is None");
            }
        }
        // Init audio hors du thread UI : l'énumération ALSA + l'ouverture du
        // device (Bluetooth/PipeWire inclus) peut prendre plusieurs secondes.
        let pending_audio_engine = if settings.audio_enabled {
            let (tx, rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("audio-init".to_string())
                .spawn(move || {
                    let _ = tx.send(AudioEngine::new().map(Arc::new));
                })
                .ok()
                .map(|_| rx)
        } else {
            None
        };
        let audio_project = ProjectAudioState::default();
        let mut sync_engine = SyncEngine::default();
        sync_engine.set_project_audio_state(&audio_project);
        let bridge = StrudelBridge::default();
        // sched-tick : reprend le scheduling audio quand le thread UI est gelé
        // (fenêtre occluse → Wayland suspend les frame callbacks → update() ne
        // tourne plus → sans ce worker, l'audio meurt ~1 s après avoir caché
        // la fenêtre). L'UI écrit son état dans `sched` à chaque frame ; le
        // worker n'agit QUE si le heartbeat UI est mort depuis >1,6 s.
        let sched_shared = Arc::new(SchedShared::new());
        let native_engine = Arc::new(std::sync::Mutex::new(
            crate::native_engine::NativeEngine::new(),
        ));

        // Watchdog « ne répond plus » : si le thread UI cesse de ticker alors
        // qu'il est censé le faire (lecture, enregistrement, init audio…),
        // on relaie l'ANR vers les notifications freedesktop → elles atterrissent
        // dans le panneau notifs de Quickshell avec le programme et le détail.
        let ui_heartbeat = Arc::new(AtomicU64::new(now_millis()));
        let ui_expect_ticks = Arc::new(AtomicBool::new(false));
        let ui_busy_label = Arc::new(std::sync::Mutex::new(String::new()));
        // Diagnostic : STRUIDEL_DEBUG_STALL_MS=<ms> fait dormir une fois le
        // thread UI dans update() pour reproduire un gel et tester le watchdog.
        let debug_ui_stall_ms = Arc::new(AtomicU64::new(
            std::env::var("STRUIDEL_DEBUG_STALL_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0),
        ));
        {
            let beat = ui_heartbeat.clone();
            let expect = ui_expect_ticks.clone();
            let label = ui_busy_label.clone();
            let _ = std::thread::Builder::new()
                .name("ui-watchdog".into())
                .spawn(move || {
                    let mut stalled = false;
                    loop {
                        thread::sleep(Duration::from_millis(500));
                        let age = now_millis().saturating_sub(beat.load(Ordering::Relaxed));
                        if !expect.load(Ordering::Relaxed) {
                            // UI parquée en attente d'événements = sain (répond aux pings).
                            stalled = false;
                            continue;
                        }
                        if !stalled && age > 4_000 {
                            stalled = true;
                            let what = label
                                .lock()
                                .map(|s| s.clone())
                                .unwrap_or_default();
                            let detail = if what.is_empty() {
                                format!("UI bloquée depuis {} s", age / 1000)
                            } else {
                                format!("UI bloquée depuis {} s ({})", age / 1000, what)
                            };
                            ui_watchdog_notify(
                                "critical",
                                "« struidel » ne répond plus",
                                &format!("{}\nVérifiez audio/Bluetooth — le thread UI est saturé.", detail),
                            );
                        } else if stalled && age <= 4_000 {
                            stalled = false;
                            ui_watchdog_notify(
                                "normal",
                                "« struidel » répond à nouveau",
                                &format!("Récupéré après un blocage UI d'environ {} s.", age.max(1) / 1000),
                            );
                        }
                    }
                });
        }
        {
            // sched-tick : reprend le scheduling audio quand le thread UI est
            // gelé (fenêtre occluse — Wayland suspend les frame callbacks et
            // update() ne tourne plus). L'UI écrit son état dans `sched` à
            // chaque frame ; le worker n'agit QUE si le heartbeat est mort
            // depuis >1,6 s. Reverse ignoré : cas rare, l'audio s'arrête
            // simplement au lieu d'être mal schedulé.
            let sched = sched_shared.clone();
            let beat = ui_heartbeat.clone();
            let native = native_engine.clone();
            let bridge_tx = bridge.command_sender();
            let _ = std::thread::Builder::new()
                .name("sched-tick".into())
                .spawn(move || loop {
                    thread::sleep(Duration::from_millis(90));
                    let age = now_millis().saturating_sub(beat.load(Ordering::Relaxed));
                    if age < 1600
                        || !sched.playing.load(Ordering::Relaxed)
                        || sched.paused.load(Ordering::Relaxed)
                        || sched.reverse.load(Ordering::Relaxed)
                        || !sched.audio_enabled.load(Ordering::Relaxed)
                    {
                        continue;
                    }
                    let from = sched.current_estimate();
                    let to = from + 0.5;
                    let scheduled_up_to = sched.scheduled_to();
                    if to <= scheduled_up_to || (scheduled_up_to - from) > 1.5 {
                        continue;
                    }
                    let pure = native.lock().map(|e| e.is_pure()).unwrap_or(false);
                    let result = if pure {
                        native.lock().map(|e| e.query_audio_window(from, to)).ok()
                    } else {
                        bridge_tx
                            .as_ref()
                            .and_then(|tx| tx.audio_window(from, to, Duration::from_millis(500)).ok())
                    };
                    let Some(result) = result else { continue };
                    eprintln!(
                        "[sched-tick] relais gel : {} événements [{from:.2}→{to:.2}]",
                        result.events.len()
                    );
                    sched.set_scheduled_to(to);
                    SchedShared::set_f64(&sched.cps, result.cps);
                    let engine = sched.engine.lock().ok().and_then(|e| e.clone());
                    let Some(engine) = engine else { continue };
                    let audio_gain = SchedShared::get_f64(&sched.audio_gain);
                    let ref_cycle = sched.current_estimate();
                    for ev in result.events {
                        if ev.begin < scheduled_up_to {
                            continue;
                        }
                        let gain = (ev.gain.unwrap_or(1.0) * audio_gain).clamp(0.0, 1.0);
                        engine.schedule_event(AudioEvent {
                            start_cycle: ev.begin - ref_cycle,
                            end_cycle: ev.end - ref_cycle,
                            midi_note: ev.midi,
                            gain: Some(gain),
                            cps: result.cps,
                            s: ev.s,
                            bank: ev.bank,
                            n: ev.n,
                            pan: ev.pan,
                            attack: ev.attack,
                            decay: ev.decay,
                            sustain: ev.sustain,
                            release: ev.release,
                            room: ev.room,
                            shape: ev.shape,
                            cut: ev.cut,
                            speed: ev.speed,
                            cutoff: ev.cutoff,
                            hcutoff: ev.hcutoff,
                            coarse: ev.coarse,
                            crush: ev.crush,
                            compressor: ev.compressor,
                            compressor_ratio: ev.compressor_ratio,
                            penv: ev.penv,
                            pattack: ev.pattack,
                            prelease: ev.prelease,
                            epoch: ev.epoch,
                        });
                    }
                });
        }

        Self {
            bridge,
            root_layout: LayoutNode::default_layout(),
            editor_tabs: Vec::new(),
            active_editor_index: 0,
            file_tree: FileTree::default(),
            samples_file_tree: FileTree::default(),
            root_folder: None,
            recent,
            settings,
            doc_store,
            doc_lookup: String::new(),
            current_time: 0.0,
            playing: false,
            paused: false,
            open_top_menu: None,
            menu_dropdown_anchor: None,
            save_as_requested: false,
            audio: None,
            pending_audio_engine,
            ui_heartbeat: ui_heartbeat.clone(),
            ui_expect_ticks,
            ui_busy_label,
            debug_ui_stall_ms,
            pending_clip_fx_renders: Vec::new(),
            clip_fx_rev: HashMap::new(),
            control: {
                let api = crate::control_api::ControlApi::start();
                api.set_heartbeat(ui_heartbeat.clone());
                api
            },
            control_status_at: Instant::now() - Duration::from_secs(1),
            sched: sched_shared.clone(),
            last_cps: 1.0,
            last_bridge_now: 0.0,
            last_bridge_instant: Instant::now(),
            timeline_display_time: 0.0,
            reload_samples_after_eval: false,
            last_samples_reload: None,
            reload_samples_at: None,
            pending_audio: None,
            awaiting_first_audio_after_start: false,
            timeline_view: ui::TimelineViewState::default(),
            last_timeline_query: None,
            last_editor_query: None,
            pending_layout_actions: Vec::new(),
            dnd_preview_alpha: 0.0,
            dnd_preview_kind: None,
            dnd_preview_rect: None,
            dnd_preview_panel: None,
            dragging_tab: None,
            open_recent_submenu_rect: None,
            active_samples_root: 0,
            samples_loading: false,
            samples_loading_started: None,
            sample_categories: crate::sample_categories::SampleCategories::new(),
            pending_sample_insert: None,
            pending_sample_delete: None,
            sound_previews: HashMap::new(),
            live_sound_sel: 0,
            live_param_sel: 0,
            live_scope: crate::live_params::LiveScope::Sound,
            live_type_sel: 0,
            run_after_insert: false,
            sound_insert_mode: SoundInsertMode::NewPart,
            sound_insert_controls: true,
            sample_browser_category: "all".to_string(),
            sample_browser_search: String::new(),
            sample_browser_entries_cache: None,
            sample_browser_cache_pending: false,
            sample_browser_filtered_rows: None,
            sound_palette_search: String::new(),
            github_import_url: String::new(),
            github_import_rx: None,
            github_import_state: GithubImportUi::Idle,
            sample_browser_filter_key: (String::new(), "all".to_string()),
            debugger: ui::DebuggerState::default(),
            transport_state: ui::TransportState::default(),
            playback_focus_content: ContentId::Workspace,
            example_playback: ExamplePlaybackState::default(),
            pre_example_main_time: None,
            example_display_time: 0.0,
            audio_project,
            sync_engine,
            pattern_document: None,
            last_code_revision: 0,
            audio_recording: RecordingController::default(),
            waveform_peaks: HashMap::new(),
            pending_recording_finalize: None,
            pending_voice_export: None,
            pending_bt_profile_restore: None,
            audio_timeline_view: ui::AudioTimelineViewState::default(),
            clip_renderer: ClipRenderEngine::default(),
            reverse: false,
            prev_time: 0.0,
            seek_ignore_now_frames: 0,
            last_reverse_seek_at: Instant::now(),
            last_metronome_beat: None,
            problems_show_errors: true,
            problems_show_warnings: true,
            problems_show_info: true,
            problems_search: String::new(),
            export_dialog: ExportDialogState::default(),
            arrangement: PatternArrangement::default(),
            arrangement_view: ui::ArrangementViewState::default(),
            arrangement_active_code: String::new(),
            arrangement_last_active: None,
            native_engine: native_engine.clone(),
            visual_stage: {
                let vs = VisualStage::new();
                vs.ensure_running();
                vs
            },
            chain_seed: {
                let mut cs = crate::chain_seed::ChainSeedState::default();
                cs.rpc_url = eth_rpc_url;
                cs.ollama_url = ollama_url.clone();
                cs.ollama_model = ollama_model.clone();
                cs.gen_mode = crate::chain_seed::GenMode::Procedural;
                cs
            },
            chain_seed_fetch_rx: None,
            hydra_studio: ui::HydraStudioState::default(),
            ai_pattern: {
                let mut ai = ui::AiPatternState::default();
                ai.ollama_url = ollama_url;
                ai.ollama_model = ollama_model;
                ai
            },
            sound_view: ui::SoundViewState::new(),
            ai_pattern_rx: None,
        }
    }

    fn close_top_menu(&mut self) {
        self.open_top_menu = None;
        self.menu_dropdown_anchor = None;
        self.open_recent_submenu_rect = None;
    }

    fn apply_pending_layout_actions(&mut self) {
        for action in self.pending_layout_actions.drain(..) {
            match action {
                LayoutAction::AddTabToPanel { content_id, path } => {
                    self.root_layout.remove_content(content_id);
                    if let Some(node) = self.root_layout.node_at_path_mut(&path) {
                        if let LayoutNode::Panel(data) = node {
                            data.add_content(content_id);
                            if let Some(pos) = data.tabs.iter().position(|&c| c == content_id) {
                                data.active = pos;
                            }
                        }
                    }
                    // После перемещения вкладки схлопываем пустые панели, когда путь уже отработал.
                    self.root_layout.collapse_empty_splits();
                }
                LayoutAction::SplitPanel {
                    path,
                    horizontal,
                    content_id,
                    new_panel_first,
                } => {
                    self.root_layout.remove_content(content_id);
                    // Сначала выполняем сплит по исходному пути,
                    // а уже потом схлопываем пустые панели, чтобы путь не «съезжал».
                    let _ = self.root_layout.split_at_path(
                        &path,
                        horizontal,
                        content_id,
                        0.5,
                        new_panel_first,
                    );
                    self.root_layout.collapse_empty_splits();
                }
            }
        }
    }

    fn active_editor_mut(&mut self) -> Option<&mut EditorTab> {
        self.editor_tabs.get_mut(self.active_editor_index)
    }

    fn active_editor(&self) -> Option<&EditorTab> {
        self.editor_tabs.get(self.active_editor_index)
    }

    fn arrangement_source_options(&self) -> Vec<ui::ArrangementSourceOption> {
        let mut out = Vec::new();
        for tab in &self.editor_tabs {
            if let Some(path) = &tab.file_path {
                let path_str = path.to_string_lossy().to_string();
                let label = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|name| format!("{name} ({path_str})"))
                    .unwrap_or(path_str.clone());
                out.push(ui::ArrangementSourceOption {
                    source_file: Some(path_str.clone()),
                    source_pattern_id: None,
                    label: format!("{label} / full file"),
                });
                let parsed = parse_pattern_document(tab.code.clone(), 0);
                for node in parsed.ir.patterns {
                    out.push(ui::ArrangementSourceOption {
                        source_file: Some(path_str.clone()),
                        source_pattern_id: Some(node.id.as_str().to_string()),
                        label: format!("{label} / pattern {}", node.id.as_str()),
                    });
                }
            }
        }
        out
    }

    fn resolve_arrangement_source_code(
        &self,
        path: &str,
        pattern_id: Option<&str>,
    ) -> Option<String> {
        let mut code = None;
        for tab in &self.editor_tabs {
            if let Some(tab_path) = &tab.file_path {
                if tab_path.to_string_lossy() == path {
                    code = Some(tab.code.clone());
                    break;
                }
            }
        }
        let code = code.or_else(|| std::fs::read_to_string(path).ok())?;
        let Some(pattern_id) = pattern_id else {
            return Some(code);
        };
        let parsed = parse_pattern_document(code.clone(), 0);
        let expr = parsed
            .ast
            .expressions
            .iter()
            .find(|expr| expr.id.as_str() == pattern_id)?;
        if expr.source_span.start <= expr.source_span.end && expr.source_span.end <= code.len() {
            Some(code[expr.source_span.start..expr.source_span.end].to_string())
        } else {
            None
        }
    }

    fn refresh_active_pattern_document(&mut self) {
        let Some(editor) = self.active_editor() else {
            self.pattern_document = None;
            self.refresh_native_engine();
            return;
        };
        let code = editor.code.clone();
        self.pattern_document = Some(parse_pattern_document(code, self.last_code_revision));
        self.refresh_native_engine();
    }

    fn add_to_recent(&mut self, path: PathBuf, kind: &str) {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let path_str = path.to_string_lossy().to_string();
        self.recent.retain(|r| r.path != path_str);
        self.recent.insert(
            0,
            RecentItem {
                path: path_str,
                kind: kind.to_string(),
                name,
            },
        );
        self.recent.truncate(10);
        settings::save_recent(&self.recent);
    }

    fn new_file(&mut self) {
        let doc = self.doc_store.as_ref();
        let tab = EditorTab::new(None, String::new(), doc);
        self.editor_tabs.push(tab);
        self.active_editor_index = self.editor_tabs.len().saturating_sub(1);
        self.refresh_active_pattern_document();
    }

    fn open_file_in_tab(&mut self, path: PathBuf, contents: String) {
        let doc = self.doc_store.as_ref();
        let tab = EditorTab::new(Some(path.clone()), contents, doc);
        self.add_to_recent(path, "file");
        self.editor_tabs.push(tab);
        self.active_editor_index = self.editor_tabs.len().saturating_sub(1);
        self.refresh_active_pattern_document();
    }

    fn open_folder(&mut self, path: PathBuf) {
        self.root_folder = Some(path.clone());
        self.file_tree.set_root(Some(path.clone()));
        self.add_to_recent(path, "folder");
    }

    fn save_file(&mut self) -> bool {
        let Some(editor) = self.active_editor_mut() else {
            return false;
        };
        let path = match &editor.file_path {
            Some(p) => p.clone(),
            None => return false, // need Save As
        };
        if std::fs::write(&path, &editor.code).is_ok() {
            self.add_to_recent(path, "file");
            true
        } else {
            false
        }
    }

    fn save_file_as(&mut self, path: PathBuf) -> bool {
        let Some(editor) = self.active_editor_mut() else {
            return false;
        };
        if std::fs::write(&path, &editor.code).is_ok() {
            editor.file_path = Some(path.clone());
            self.add_to_recent(path, "file");
            true
        } else {
            false
        }
    }

    /// Центральная область редактора: полоска вкладок (как у панелей) и код на всю область.
    fn editor_workspace_ui(&mut self, ui: &mut egui::Ui) {
        let full = ui.available_rect_before_wrap();
        ui.allocate_exact_size(full.size(), egui::Sense::hover());
        const TAB_STRIP_H: f32 = 24.0;
        const SEP_H: f32 = 2.0;
        let tabs_rect = egui::Rect::from_min_size(full.min, egui::vec2(full.width(), TAB_STRIP_H));
        let content_rect = egui::Rect::from_min_max(
            egui::pos2(full.min.x, full.min.y + TAB_STRIP_H + SEP_H),
            full.max,
        );

        if self.editor_tabs.is_empty() {
            ui.allocate_ui_at_rect(full, |ui| {
                ui.label("Aucun fichier ouvert. File → New File ou Open File…");
                if ui.button("New File").clicked() {
                    self.new_file();
                }
            });
            return;
        }

        // Полоска вкладок файлов — оформление как у обычных вкладок панели (без отступов).
        let mut tab_to_close = None;
        let mut tab_to_activate = None;
        ui.allocate_ui_at_rect(tabs_rect, |ui| {
            let bg = ui.visuals().window_fill();
            ui.painter().rect_filled(tabs_rect, 0.0, bg);
            // Немного поднимаем область рисования скроллбара, чтобы горизонтальная
            // полоска прокрутки вкладок визуально примыкала к ним, а не «висела» ниже.
            let bar_h = 6.0;
            let bar_rect = egui::Rect::from_min_max(
                egui::pos2(tabs_rect.min.x, tabs_rect.max.y - bar_h),
                egui::pos2(tabs_rect.max.x, tabs_rect.max.y),
            );
            egui::ScrollArea::horizontal()
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                .scroll_bar_rect(bar_rect)
                .id_salt(egui::Id::new("editor_tabs_scroll"))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let active_bg = egui::Color32::from_white_alpha(12);
                        let hover_bg = egui::Color32::from_white_alpha(8);
                        let text_active = egui::Color32::from_gray(220);
                        let text_inactive = egui::Color32::from_gray(140);
                        let separator_color = egui::Color32::from_white_alpha(16);

                        for (i, tab) in self.editor_tabs.iter().enumerate() {
                            let selected = i == self.active_editor_index;
                            let title = tab.title();
                            let text_width = ui
                                .painter()
                                .layout_no_wrap(
                                    title.to_string(),
                                    egui::FontId::proportional(13.0),
                                    egui::Color32::WHITE,
                                )
                                .size()
                                .x;
                            let tab_w = text_width + 24.0 + 20.0; // текст + отступ + крестик
                            let tab_size = egui::vec2(tab_w, TAB_STRIP_H);
                            let (rect, resp) =
                                ui.allocate_exact_size(tab_size, egui::Sense::click_and_drag());

                            if selected {
                                ui.painter().rect_filled(rect, 0.0, active_bg);
                            } else if resp.hovered() {
                                ui.painter().rect_filled(rect, 0.0, hover_bg);
                            }

                            let text_color = if selected { text_active } else { text_inactive };
                            ui.painter().text(
                                egui::pos2(rect.min.x + 12.0, rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                title,
                                egui::FontId::proportional(13.0),
                                text_color,
                            );

                            let close_rect = egui::Rect::from_min_max(
                                egui::pos2(rect.max.x - 20.0, rect.min.y),
                                rect.max,
                            );
                            let close_resp = ui.interact(
                                close_rect,
                                ui.id().with(i).with("close"),
                                egui::Sense::click(),
                            );
                            if close_resp.hovered() {
                                ui.painter().rect_filled(close_rect, 0.0, hover_bg);
                            }
                            ui.painter().text(
                                close_rect.center(),
                                egui::Align2::CENTER_CENTER,
                                "✕",
                                egui::FontId::proportional(12.0),
                                text_inactive,
                            );
                            if close_resp.clicked() {
                                tab_to_close = Some(i);
                            } else if resp.clicked() {
                                tab_to_activate = Some(i);
                            }

                            if i + 1 < self.editor_tabs.len() {
                                let sep_x = rect.max.x;
                                ui.painter().line_segment(
                                    [
                                        egui::pos2(sep_x, rect.min.y + 5.0),
                                        egui::pos2(sep_x, rect.max.y - 5.0),
                                    ],
                                    egui::Stroke::new(1.0, separator_color),
                                );
                            }
                        }
                    });
                });
        });

        let sep_y = tabs_rect.max.y;
        ui.painter().line_segment(
            [egui::pos2(full.min.x, sep_y), egui::pos2(full.max.x, sep_y)],
            egui::Stroke::new(1.0, egui::Color32::from_white_alpha(16)),
        );

        if let Some(i) = tab_to_activate {
            self.active_editor_index = i;
        }
        if let Some(i) = tab_to_close {
            self.close_tab(i);
            if self.editor_tabs.is_empty() {
                return;
            }
        }

        // Редактор кода на всю оставшуюся область.
        if content_rect.width() > 0.0 && content_rect.height() > 0.0 {
            let idx = self.active_editor_index;
            let isolate_example_playback = self.example_playback.is_active();
            if self.playing && !isolate_example_playback {
                let now_inst = Instant::now();
                let should_query = self
                    .last_editor_query
                    .map(|prev| now_inst.duration_since(prev).as_millis() >= 100)
                    .unwrap_or(true);
                if should_query {
                    let t = self.timeline_display_time;
                    self.bridge.request_query((t - 2.0).max(0.0), t + 4.0);
                    self.last_editor_query = Some(now_inst);
                }
            }
            let playing_events: Vec<(String, f64)> = if self.playing && !isolate_example_playback {
                let t = self.timeline_display_time;
                self.bridge
                    .query_arc(0.0, 0.0)
                    .into_iter()
                    .filter(|e| e.begin <= t && t < e.end)
                    .filter_map(|e| e.s.map(|name| (name, e.begin.fract())))
                    .collect()
            } else {
                Vec::new()
            };
            let example_playing_events: Vec<ActiveSampleEvent> =
                if self.playing && isolate_example_playback {
                    let now_inst = Instant::now();
                    let should_query = self
                        .last_editor_query
                        .map(|prev| now_inst.duration_since(prev).as_millis() >= 100)
                        .unwrap_or(true);
                    if should_query {
                        // Используем отдельное время example вместо timeline_display_time,
                        // чтобы курсор основной шкалы не дёргался во время проигрывания примера.
                        let t = self.example_display_time;
                        self.bridge.request_query((t - 1.0).max(0.0), t + 2.0);
                        self.last_editor_query = Some(now_inst);
                    }
                    let t = self.example_display_time;
                    self.bridge
                        .query_arc(0.0, 0.0)
                        .into_iter()
                        .filter(|e| e.begin <= t && t < e.end)
                        .filter_map(|e| {
                            e.s.map(|name| ActiveSampleEvent {
                                s: name,
                                begin: e.begin,
                                end: e.end,
                                n: e.n,
                            })
                        })
                        .collect()
                } else {
                    Vec::new()
                };
            let insert_text = self.pending_sample_insert.take();
            let run_after_insert = insert_text.is_some() && std::mem::take(&mut self.run_after_insert);
            let mut sync_code_after: Option<String> = None;
            let mut editor_debug_actions = Vec::new();
            ui.allocate_ui_at_rect(content_rect, |ui| {
                if let Some(editor) = self.editor_tabs.get_mut(idx) {
                    let before = editor.code.clone();
                    let visuals_data = crate::code_visuals::VisualsData {
                        spectrum: self.audio.as_ref().map(|a| a.spectrum_tap()),
                        doc: self.pattern_document.as_ref(),
                        current_cycle: self.current_time,
                        playing: self.playing,
                        sample_rate_hz: self
                            .audio
                            .as_ref()
                            .map(|a| a.sample_rate())
                            .unwrap_or(48_000) as f32,
                    };
                    let actions = editor.ui(
                        ui,
                        &self.settings,
                        self.doc_store.as_ref(),
                        &playing_events[..],
                        insert_text,
                        Some(&self.debugger),
                        &self.example_playback,
                        &example_playing_events,
                        Some(&visuals_data),
                    );
                    editor_debug_actions.extend(actions);
                    if editor.code != before {
                        sync_code_after = Some(editor.code.clone());
                    }
                }
            });
            for action in editor_debug_actions {
                self.handle_editor_debug_action(action);
            }
            if let Some(code) = sync_code_after {
                self.last_code_revision = self.last_code_revision.saturating_add(1);
                self.sync_engine
                    .set_code(code.clone(), self.last_code_revision);
                self.pattern_document = Some(parse_pattern_document(code, self.last_code_revision));
                self.refresh_native_engine();
            }
            if run_after_insert {
                self.run_editor_code();
            }
        }
    }

    fn reset_example_playback(&mut self) {
        self.example_playback.clear();
        self.bridge.clear_events_cache();
    }

    /// Обновить кэш нативного движка из текущего pattern_document и BPM.
    /// Зови после любого обновления документа или темпа,
    /// чтобы is_pure()/query_audio_window() были в синхроне.
    fn refresh_native_engine(&mut self) {
        let bpm = self.transport_state.bpm;
        if let Some(doc) = self.pattern_document.as_ref() {
            let pure = self.native_engine.lock().map(|mut e| e.update(doc, bpm)).unwrap_or(false);
            // Прогреваем кэш сэмплов в фоне: без этого первый schedule_event для нового
            // сэмпла выполнит синхронный load_wav_sample в UI thread, voice опоздает
            // в render thread и первый удар «съедается» по t > end_time.
            if pure {
                if let Some(engine) = &self.audio {
                    let requests = self.native_engine.lock().map(|e| e.sample_requests()).unwrap_or_default();
                    engine.prewarm_samples(requests);
                }
            }
        } else {
            // Нет документа — принудительно сбрасываем нативный путь.
            let empty = crate::pattern_ir::PatternDocument {
                revision: 0,
                code: String::new(),
                ast: crate::pattern_ir::StrudelAst::default(),
                ir: crate::pattern_ir::PatternIr::default(),
                source_map: crate::pattern_ir::SourceMap::default(),
                diagnostics: Vec::new(),
            };
            if let Ok(mut e) = self.native_engine.lock() { let _ = e.update(&empty, bpm); }
        }
    }

    fn stop_active_example_cleanup(&mut self) -> bool {
        if !self.example_playback.is_active() {
            return false;
        }
        let restore = self.pre_example_main_time.take();
        let _ = self.bridge.stop();
        self.reset_example_playback();
        if let Some(engine) = &self.audio {
            engine.clear();
        }
        self.pending_audio = None;
        self.sched.set_scheduled_to(-1.0);
        self.example_display_time = 0.0;
        // Восстанавливаем позицию основной шкалы, которую заморозили на момент запуска example.
        if let Some(t) = restore {
            let _ = self.bridge.seek(t);
            self.current_time = t;
            self.prev_time = t;
            self.timeline_display_time = t;
            self.last_bridge_now = t;
            self.last_bridge_instant = Instant::now();
            self.seek_ignore_now_frames = 5;
        }
        true
    }

    fn run_editor_code(&mut self) {
        self.reset_example_playback();
        if let Some(editor) = self.active_editor() {
            let code = editor.code.clone();
            if code != self.sync_engine.state.code {
                self.last_code_revision = self.last_code_revision.saturating_add(1);
                self.sync_engine
                    .set_code(code.clone(), self.last_code_revision);
                self.pattern_document = Some(parse_pattern_document(
                    code.clone(),
                    self.last_code_revision,
                ));
                self.refresh_native_engine();
            }
            // Сбросить позицию в JS перед eval — иначе _pausedTime от предыдущей паузы
            // заставит новый запуск начаться не с 0
            let _ = self.bridge.seek(0.0);
            self.seek_ignore_now_frames = 5;
            // Сбросить временные опорные точки, иначе интерполяция времени
            // в update() выдаст огромный current_time от старого last_bridge_instant,
            // и первый цикл будет пропущен / звук пойдёт с задержкой.
            self.current_time = 0.0;
            self.prev_time = 0.0;
            self.timeline_display_time = 0.0;
            self.last_bridge_now = 0.0;
            self.last_bridge_instant = Instant::now();
            self.last_cps = (self.transport_state.bpm / 60.0).max(0.000_001);
            self.awaiting_first_audio_after_start = true;
            if let Err(e) = self.bridge.evaluate(&code) {
                if let Some(ed) = self.active_editor_mut() {
                    ed.eval_error = Some(e);
                    ed.eval_error_line = None;
                    ed.eval_error_column = None;
                }
            } else {
                self.playing = true;
                self.paused = false;
                self.sched.set_scheduled_to(-1.0);
                self.reload_samples_after_eval = true;
                if let Some(ed) = self.active_editor_mut() {
                    ed.eval_error = None;
                    ed.eval_error_line = None;
                    ed.eval_error_column = None;
                    ed.flash_until = Some(Instant::now() + Duration::from_millis(200));
                }
                // Гарантируем, что AudioEngine создан — нативный путь без него не работает.
                // Init async : l'ouverture du device peut prendre des secondes.
                if self.settings.audio_enabled && self.audio.is_none() {
                    self.request_audio_engine(0);
                }
                // Синхронный прогрев сэмплов ДО первого schedule_audio_window:
                // первый удар иначе будет ждать фоновый prewarm и «съедаться» retain'ом.
                if let Some(engine) = &self.audio {
                    let requests = self.native_engine.lock().map(|e| e.sample_requests()).unwrap_or_default();
                    if !requests.is_empty() {
                        engine.prewarm_samples_blocking(requests);
                    }
                }
            }
        }
    }

    fn effective_playback_source(&self) -> ui::PlaybackSource {
        if self.transport_state.playback_source_locked {
            self.transport_state.playback_source
        } else if self.playback_focus_content == ContentId::Arrangement {
            ui::PlaybackSource::Arrangement
        } else {
            ui::PlaybackSource::Editor
        }
    }

    fn sync_playback_source_from_active_tab(&mut self) {
        if self.transport_state.playback_source_locked {
            return;
        }
        self.transport_state.playback_source =
            if self.playback_focus_content == ContentId::Arrangement {
                ui::PlaybackSource::Arrangement
            } else {
                ui::PlaybackSource::Editor
            };
    }

    fn arrangement_playback_selected(&self) -> bool {
        self.effective_playback_source() == ui::PlaybackSource::Arrangement
    }

    fn run_arrangement_code(&mut self) {
        self.reset_example_playback();
        let _ = self.bridge.seek(0.0);
        self.seek_ignore_now_frames = 5;
        self.current_time = 0.0;
        self.prev_time = 0.0;
        self.timeline_display_time = 0.0;
        self.last_bridge_now = 0.0;
        self.last_bridge_instant = Instant::now();
        self.last_cps = (self.transport_state.bpm / 60.0).max(0.000_001);
        self.awaiting_first_audio_after_start = true;
        self.arrangement_last_active = None;
        let code = self
            .arrangement
            .active_code_at_with(0.0, |path, pattern_id| {
                self.resolve_arrangement_source_code(path, pattern_id)
            });
        if let Some(code) = code {
            let _ = self.bridge.evaluate(&code);
            self.arrangement_active_code = code;
        } else {
            self.arrangement_active_code.clear();
        }
        self.playing = true;
        self.paused = false;
        self.sched.set_scheduled_to(-1.0);
        self.reload_samples_after_eval = true;
    }

    /// Забирает ответ по запросу аудио-окна (если есть) и планирует события. Не блокирует.
    fn poll_pending_audio(&mut self) {
        let Some((requested_from, requested_to, requested_reverse, rx)) =
            self.pending_audio.as_mut()
        else {
            return;
        };
        let (from, to, reverse_request) = (*requested_from, *requested_to, *requested_reverse);
        match rx.try_recv() {
            Ok(Ok(result)) => {
                self.pending_audio = None;
                let scheduled_up_to = self.sched.scheduled_to();
                self.last_cps = result.cps;
                // Первый ответ после свежего запуска — «отпускаем» интерполяцию времени
                // и выравниваем last_bridge_instant, чтобы первые события не уехали в прошлое.
                if self.awaiting_first_audio_after_start {
                    self.awaiting_first_audio_after_start = false;
                    self.last_bridge_instant = Instant::now();
                    self.last_bridge_now = self.current_time;
                }
                self.sched.set_scheduled_to(if reverse_request { from } else { to });
                // Привязка к текущему циклу в момент ответа, а не к from: иначе переменная задержка
                // ответа (20–100 ms) даёт разный сдвиг по фазе и скачки.
                let ref_cycle = self.current_time;

                // Fan-out ALL hits in this window as one batch.
                // Any Strudel pattern → Visual Stage buses → whatever Hydra look is up.
                {
                    let mut hits = Vec::new();
                    for ev in &result.events {
                        let include = if reverse_request {
                            ev.begin >= from && ev.begin < to && ev.begin <= ref_cycle
                        } else {
                            ev.begin >= scheduled_up_to
                        };
                        if !include {
                            continue;
                        }
                        hits.push(visual_stage::hit_json_full(
                            ev.begin,
                            ev.s.as_deref(),
                            ev.bank.as_deref(),
                            ev.gain,
                            ev.n,
                            ev.midi,
                        ));
                    }
                    if !hits.is_empty() {
                        self.visual_stage.broadcast_hits(&hits);
                    }
                }

                if let Some(engine) = &self.audio {
                    let audio_gain = self.settings.audio_gain as f64;
                    // Один cps на всё окно: циклы (begin/end) в глобальной шкале, переводим в секунды одним темпом.
                    let window_cps = result.cps;
                    let build_ev = |ev: crate::strudel_bridge::StrudelAudioEvent| {
                        let pattern_gain = ev.gain.unwrap_or(1.0);
                        let gain = (pattern_gain * audio_gain).clamp(0.0, 1.0);
                        let duration = (ev.end - ev.begin).max(0.0);
                        let (start_cycle, end_cycle) = if reverse_request {
                            let start = ref_cycle - ev.begin;
                            (start, start + duration)
                        } else {
                            (ev.begin - ref_cycle, ev.end - ref_cycle)
                        };
                        AudioEvent {
                            start_cycle,
                            end_cycle,
                            midi_note: ev.midi,
                            gain: Some(gain),
                            cps: window_cps,
                            s: ev.s,
                            bank: ev.bank,
                            n: ev.n,
                            pan: ev.pan,
                            attack: ev.attack,
                            decay: ev.decay,
                            sustain: ev.sustain,
                            release: ev.release,
                            room: ev.room,
                            shape: ev.shape,
                            cut: ev.cut,
                            speed: ev.speed,
                            cutoff: ev.cutoff,
                            hcutoff: ev.hcutoff,
                            coarse: ev.coarse,
                            crush: ev.crush,
                            compressor: ev.compressor,
                            compressor_ratio: ev.compressor_ratio,
                            penv: ev.penv,
                            pattack: ev.pattack,
                            prelease: ev.prelease,
                            epoch: ev.epoch,
                        }
                    };
                    let (gm_events, other_events): (Vec<_>, Vec<_>) = result
                        .events
                        .into_iter()
                        .filter(|ev| {
                            if reverse_request {
                                ev.begin >= from && ev.begin < to && ev.begin <= ref_cycle
                            } else {
                                ev.begin >= scheduled_up_to
                            }
                        })
                        .map(build_ev)
                        .partition(|ev| ev.s.as_ref().map_or(false, |s| s.starts_with("gm_")));
                    for ev in other_events {
                        engine.schedule_event(ev);
                    }
                    if !gm_events.is_empty() {
                        let engine = Arc::clone(engine);
                        thread::spawn(move || {
                            for ev in gm_events {
                                engine.schedule_event(ev);
                            }
                        });
                    }
                }
            }
            Ok(Err(e)) => {
                self.pending_audio = None;
                eprintln!("[AudioWindow] {}", e);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending_audio = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    /// Запрашивает новое аудио-окно только если нужно и нет уже ожидающего запроса. Не блокирует.
    fn schedule_audio_window(&mut self) {
        if !self.settings.audio_enabled {
            return;
        }
        self.poll_pending_audio();
        if self.pending_audio.is_some() {
            return;
        }
        let (from, to, reverse_request) = if self.reverse {
            let session_start = self.session_start();
            let current = self.current_time.max(session_start);
            if current <= session_start {
                return;
            }
            if self.sched.scheduled_to() < 0.0 || self.sched.scheduled_to() > current + 0.75 {
                self.sched.set_scheduled_to(current);
            }
            let boundary = self.sched.scheduled_to().min(current);
            if current - boundary > 1.5 {
                return;
            }
            let from = (boundary - 0.5).max(session_start);
            if from >= boundary {
                return;
            }
            (from, boundary, true)
        } else {
            let from = self.current_time;
            let to = from + 0.5;
            let scheduled_up_to = self.sched.scheduled_to();
            if to <= scheduled_up_to || (scheduled_up_to - from) > 1.5 {
                return;
            }
            (from, to, false)
        };
        // Нативный быстрый путь: для простых s("...")/sound("...") паттернов
        // минуем node.js bridge и сразу кладём результат в pending_audio через mpsc::channel.
        // poll_pending_audio на следующем кадре подхватит и зашедулит в AudioEngine.
        // Reverse-путь остаётся за bridge — он в нём реализован.
        if self.native_engine.lock().map(|e| e.is_pure()).unwrap_or(false) && !reverse_request {
            let result = self
                .native_engine
                .lock()
                .map(|e| e.query_audio_window(from, to))
                .unwrap_or_else(|_| crate::strudel_bridge::AudioWindowResult {
                    cps: 1.0,
                    events: Vec::new(),
                });
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = tx.send(Ok(result));
            self.pending_audio = Some((from, to, reverse_request, rx));
            return;
        }
        let Ok(rx) = self.bridge.start_audio_window_request(from, to) else {
            return;
        };
        self.pending_audio = Some((from, to, reverse_request, rx));
    }

    fn close_tab(&mut self, index: usize) {
        if index >= self.editor_tabs.len() {
            return;
        }
        self.editor_tabs.remove(index);
        if self.active_editor_index >= self.editor_tabs.len() && !self.editor_tabs.is_empty() {
            self.active_editor_index = self.editor_tabs.len() - 1;
        } else if index < self.active_editor_index {
            self.active_editor_index = self.active_editor_index.saturating_sub(1);
        }
    }

    /// Отрисовка контента по типу вкладки (внутри одной панели).
    fn content_ui(&mut self, ui: &mut egui::Ui, content_id: ContentId) {
        match content_id {
            ContentId::Transport => self.transport_content_ui(ui),
            ContentId::Workspace => self.editor_workspace_ui(ui),
            ContentId::Timeline => self.timeline_content_ui(ui),
            ContentId::Mixer => self.mixer_content_ui(ui),
            ContentId::Debugger => self.debugger_content_ui(ui),
            ContentId::Files => self.files_content_ui(ui),
            ContentId::Samples => self.samples_content_ui(ui),
            ContentId::SoundPalette => self.sound_palette_ui(ui),
            ContentId::SamplesSettings => self.samples_settings_content_ui(ui),
            ContentId::Documentation => self.docs_content_ui(ui),
            ContentId::Settings => self.settings_content_ui(ui),
            ContentId::Terminal => self.terminal_content_ui(ui),
            ContentId::Problems => self.problems_content_ui(ui),
            ContentId::Arrangement => self.arrangement_content_ui(ui),
            ContentId::ChainSeed => self.chain_seed_content_ui(ui),
            ContentId::Hydra => self.hydra_content_ui(ui),
            ContentId::AiPattern => self.ai_pattern_content_ui(ui),
            ContentId::SoundView => self.sound_view_content_ui(ui),
        }
    }

    /// Onglet « Vue » : rythme du pattern, ondes des samples, spectre live.
    fn sound_view_content_ui(&mut self, ui: &mut egui::Ui) {
        ui::sound_view_ui(
            ui,
            &mut self.sound_view,
            self.pattern_document.as_ref(),
            self.current_time,
            self.playing,
            self.audio.as_ref(),
        );
    }

    /// Транспорт как полноценная вкладка рабочего пространства.
    fn transport_content_ui(&mut self, ui: &mut egui::Ui) {
        let current_time = self.current_time;
        let playing = self.playing;
        let paused = self.paused;
        let focus_label = self.playback_focus_content.label();
        let transport_actions = ui::transport_ui(
            ui,
            current_time,
            playing,
            paused,
            focus_label,
            &mut self.transport_state,
        );
        for action in transport_actions {
            self.handle_transport_action(action);
        }
    }

    /// Дерево файлов открытой папки (полноценная вкладка).
    fn files_content_ui(&mut self, ui: &mut egui::Ui) {
        let full_w = ui.available_width();
        egui::ScrollArea::vertical()
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
            .id_salt("files_panel_scroll")
            .show(ui, |ui| {
                ui.set_min_width(full_w);
                if let Some((path, contents)) = self.file_tree.ui(ui, "") {
                    self.open_file_in_tab(path, contents);
                }
            });
    }

    /// Браузер семплов: поиск, категории, превью, вставка в код.
    fn samples_content_ui(&mut self, ui: &mut egui::Ui) {
        use std::path::Path;

        self.sample_categories
            .load_from_path(Path::new(&self.settings.sample_folder));

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("🔍 Recherche de samples");
            ui.add(
                egui::TextEdit::singleline(&mut self.sample_browser_search)
                    .hint_text("nom ou banque…")
                    .desired_width(180.0),
            );
        });
        ui.add_space(4.0);

        // Фильтр по категории
        let order = self.sample_categories.category_order();
        let mut categories_for_combo: Vec<String> = vec!["all".to_string()];
        categories_for_combo.extend(order.iter().cloned());
        let cat_key = if self.sample_browser_category.is_empty() {
            "all"
        } else {
            &self.sample_browser_category
        };
        let cat_label = if cat_key == "all" {
            "Toutes les catégories".to_string()
        } else {
            self.sample_categories.category_label(cat_key)
        };
        egui::ComboBox::from_id_salt(egui::Id::new("sample_browser_category"))
            .selected_text(cat_label)
            .show_ui(ui, |ui| {
                for cat in &categories_for_combo {
                    let label = if cat == "all" {
                        "Toutes les catégories".to_string()
                    } else {
                        self.sample_categories.category_label(cat)
                    };
                    if ui
                        .selectable_label(self.sample_browser_category == *cat, &label)
                        .clicked()
                    {
                        self.sample_browser_category = cat.clone();
                    }
                }
            });
        ui.add_space(8.0);

        // Кэш: заполняем не в первый кадр (чтобы не подвисать при переключении на вкладку), а по кнопке или на следующий кадр.
        if self.sample_browser_entries_cache.is_none() {
            if self.sample_browser_cache_pending {
                if let Some(engine) = &self.audio {
                    self.sample_browser_entries_cache = Some(engine.list_sample_entries());
                } else {
                    self.sample_browser_entries_cache = Some(Vec::new());
                }
                self.sample_browser_cache_pending = false;
            } else {
                self.sample_browser_cache_pending = true;
            }
        }
        if ui.button("🔄 Rafraîchir la liste").clicked() {
            self.sample_browser_cache_pending = false;
            self.sample_browser_filtered_rows = None;
            if let Some(engine) = &self.audio {
                self.sample_browser_entries_cache = Some(engine.list_sample_entries());
            } else {
                self.sample_browser_entries_cache = Some(Vec::new());
            }
        }
        ui.add_space(4.0);
        ui.label("Samples de la catégorie sélectionnée :");
        ui.add_space(4.0);
        let entries: &[(SampleKey, usize)] =
            self.sample_browser_entries_cache.as_deref().unwrap_or(&[]);
        if entries.is_empty() && self.sample_browser_cache_pending {
            ui.label(egui::RichText::new("Chargement de la liste…").color(ui.visuals().weak_text_color()));
            return;
        }

        let search_lc = self.sample_browser_search.to_lowercase();
        let cat_filter = self.sample_browser_category.as_str();
        let current_key = (search_lc.clone(), cat_filter.to_string());

        // Пересчитываем отфильтрованный список только при смене поиска, категории или кэша.
        if self.sample_browser_filtered_rows.is_none()
            || self.sample_browser_filter_key != current_key
        {
            let mut rows: Vec<(SampleKey, usize)> = entries
                .iter()
                .filter(|(key, _)| {
                    let name_ok = search_lc.is_empty()
                        || key.name.to_lowercase().contains(&search_lc)
                        || key
                            .bank
                            .as_ref()
                            .map(|b| b.to_lowercase().contains(&search_lc))
                            .unwrap_or(false);
                    if !name_ok {
                        return false;
                    }
                    if cat_filter != "all" {
                        let info = self
                            .sample_categories
                            .category_for_bank(key.bank.as_deref().unwrap_or(""));
                        if info.key != cat_filter {
                            return false;
                        }
                    }
                    true
                })
                .cloned()
                .collect();
            rows.sort_by(|a, b| {
                let cat_a = self
                    .sample_categories
                    .category_for_bank(a.0.bank.as_deref().unwrap_or(""))
                    .key;
                let cat_b = self
                    .sample_categories
                    .category_for_bank(b.0.bank.as_deref().unwrap_or(""))
                    .key;
                cat_a
                    .cmp(&cat_b)
                    .then_with(|| a.0.bank.cmp(&b.0.bank))
                    .then_with(|| a.0.name.cmp(&b.0.name))
            });
            self.sample_browser_filtered_rows = Some(rows);
            self.sample_browser_filter_key = current_key;
        }

        let filtered = self.sample_browser_filtered_rows.as_deref().unwrap_or(&[]);
        const MAX_ROWS: usize = 2000;
        let total_count = filtered.len();
        let display_rows: &[(SampleKey, usize)] = if total_count > MAX_ROWS {
            &filtered[..MAX_ROWS]
        } else {
            filtered
        };
        let num_display = display_rows.len();

        let full_w = ui.available_width();
        const ROW_H: f32 = 28.0;
        egui::ScrollArea::vertical()
            .id_salt("sample_browser_list")
            .show_rows(ui, ROW_H, num_display, |ui, row_range| {
                ui.set_min_width(full_w);
                for row in row_range {
                    if row >= display_rows.len() {
                        continue;
                    }
                    let (key, count) = &display_rows[row];
                    let display_name = key
                        .bank
                        .as_ref()
                        .map(|b| format!("{} / {}", b, key.name))
                        .unwrap_or_else(|| key.name.clone());
                    let insert_code = insert_snippet(
                        &key.name,
                        key.bank.as_deref(),
                        self.sound_insert_controls,
                        &self.sample_categories,
                    );

                    let (row_rect, row_resp) = ui.allocate_exact_size(
                        egui::vec2(full_w, ROW_H),
                        egui::Sense::click_and_drag(),
                    );
                    if row_resp.hovered() {
                        ui.painter()
                            .rect_filled(row_rect, 2.0, egui::Color32::from_white_alpha(8));
                    }
                    ui.painter().text(
                        egui::pos2(row_rect.min.x + 8.0, row_rect.center().y - 6.0),
                        egui::Align2::LEFT_CENTER,
                        format!("{} ({})", display_name, count),
                        egui::FontId::monospace(12.0),
                        ui.visuals().text_color(),
                    );

                    let preview_rect = egui::Rect::from_min_max(
                        egui::pos2(row_rect.max.x - 52.0, row_rect.min.y + 4.0),
                        egui::pos2(row_rect.max.x - 8.0, row_rect.max.y - 4.0),
                    );
                    let preview_resp = ui.interact(
                        preview_rect,
                        ui.id().with(row).with("preview"),
                        egui::Sense::click(),
                    );
                    ui.painter().text(
                        preview_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "▶",
                        egui::FontId::proportional(12.0),
                        ui.visuals().text_color(),
                    );
                    if preview_resp.clicked() {
                        if let Some(engine) = &self.audio {
                            engine.play_sample_preview(key, Some(0));
                        }
                    }

                    if row_resp.double_clicked() {
                        self.pending_sample_insert = Some(insert_code.clone());
                    }
                    if row_resp.drag_started() {
                        row_resp.dnd_set_drag_payload(insert_code.clone());
                    }
                }
            });
        if total_count > MAX_ROWS {
            ui.label(
                egui::RichText::new(format!(
                    "{} affichés sur {}. Affinez la recherche ou choisissez une catégorie.",
                    num_display, total_count
                ))
                .color(ui.visuals().weak_text_color())
                .small(),
            );
        }
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(
                "Double-clic — insère dans le code (à la position du curseur). ▶ — aperçu.",
            )
            .color(ui.visuals().weak_text_color())
            .small(),
        );
    }

    /// Onglet « Sons » (à côté de Mixer) : tous les sons par catégorie,
    /// insertion en un clic dans le code Strudel.
    fn sound_palette_ui(&mut self, ui: &mut egui::Ui) {
        use std::path::Path;

        // Tous les instruments GM (nécessitent FluidR3_GM.sf2 dans strudel_files).
        let gm_sounds = crate::soundfont::gm_sound_names();
        const MAX_ROWS_PER_SECTION: usize = 150;

        self.sample_categories
            .load_from_path(Path::new(&self.settings.sample_folder));

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("🔍");
            ui.add(
                egui::TextEdit::singleline(&mut self.sound_palette_search)
                    .hint_text("son…")
                    .desired_width(140.0),
            );
            if ui
                .button("🔄")
                .on_hover_text("Rescanner la bibliothèque")
                .clicked()
            {
                self.sample_browser_entries_cache = None;
                self.sample_browser_filtered_rows = None;
                if let Some(engine) = &self.audio {
                    engine.reload_samples_async(Path::new(&self.settings.sample_folder));
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("ajouter :").color(ui.visuals().weak_text_color()),
            );
            if ui
                .selectable_label(
                    self.sound_insert_mode == SoundInsertMode::NewPart,
                    "nouvelle partie",
                )
                .on_hover_text(
                    "Ajoute le son dans un stack(…) : il joue en parallèle des parties existantes",
                )
                .clicked()
            {
                self.sound_insert_mode = SoundInsertMode::NewPart;
            }
            if ui
                .selectable_label(
                    self.sound_insert_mode == SoundInsertMode::AtCursor,
                    "au curseur",
                )
                .on_hover_text("Insère s(…) à la position du curseur dans le code")
                .clicked()
            {
                self.sound_insert_mode = SoundInsertMode::AtCursor;
            }
            ui.checkbox(&mut self.sound_insert_controls, "contrôles")
                .on_hover_text(
                    "Ajoute des widgets live au son inséré :\n.gain(slider(…)) .mask(toggler(1)) …",
                );
        });
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new("+ : insère `s(…)` dans le code · ▶ : aperçu · contrôles = widgets dans le code")
                .color(ui.visuals().weak_text_color())
                .small(),
        );
        ui.add_space(4.0);

        // Import de sons depuis un dépôt GitHub (thread de fond + progression).
        egui::CollapsingHeader::new("⬇Importer depuis GitHub")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.github_import_url)
                            .hint_text("github.com/user/repo-de-sons")
                            .desired_width(230.0),
                    );
                    let running = self.github_import_rx.is_some();
                    if ui
                        .add_enabled(!running, egui::Button::new("Importer"))
                        .on_hover_text("Télécharge le tarball du repo et extrait les sons\n(wav/mp3/flac/ogg/… + manifestes json)")
                        .clicked()
                    {
                        self.start_github_import();
                    }
                });
                match &self.github_import_state {
                    GithubImportUi::Running { files, bytes } => {
                        ui.label(
                            egui::RichText::new(format!(
                                "import… {} fichiers · {:.1} Mio",
                                files,
                                *bytes as f64 / 1_048_576.0
                            ))
                            .small()
                            .color(ui.visuals().weak_text_color()),
                        );
                    }
                    GithubImportUi::Done { files, bytes } => {
                        ui.colored_label(
                            ui.visuals().hyperlink_color,
                            format!(
                                "✓ {} sons importés ({:.1} Mio) — rescannés dans la liste",
                                files,
                                *bytes as f64 / 1_048_576.0
                            ),
                        );
                    }
                    GithubImportUi::Error(e) => {
                        ui.colored_label(ui.visuals().error_fg_color, format!("✗ {e}"));
                    }
                    GithubImportUi::Idle => {}
                }
                ui.label(
                    egui::RichText::new(
                        "un dossier = une banque · strudel.json à la racine = banques nommées",
                    )
                    .color(ui.visuals().weak_text_color())
                    .small(),
                );
            });
        ui.add_space(4.0);
        self.live_params_ui(ui);
        ui.add_space(4.0);

        // Cache partagé avec l'onglet Samples.
        if self.sample_browser_entries_cache.is_none() {
            if self.sample_browser_cache_pending {
                if let Some(engine) = &self.audio {
                    self.sample_browser_entries_cache = Some(engine.list_sample_entries());
                } else {
                    self.sample_browser_entries_cache = Some(Vec::new());
                }
                self.sample_browser_cache_pending = false;
            } else {
                self.sample_browser_cache_pending = true;
            }
        }
        if self.sample_browser_entries_cache.is_none() {
            ui.label(
                egui::RichText::new("Chargement de la liste…")
                    .color(ui.visuals().weak_text_color()),
            );
            return;
        }
        let entries = self.sample_browser_entries_cache.clone().unwrap_or_default();
        let search_lc = self.sound_palette_search.to_lowercase();
        let order = self.sample_categories.category_order().to_vec();
        let audio = self.audio.clone();
        let pending_delete = self.pending_sample_delete.clone();
        let now = Instant::now();
        self.sound_previews.retain(|_, until| now < *until);
        let playing_codes = self.sound_previews.clone();
        if !playing_codes.is_empty() {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
        let mut insert_code: Option<String> = None;
        // Some(Some(k)) = armer la confirmation sur k, Some(None) = désarmer.
        let mut delete_arm: Option<Option<SampleKey>> = None;
        let mut delete_do: Option<SampleKey> = None;
        let mut preview_do: Option<(String, PreviewSource)> = None;

        let row_ui = |ui: &mut egui::Ui,
                      display: &str,
                      code: &str,
                      key: Option<&SampleKey>,
                      insert_code: &mut Option<String>,
                      delete_arm: &mut Option<Option<SampleKey>>,
                      delete_do: &mut Option<SampleKey>,
                      preview_do: &mut Option<(String, PreviewSource)>| {
            ui.horizontal(|ui| {
                ui.monospace(display);
                if sound_icon_button(ui, SoundIcon::Add, "Insérer dans le code et jouer").clicked() {
                    *insert_code = Some(code.to_string());
                }
                if audio.is_some() {
                    let playing = playing_codes.contains_key(code);
                    let (icon, tip) = if playing {
                        (SoundIcon::Playing, "En lecture")
                    } else {
                        (SoundIcon::Play, "Jouer")
                    };
                    if sound_icon_button(ui, icon, tip).clicked() && !playing {
                        let source = match key {
                            Some(key) => PreviewSource::Disk(key.clone()),
                            None => PreviewSource::Generated(display.to_string()),
                        };
                        *preview_do = Some((code.to_string(), source));
                    }
                }
                if let (Some(_), Some(key)) = (&audio, key.filter(|k| is_voice_key(k))) {
                    if pending_delete.as_ref() == Some(key) {
                        if sound_icon_button(ui, SoundIcon::Check, "Confirmer : supprime le fichier du disque").clicked() {
                            *delete_do = Some(key.clone());
                        }
                        if sound_icon_button(ui, SoundIcon::Cross, "Annuler").clicked() {
                            *delete_arm = Some(None);
                        }
                    } else if sound_icon_button(ui, SoundIcon::Trash, "Supprimer le fichier").clicked() {
                        *delete_arm = Some(Some(key.clone()));
                    }
                }
            });
        };

        egui::ScrollArea::vertical()
            .id_salt("sound_palette_scroll")
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());

                egui::CollapsingHeader::new("🎹 Synthèse")
                    .default_open(search_lc.is_empty() || order.is_empty())
                    .show(ui, |ui| {
                        for name in SYNTH_SOUNDS {
                            if !search_lc.is_empty() && !name.contains(&search_lc) {
                                continue;
                            }
                            row_ui(
                                ui,
                                name,
                                &insert_snippet(
                                    name,
                                    None,
                                    self.sound_insert_controls,
                                    &self.sample_categories,
                                ),
                                None,
                                &mut insert_code,
                                &mut delete_arm,
                                &mut delete_do,
                                &mut preview_do,
                            );
                        }
                    });

                egui::CollapsingHeader::new("🥁 Percussions synthétisées")
                    .default_open(!search_lc.is_empty())
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(
                                "Kick, caisse, hats, clap générés — type 808/909",
                            )
                            .color(ui.visuals().weak_text_color())
                            .small(),
                        );
                        for name in DRUM_SOUNDS {
                            if !search_lc.is_empty() && !name.contains(&search_lc) {
                                continue;
                            }
                            row_ui(
                                ui,
                                name,
                                &insert_snippet(
                                    name,
                                    None,
                                    self.sound_insert_controls,
                                    &self.sample_categories,
                                ),
                                None,
                                &mut insert_code,
                                &mut delete_arm,
                                &mut delete_do,
                                &mut preview_do,
                            );
                        }
                    });

                egui::CollapsingHeader::new("🎺 Instruments GM")
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(
                                "Joués via FluidR3_GM.sf2 si présent dans strudel_files",
                            )
                            .color(ui.visuals().weak_text_color())
                            .small(),
                        );
                        for name in &gm_sounds {
                            if !search_lc.is_empty() && !name.contains(&search_lc) {
                                continue;
                            }
                            row_ui(
                                ui,
                                name,
                                &insert_snippet(
                                    name,
                                    None,
                                    self.sound_insert_controls,
                                    &self.sample_categories,
                                ),
                                None,
                                &mut insert_code,
                                &mut delete_arm,
                                &mut delete_do,
                                &mut preview_do,
                            );
                        }
                    });

                for cat in &order {
                    let label = self.sample_categories.category_label(cat);
                    let mut shown = 0usize;
                    let cat_rows: Vec<&(SampleKey, usize)> = entries
                        .iter()
                        .filter(|(key, _)| {
                            if !search_lc.is_empty()
                                && !key.name.to_lowercase().contains(&search_lc)
                                && !key
                                    .bank
                                    .as_ref()
                                    .map(|b| b.to_lowercase().contains(&search_lc))
                                    .unwrap_or(false)
                            {
                                return false;
                            }
                            self.sample_categories
                                .category_for_bank(key.bank.as_deref().unwrap_or(""))
                                .key
                                == *cat
                        })
                        .collect();
                    if cat_rows.is_empty() {
                        continue;
                    }
                    egui::CollapsingHeader::new(format!("{} ({})", label, cat_rows.len()))
                        .default_open(!search_lc.is_empty() || *cat == "vocal")
                        .id_salt(format!("palette_cat_{cat}"))
                        .show(ui, |ui| {
                            for (key, count) in &cat_rows {
                                if shown >= MAX_ROWS_PER_SECTION {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "… +{} — affinez la recherche",
                                            cat_rows.len() - shown
                                        ))
                                        .color(ui.visuals().weak_text_color())
                                        .small(),
                                    );
                                    break;
                                }
                                shown += 1;
                                let display = key
                                    .bank
                                    .as_ref()
                                    .map(|b| format!("{} / {} ({})", b, key.name, count))
                                    .unwrap_or_else(|| format!("{} ({})", key.name, count));
                                let code = insert_snippet(
                                    &key.name,
                                    key.bank.as_deref(),
                                    self.sound_insert_controls,
                                    &self.sample_categories,
                                );
                                row_ui(
                                    ui,
                                    &display,
                                    &code,
                                    Some(key),
                                    &mut insert_code,
                                    &mut delete_arm,
                                    &mut delete_do,
                                    &mut preview_do,
                                );
                            }
                        });
                }
            });

        if let Some(code) = insert_code {
            // Sans fichier ouvert l'insertion resterait en file indéfiniment :
            // on crée un onglet pour que le code aille quelque part.
            if self.editor_tabs.is_empty() {
                self.new_file();
            }
            match self.sound_insert_mode {
                SoundInsertMode::AtCursor => {
                    self.pending_sample_insert = Some(code);
                    self.run_after_insert = true;
                }
                SoundInsertMode::NewPart => {
                    let current = self
                        .active_editor()
                        .map(|e| e.code.clone())
                        .unwrap_or_default();
                    self.apply_live_code(extend_stack(&current, &code));
                    self.run_editor_code();
                }
            }
        }

        if let Some(arm) = delete_arm {
            self.pending_sample_delete = arm;
        }
        if let Some(key) = delete_do {
            self.pending_sample_delete = None;
            if let Some(engine) = &self.audio {
                // Toutes les variantes du nom (un "bd (4)" = 4 fichiers).
                let count = entries
                    .iter()
                    .find(|(k, _)| k == &key)
                    .map(|(_, n)| (*n).max(1))
                    .unwrap_or(1);
                let sample_root = PathBuf::from(&self.settings.sample_folder);
                for variant in 0..count {
                    if let Some(path) = engine.lookup_sample_path(&key, Some(variant)) {
                        match move_to_trash(&path, &sample_root) {
                            Ok(dest) => {
                                eprintln!("[Sons] corbeille {:?} → {:?}", path, dest);
                                // Cache de forme d'onde : régénérable, suppression sûre.
                                let _ = std::fs::remove_file(
                                    crate::waveform_cache::peak_path_for_audio(&path),
                                );
                            }
                            Err(e) => {
                                eprintln!("[Sons] suppression annulée pour {:?}: {e}", path);
                            }
                        }
                    }
                }
                engine.reload_samples_async(Path::new(&self.settings.sample_folder));
            }
            self.sample_browser_entries_cache = None;
            self.sample_browser_filtered_rows = None;
            self.sample_browser_cache_pending = false;
        }
        if let Some((code, source)) = preview_do {
            if !self.sound_previews.contains_key(&code) {
                let secs = match source {
                    PreviewSource::Disk(key) => self.audio.as_ref().and_then(|e| e.play_sample_preview(&key, Some(0))),
                    PreviewSource::Generated(name) => self.audio.as_ref().and_then(|e| e.play_generated_preview(&name)),
                };
                if let Some(secs) = secs {
                    self.sound_previews.insert(code, Instant::now() + Duration::from_secs_f32(secs));
                }
            }
        }
    }

    /// Slider + listes pour régler gain, pan, room… pendant que le motif tourne.
    /// Portée : un son, un type (synthé, GM, voix, catégorie de banque), ou tous les sons.
    fn live_params_ui(&mut self, ui: &mut egui::Ui) {
        use crate::live_params::{
            aggregate_value, find_inline_widgets, find_sound_calls, set_method_where, sound_type, to_written, types_in,
            LiveScope, LIVE_PARAMS,
        };
        let code = self.active_editor().map(|e| e.code.clone()).unwrap_or_default();
        let calls = find_sound_calls(&code);
        let inline_sliders = find_inline_widgets(&code);
        // Libellé de catégorie pour chaque banque utilisée (lookup figé pour ce frame).
        let mut bank_labels: HashMap<String, String> = HashMap::new();
        for c in &calls {
            if let Some(b) = &c.bank {
                let key = self.sample_categories.category_for_bank(b).key;
                bank_labels.entry(b.clone()).or_insert_with(|| self.sample_categories.category_label(&key));
            }
        }
        let bank_category = |bank: &str| -> String {
            if bank.is_empty() {
                return "Autres".to_string();
            }
            bank_labels.get(bank).cloned().unwrap_or_else(|| bank.to_string())
        };
        // Types proposés : synthés, GM, voix, puis toutes les catégories de la bibliothèque (même absentes du motif).
        let mut types: Vec<String> = vec!["Synthèse".into(), "Instruments GM".into(), "Voix".into()];
        for cat in self.sample_categories.category_order().to_vec() {
            let label = self.sample_categories.category_label(&cat);
            if !types.contains(&label) {
                types.push(label);
            }
        }
        for t in types_in(&calls, &bank_category) {
            if !types.contains(&t) {
                types.push(t);
            }
        }
        if !types.contains(&"Autres".to_string()) {
            types.push("Autres".into());
        }
        let mut edit: Option<String> = None;
        egui::CollapsingHeader::new("🎛 Paramètres live")
            .default_open(true)
            .show(ui, |ui| {
                if calls.is_empty() {
                    if inline_sliders.is_empty() {
                        ui.label(
                            egui::RichText::new("Ouvre un motif contenant s(\"…\") pour régler ses paramètres.")
                                .color(ui.visuals().weak_text_color())
                                .small(),
                        );
                    } else {
                        edit = render_inline_sliders(ui, &code, &inline_sliders);
                    }
                    return;
                }
                self.live_sound_sel = self.live_sound_sel.min(calls.len() - 1);
                self.live_param_sel = self.live_param_sel.min(LIVE_PARAMS.len() - 1);
                self.live_type_sel = self.live_type_sel.min(types.len().saturating_sub(1));
                ui.horizontal(|ui| {
                    ui.label("Portée");
                    egui::ComboBox::from_id_salt("live_scope")
                        .selected_text(match self.live_scope {
                            LiveScope::Sound => "un son",
                            LiveScope::Type => "un type",
                            LiveScope::All => "tous les sons",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.live_scope, LiveScope::Sound, "un son");
                            ui.selectable_value(&mut self.live_scope, LiveScope::Type, "un type");
                            ui.selectable_value(&mut self.live_scope, LiveScope::All, "tous les sons");
                        });
                });
                match self.live_scope {
                    LiveScope::Sound => {
                        ui.horizontal(|ui| {
                            ui.label("Son");
                            egui::ComboBox::from_id_salt("live_sound")
                                .selected_text(calls[self.live_sound_sel].label.clone())
                                .show_ui(ui, |ui| {
                                    for (i, c) in calls.iter().enumerate() {
                                        ui.selectable_value(&mut self.live_sound_sel, i, c.label.clone());
                                    }
                                });
                        });
                    }
                    LiveScope::Type => {
                        ui.horizontal(|ui| {
                            ui.label("Type");
                            egui::ComboBox::from_id_salt("live_type")
                                .selected_text(types[self.live_type_sel].clone())
                                .show_ui(ui, |ui| {
                                    for (i, t) in types.iter().enumerate() {
                                        ui.selectable_value(&mut self.live_type_sel, i, t.clone());
                                    }
                                });
                            let n = calls.iter().filter(|c| sound_type(c, &bank_category) == types[self.live_type_sel]).count();
                            let txt = if n == 0 { "absent du motif".to_string() } else { format!("{n} son(s)") };
                            ui.label(egui::RichText::new(txt).color(ui.visuals().weak_text_color()).small());
                        });
                    }
                    LiveScope::All => {}
                }
                ui.horizontal(|ui| {
                    ui.label("Paramètre");
                    egui::ComboBox::from_id_salt("live_param")
                        .selected_text(LIVE_PARAMS[self.live_param_sel].label)
                        .show_ui(ui, |ui| {
                            for (i, p) in LIVE_PARAMS.iter().enumerate() {
                                ui.selectable_value(&mut self.live_param_sel, i, p.label);
                            }
                        });
                });
                let spec = LIVE_PARAMS[self.live_param_sel];
                let call = calls[self.live_sound_sel].clone();
                let chosen_type = types[self.live_type_sel].clone();
                let keep = |c: &crate::live_params::SoundCall| -> bool {
                    match self.live_scope {
                        LiveScope::Sound => c == &call,
                        LiveScope::Type => sound_type(c, &bank_category) == chosen_type,
                        LiveScope::All => true,
                    }
                };
                let aggregated = aggregate_value(&code, &calls, &spec, &keep).unwrap_or(spec.default);
                let mut value = aggregated;
                ui.horizontal(|ui| {
                    let resp = ui
                        .scope(|ui| {
                            ui.visuals_mut().widgets.inactive.bg_fill = egui::Color32::from_gray(110);
                            ui.add(egui::Slider::new(&mut value, spec.min..=spec.max).fixed_decimals(2))
                        })
                        .inner;
                    if resp.changed() {
                        edit = Some(set_method_where(&code, spec.method, Some(&to_written(&spec, value)), &keep));
                    }
                    if ui.small_button("retirer").on_hover_text("Supprime ce paramètre de la portée choisie").clicked() {
                        edit = Some(set_method_where(&code, spec.method, None, &keep));
                    }
                });
                if let Some(e) = render_inline_sliders(ui, &code, &inline_sliders) {
                    edit = Some(e);
                }
            });
        if let Some(new_code) = edit {
            self.apply_live_code(new_code);
        }
    }

    /// Écrit le code modifié dans l'éditeur actif et le relance sans remettre la lecture à zéro.
    fn apply_live_code(&mut self, code: String) {
        if let Some(ed) = self.active_editor_mut() {
            ed.code = code.clone();
        }
        self.last_code_revision = self.last_code_revision.saturating_add(1);
        self.sync_engine.set_code(code.clone(), self.last_code_revision);
        self.pattern_document = Some(parse_pattern_document(code.clone(), self.last_code_revision));
        self.refresh_native_engine();
        if self.playing {
            let _ = self.bridge.evaluate(&code);
            let _ = self.bridge.set_bpm(self.transport_state.bpm);
        }
    }

    /// Traite les requêtes de l'API de contrôle locale (serveur MCP `struidel`)
    /// et publie un statut frais ~4 fois par seconde.
    fn poll_control_api(&mut self, ctx: &egui::Context) {
        self.control.set_wakeup(ctx.clone());
        for req in self.control.take_requests() {
            let resp = self.dispatch_control(&req.body);
            self.control.respond(req.id, resp);
        }
        if self.control_status_at.elapsed() >= Duration::from_millis(250) {
            self.control_status_at = Instant::now();
            self.control.publish_status(self.control_status_json());
        }
    }

    fn control_status_json(&self) -> serde_json::Value {
        let (code, file) = self
            .active_editor()
            .map(|e| (e.code.clone(), e.title()))
            .unwrap_or_else(|| (String::new(), "aucun".to_string()));
        let meter = self.audio.as_ref().map(|e| {
            let m = e.master_meter();
            serde_json::json!({
                "peak": m.peak,
                "rms": m.rms,
                "lufs": m.lufs_short,
                "clipped": m.clipped,
            })
        });
        serde_json::json!({
            "ok": true,
            "playing": self.playing,
            "paused": self.paused,
            "bpm": self.transport_state.bpm,
            "cps": self.transport_state.bpm / 60.0,
            "time_cycles": self.current_time,
            "engine": if self.native_engine.lock().map(|e| e.is_pure()).unwrap_or(false) { "native" } else { "bridge" },
            "file": file,
            "code_len": code.len(),
            "audio_engine": self.audio.is_some(),
            "audio_load": self.audio.as_ref().map(|e| e.load_snapshot()),
            "meter": meter,
            "last_error": self.bridge.last_error(),
            "github_import": match &self.github_import_state {
                GithubImportUi::Idle => serde_json::json!({"state": "idle"}),
                GithubImportUi::Running { files, bytes } => serde_json::json!({
                    "state": "running", "files": files, "bytes": bytes,
                }),
                GithubImportUi::Done { files, bytes } => serde_json::json!({
                    "state": "done", "files": files, "bytes": bytes,
                }),
                GithubImportUi::Error(e) => serde_json::json!({
                    "state": "error", "error": e,
                }),
            },
            "timeline": {
                "clips": self.audio_project.tracks.iter().map(|t| t.clips.len()).sum::<usize>(),
                "selected_clip": self.audio_project.selected_clip,
                "selected_clip_fx": self.audio_project.selected_clip
                    .and_then(|id| self.audio_project.clip(id))
                    .map(|c| serde_json::json!({
                        "voice_fx": c.voice_fx.is_some(),
                        "fx_source": c.fx_source_file_id.is_some(),
                    })),
            },
        })
    }

    /// Exécute une requête `POST /api/action` sur le thread UI.
    fn dispatch_control(&mut self, body: &serde_json::Value) -> serde_json::Value {
        use serde_json::json;
        let action = body.get("action").and_then(|a| a.as_str()).unwrap_or("");
        match action {
            "ping" => json!({"ok": true, "app": "struidel"}),
            "get_status" => self.control_status_json(),
            "get_code" => {
                let (code, file) = self
                    .active_editor()
                    .map(|e| (e.code.clone(), e.title()))
                    .unwrap_or_else(|| (String::new(), "aucun".to_string()));
                json!({"ok": true, "file": file, "code": code})
            }
            "eval" | "set_code" => {
                let Some(code) = body.get("code").and_then(|c| c.as_str()) else {
                    return json!({"ok": false, "error": "champ \"code\" manquant"});
                };
                if self.editor_tabs.is_empty() {
                    self.new_file();
                }
                self.apply_live_code(code.to_string());
                json!({"ok": true})
            }
            "play" => {
                self.handle_transport_action(ui::TransportAction::Play);
                json!({"ok": true})
            }
            "pause" => {
                self.handle_transport_action(ui::TransportAction::Pause);
                json!({"ok": true})
            }
            "stop" => {
                self.handle_transport_action(ui::TransportAction::Stop);
                json!({"ok": true})
            }
            "set_bpm" => {
                let Some(bpm) = body.get("bpm").and_then(|b| b.as_f64()) else {
                    return json!({"ok": false, "error": "champ \"bpm\" manquant"});
                };
                self.handle_transport_action(ui::TransportAction::SetBpm(bpm));
                json!({"ok": true})
            }
            "sounds" => {
                let root = PathBuf::from(&self.settings.sample_folder);
                let by_bank = crate::sample_loader::available_sounds_by_bank(&root);
                json!({"ok": true, "banks": by_bank})
            }
            // Démarre l'import d'un repo GitHub de sons (async — suivre via
            // get_status → github_import).
            "import_github" => {
                let Some(url) = body.get("url").and_then(|u| u.as_str()) else {
                    return json!({"ok": false, "error": "champ \"url\" manquant"});
                };
                if crate::github_samples::parse_github_url(url).is_none() {
                    return json!({"ok": false, "error": "URL GitHub invalide"});
                }
                self.github_import_url = url.to_string();
                self.start_github_import();
                json!({"ok": true, "started": true})
            }
            "preview" => {
                let name = body.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let bank = body
                    .get("bank")
                    .and_then(|b| b.as_str())
                    .filter(|b| !b.is_empty())
                    .map(|b| b.to_string());
                let Some(engine) = &self.audio else {
                    return json!({"ok": false, "error": "pas de moteur audio"});
                };
                let secs = match bank {
                    Some(b) => engine.play_sample_preview(
                        &SampleKey { bank: Some(b), name: name.to_string() },
                        Some(0),
                    ),
                    None => engine.play_generated_preview(name),
                };
                match secs {
                    Some(s) => json!({"ok": true, "duration_s": s}),
                    None => json!({"ok": false, "error": format!("son inconnu ou muet: {name}")}),
                }
            }
            "param" => self.control_set_param(body),
            // Insère un fichier audio (typiquement une prise dans recordings/)
            // comme clip sur la piste sélectionnée — permet aussi au MCP de
            // poser des prises sur la timeline.
            "add_take_clip" => {
                let Some(path_str) = body.get("path").and_then(|p| p.as_str()) else {
                    return json!({"ok": false, "error": "champ \"path\" manquant"});
                };
                let path = PathBuf::from(path_str);
                if !path.exists() {
                    return json!({"ok": false, "error": format!("fichier absent: {path_str}")});
                }
                let Some(track_id) = self.audio_project.selected_track else {
                    return json!({"ok": false, "error": "pas de piste sélectionnée"});
                };
                match crate::audio_clip_engine::load_audio_file(&path) {
                    Ok(audio) => {
                        let frames = audio.frames() as u64;
                        let secs =
                            audio.frames() as f64 / audio.sample_rate.max(1) as f64;
                        let cycles = secs * self.last_cps.max(0.001);
                        let source = AudioSource {
                            id: 0,
                            path,
                            sample_rate: audio.sample_rate,
                            channels: audio.channels,
                            frames,
                            created_by_recording: true,
                            latency_compensation_frames: 0,
                            peak_path: None,
                        };
                        let source_id = self.audio_project.add_source(source);
                        let start = self
                            .audio_project
                            .tracks
                            .iter()
                            .find(|t| t.id == track_id)
                            .map(|t| {
                                t.clips.iter().map(|c| c.end()).fold(0.0, f64::max)
                            })
                            .unwrap_or(0.0);
                        let clip_id = self.audio_project.insert_recording_clip(
                            track_id,
                            source_id,
                            start,
                            cycles.max(0.25),
                        );
                        self.sync_audio_mixer_runtime();
                        json!({"ok": true, "clip_id": clip_id, "cycles": cycles})
                    }
                    Err(err) => json!({"ok": false, "error": err}),
                }
            }
            "remove_clip" => {
                let Some(clip_id) = body.get("clip_id").and_then(|c| c.as_u64()) else {
                    return json!({"ok": false, "error": "champ \"clip_id\" manquant"});
                };
                if self.audio_project.delete_clip(clip_id as AudioClipId, false) {
                    self.sync_audio_mixer_runtime();
                    json!({"ok": true})
                } else {
                    json!({"ok": false, "error": format!("clip introuvable: {clip_id}")})
                }
            }
            "meter" => {
                let Some(engine) = &self.audio else {
                    return json!({"ok": false, "error": "pas de moteur audio"});
                };
                let m = engine.master_meter();
                json!({
                    "ok": true,
                    "peak": m.peak,
                    "rms": m.rms,
                    "lufs": m.lufs_short,
                    "clipped": m.clipped,
                })
            }
            "errors" => {
                let terminal = self.bridge.terminal_lines();
                let tail: Vec<String> =
                    terminal.iter().rev().take(50).rev().cloned().collect();
                json!({
                    "ok": true,
                    "last_error": self.bridge.last_error(),
                    "terminal": tail,
                })
            }
            other => json!({"ok": false, "error": format!("action inconnue: {other}")}),
        }
    }

    /// `action=param` : règle un paramètre live (même réécriture que le panneau).
    fn control_set_param(&mut self, body: &serde_json::Value) -> serde_json::Value {
        use crate::live_params::{
            find_sound_calls, set_method_where, sound_type, to_written, LiveScope, LIVE_PARAMS,
        };
        use serde_json::json;

        let param_name = body.get("param").and_then(|p| p.as_str()).unwrap_or("");
        let Some(spec) = LIVE_PARAMS
            .iter()
            .find(|p| p.method == param_name || p.label.starts_with(param_name))
        else {
            return json!({
                "ok": false,
                "error": format!("paramètre inconnu: {param_name}"),
                "params": LIVE_PARAMS.iter().map(|p| p.method).collect::<Vec<_>>(),
            });
        };
        let scope = body.get("scope").and_then(|s| s.as_str()).unwrap_or("all");
        let target = body.get("target").and_then(|t| t.as_str()).unwrap_or("");
        let code = self.active_editor().map(|e| e.code.clone()).unwrap_or_default();
        let calls = find_sound_calls(&code);
        if calls.is_empty() {
            return json!({"ok": false, "error": "aucun appel s(...) dans le motif"});
        }
        // Catégories figées hors de `self` pour pouvoir muter l'app ensuite.
        let mut bank_labels: HashMap<String, String> = HashMap::new();
        for c in &calls {
            if let Some(b) = &c.bank {
                let key = self.sample_categories.category_for_bank(b).key;
                bank_labels
                    .entry(b.clone())
                    .or_insert_with(|| self.sample_categories.category_label(&key));
            }
        }
        let bank_category = move |bank: &str| -> String {
            if bank.is_empty() {
                return "Autres".to_string();
            }
            bank_labels
                .get(bank)
                .cloned()
                .unwrap_or_else(|| bank.to_string())
        };
        let scope_enum = match scope {
            "sound" | "son" => LiveScope::Sound,
            "type" => LiveScope::Type,
            "all" | "tous" => LiveScope::All,
            other => {
                return json!({"ok": false, "error": format!("scope inconnu: {other} (sound|type|all)")})
            }
        };
        let keep: Box<dyn Fn(&crate::live_params::SoundCall) -> bool> = match scope_enum {
            LiveScope::Sound => {
                let Some(call) = calls
                    .iter()
                    .find(|c| c.name == target || c.label == target)
                    .cloned()
                else {
                    return json!({
                        "ok": false,
                        "error": format!("son introuvable dans le motif: {target}"),
                        "sons": calls.iter().map(|c| c.label.clone()).collect::<Vec<_>>(),
                    });
                };
                Box::new(move |c| c.start == call.start)
            }
            LiveScope::Type => {
                let t = target.to_string();
                Box::new(move |c| sound_type(c, &bank_category) == t)
            }
            LiveScope::All => Box::new(|_| true),
        };
        // `value` numérique = valeur du slider ; absent ou null = retire la méthode.
        let written = match body.get("value") {
            Some(v) if v.is_number() => {
                Some(to_written(spec, v.as_f64().unwrap_or(spec.default as f64) as f32))
            }
            _ => None,
        };
        let new_code = set_method_where(&code, spec.method, written.as_deref(), &*keep);
        if new_code == code {
            return json!({"ok": false, "error": "aucun appel correspondant dans la portée"});
        }
        self.apply_live_code(new_code);
        json!({"ok": true, "applied": spec.method})
    }

    /// Вкладка «Настройки семплов» (отдельный ContentId рядом с «Семплами»).
    fn samples_settings_content_ui(&mut self, ui: &mut egui::Ui) {
        use std::path::Path;

        ui.heading("Paramètres des samples");
        ui.add_space(8.0);

        ui.label("Dossier principal des samples :");
        ui.monospace(&self.settings.sample_folder);
        if ui.button("📂 Choisir le dossier principal…").clicked() {
            if let Some(path) = rfd::FileDialog::new()
                .set_directory(&self.settings.sample_folder)
                .pick_folder()
            {
                if let Some(p) = path.to_str() {
                    self.settings.sample_folder = p.to_string();
                    settings::save_settings(&self.settings);
                    if let Some(engine) = &self.audio {
                        engine.reload_samples_async(Path::new(&self.settings.sample_folder));
                    }
                }
            }
        }

        ui.separator();
        ui.label("Dossiers de samples supplémentaires :");
        let mut remove_idx: Option<usize> = None;
        for (i, folder) in self.settings.extra_sample_folders.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.monospace(folder);
                if ui.small_button("Supprimer").clicked() {
                    remove_idx = Some(i);
                }
            });
        }
        if let Some(i) = remove_idx {
            self.settings.extra_sample_folders.remove(i);
            settings::save_settings(&self.settings);
            if let Some(engine) = &self.audio {
                engine.reload_samples_async(Path::new(&self.settings.sample_folder));
            }
        }
        if ui.button("➕ Ajouter un dossier…").clicked() {
            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                if let Some(p) = path.to_str() {
                    let s = p.to_string();
                    if !self.settings.extra_sample_folders.contains(&s) {
                        self.settings.extra_sample_folders.push(s);
                        settings::save_settings(&self.settings);
                        if let Some(engine) = &self.audio {
                            engine.reload_samples_async(Path::new(&self.settings.sample_folder));
                        }
                    }
                }
            }
        }

        ui.separator();
        ui.label("Packs de samples standards (Dirt-Samples, VCSL, uzu-*, mridangam, etc.) :");
        if ui.button("🎛 Charger les packs standards dans le cache").clicked()
        {
            let _ = self.bridge.load_samples();
            self.samples_loading = true;
            self.samples_loading_started = Some(Instant::now());
        }
        if self.samples_loading {
            ui.add_space(4.0);
            let prog = self.bridge.download_progress();
            let (label, fraction) = if let Some(p) = &prog {
                if p.total > 0 {
                    let frac = (p.current as f32 / p.total as f32).min(1.0);
                    (
                        format!("{} — {} / {}", p.pack, p.current, p.total),
                        Some(frac),
                    )
                } else {
                    ("Téléchargement…".to_string(), None)
                }
            } else {
                ("En attente du début du téléchargement…".to_string(), None)
            };
            ui.horizontal(|ui| {
                if let Some(f) = fraction {
                    let bar = egui::ProgressBar::new(f).show_percentage();
                    ui.add(bar);
                } else {
                    ui.spinner();
                }
                ui.label(label);
            });
            if ui.button("Arrêter").clicked() {
                let _ = self.bridge.cancel_loadsamples();
                self.samples_loading = false;
                self.samples_loading_started = None;
            }
        }

        ui.add_space(8.0);
        if ui
            .button("🔄 Rescanner la bibliothèque disque")
            .clicked()
        {
            self.sample_browser_entries_cache = None;
            self.sample_browser_filtered_rows = None;
            if let Some(engine) = &self.audio {
                engine.reload_samples_async(Path::new(&self.settings.sample_folder));
            }
        }
    }

    /// Единый таймлайн: один редактор Strudel-блоков с волнами, группами и drag/resize.
    fn timeline_content_ui(&mut self, ui: &mut egui::Ui) {
        self.refresh_active_pattern_document();
        self.sample_categories
            .load_from_path(std::path::Path::new(&self.settings.sample_folder));

        let pattern_document = self.pattern_document.clone();
        let empty_ir = PatternIr::default();
        let code = self
            .active_editor()
            .map(|e| e.code.clone())
            .or_else(|| pattern_document.as_ref().map(|document| document.code.clone()))
            .unwrap_or_default();
        let ir = pattern_document
            .as_ref()
            .map(|document| &document.ir)
            .unwrap_or(&empty_ir);
        let diagnostics = pattern_document
            .as_ref()
            .map(|document| document.diagnostics.as_slice())
            .unwrap_or(&[]);
        let timeline_playing = self.playing && !self.example_playback.is_active();
        // Во время example визуальный курсор замораживается в позиции до запуска.
        let timeline_cursor_time = self
            .pre_example_main_time
            .filter(|_| self.example_playback.is_active())
            .unwrap_or(self.timeline_display_time);
        let audio = self.audio.clone();
        let pattern_waveform: Option<Box<dyn Fn(Option<&str>, &str) -> Option<Arc<[f32]>>>> =
            audio.as_ref().map(|engine| {
                let engine = Arc::clone(engine);
                Box::new(move |bank: Option<&str>, sample: &str| {
                    let key = SampleKey {
                        bank: bank.map(str::to_string),
                        name: sample.to_string(),
                    };
                    engine.get_waveform_preview(&key, None)
                }) as Box<dyn Fn(Option<&str>, &str) -> Option<Arc<[f32]>>>
            });

        let full_h = ui.available_height();
        let strudel_h = (full_h * 0.48).clamp(180.0, 420.0);
        ui.allocate_ui(egui::vec2(ui.available_width(), strudel_h), |ui| {
            let tl_actions = ui::pattern_timeline_ui(
                ui,
                &code,
                ir,
                diagnostics,
                timeline_cursor_time,
                timeline_playing,
                &mut self.timeline_view,
                Some(&self.debugger),
                Some(&self.sample_categories),
                pattern_waveform.as_deref(),
            );
            for action in tl_actions {
                self.handle_timeline_action(action);
            }
        });

        ui.separator();
        self.audio_timeline_content_ui(ui);
    }

    fn handle_editor_debug_action(&mut self, action: ui::EditorAction) {
        match action {
            ui::EditorAction::ToggleSourceBreakpoint(line) => {
                self.debugger.toggle_source_line(line);
            }
            ui::EditorAction::PlayExample(code) => {
                self.reset_example_playback();
                self.bridge.clear_events_cache();
                self.playback_focus_content = ContentId::Workspace;
                if !self.transport_state.playback_source_locked {
                    self.transport_state.playback_source = ui::PlaybackSource::Editor;
                }
                self.arrangement_last_active = None;
                // Замораживаем основную шкалу: сохраняем текущую позицию, чтобы восстановить её после example.
                self.pre_example_main_time = Some(self.current_time);
                self.example_display_time = 0.0;
                let _ = self.bridge.seek(0.0);
                self.seek_ignore_now_frames = 5;
                if let Err(e) = self.bridge.evaluate(&code) {
                    // Откатываем заморозку, раз example не стартовал.
                    self.pre_example_main_time = None;
                    if let Some(ed) = self.active_editor_mut() {
                        ed.eval_error = Some(e);
                        ed.eval_error_line = None;
                        ed.eval_error_column = None;
                    }
                } else {
                    self.bridge.clear_events_cache();
                    self.example_playback.mark_started(code);
                    self.playing = true;
                    self.paused = false;
                    self.sched.set_scheduled_to(-1.0);
                    self.reload_samples_after_eval = true;
                }
            }
            ui::EditorAction::StopExample => {
                if !self.stop_active_example_cleanup() {
                    return;
                }
                self.playing = false;
                self.paused = false;
            }
            ui::EditorAction::ApplyLiveCode => {
                if let Some(code) = self.active_editor().map(|e| e.code.clone()) {
                    self.apply_live_code(code);
                }
            }
        }
    }

    fn handle_timeline_action(&mut self, action: ui::TimelineAction) {
        match action {
            ui::TimelineAction::AddBreakpoint(pos) => {
                self.debugger.add_breakpoint(pos, String::new());
            }
            ui::TimelineAction::ToggleSourceBreakpoint(line) => {
                self.debugger.toggle_source_line(line);
            }
            ui::TimelineAction::RemoveBreakpoint(id) => {
                self.debugger.breakpoints.retain(|bp| bp.id != id);
            }
            ui::TimelineAction::AddWatchpoint(label) => {
                self.debugger.add_watchpoint(label);
            }
            ui::TimelineAction::Seek(pos) => {
                self.do_seek(pos);
            }
            ui::TimelineAction::RemoveSoundToken { sample, begin } => {
                self.remove_sound_token_from_active_code(&sample, begin);
            }
            ui::TimelineAction::ApplyPatternEdit(edit) => {
                self.apply_pattern_edit_to_active_code(edit);
            }
        }
    }

    fn apply_pattern_edit_to_active_code(&mut self, edit: crate::pattern_codegen::PatternEdit) {
        let Some(code) = self.active_editor().map(|editor| editor.code.clone()) else {
            return;
        };
        match crate::pattern_codegen::apply_pattern_edit(&code, self.last_code_revision, edit) {
            Ok(applied) => {
                self.last_code_revision = applied.revision;
                if let Some(editor) = self.active_editor_mut() {
                    editor.code = applied.code.clone();
                    editor.eval_error = None;
                    editor.eval_error_line = None;
                    editor.eval_error_column = None;
                }
                self.sync_engine
                    .set_code(applied.code.clone(), self.last_code_revision);
                self.pattern_document = Some(applied.document);
                self.refresh_native_engine();
            }
            Err(message) => {
                if let Some(editor) = self.active_editor_mut() {
                    editor.eval_error = Some(message);
                    editor.eval_error_line = None;
                    editor.eval_error_column = None;
                }
            }
        }
    }

    fn remove_sound_token_from_active_code(&mut self, sample: &str, begin: f64) {
        let Some(code) = self.active_editor().map(|editor| editor.code.clone()) else {
            return;
        };
        match remove_sound_token_from_code(&code, sample, begin) {
            Some(next_code) => {
                self.last_code_revision = self.last_code_revision.saturating_add(1);
                let revision = self.last_code_revision;
                if let Some(editor) = self.active_editor_mut() {
                    editor.code = next_code.clone();
                    editor.eval_error = None;
                    editor.eval_error_line = None;
                    editor.eval_error_column = None;
                }
                self.sync_engine.set_code(next_code, revision);
                self.refresh_active_pattern_document();
            }
            None => {
                if let Some(editor) = self.active_editor_mut() {
                    editor.eval_error = Some(format!(
                        "Could not map `{sample}` event back to a simple sound(\"...\") token"
                    ));
                    editor.eval_error_line = None;
                    editor.eval_error_column = None;
                }
            }
        }
    }

    fn poll_sync_engine(&mut self) {
        for result in self.sync_engine.poll() {
            match result {
                SyncResult::EditApplied {
                    new_revision,
                    code,
                    ir,
                    ..
                } => {
                    self.last_code_revision = new_revision;
                    if let Some(editor) = self.active_editor_mut() {
                        editor.code = code;
                        editor.eval_error = None;
                        editor.eval_error_line = None;
                        editor.eval_error_column = None;
                    }
                    self.audio_project = ir.to_project_audio_state();
                }
                SyncResult::Conflict { message, .. } | SyncResult::Error { message, .. } => {
                    if let Some(editor) = self.active_editor_mut() {
                        editor.eval_error = Some(message);
                        editor.eval_error_line = None;
                        editor.eval_error_column = None;
                    }
                }
                SyncResult::CodeParsed { .. } => {}
            }
        }
    }

    fn send_arrangement_edit(&mut self, edit: ArrangementEdit) {
        let _ = edit;
    }

    fn audio_timeline_content_ui(&mut self, ui: &mut egui::Ui) {
        let removed = self.audio_project.prune_broken_audio_sources();
        if removed > 0 {
            eprintln!("[Audio] removed {removed} empty recording clip(s)");
        }
        for (id, source) in &self.audio_project.pool.sources {
            if !self.waveform_peaks.contains_key(id) {
                if let Ok(peak) = crate::waveform_cache::load_or_build_peak(&source.path) {
                    self.waveform_peaks.insert(*id, peak);
                }
            }
        }

        let meters = self.audio_recording.meters();
        let recording_active = self.audio_recording.is_recording();
        let finalizing_recording = self.pending_recording_finalize.is_some();
        let input_devices = RecordingController::input_devices_cached();
        // Во время проигрывания example аудио-таймлайн не должен ехать,
        // а курсор остаётся в той же позиции, в которой был до запуска example.
        let timeline_playing = self.playing && !self.example_playback.is_active();
        let display_time = self
            .pre_example_main_time
            .filter(|_| self.example_playback.is_active())
            .unwrap_or(self.current_time);
        let actions = ui::audio_timeline_ui(
            ui,
            &mut self.audio_project,
            display_time,
            timeline_playing,
            recording_active,
            finalizing_recording,
            meters,
            &input_devices,
            &self.settings.voice_fx,
            |source_id| self.waveform_peaks.get(&source_id).cloned(),
            &mut self.audio_timeline_view,
            self.last_cps,
        );
        for action in actions {
            self.handle_audio_timeline_action(action);
        }
    }

    fn mixer_content_ui(&mut self, ui: &mut egui::Ui) {
        ui::mixer_ui(ui, &mut self.audio_project);
        self.sync_engine
            .set_project_audio_state(&self.audio_project);
    }

    fn handle_audio_timeline_action(&mut self, action: ui::AudioTimelineAction) {
        match action {
            ui::AudioTimelineAction::ArmTrack(track_id, armed) => {
                if let Some(track) = self.audio_project.track_mut(track_id) {
                    track.record_state.armed = armed;
                    self.transport_state.record_armed = self
                        .audio_project
                        .tracks
                        .iter()
                        .any(|track| track.record_state.armed);
                }
            }
            ui::AudioTimelineAction::SetMonitoring(track_id, mode) => {
                if let Some(track) = self.audio_project.track_mut(track_id) {
                    track.monitoring = mode;
                }
            }
            ui::AudioTimelineAction::SetInputMode(track_id, mode) => {
                if let Some(track) = self.audio_project.track_mut(track_id) {
                    track.input.channel_mode = mode;
                }
            }
            ui::AudioTimelineAction::StartRecording(track_id) => {
                if let Err(err) = self.start_audio_recording(track_id) {
                    eprintln!("[Recording] start failed: {err}");
                }
            }
            ui::AudioTimelineAction::StopRecording => {
                if let Err(err) = self.stop_audio_recording() {
                    eprintln!("[Recording] stop failed: {err}");
                }
            }
            ui::AudioTimelineAction::SelectTrack(track_id) => {
                self.audio_project.selected_track = Some(track_id);
            }
            ui::AudioTimelineAction::SelectClip(clip_id) => {
                self.audio_project.selected_clip = Some(clip_id);
            }
            ui::AudioTimelineAction::SplitSelected => {
                if let Some(clip_id) = self.audio_project.selected_clip {
                    let at = crate::project_audio::snap_cycle(
                        self.current_time,
                        &self.audio_project.snap,
                    );
                    self.send_arrangement_edit(ArrangementEdit::SplitClip {
                        clip_id: stable_clip_id(clip_id),
                        at,
                    });
                    let _ = self.audio_project.split_clip(clip_id, at);
                }
            }
            ui::AudioTimelineAction::SplitClip(clip_id, at) => {
                self.send_arrangement_edit(ArrangementEdit::SplitClip {
                    clip_id: stable_clip_id(clip_id),
                    at,
                });
                let _ = self.audio_project.split_clip(clip_id, at);
            }
            ui::AudioTimelineAction::DuplicateSelected => {
                if let Some(clip_id) = self.audio_project.selected_clip {
                    self.send_arrangement_edit(ArrangementEdit::DuplicateClip {
                        clip_id: stable_clip_id(clip_id),
                    });
                    let _ = self.audio_project.duplicate_clip(clip_id);
                }
            }
            ui::AudioTimelineAction::DeleteSelected => {
                if let Some(clip_id) = self.audio_project.selected_clip {
                    let ripple = self.audio_project.snap.ripple_edit;
                    self.send_arrangement_edit(ArrangementEdit::DeleteClip {
                        clip_id: stable_clip_id(clip_id),
                    });
                    let _ = self.audio_project.delete_clip(clip_id, ripple);
                }
            }
            ui::AudioTimelineAction::MoveClip(clip_id, start) => {
                self.send_arrangement_edit(ArrangementEdit::MoveClip {
                    clip_id: stable_clip_id(clip_id),
                    new_start: start,
                });
                let _ = self.audio_project.move_clip(clip_id, start);
            }
            ui::AudioTimelineAction::TrimClipStart(clip_id, start) => {
                self.send_arrangement_edit(ArrangementEdit::TrimClipStart {
                    clip_id: stable_clip_id(clip_id),
                    new_start: start,
                });
                let _ = self.audio_project.trim_clip(clip_id, Some(start), None);
            }
            ui::AudioTimelineAction::TrimClipEnd(clip_id, end) => {
                self.send_arrangement_edit(ArrangementEdit::TrimClipEnd {
                    clip_id: stable_clip_id(clip_id),
                    new_end: end,
                });
                let _ = self.audio_project.trim_clip(clip_id, None, Some(end));
            }
            ui::AudioTimelineAction::SetFade(clip_id, edge, length, curve) => {
                self.send_arrangement_edit(ArrangementEdit::SetFade {
                    clip_id: stable_clip_id(clip_id),
                    edge,
                    length,
                    curve,
                });
                let _ = self
                    .audio_project
                    .set_clip_fade(clip_id, edge, length, Some(curve));
            }
            ui::AudioTimelineAction::SetStretch(clip_id, stretch) => {
                self.send_arrangement_edit(ArrangementEdit::SetStretch {
                    clip_id: stable_clip_id(clip_id),
                    stretch: stretch.clone(),
                });
                let _ = self.audio_project.set_clip_stretch(clip_id, stretch);
            }
            ui::AudioTimelineAction::SetPitch(clip_id, pitch) => {
                self.send_arrangement_edit(ArrangementEdit::SetPitch {
                    clip_id: stable_clip_id(clip_id),
                    pitch: pitch.clone(),
                });
                let _ = self.audio_project.set_clip_pitch(clip_id, pitch);
            }
            ui::AudioTimelineAction::SetVoiceFx(vfx) => {
                self.settings.voice_fx = vfx;
                settings::save_settings(&self.settings);
            }
            ui::AudioTimelineAction::SetClipVoiceFx(clip_id, fx) => {
                if let Some(clip) = self.audio_project.clip_mut(clip_id) {
                    clip.voice_fx = Some(fx);
                }
                self.request_clip_fx_render(clip_id);
                self.sync_audio_mixer_runtime();
            }
            ui::AudioTimelineAction::SetClipGainDb(clip_id, db) => {
                if let Some(clip) = self.audio_project.clip_mut(clip_id) {
                    clip.gain = crate::mixer_dsp::db_to_gain(db);
                }
                self.sync_audio_mixer_runtime();
            }
            ui::AudioTimelineAction::SetClipFadeSecs(clip_id, edge, secs, curve) => {
                let cycles = (secs.max(0.0) as f64) * self.last_cps.max(0.001);
                let _ = self
                    .audio_project
                    .set_clip_fade(clip_id, edge, cycles, Some(curve));
                self.sync_audio_mixer_runtime();
            }
            ui::AudioTimelineAction::BounceSession => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("WAV", &["wav"])
                    .set_file_name("struidel-bounce.wav")
                    .save_file()
                {
                    let _ = self.bounce_audio_selection(&path);
                }
            }
            ui::AudioTimelineAction::ExportPatterns => {
                self.export_dialog.open = true;
                if self.export_dialog.duration_cycles.is_empty() {
                    self.export_dialog.duration_cycles = "8".to_string();
                }
                if self.export_dialog.output_path.is_empty() {
                    self.export_dialog.output_path = std::env::var("HOME")
                        .map(|h| {
                            std::path::PathBuf::from(h)
                                .join("export.wav")
                                .to_string_lossy()
                                .into_owned()
                        })
                        .unwrap_or_else(|_| "export.wav".to_string());
                }
            }
            ui::AudioTimelineAction::Undo => {
                let _ = self.audio_project.undo();
            }
            ui::AudioTimelineAction::Redo => {
                let _ = self.audio_project.redo();
            }
            ui::AudioTimelineAction::SetCursorMode(mode) => {
                self.audio_project.cursor_mode = mode;
            }
        }
    }

    fn start_audio_recording(
        &mut self,
        track_id: crate::project_audio::AudioTrackId,
    ) -> Result<(), String> {
        if self.audio_recording.is_recording() {
            return Ok(());
        }
        let session_id = self.audio_project.alloc_id();
        let directory = self.recordings_directory();
        let Some(track) = self.audio_project.track_mut(track_id) else {
            return Err("record track not found".to_string());
        };
        track.record_state.armed = true;
        let input = track.input.clone();
        // Casque Bluetooth : force HFP comme stt_toggle.sh, sinon la capture gèle en ~1 s.
        self.pending_bt_profile_restore = force_hfp_if_bluetooth_input(input.device_name.as_deref());
        if self.pending_bt_profile_restore.is_some() {
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
        let started = match self.audio_recording.start(RecordingRequest {
            session_id,
            directory,
            input,
            ring_capacity_samples: None,
        }) {
            Ok(started) => started,
            Err(err) => {
                self.finish_bt_profile_restore();
                return Err(err);
            }
        };
        let started_at_cycle = if self.transport_state.punch_enabled {
            self.transport_state.punch_in.max(0.0)
        } else {
            self.current_time
        };
        track.record_state.active_session = Some(RecordingSessionInfo {
            session_id: started.session_id,
            started_at_cycle,
            current_cycle: started_at_cycle,
            file_path: started.file_path,
            sample_rate: started.sample_rate,
            channels: started.channels,
        });
        self.transport_state.record_armed = true;
        Ok(())
    }

    fn stop_audio_recording(&mut self) -> Result<(), String> {
        if !self.audio_recording.is_recording() {
            return Ok(());
        }
        if self.pending_recording_finalize.is_some() {
            return Ok(());
        }
        let track_id = self
            .audio_project
            .tracks
            .iter()
            .find(|track| track.record_state.active_session.is_some())
            .map(|track| track.id)
            .ok_or_else(|| "record track not found".to_string())?;
        let frozen_end_cycle = self.current_time;
        let started_at_cycle = self
            .audio_project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.record_state.active_session.as_ref())
            .map(|session| session.started_at_cycle)
            .unwrap_or(frozen_end_cycle);
        if let Some(track) = self.audio_project.track_mut(track_id) {
            if let Some(session) = track.record_state.active_session.as_mut() {
                session.current_cycle = frozen_end_cycle.max(session.started_at_cycle);
            }
        }
        let rx = match self.audio_recording.stop_async() {
            Ok(rx) => rx,
            Err(err) => {
                self.finish_bt_profile_restore();
                return Err(err);
            }
        };
        self.pending_recording_finalize = Some((
            rx,
            RecordingStopContext {
                track_id,
                started_at_cycle,
                frozen_end_cycle,
            },
        ));
        Ok(())
    }

    fn poll_pending_recording_finalize(&mut self) {
        let Some((rx, context)) = self.pending_recording_finalize.as_ref() else {
            return;
        };
        let Ok(result) = rx.try_recv() else {
            return;
        };
        let context = *context;
        self.pending_recording_finalize = None;
        match result {
            Ok(finalize) => {
                if let Err(err) = self.apply_recording_finalize(finalize, context) {
                    eprintln!("[Recording] finalize failed: {err}");
                    self.finish_bt_profile_restore();
                }
            }
            Err(err) => {
                eprintln!("[Recording] stop failed: {err}");
                self.finish_bt_profile_restore();
            }
        }
    }

    fn apply_recording_finalize(
        &mut self,
        finalize: RecordingFinalize,
        context: RecordingStopContext,
    ) -> Result<(), String> {
        let summary = finalize.summary;
        let track_id = context.track_id;
        if summary.frames == 0 {
            let _ = std::fs::remove_file(&summary.file_path);
            eprintln!(
                "[Recording] discarded empty take {:?} — no input signal (use Bounce WAV for Strudel output)",
                summary.file_path
            );
            if let Some(track) = self.audio_project.track_mut(track_id) {
                track.record_state.active_session = None;
                track.level_peak = 0.0;
                track.clipped = false;
            }
            self.transport_state.record_armed = self
                .audio_project
                .tracks
                .iter()
                .any(|track| track.record_state.armed);
            self.finish_bt_profile_restore();
            return Ok(());
        }
        let source_id = self.audio_project.alloc_id();
        let mut source = summary.to_audio_source(source_id, summary.input_latency_frames);
        let peak = finalize
            .peak
            .or_else(|| crate::waveform_cache::load_or_build_peak(&source.path).ok());
        if let Some(peak) = peak {
            let peak_path = crate::waveform_cache::peak_path_for_audio(&source.path);
            source.peak_path = Some(peak_path.clone());
            self.audio_project
                .pool
                .peak_files
                .insert(source_id, peak_path);
            self.waveform_peaks.insert(source_id, peak);
        } else {
            eprintln!(
                "[Recording] waveform peak not built for {:?}",
                source.path
            );
        }
        let recorded_path = source.path.clone();
        self.audio_project.add_source(source);
        let started_at = context.started_at_cycle;
        let frozen_end = context.frozen_end_cycle;
        let timeline_length_cycles = (frozen_end - started_at).max(0.0);
        let wall_clock_length_cycles =
            summary.duration.as_secs_f64() * self.last_cps.max(0.000_001);
        let frames_length_cycles = if summary.sample_rate > 0 && summary.frames > 0 {
            (summary.frames as f64 / summary.sample_rate as f64) * self.last_cps.max(0.000_001)
        } else {
            0.0
        };
        let raw_length_cycles = [wall_clock_length_cycles, frames_length_cycles, timeline_length_cycles]
            .into_iter()
            .find(|len| *len > 0.001)
            .unwrap_or(0.001);
        let length_cycles = if self.transport_state.punch_enabled {
            (self.transport_state.punch_out - self.transport_state.punch_in)
                .max(0.001)
                .min(raw_length_cycles.max(0.001))
        } else {
            raw_length_cycles.max(0.001)
        };
        let latency_seconds = if summary.sample_rate > 0 {
            summary.input_latency_frames.max(0) as f64 / summary.sample_rate as f64
        } else {
            0.0
        };
        let (compensated_start, source_offset) =
            compensated_recording_placement(started_at, latency_seconds, self.last_cps);
        let before = crate::project_audio::ProjectAudioSnapshot::from_state(&self.audio_project);
        self.apply_recording_mode(track_id, compensated_start, length_cycles);
        let new_clip_id = self.audio_project.insert_recording_clip(
            track_id,
            source_id,
            compensated_start,
            length_cycles,
        );
        if let Some(clip_id) = new_clip_id {
            if let Some(clip) = self.audio_project.clip_mut(clip_id) {
                clip.source_offset = source_offset;
            }
            self.apply_loop_recording_takes(
                clip_id,
                source_id,
                compensated_start,
                source_offset,
                length_cycles,
            );
            self.apply_voice_fx_to_recording(track_id, clip_id, &recorded_path);
        }
        self.audio_project
            .push_undo_snapshot("Record audio", before);
        if let Some(track) = self.audio_project.track_mut(track_id) {
            track.record_state.active_session = None;
            track.level_peak = summary.peak;
            track.clipped = summary.clipped;
        }
        self.transport_state.record_armed = self
            .audio_project
            .tracks
            .iter()
            .any(|track| track.record_state.armed);
        self.sync_engine
            .set_project_audio_state(&self.audio_project);
        self.finish_bt_profile_restore();
        Ok(())
    }

    /// Restaure le profil Bluetooth sauvegardé au début de l'enregistrement,
    /// puis recrée l'engine : le stream de sortie ALSA meurt (POLLERR) quand
    /// le sink A2DP disparaît pendant le profil HFP — sans rebuild, l'app
    /// reste muette après un enregistrement sur casque.
    fn finish_bt_profile_restore(&mut self) {
        let saved = self.pending_bt_profile_restore.take();
        if saved.is_none() {
            return;
        }
        restore_bt_profile(saved);
        // Laisse PipeWire recréer le sink A2DP avant de rouvrir le device —
        // le délai + l'init se font sur le thread audio-init, pas sur l'UI.
        if self.settings.audio_enabled {
            self.audio = None;
            self.request_audio_engine(400);
        }
    }

    /// Lance l'init de l'engine audio en arrière-plan (`delay_ms` d'attente
    /// préalable, ex. laisser PipeWire recréer le sink A2DP). Résultat dans
    /// `pending_audio_engine`, ramassé par `poll_pending_audio_engine`.
    fn request_audio_engine(&mut self, delay_ms: u64) {
        if self.audio.is_some() || self.pending_audio_engine.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let ok = std::thread::Builder::new()
            .name("audio-init".to_string())
            .spawn(move || {
                if delay_ms > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                }
                let _ = tx.send(AudioEngine::new().map(Arc::new));
            })
            .is_ok();
        if ok {
            self.pending_audio_engine = Some(rx);
        }
    }

    /// Un stream dont le device est mort (BT off, PipeWire relancé…) accumule
    /// des POLLERR sans jamais guérir : on le jette et on reconstruit l'engine
    /// en arrière-plan — même chemin que l'init, rien sur le thread UI.
    fn poll_dead_audio_stream(&mut self) {
        let Some(engine) = &self.audio else {
            return;
        };
        if engine.stream_error_count() < 50 {
            return;
        }
        eprintln!("[audio] stream mort ({} erreurs) — reconstruction de l'engine",
            engine.stream_error_count());
        self.audio = None; // drop du stream → fin du callback en erreur
        self.request_audio_engine(750);
    }

    /// Ramasse l'engine audio prête (init faite en arrière-plan).
    fn poll_pending_audio_engine(&mut self) {
        let Some(rx) = &self.pending_audio_engine else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(engine)) => {
                eprintln!("[audio] engine prête (init async)");
                self.audio = Some(engine);
                self.pending_audio_engine = None;
            }
            Ok(Err(e)) => {
                eprintln!("[audio] init engine échouée: {e}");
                self.pending_audio_engine = None;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending_audio_engine = None;
            }
        }
    }

    /// Cache des sources dérivées FX : caché dans la racine des samples
    /// (`.fx-cache/` — les dossiers cachés ne sont pas indexés en banque).
    fn clip_fx_cache_dir(&self) -> PathBuf {
        self.root_folder
            .clone()
            .unwrap_or_else(|| PathBuf::from(&self.settings.sample_folder))
            .join(".fx-cache")
    }

    /// Démarre un rendu des FX « lourds » du clip en arrière-plan, ou revient
    /// à la source d'origine si tout est à off. Jamais de DSP sur le thread UI.
    fn request_clip_fx_render(&mut self, clip_id: AudioClipId) {
        let Some(clip) = self.audio_project.clip(clip_id) else {
            return;
        };
        let Some(fx) = clip.voice_fx.clone() else {
            return;
        };
        let Some(src) = self
            .audio_project
            .pool
            .sources
            .get(&clip.source_file_id)
        else {
            return;
        };
        if !fx.needs_render(src.sample_rate) {
            if let Some(clip) = self.audio_project.clip_mut(clip_id) {
                clip.fx_source_file_id = None;
            }
            return;
        }
        let rev = {
            let r = self.clip_fx_rev.entry(clip_id).or_insert(0);
            *r = r.saturating_add(1);
            *r
        };
        let path = src.path.clone();
        let out_dir = self.clip_fx_cache_dir();
        let (tx, rx) = mpsc::channel();
        let _ = std::thread::Builder::new()
            .name("clip-fx-render".into())
            .spawn(move || {
                let res = crate::audio_clip_engine::load_audio_file(&path)
                    .and_then(|mut audio| {
                        crate::voice_fx::render_clip_voice_fx(&mut audio, &fx);
                        std::fs::create_dir_all(&out_dir)
                            .map_err(|e| format!("create {:?}: {e}", out_dir))?;
                        let out = out_dir.join(format!("clip-{clip_id}-r{rev}.wav"));
                        crate::audio_clip_engine::write_wav_float(
                            &out,
                            &audio.samples,
                            audio.sample_rate,
                            audio.channels,
                        )?;
                        Ok((out, audio.sample_rate, audio.channels, audio.frames() as u64))
                    });
                let _ = tx.send((clip_id, rev, res));
            });
        self.pending_clip_fx_renders.push(rx);
    }

    /// Installe les sources dérivées rendues (les rendus périmés sont jetés).
    fn poll_clip_fx_renders(&mut self) {
        let mut i = 0;
        while i < self.pending_clip_fx_renders.len() {
            match self.pending_clip_fx_renders[i].try_recv() {
                Err(mpsc::TryRecvError::Empty) => {
                    i += 1;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending_clip_fx_renders.remove(i);
                }
                Ok((clip_id, rev, res)) => {
                    self.pending_clip_fx_renders.remove(i);
                    if self.clip_fx_rev.get(&clip_id) != Some(&rev) {
                        continue; // un réglage plus récent a relancé un rendu
                    }
                    match res {
                        Ok((path, sample_rate, channels, frames)) => {
                            let peak =
                                crate::waveform_cache::load_or_build_peak(&path).ok();
                            let peak_path = peak
                                .as_ref()
                                .map(|_| crate::waveform_cache::peak_path_for_audio(&path));
                            let source = AudioSource {
                                id: 0,
                                path,
                                sample_rate,
                                channels,
                                frames,
                                created_by_recording: false,
                                latency_compensation_frames: 0,
                                peak_path: peak_path.clone(),
                            };
                            let id = self.audio_project.add_source(source);
                            if let (Some(peak), Some(peak_path)) = (peak, peak_path) {
                                self.audio_project.pool.peak_files.insert(id, peak_path);
                                self.waveform_peaks.insert(id, peak);
                            }
                            if let Some(clip) = self.audio_project.clip_mut(clip_id) {
                                clip.fx_source_file_id = Some(id);
                            }
                            self.sync_audio_mixer_runtime();
                        }
                        Err(err) => {
                            eprintln!("[ClipFx] rendu clip {clip_id} échoué : {err}");
                        }
                    }
                }
            }
        }
    }

    fn apply_loop_recording_takes(
        &mut self,
        clip_id: crate::project_audio::AudioClipId,
        source_id: crate::project_audio::AudioSourceId,
        started_at: f64,
        base_source_offset: f64,
        length_cycles: f64,
    ) {
        if !self.transport_state.loop_enabled {
            return;
        }
        let loop_len = (self.transport_state.loop_end - self.transport_state.loop_start).max(0.0);
        if loop_len <= 0.000_001 || length_cycles <= loop_len * 1.05 {
            return;
        }
        let take_count = (length_cycles / loop_len).ceil() as u32;
        let mut next_take_id = Vec::new();
        for _ in 0..take_count {
            next_take_id.push(self.audio_project.alloc_id());
        }
        if let Some(clip) = self.audio_project.clip_mut(clip_id) {
            clip.timeline_position = self.transport_state.loop_start.max(started_at);
            clip.length = loop_len;
            clip.take_lanes.clear();
            for (idx, take_id) in next_take_id.into_iter().enumerate() {
                clip.take_lanes.push(crate::project_audio::TakeLane {
                    id: take_id,
                    source_file_id: source_id,
                    loop_index: idx as u32,
                    timeline_position: clip.timeline_position,
                    source_offset: base_source_offset + idx as f64 * loop_len,
                    length: loop_len,
                    name: Some(format!("Take {}", idx + 1)),
                    muted: idx + 1 != take_count as usize,
                });
            }
            clip.selected_take = clip.take_lanes.last().map(|take| take.id);
        }
    }

    fn apply_recording_mode(
        &mut self,
        track_id: crate::project_audio::AudioTrackId,
        start: f64,
        length: f64,
    ) {
        let Some(track) = self.audio_project.track_mut(track_id) else {
            return;
        };
        let end = start + length;
        if matches!(
            track.record_state.mode,
            crate::project_audio::RecordMode::Replace
        ) {
            track
                .clips
                .retain(|clip| clip.end() <= start || clip.timeline_position >= end);
        }
    }

    fn recordings_directory(&self) -> PathBuf {
        self.root_folder
            .clone()
            .unwrap_or_else(|| PathBuf::from(&self.settings.sample_folder))
            .join("recordings")
    }

    /// Applique le preset « FX voix » à la prise qui vient d'être insérée :
    /// pitch/fades en paramètres non destructifs du clip, passe-haut + compresseur
    /// en inserts de la piste, puis export « baked » vers la banque de samples.
    fn apply_voice_fx_to_recording(
        &mut self,
        track_id: crate::project_audio::AudioTrackId,
        clip_id: crate::project_audio::AudioClipId,
        recorded_path: &Path,
    ) {
        let vfx = self.settings.voice_fx.clone();
        if vfx.enabled {
            let cps = self.last_cps.max(0.000_001);
            if let Some(clip) = self.audio_project.clip_mut(clip_id) {
                clip.pitch.semitones = vfx.pitch_semitones;
                clip.fade_in = Fade {
                    length: (vfx.fade_in_secs as f64 * cps).min(clip.length),
                    curve: FadeCurve::Linear,
                };
                clip.fade_out = Fade {
                    length: (vfx.fade_out_secs as f64 * cps).min(clip.length),
                    curve: FadeCurve::Exponential,
                };
            }
            if let Some(track) = self.audio_project.track_mut(track_id) {
                upsert_insert(
                    &mut track.channel.inserts,
                    FxSlot::high_pass(vfx.highpass_hz.max(10.0)),
                );
                if vfx.comp_enabled {
                    upsert_insert(
                        &mut track.channel.inserts,
                        FxSlot::compressor(
                            vfx.comp_threshold_db,
                            vfx.comp_ratio,
                            vfx.comp_makeup_db,
                        ),
                    );
                }
            }
        }
        let bank = vfx.export_bank.trim().to_string();
        if !bank.is_empty() {
            let input = recorded_path.to_path_buf();
            let root = PathBuf::from(&self.settings.sample_folder);
            let (tx, rx) = mpsc::channel();
            thread::spawn(move || {
                let result = crate::voice_fx::render_take_to_bank(&input, &root, &bank, &vfx);
                let _ = tx.send(result);
            });
            self.pending_voice_export = Some(rx);
        }
    }

    fn poll_pending_voice_export(&mut self) {
        let Some(rx) = self.pending_voice_export.as_ref() else {
            return;
        };
        let Ok(result) = rx.try_recv() else {
            return;
        };
        self.pending_voice_export = None;
        match result {
            Ok(path) => {
                eprintln!("[VoiceFX] take exported to sample bank: {:?}", path);
                self.sample_browser_entries_cache = None;
                self.sample_browser_filtered_rows = None;
                if let Some(engine) = &self.audio {
                    engine.reload_samples_async(Path::new(&self.settings.sample_folder));
                }
            }
            Err(err) => eprintln!("[VoiceFX] export failed: {err}"),
        }
    }

    fn update_audio_recording_state(&mut self) {
        if self.pending_recording_finalize.is_some() {
            return;
        }
        let meters = self.audio_recording.meters();
        for track in &mut self.audio_project.tracks {
            if let Some(session) = track.record_state.active_session.as_mut() {
                session.current_cycle = self.current_time.max(session.started_at_cycle);
                track.level_peak = meters.peak;
                track.clipped = meters.clipped;
            }
        }
    }

    fn sync_audio_mixer_runtime(&mut self) {
        if let Some(engine) = &self.audio {
            engine.update_mixer_snapshot(&self.audio_project);
            engine.update_daw_snapshot(&self.audio_project);
            engine.update_daw_transport(
                self.playing,
                self.current_time,
                self.last_cps.max(0.000_001),
                self.reverse,
            );
            self.audio_project.master.meter = engine.master_meter();
            for (channel_id, meter) in engine.daw_meters() {
                if let Some(channel) = self.audio_project.mixer_channel_mut(channel_id) {
                    channel.meter = meter;
                }
            }
        }
    }

    fn chain_seed_content_ui(&mut self, ui: &mut egui::Ui) {
        self.poll_chain_seed_fetch();
        let actions = ui::chain_seed_ui(ui, &mut self.chain_seed);
        for action in actions {
            self.handle_chain_seed_action(action);
        }
    }

    fn hydra_content_ui(&mut self, ui: &mut egui::Ui) {
        let has_seed = !self.chain_seed.seed_hex.is_empty();
        let actions =
            ui::hydra_studio_ui(ui, &mut self.hydra_studio, &self.visual_stage, has_seed);
        for action in actions {
            self.handle_hydra_action(action);
        }
        ui.add_space(8.0);
        if ui
            .button("Open as floating window")
            .on_hover_text("Dedicated Hydra editor window")
            .clicked()
        {
            self.hydra_studio.window_open = true;
        }
    }

    fn ai_pattern_content_ui(&mut self, ui: &mut egui::Ui) {
        self.poll_ai_pattern();
        let actions = ui::ai_pattern_ui(ui, &mut self.ai_pattern);
        for action in actions {
            self.handle_ai_pattern_action(action);
        }
        ui.add_space(8.0);
        if ui.button("Open as floating window").clicked() {
            self.ai_pattern.window_open = true;
        }
    }

    fn poll_chain_seed_fetch(&mut self) {
        let Some(rx) = self.chain_seed_fetch_rx.as_ref() else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(crate::chain_seed::ChainSeedEvent::Status { phase, detail }) => {
                    self.chain_seed.phase = phase.clone();
                    self.chain_seed.status = format!("{phase} — {detail}");
                }
                Ok(crate::chain_seed::ChainSeedEvent::Done(Ok(result))) => {
                    self.chain_seed_fetch_rx = None;
                    self.chain_seed.busy = false;
                    self.chain_seed.phase.clear();
                    let from_live = self.chain_seed.live_poll_pending;
                    self.chain_seed.live_poll_pending = false;
                    let unchanged = from_live && result.seed_hex == self.chain_seed.base_seed_hex;

                    if unchanged {
                        self.chain_seed.status =
                            format!("Live · waiting for new block · {}", result.source_label);
                        break;
                    }

                    self.chain_seed.seed_hex = result.seed_hex.clone();
                    self.chain_seed.base_seed_hex = result.seed_hex.clone();
                    self.chain_seed.source_label = result.source_label.clone();
                    self.chain_seed.meta_note = result.meta_note.clone().unwrap_or_default();
                    self.chain_seed.pipeline = result.pipeline;
                    self.chain_seed.last_generator = result.generator.clone();
                    self.chain_seed.last_bpm = result.bpm;
                    self.chain_seed.last_hydra_code = result.hydra_code.clone();
                    crate::chain_seed::push_seed_history(
                        &mut self.chain_seed,
                        &result.source_label,
                        &result.seed_hex,
                    );
                    if from_live {
                        self.chain_seed.live_tick =
                            self.chain_seed.live_tick.saturating_add(1);
                    }

                    let base = self
                        .active_editor()
                        .map(|e| e.code.clone())
                        .filter(|c| !c.trim().is_empty())
                        .or_else(|| {
                            if !result.code.trim().is_empty() {
                                Some(result.code.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_else(|| crate::chain_seed::demo_pattern_code().to_string());
                    let injected = crate::chain_seed::inject_seed_math(
                        &base,
                        &result.seed_hex,
                        &result.source_label,
                        result.meta_note.as_deref(),
                    );
                    self.chain_seed.preview_code = injected;
                    self.broadcast_seed_pulse(&result.seed_hex, from_live);

                    if self.chain_seed.push_hydra && !result.hydra_code.is_empty() {
                        self.hydra_studio.code = result.hydra_code.clone();
                        self.hydra_studio.status =
                            format!("Sketch from seed · {}", result.source_label);
                    }

                    if from_live && self.chain_seed.live_apply {
                        self.apply_code_to_editor(self.chain_seed.preview_code.clone());
                    }
                    if from_live && self.chain_seed.push_hydra && !result.hydra_code.is_empty() {
                        self.visual_stage.ensure_running();
                        self.visual_stage.broadcast_json(&serde_json::json!({
                            "type": "eval",
                            "code": result.hydra_code,
                            "followMusic": self.hydra_studio.follow_music,
                        }));
                    }

                    self.chain_seed.status = if from_live {
                        format!(
                            "Live tick {} · {} · {} BPM",
                            self.chain_seed.live_tick, result.source_label, result.bpm
                        )
                    } else {
                        format!(
                            "Ready · inject preview · {} · {} BPM · {}",
                            result.source_label, result.bpm, result.generator
                        )
                    };
                    break;
                }
                Ok(crate::chain_seed::ChainSeedEvent::Done(Err(e))) => {
                    self.chain_seed_fetch_rx = None;
                    self.chain_seed.busy = false;
                    self.chain_seed.phase.clear();
                    self.chain_seed.live_poll_pending = false;
                    self.chain_seed.status = format!("Failed: {e}");
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.chain_seed_fetch_rx = None;
                    self.chain_seed.busy = false;
                    self.chain_seed.phase.clear();
                    if self.chain_seed.status.contains("Fetching")
                        || self.chain_seed.status.contains("Generating")
                    {
                        self.chain_seed.status = "Worker disconnected".into();
                    }
                    break;
                }
            }
        }
    }

    fn poll_ai_pattern(&mut self) {
        let Some(rx) = self.ai_pattern_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok((code, bpm, generator))) => {
                self.ai_pattern_rx = None;
                self.ai_pattern.busy = false;
                self.ai_pattern.preview_code = code;
                self.ai_pattern.status = format!("Ready · {bpm} BPM · {generator}");
            }
            Ok(Err(e)) => {
                self.ai_pattern_rx = None;
                self.ai_pattern.busy = false;
                self.ai_pattern.status = format!("Failed: {e}");
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                self.ai_pattern_rx = None;
                self.ai_pattern.busy = false;
                self.ai_pattern.status = "Worker disconnected".into();
            }
        }
    }

    fn handle_hydra_action(&mut self, action: ui::HydraAction) {
        match action {
            ui::HydraAction::OpenStage => {
                self.visual_stage.open_in_browser();
                self.hydra_studio.status =
                    format!("Stage opened · {}", self.visual_stage.stage_url());
            }
            ui::HydraAction::Eval => {
                let code = self.hydra_studio.code.clone();
                if code.trim().is_empty() {
                    self.hydra_studio.status = "Nothing to eval".into();
                    return;
                }
                self.visual_stage.ensure_running();
                if self.visual_stage.client_count() == 0 {
                    self.visual_stage.open_in_browser();
                }
                self.visual_stage.broadcast_json(&serde_json::json!({
                    "type": "eval",
                    "code": code,
                    "followMusic": self.hydra_studio.follow_music,
                }));
                self.hydra_studio.status = if self.visual_stage.client_count() == 0 {
                    "Eval sent — waiting for Stage client…".into()
                } else {
                    "Eval sent to Stage".into()
                };
            }
            ui::HydraAction::LoadPreset(i) => {
                if let Some(p) = ui::HYDRA_PRESETS.get(i) {
                    self.hydra_studio.preset_idx = i;
                    self.hydra_studio.code = p.code.into();
                    self.hydra_studio.status = format!("Loaded preset “{}”", p.name);
                }
            }
            ui::HydraAction::FromChainSeed => {
                if self.chain_seed.seed_hex.is_empty() {
                    self.hydra_studio.status =
                        "No Chain Seed yet — Fetch an NFT/block first".into();
                    return;
                }
                let sketch = if !self.chain_seed.last_hydra_code.is_empty() {
                    self.chain_seed.last_hydra_code.clone()
                } else {
                    crate::chain_seed::generate_hydra_sketch(
                        &self.chain_seed.seed_hex,
                        &self.chain_seed.source_label,
                    )
                };
                self.hydra_studio.code = sketch.clone();
                self.chain_seed.last_hydra_code = sketch;
                self.hydra_studio.status = format!(
                    "Unusual sketch from {} — Eval to run",
                    self.chain_seed.source_label
                );
            }
        }
    }

    fn handle_ai_pattern_action(&mut self, action: ui::AiPatternAction) {
        match action {
            ui::AiPatternAction::SavePrefs { url, model } => {
                self.settings.ollama_url = url.clone();
                self.settings.ollama_model = model.clone();
                self.ai_pattern.ollama_url = url.clone();
                self.ai_pattern.ollama_model = model.clone();
                self.chain_seed.ollama_url = url;
                self.chain_seed.ollama_model = model;
                settings::save_settings(&self.settings);
                self.ai_pattern.status = "Ollama prefs saved".into();
            }
            ui::AiPatternAction::Apply => {
                let code = self.ai_pattern.preview_code.clone();
                if code.trim().is_empty() {
                    self.ai_pattern.status = "Nothing to apply".into();
                    return;
                }
                self.apply_code_to_editor(code);
                self.ai_pattern.status = "Applied to Workspace editor".into();
            }
            ui::AiPatternAction::Generate => {
                if self.ai_pattern.busy {
                    return;
                }
                self.ai_pattern.busy = true;
                self.ai_pattern.status = "Generating with Ollama…".into();
                let prompt = self.ai_pattern.prompt.clone();
                let url = self.ai_pattern.ollama_url.clone();
                let model = self.ai_pattern.ollama_model.clone();
                let (tx, rx) = mpsc::channel();
                self.ai_pattern_rx = Some(rx);
                thread::spawn(move || {
                    let result = crate::chain_seed::generate_from_prompt(&prompt, &url, &model);
                    let _ = tx.send(result);
                });
            }
        }
    }

    fn handle_chain_seed_action(&mut self, action: ui::ChainSeedAction) {
        match action {
            ui::ChainSeedAction::SaveRpcUrl(url) => {
                self.settings.eth_rpc_url = url.clone();
                self.chain_seed.rpc_url = url;
                settings::save_settings(&self.settings);
                self.chain_seed.status = "RPC URL saved".into();
            }
            ui::ChainSeedAction::Recall(idx) => {
                let Some((lab, seed)) = self.chain_seed.seed_history.get(idx).cloned() else {
                    return;
                };
                self.chain_seed.seed_hex = seed.clone();
                self.chain_seed.base_seed_hex = seed.clone();
                self.chain_seed.source_label = lab.clone();
                let hydra = crate::chain_seed::generate_hydra_sketch(&seed, &lab);
                self.chain_seed.last_hydra_code = hydra.clone();
                self.chain_seed.last_bpm = crate::chain_seed::bpm_from_seed(&seed);
                let base = self
                    .active_editor()
                    .map(|e| e.code.clone())
                    .filter(|c| !c.trim().is_empty())
                    .unwrap_or_else(|| self.chain_seed.preview_code.clone());
                self.chain_seed.preview_code =
                    crate::chain_seed::inject_seed_math(&base, &seed, &lab, None);
                if self.chain_seed.push_hydra {
                    self.hydra_studio.code = hydra;
                }
                self.broadcast_seed_pulse(&seed, true);
                self.chain_seed.status = format!("Recalled · {lab}");
            }
            ui::ChainSeedAction::LoadDemo => {
                let code = crate::chain_seed::demo_pattern_code().to_string();
                self.chain_seed.preview_code = code.clone();
                self.chain_seed.seed_hex.clear();
                self.chain_seed.base_seed_hex.clear();
                self.chain_seed.last_hydra_code.clear();
                self.chain_seed.pipeline = vec![
                    "① Demo source pattern loaded into preview".into(),
                    "② Fetch a real seed → inject //@chain-seed into this code".into(),
                    "③ Apply writes injected code to Workspace".into(),
                ];
                self.chain_seed.last_generator = "demo-source".into();
                self.chain_seed.last_bpm = 120;
                self.apply_code_to_editor(code);
                self.chain_seed.status =
                    "Demo source in editor — Fetch seed to inject math".into();
            }
            ui::ChainSeedAction::Apply => {
                let code = self.chain_seed.preview_code.clone();
                if code.trim().is_empty() {
                    self.chain_seed.status = "Nothing to apply — fetch a seed first".into();
                    return;
                }
                if self.chain_seed.seed_hex.is_empty() {
                    self.chain_seed.status = "Fetch a seed before Apply".into();
                    return;
                }
                // Re-inject against latest editor if preview stale? Prefer preview (user may edit).
                self.apply_code_to_editor(code);
                let mut status =
                    "Injected into editor — use seedGain / seedLpf / … in your stack".to_string();
                if self.chain_seed.push_hydra && !self.chain_seed.last_hydra_code.is_empty() {
                    self.hydra_studio.code = self.chain_seed.last_hydra_code.clone();
                    self.visual_stage.ensure_running();
                    if self.visual_stage.client_count() == 0 {
                        self.visual_stage.open_in_browser();
                    }
                    self.visual_stage.broadcast_json(&serde_json::json!({
                        "type": "eval",
                        "code": self.hydra_studio.code,
                        "followMusic": self.hydra_studio.follow_music,
                    }));
                    self.hydra_studio.status = "Eval from Chain Seed apply".into();
                    status.push_str(" · Hydra pushed + Eval");
                }
                self.chain_seed.status = status;
            }
            ui::ChainSeedAction::Fetch => {
                if self.chain_seed.busy {
                    return;
                }
                self.chain_seed.live_poll_pending = false;
                self.chain_seed.busy = true;
                self.chain_seed.phase = "1/2 Fetching".into();
                self.chain_seed.status = "1/2 Fetching — starting…".into();
                self.spawn_chain_seed_fetch(self.chain_seed.mode);
            }
        }
    }

    /// Lance l'import d'un repo GitHub de sons dans un thread de fond.
    fn start_github_import(&mut self) {
        let input = self.github_import_url.trim().to_string();
        let Some(repo) = crate::github_samples::parse_github_url(&input) else {
            self.github_import_state =
                GithubImportUi::Error("URL invalide — ex. github.com/user/repo".to_string());
            return;
        };
        if self.github_import_rx.is_some() {
            return; // import déjà en cours
        }
        let (tx, rx) = mpsc::channel();
        self.github_import_rx = Some(rx);
        self.github_import_state = GithubImportUi::Running {
            files: 0,
            bytes: 0,
        };
        let dest_root = crate::paths::github_repos_root();
        thread::spawn(move || {
            let txp = tx.clone();
            let result = crate::github_samples::import_repo(
                &repo,
                &dest_root,
                &mut |p| {
                    let _ = txp.send(crate::github_samples::GithubImportEvent::Progress {
                        files: p.files,
                        bytes: p.bytes,
                    });
                },
            );
            let _ = tx.send(crate::github_samples::GithubImportEvent::Done(result));
        });
    }

    /// Draine les événements de l'import GitHub en cours.
    fn poll_github_import(&mut self) {
        let Some(rx) = self.github_import_rx.as_ref() else {
            return;
        };
        loop {
            match rx.try_recv() {
                Ok(crate::github_samples::GithubImportEvent::Progress { files, bytes }) => {
                    self.github_import_state = GithubImportUi::Running { files, bytes };
                }
                Ok(crate::github_samples::GithubImportEvent::Done(Ok(report))) => {
                    self.github_import_rx = None;
                    self.github_import_state = GithubImportUi::Done {
                        files: report.files,
                        bytes: report.bytes,
                    };
                    // Le repo est une nouvelle racine de samples : rescan complet.
                    self.sample_browser_entries_cache = None;
                    self.sample_browser_filtered_rows = None;
                    self.sample_browser_cache_pending = true;
                    if let Some(engine) = &self.audio {
                        engine.reload_samples_async(Path::new(&self.settings.sample_folder));
                    }
                    break;
                }
                Ok(crate::github_samples::GithubImportEvent::Done(Err(e))) => {
                    self.github_import_rx = None;
                    self.github_import_state = GithubImportUi::Error(e);
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.github_import_rx = None;
                    self.github_import_state =
                        GithubImportUi::Error("thread d'import interrompue".to_string());
                    break;
                }
            }
        }
    }

    fn spawn_chain_seed_fetch(&mut self, mode: crate::chain_seed::SeedMode) {
        let gen_mode = crate::chain_seed::GenMode::Procedural;
        let rpc = self.chain_seed.rpc_url.clone();
        let ollama_url = self.chain_seed.ollama_url.clone();
        let ollama_model = self.chain_seed.ollama_model.clone();
        let block = self.chain_seed.block_number.clone();
        let addr = self.chain_seed.address.clone();
        let tx = self.chain_seed.tx_hash.clone();
        let nft_c = self.chain_seed.nft_contract.clone();
        let nft_t = self.chain_seed.nft_token_id.clone();
        let (tx_ch, rx) = mpsc::channel();
        self.chain_seed_fetch_rx = Some(rx);
        thread::spawn(move || {
            let result = crate::chain_seed::fetch_seed(
                mode,
                &rpc,
                &block,
                &addr,
                &tx,
                &nft_c,
                &nft_t,
                gen_mode,
                &ollama_url,
                &ollama_model,
                Some(&tx_ch),
            );
            let _ = tx_ch.send(crate::chain_seed::ChainSeedEvent::Done(result));
        });
    }

    fn broadcast_seed_pulse(&mut self, seed: &str, flash: bool) {
        if !self.visual_stage.is_running() {
            return;
        }
        let buses = crate::chain_seed::seed_bus_values(seed);
        self.visual_stage.broadcast_json(&serde_json::json!({
            "type": "seed",
            "seed": seed,
            "flash": flash,
            "buses": buses,
        }));
    }

    /// Follow chain blocks + morph seed with playhead cycles.
    fn tick_chain_seed_live(&mut self) {
        if self.chain_seed.live_follow {
            let interval =
                Duration::from_secs_f32(self.chain_seed.live_interval_secs.clamp(4.0, 90.0));
            let due = self
                .chain_seed
                .next_live_poll
                .map(|t| Instant::now() >= t)
                .unwrap_or(true);
            if due && !self.chain_seed.busy && self.chain_seed_fetch_rx.is_none() {
                self.chain_seed.live_poll_pending = true;
                self.chain_seed.busy = true;
                self.chain_seed.phase = "1/2 Fetching".into();
                self.chain_seed.status = "Live poll — latest block…".into();
                self.spawn_chain_seed_fetch(crate::chain_seed::SeedMode::LatestBlock);
                self.chain_seed.next_live_poll = Some(Instant::now() + interval);
            } else if self.chain_seed.next_live_poll.is_none() {
                self.chain_seed.next_live_poll = Some(Instant::now() + interval);
            }
        } else {
            self.chain_seed.next_live_poll = None;
        }

        if !self.chain_seed.live_morph || self.chain_seed.base_seed_hex.is_empty() || !self.playing
        {
            return;
        }
        let cycle = self.current_time.floor() as i64;
        if cycle == self.chain_seed.last_morph_cycle || cycle < 0 {
            return;
        }
        self.chain_seed.last_morph_cycle = cycle;
        let morphed =
            crate::chain_seed::morph_seed(&self.chain_seed.base_seed_hex, cycle as u64);
        self.chain_seed.seed_hex = morphed.clone();
        self.chain_seed.live_tick = self.chain_seed.live_tick.saturating_add(1);
        let label = format!(
            "{} · morph c{cycle}",
            if self.chain_seed.source_label.is_empty() {
                "seed"
            } else {
                self.chain_seed.source_label.as_str()
            }
        );
        let base_code = self
            .active_editor()
            .map(|e| e.code.clone())
            .filter(|c| !c.trim().is_empty())
            .unwrap_or_else(|| self.chain_seed.preview_code.clone());
        let injected = crate::chain_seed::inject_seed_math_phased(
            &base_code,
            &morphed,
            &label,
            {
                let m = self.chain_seed.meta_note.as_str();
                if m.is_empty() {
                    None
                } else {
                    Some(m)
                }
            },
            cycle as u64,
        );
        self.chain_seed.preview_code = injected.clone();
        self.chain_seed.last_bpm = crate::chain_seed::bpm_from_seed(&morphed);
        self.broadcast_seed_pulse(&morphed, true);
        if cycle % 4 == 0 {
            let hydra = crate::chain_seed::generate_hydra_sketch(&morphed, &label);
            self.chain_seed.last_hydra_code = hydra.clone();
            if self.chain_seed.push_hydra {
                self.hydra_studio.code = hydra.clone();
                self.visual_stage.ensure_running();
                self.visual_stage.broadcast_json(&serde_json::json!({
                    "type": "eval",
                    "code": hydra,
                    "followMusic": self.hydra_studio.follow_music,
                }));
            }
        }
        if self.chain_seed.live_apply {
            self.apply_code_to_editor(injected);
        }
        self.chain_seed.status = format!(
            "Morph c{cycle} · {} · tick {}",
            crate::chain_seed::short_seed(&morphed),
            self.chain_seed.live_tick
        );
    }

    fn show_tool_viewports(&mut self, ctx: &egui::Context) {
        if self.hydra_studio.window_open {
            let mut open = true;
            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of("hydra_studio"),
                egui::ViewportBuilder::default()
                    .with_title("Hydra")
                    .with_inner_size([560.0, 720.0])
                    .with_min_inner_size([360.0, 320.0]),
                |ctx, _class| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let has_seed = !self.chain_seed.seed_hex.is_empty();
                        let actions = ui::hydra_studio_ui(
                            ui,
                            &mut self.hydra_studio,
                            &self.visual_stage,
                            has_seed,
                        );
                        for action in actions {
                            self.handle_hydra_action(action);
                        }
                    });
                    if ctx.input(|i| i.viewport().close_requested()) {
                        open = false;
                    }
                },
            );
            self.hydra_studio.window_open = open;
        }

        if self.ai_pattern.window_open {
            let mut open = true;
            self.poll_ai_pattern();
            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of("ai_pattern"),
                egui::ViewportBuilder::default()
                    .with_title("AI Pattern")
                    .with_inner_size([520.0, 640.0])
                    .with_min_inner_size([360.0, 320.0]),
                |ctx, _class| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let actions = ui::ai_pattern_ui(ui, &mut self.ai_pattern);
                        for action in actions {
                            self.handle_ai_pattern_action(action);
                        }
                    });
                    if ctx.input(|i| i.viewport().close_requested()) {
                        open = false;
                    }
                },
            );
            self.ai_pattern.window_open = open;
        }
    }

    fn apply_code_to_editor(&mut self, code: String) {
        if self.editor_tabs.is_empty() {
            let tab = EditorTab::new(None, code.clone(), self.doc_store.as_ref());
            self.editor_tabs.push(tab);
            self.active_editor_index = 0;
        } else {
            let idx = self
                .active_editor_index
                .min(self.editor_tabs.len().saturating_sub(1));
            if let Some(editor) = self.editor_tabs.get_mut(idx) {
                editor.code = code.clone();
            }
        }
        self.last_code_revision = self.last_code_revision.saturating_add(1);
        self.sync_engine
            .set_code(code.clone(), self.last_code_revision);
        self.pattern_document = Some(parse_pattern_document(code, self.last_code_revision));
        self.refresh_native_engine();
    }

    fn arrangement_content_ui(&mut self, ui: &mut egui::Ui) {
        let active_code = self.arrangement_active_code.clone();
        let source_options = self.arrangement_source_options();
        let arrangement_playing = self.playing && self.arrangement_playback_selected();
        let arrangement_time = if arrangement_playing {
            self.current_time
        } else {
            0.0
        };
        let actions = ui::arrangement_ui(
            ui,
            &self.arrangement,
            arrangement_time,
            arrangement_playing,
            &active_code,
            &source_options,
            &mut self.arrangement_view,
        );
        for action in actions {
            self.handle_arrangement_action(action);
        }
    }

    fn handle_arrangement_action(&mut self, action: ui::ArrangementAction) {
        use ui::ArrangementAction::*;
        match action {
            SetEnabled(enabled) => {
                self.arrangement.enabled = enabled;
                if !enabled {
                    self.arrangement_last_active = None;
                }
            }
            AddRow => {
                self.arrangement.add_row();
            }
            DeleteRow(id) => {
                self.arrangement.rows.retain(|r| r.id != id);
            }
            MuteRow(id, muted) => {
                if let Some(r) = self.arrangement.rows.iter_mut().find(|r| r.id == id) {
                    r.muted = muted;
                }
            }
            SoloRow(id, solo) => {
                if let Some(r) = self.arrangement.rows.iter_mut().find(|r| r.id == id) {
                    r.solo = solo;
                }
            }
            RenameRow(id, name) => {
                if let Some(r) = self.arrangement.rows.iter_mut().find(|r| r.id == id) {
                    r.name = name;
                }
            }
            AddBlock {
                row_id,
                start,
                length,
                code,
                name,
                source_file,
                source_pattern_id,
            } => {
                if source_file.is_none() {
                    return;
                }
                self.arrangement.add_block(
                    row_id,
                    start,
                    length,
                    code,
                    name,
                    source_file,
                    source_pattern_id,
                );
            }
            MoveBlock {
                row_id,
                block_id,
                new_start,
            } => {
                if let Some(b) = self.arrangement.block_mut(row_id, block_id) {
                    b.start = new_start.max(0.0);
                }
            }
            ResizeBlock {
                row_id,
                block_id,
                new_length,
            } => {
                if let Some(b) = self.arrangement.block_mut(row_id, block_id) {
                    b.length = new_length.max(0.5);
                }
            }
            DeleteBlock { row_id, block_id } => {
                self.arrangement.delete_block(row_id, block_id);
            }
            MuteBlock {
                row_id,
                block_id,
                muted,
            } => {
                if let Some(b) = self.arrangement.block_mut(row_id, block_id) {
                    b.muted = muted;
                }
            }
            SelectBlock(sel) => {
                self.arrangement.selected = sel;
            }
            EditBlockCode {
                row_id,
                block_id,
                code,
            } => {
                if let Some(b) = self.arrangement.block_mut(row_id, block_id) {
                    b.code = code;
                }
                self.arrangement_last_active = None;
            }
            EditBlockName {
                row_id,
                block_id,
                name,
            } => {
                if let Some(b) = self.arrangement.block_mut(row_id, block_id) {
                    b.name = name;
                }
            }
            SetBlockSource {
                row_id,
                block_id,
                source_file,
                source_pattern_id,
            } => {
                if let Some(b) = self.arrangement.block_mut(row_id, block_id) {
                    b.source_file = source_file;
                    b.source_pattern_id = source_pattern_id;
                }
                self.arrangement_last_active = None;
            }
            ExportGeneratedScript => {
                let generated = self.arrangement.generate_script(|path, pattern_id| {
                    self.resolve_arrangement_source_code(path, pattern_id)
                });
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Strudel", &["strudel"])
                    .set_file_name("arrangement.generated.strudel")
                    .save_file()
                {
                    let _ = std::fs::write(path, generated);
                }
            }
            Seek(pos) => {
                self.do_seek(pos);
            }
        }
    }

    /// Вызывается каждый кадр при включённом arrangement-режиме и воспроизведении.
    /// Если набор активных блоков изменился — отправляем новый код в bridge.
    fn tick_arrangement(&mut self) {
        if !ui::should_tick_arrangement_for_example_state(&self.example_playback)
            || !self.arrangement_playback_selected()
            || !self.playing
        {
            return;
        }
        let code = self
            .arrangement
            .active_code_at_with(self.current_time, |path, pattern_id| {
                self.resolve_arrangement_source_code(path, pattern_id)
            });
        let key = code.clone().unwrap_or_default();
        if self.arrangement_last_active.as_deref() != Some(&key) {
            self.arrangement_last_active = Some(key);
            if let Some(ref code) = code {
                self.arrangement_active_code = code.clone();
                let _ = self.bridge.evaluate(code);
            } else {
                self.arrangement_active_code.clear();
            }
        }
    }

    fn bounce_audio_selection(&self, path: &std::path::Path) -> Result<(), String> {
        let config = RenderConfig {
            sample_rate: 48_000,
            channels: 2,
            start_cycle: self.transport_state.session_start,
            end_cycle: self
                .transport_state
                .session_end
                .max(self.transport_state.session_start + 1.0),
            cycles_per_second: self.last_cps.max(0.000_001),
            realtime_preview: false,
        };
        let samples = self
            .clip_renderer
            .render_project(&self.audio_project, &config)?;
        write_wav_with_options(
            path,
            &samples,
            config.sample_rate,
            config.channels,
            self.audio_project.master_options,
        )
    }

    /// Рисует диалог офлайн-экспорта в WAV.
    fn show_export_dialog(&mut self, ctx: &egui::Context) {
        if !self.export_dialog.open {
            return;
        }

        let mut open = true;
        egui::Window::new("Export en WAV")
            .collapsible(false)
            .resizable(false)
            .min_width(420.0)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                egui::Grid::new("export_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Durée (cycles) :");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.export_dialog.duration_cycles)
                                .desired_width(80.0)
                                .hint_text("8"),
                        );
                        ui.end_row();

                        let cps = self.last_cps.max(0.01);
                        let cycles: f64 = self
                            .export_dialog
                            .duration_cycles
                            .trim()
                            .parse()
                            .unwrap_or(8.0);
                        let secs = cycles / cps;
                        ui.label("");
                        ui.label(
                            egui::RichText::new(format!(
                                "≈ {:.1} s à {:.2} CPS ({:.1} BPM)",
                                secs,
                                cps,
                                cps * 60.0
                            ))
                            .weak(),
                        );
                        ui.end_row();

                        ui.label("Fichier de sortie :");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.export_dialog.output_path)
                                    .desired_width(260.0),
                            );
                            if ui.button("…").clicked() {
                                if let Some(path) = rfd::FileDialog::new()
                                    .add_filter("WAV", &["wav"])
                                    .set_file_name("export.wav")
                                    .save_file()
                                {
                                    self.export_dialog.output_path =
                                        path.to_string_lossy().to_string();
                                }
                            }
                        });
                        ui.end_row();
                    });

                ui.add_space(8.0);

                match &self.export_dialog.status {
                    ExportStatus::Running => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Rendu…");
                        });
                    }
                    ExportStatus::Done => {
                        ui.label(
                            egui::RichText::new("✔ Export terminé !")
                                .color(egui::Color32::from_rgb(80, 200, 80))
                                .strong(),
                        );
                    }
                    ExportStatus::Error(e) => {
                        ui.label(
                            egui::RichText::new(format!("✖ Erreur : {e}"))
                                .color(egui::Color32::from_rgb(220, 80, 80)),
                        );
                    }
                    ExportStatus::Idle => {}
                }

                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    let running = self.export_dialog.is_running();
                    let btn = ui.add_enabled(
                        !running,
                        egui::Button::new(if running {
                            "Rendu…"
                        } else {
                            "Exporter"
                        }),
                    );
                    if btn.clicked() {
                        self.start_offline_export();
                    }
                    if ui.button("Fermer").clicked() {
                        self.export_dialog.open = false;
                        self.export_dialog.status = ExportStatus::Idle;
                    }
                });
            });

        if !open {
            self.export_dialog.open = false;
        }
    }

    /// Запускает фоновый поток офлайн-рендера.
    fn start_offline_export(&mut self) {
        let cycles: f64 = self
            .export_dialog
            .duration_cycles
            .trim()
            .parse()
            .unwrap_or(8.0);
        let output_path = std::path::PathBuf::from(&self.export_dialog.output_path);
        if output_path.as_os_str().is_empty() {
            self.export_dialog.status =
                ExportStatus::Error("Indiquez le chemin du fichier de sortie.".to_string());
            return;
        }

        // Синхронно собираем события из bridge (быстро — только JSON по пайпу).
        let (events, cps) = match offline_export::collect_events(&mut self.bridge, cycles) {
            Ok(r) => r,
            Err(e) => {
                self.export_dialog.status = ExportStatus::Error(e);
                return;
            }
        };

        if events.is_empty() {
            self.export_dialog.status = ExportStatus::Error(
                "Aucun événement. Exécutez d'abord le pattern (Ctrl+Enter).".to_string(),
            );
            return;
        }

        let Some(audio) = self.audio.clone() else {
            self.export_dialog.status =
                ExportStatus::Error("Moteur audio non initialisé.".to_string());
            return;
        };

        let config = offline_export::ExportConfig {
            duration_cycles: cycles,
            sample_rate: 48_000,
            channels: 2,
            master_gain: self.settings.audio_gain as f32,
            master_options: self.audio_project.master_options,
        };

        let rx = offline_export::start_export(events, cps, output_path, config, &audio);
        self.export_dialog.rx = Some(rx);
        self.export_dialog.status = ExportStatus::Running;
    }

    /// Пауза с немедленной остановкой аудио (без hush — паттерн сохраняется).
    fn do_pause(&mut self) {
        let paused_time = self.current_time.max(0.0);
        if !self.stop_active_example_cleanup() {
            self.reset_example_playback();
            let _ = self.bridge.pause(paused_time);
        }
        self.current_time = paused_time;
        self.prev_time = paused_time;
        self.last_bridge_now = paused_time;
        self.last_bridge_instant = Instant::now();
        self.timeline_display_time = paused_time;
        self.seek_ignore_now_frames = 3;
        // Немедленно убиваем все звучащие голоса — иначе lookahead-буфер доигрывает
        if let Some(engine) = &self.audio {
            engine.clear();
        }
        // Отбрасываем незавершённый запрос аудио-окна
        self.pending_audio = None;
        self.sched.set_scheduled_to(-1.0);
    }

    fn active_source_keys_now(&self) -> std::collections::BTreeSet<ui::SourceBreakpointKey> {
        use std::collections::BTreeSet;
        let Some(editor) = self.active_editor() else {
            return BTreeSet::new();
        };
        let t = self.current_time;
        let events = self.bridge.query_arc(0.0, 0.0);
        let playing_events: Vec<(String, f64)> = events
            .iter()
            .filter(|e| e.begin <= t && t < e.end)
            .filter_map(|e| e.s.as_ref().map(|name| (name.clone(), e.begin.fract())))
            .collect();
        ui::EditorTab::active_source_keys(
            &editor.code,
            self.current_time,
            self.pattern_document.as_ref(),
            &playing_events,
        )
    }

    /// До какого абсолютного времени (циклы) не ловить строковую точку после F5.
    fn skip_source_lines_until_continue(
        &self,
        active: &std::collections::BTreeSet<ui::SourceBreakpointKey>,
    ) -> std::collections::HashMap<usize, f64> {
        use std::collections::HashMap;
        let t = self.current_time;
        let cycle_base = t.floor();
        let frac = t.rem_euclid(1.0);
        let mut out = HashMap::new();

        let Some(editor) = self.active_editor() else {
            return out;
        };
        let code = &editor.code;

        for key in active {
            if !self.debugger.has_source_breakpoint(key.line) {
                continue;
            }
            let slot_begin = f64::from_bits(key.instance);
            let mut pass_until = t + 1e-4;

            if let Some(doc) = self.pattern_document.as_ref() {
                for ev in &doc.ir.events {
                    let span = ev.token_span.unwrap_or(ev.source_span);
                    let line = ui::EditorTab::byte_offset_to_line(code, span.start);
                    if line != key.line {
                        continue;
                    }
                    if (ev.begin - slot_begin).abs() > 1e-5 {
                        continue;
                    }
                    if ev.begin <= frac && frac < ev.end {
                        pass_until = pass_until.max(cycle_base + ev.end + 1e-4);
                    }
                }
            } else {
                pass_until = pass_until.max(cycle_base + frac + 0.25);
            }

            out.insert(
                key.line,
                out.get(&key.line).copied().unwrap_or(0.0).max(pass_until),
            );
        }

        if let Some(line) = self.debugger.paused_source_line {
            out.entry(line).or_insert(t + 1e-4);
        }

        out
    }

    fn skip_watch_until_continue(
        &self,
        events: &[crate::strudel_bridge::StrudelEvent],
    ) -> std::collections::HashMap<usize, f64> {
        use std::collections::HashMap;
        let t = self.current_time;
        let mut out = HashMap::new();
        for wp in &self.debugger.watchpoints {
            if !wp.enabled {
                continue;
            }
            let mut pass_until = t + 1e-4;
            for ev in events {
                if ev.begin > t || ev.end <= t {
                    continue;
                }
                let matches = ev.label.contains(&wp.label)
                    || wp.label.contains(&ev.label)
                    || ev
                        .s
                        .as_ref()
                        .map(|s| s == &wp.label || s.contains(&wp.label) || wp.label.contains(s))
                        .unwrap_or(false);
                if matches {
                    pass_until = pass_until.max(ev.end + 1e-4);
                }
            }
            out.insert(wp.id, pass_until);
        }
        out
    }

    fn skip_catch_until_continue(&self) -> std::collections::HashMap<usize, f64> {
        use std::collections::HashMap;
        let Some(editor) = self.active_editor() else {
            return HashMap::new();
        };
        let Some(doc) = self.pattern_document.as_ref() else {
            return HashMap::new();
        };
        self.debugger
            .skip_catch_until_continue(self.current_time, &editor.code, doc)
    }

    /// Продолжить после точки останова (пропустить текущий удар, ждать следующий).
    fn debugger_continue_playback(&mut self) {
        let t = self.current_time;
        let events = self.bridge.query_arc(0.0, 0.0);
        let active = self.active_source_keys_now();
        let skip_lines = self.skip_source_lines_until_continue(&active);
        let skip_watch = self.skip_watch_until_continue(&events);
        let skip_catch = self.skip_catch_until_continue();
        let code_owned = self.active_editor().map(|e| e.code.clone());
        let code = code_owned.as_deref();
        let doc = self.pattern_document.as_ref();
        self.debugger.resume_continuing(
            &active, t, skip_lines, skip_watch, skip_catch, &events, code, doc,
        );
        self.prev_time = t;
        let _ = self.bridge.start();
        self.playing = true;
        self.paused = false;
    }

    fn reset_audio_schedule_for_direction(&mut self) {
        self.pending_audio = None;
        self.sched.set_scheduled_to(if self.reverse {
            self.current_time.max(0.0)
        } else {
            (self.current_time - 0.5).max(-1.0)
        });
        self.last_reverse_seek_at = Instant::now();
        if let Some(engine) = &self.audio {
            engine.clear();
        }
    }

    fn do_seek(&mut self, pos: f64) {
        let pos = pos.max(0.0);
        let _ = self.bridge.seek(pos);
        self.current_time = pos;
        self.prev_time = pos;
        self.last_bridge_now = pos;
        self.last_bridge_instant = Instant::now();
        self.timeline_display_time = pos;
        self.sched.set_scheduled_to(if self.reverse {
            pos
        } else {
            (pos - 0.5).max(-1.0)
        });
        self.last_reverse_seek_at = Instant::now();
        // Игнорировать ответы bridge.now() несколько кадров — пока JS-планировщик
        // не обработал seek и не вернул правильную позицию
        self.seek_ignore_now_frames = 5;
    }

    fn maybe_resync_reverse_seek(&mut self) {
        if !self.playing || !self.reverse {
            return;
        }
        let now = Instant::now();
        if now.duration_since(self.last_reverse_seek_at) < REVERSE_SEEK_MIN_INTERVAL {
            return;
        }
        let target = self.current_time.max(self.session_start());
        let drift_cycles = (self.last_bridge_now - target).abs();
        if drift_cycles < REVERSE_SEEK_BATCH_WINDOW_CYCLES {
            return;
        }
        let _ = self.bridge.seek(target);
        self.last_bridge_now = target;
        self.last_bridge_instant = now;
        self.last_reverse_seek_at = now;
    }

    fn do_midi_panic(&mut self) {
        if let Some(engine) = &self.audio {
            engine.clear();
        }
        self.pending_audio = None;
        if self.playing {
            self.reset_audio_schedule_for_direction();
        }
    }

    fn session_start(&self) -> f64 {
        self.transport_state.session_start.max(0.0)
    }

    fn session_end(&self) -> f64 {
        self.transport_state
            .session_end
            .max(self.session_start() + self.transport_state.step_size.max(0.25))
    }

    fn maybe_jump_to_reverse_start(&mut self) {
        let start = self.session_start();
        if self.current_time <= start + 0.000_1 {
            self.do_seek(self.session_end());
        }
    }

    fn update_metronome(&mut self) {
        if !self.transport_state.metronome || !self.playing {
            self.last_metronome_beat = None;
            return;
        }
        if !self.settings.audio_enabled {
            return;
        }
        if self.audio.is_none() {
            self.audio = AudioEngine::new().ok().map(Arc::new);
        }

        let beat = (self.current_time.max(0.0) * 4.0).floor() as i64;
        if self.last_metronome_beat == Some(beat) {
            return;
        }
        let should_click = self
            .last_metronome_beat
            .map(|last| {
                if self.reverse {
                    beat < last
                } else {
                    beat > last
                }
            })
            .unwrap_or(true);
        self.last_metronome_beat = Some(beat);
        if should_click {
            if let Some(engine) = &self.audio {
                let accent = beat.rem_euclid(4) == 0;
                let gain =
                    self.settings.audio_gain.clamp(0.0, 1.0) * if accent { 0.55 } else { 0.38 };
                engine.play_metronome_click(accent, gain);
            }
        }
    }

    fn handle_transport_action(&mut self, action: ui::TransportAction) {
        match action {
            ui::TransportAction::Play => {
                self.reverse = self.transport_state.reverse;
                self.arrangement.enabled = self.arrangement_playback_selected();
                if self.reverse {
                    self.maybe_jump_to_reverse_start();
                }
                let was_paused_or_stopped =
                    self.paused || (!self.playing && self.current_time > 0.0);
                if was_paused_or_stopped {
                    // Возобновить с места (JS держит _pausedTime через bridge.pause())
                    let _ = self.bridge.start();
                    self.playing = true;
                    self.paused = false;
                    self.last_bridge_now = self.current_time;
                    self.last_bridge_instant = Instant::now();
                    self.reset_audio_schedule_for_direction();
                } else {
                    // Свежий запуск с нуля (eval + start)
                    if self.arrangement_playback_selected() {
                        self.run_arrangement_code();
                    } else {
                        self.run_editor_code();
                    }
                    if self.reverse {
                        self.maybe_jump_to_reverse_start();
                        self.reset_audio_schedule_for_direction();
                    }
                }
            }
            ui::TransportAction::Pause => {
                self.do_pause();
                self.paused = true;
                self.playing = false;
            }
            ui::TransportAction::Stop => {
                self.do_pause();
                self.playing = false;
                self.paused = false;
                self.reverse = false;
                self.transport_state.reverse = false;
                self.debugger.resume();
            }
            ui::TransportAction::Rewind => {
                // Полная остановка + переход к началу сессии.
                self.do_pause();
                let _ = self.bridge.stop(); // hush + сброс _pausedTime в JS
                self.playing = false;
                self.paused = false;
                let session_start = self.session_start();
                let _ = self.bridge.seek(session_start);
                self.current_time = session_start;
                self.prev_time = session_start;
                self.last_bridge_now = session_start;
                self.last_bridge_instant = Instant::now();
                self.timeline_display_time = session_start;
                self.seek_ignore_now_frames = 8;
                self.debugger.resume();
            }
            ui::TransportAction::StepBack => {
                let target =
                    (self.current_time - self.transport_state.step_size).max(self.session_start());
                self.do_pause();
                self.playing = false;
                self.paused = true;
                self.do_seek(target);
            }
            ui::TransportAction::StepForward => {
                let target = self.current_time + self.transport_state.step_size;
                self.do_pause();
                self.playing = false;
                self.paused = true;
                self.do_seek(target);
            }
            ui::TransportAction::ToggleReverse => {
                self.transport_state.reverse = !self.transport_state.reverse;
                self.reverse = self.transport_state.reverse;
                if self.reverse {
                    self.maybe_jump_to_reverse_start();
                }
                self.last_bridge_now = self.current_time;
                self.last_bridge_instant = Instant::now();
                self.reset_audio_schedule_for_direction();
                if self.playing {
                    let _ = self.bridge.seek(self.current_time);
                }
            }
            ui::TransportAction::ToggleLoop => {
                self.transport_state.loop_enabled = !self.transport_state.loop_enabled;
            }
            ui::TransportAction::SetLoopStart => {
                self.transport_state.loop_start = self.current_time;
                if self.transport_state.loop_end <= self.transport_state.loop_start {
                    self.transport_state.loop_end = self.transport_state.loop_start + 4.0;
                }
            }
            ui::TransportAction::SetLoopEnd => {
                self.transport_state.loop_end = self.current_time;
                if self.transport_state.loop_end <= self.transport_state.loop_start {
                    self.transport_state.loop_start =
                        (self.transport_state.loop_end - 4.0).max(0.0);
                }
            }
            ui::TransportAction::SetBpm(bpm) => {
                self.transport_state.bpm = bpm;
                let _ = self.bridge.set_bpm(bpm);
                // Обновить CPS для интерполяции
                self.last_cps = bpm / 60.0;
                // Нативный движок пользуется bpm внутри — пересчитать кэш.
                self.refresh_native_engine();
            }
            ui::TransportAction::SetStepSize(size) => {
                self.transport_state.step_size = size;
            }
            ui::TransportAction::ToggleMetronome => {
                self.transport_state.metronome = !self.transport_state.metronome;
                self.last_metronome_beat = None;
            }
            ui::TransportAction::MidiPanic => {
                self.do_midi_panic();
            }
            ui::TransportAction::SetSessionStart => {
                let start = self.current_time.max(0.0);
                self.transport_state.session_start = start;
                if self.transport_state.session_end <= start {
                    self.transport_state.session_end = start + 4.0;
                }
                if self.transport_state.loop_start < start {
                    self.transport_state.loop_start = start;
                }
            }
            ui::TransportAction::SetSessionEnd => {
                let end = self
                    .current_time
                    .max(self.session_start() + self.transport_state.step_size.max(0.25));
                self.transport_state.session_end = end;
                if self.transport_state.loop_end > end
                    || self.transport_state.loop_end <= self.transport_state.loop_start
                {
                    self.transport_state.loop_end = end;
                }
            }
            ui::TransportAction::GoToSessionStart => {
                self.do_pause();
                self.playing = false;
                self.paused = true;
                self.do_seek(self.session_start());
            }
            ui::TransportAction::GoToSessionEnd => {
                self.do_pause();
                self.playing = false;
                self.paused = true;
                self.do_seek(self.session_end());
            }
        }
    }

    /// Документация по функциям.
    fn docs_content_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Documentation Strudel");
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("Fonction :");
            ui.text_edit_singleline(&mut self.doc_lookup);
        });
        ui.add_space(8.0);
        if let Some(store) = self.doc_store.as_ref() {
            let query = self.doc_lookup.trim();
            if query.is_empty() {
                ui.label("Saisissez le nom d'une fonction (ou un fragment) pour voir sa description.");
            } else if let Some(entry) = store.get(query) {
                ui.label(format!(
                    "{} — {}",
                    entry.longname,
                    entry
                        .description
                        .as_deref()
                        .unwrap_or("Description indisponible")
                ));
                if let Some(params) = &entry.params {
                    ui.add_space(4.0);
                    ui.label("Paramètres :");
                    for p in params {
                        let ty = p
                            .r#type
                            .as_ref()
                            .and_then(|t| t.names.as_ref())
                            .map(|names| names.join(" | "))
                            .unwrap_or_default();
                        let descr = p.description.as_deref().unwrap_or("");
                        ui.label(format!(
                            "- {}: {} {}",
                            p.name,
                            ty,
                            if descr.is_empty() { "" } else { "-" }
                        ));
                        if !descr.is_empty() {
                            ui.label(format!("  {}", descr));
                        }
                    }
                }
                if let Some(examples) = &entry.examples {
                    ui.add_space(4.0);
                    ui.label("Exemples :");
                    for ex in examples.iter().take(5) {
                        ui.monospace(ex);
                    }
                }
            } else {
                ui.label("Aucune correspondance exacte. Fonctions similaires :");
                let lower = query.to_lowercase();
                let mut shown = 0;
                for name in store.all_labels() {
                    if name.to_lowercase().contains(&lower) {
                        if ui.link(name).clicked() {
                            self.doc_lookup = name.clone();
                        }
                        shown += 1;
                        if shown >= 25 {
                            break;
                        }
                    }
                }
                if shown == 0 {
                    ui.label("Aucune fonction similaire trouvée dans doc.json.");
                }
            }
        } else {
            ui.label("doc.json non chargé. Vérifiez la présence du fichier strudel/doc.json.");
        }
    }

    /// Настройки приложения.
    fn settings_content_ui(&mut self, ui: &mut egui::Ui) {
        ui::settings_ui(ui, &mut self.settings);
        if self.settings.audio_test_requested {
            self.settings.audio_test_requested = false;
            if self.settings.audio_enabled && self.audio.is_none() {
                self.audio = AudioEngine::new().ok().map(Arc::new);
            }
            if let Some(engine) = &self.audio {
                let gain = self.settings.audio_gain.clamp(0.0, 1.0);
                engine.play_test_tone(gain);
            }
        }
        if self.settings.reload_samples_requested {
            self.settings.reload_samples_requested = false;
            if self.settings.audio_enabled {
                if self.audio.is_none() {
                    self.audio = AudioEngine::new().ok().map(Arc::new);
                }
                if let Some(engine) = &self.audio {
                    use std::path::Path;
                    let root = Path::new(&self.settings.sample_folder);
                    engine.reload_samples_async(root);
                }
            }
        }
        if ui.button("Enregistrer").clicked() {
            settings::save_settings(&self.settings);
        }
    }

    /// Вкладка «Терминал»: логи stderr runner (отладка, скачивание семплов и т.д.).
    fn terminal_content_ui(&mut self, ui: &mut egui::Ui) {
        let lines = self.bridge.terminal_lines();
        let full_w = ui.available_width();
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
            .id_salt("terminal_scroll")
            .show(ui, |ui| {
                ui.set_min_width(full_w);
                if lines.is_empty() {
                    ui.label(
                        egui::RichText::new("Le log du runner (stderr) apparaîtra ici après le lancement.")
                            .color(ui.visuals().weak_text_color()),
                    );
                } else {
                    for line in &lines {
                        ui.monospace(line);
                    }
                }
            });
    }

    fn byte_to_line_col(text: &str, byte_offset: usize) -> (usize, usize) {
        let mut line = 1usize;
        let mut col = 1usize;
        for (b, ch) in text.char_indices() {
            if b >= byte_offset.min(text.len()) {
                break;
            }
            if ch == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    fn problems_content_ui(&mut self, ui: &mut egui::Ui) {
        #[derive(Clone)]
        struct ProblemRow {
            tab_index: usize,
            file_label: String,
            severity: &'static str,
            message: String,
            line: Option<usize>,
            col: Option<usize>,
        }

        let severity_enabled = |severity: &str, app: &StruidelApp| match severity {
            "Error" => app.problems_show_errors,
            "Warning" => app.problems_show_warnings,
            "Info" => app.problems_show_info,
            _ => true,
        };

        let mut rows: Vec<ProblemRow> = Vec::new();
        for (idx, editor) in self.editor_tabs.iter().enumerate() {
            let file_label = editor
                .file_path
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("Untitled {}", idx + 1));

            if let Some(err) = &editor.eval_error {
                rows.push(ProblemRow {
                    tab_index: idx,
                    file_label: file_label.clone(),
                    severity: "Error",
                    message: err.clone(),
                    line: editor.eval_error_line,
                    col: editor.eval_error_column,
                });
            }

            let parsed = parse_pattern_document(editor.code.clone(), 0);
            for diag in parsed.diagnostics {
                let (line, col) = Self::byte_to_line_col(&editor.code, diag.span.start);
                let severity = match diag.severity {
                    crate::pattern_ir::DiagnosticSeverity::Info => "Info",
                    crate::pattern_ir::DiagnosticSeverity::Warning => "Warning",
                    crate::pattern_ir::DiagnosticSeverity::Error => "Error",
                };
                rows.push(ProblemRow {
                    tab_index: idx,
                    file_label: file_label.clone(),
                    severity,
                    message: diag.message,
                    line: Some(line),
                    col: Some(col),
                });
            }
            for prob in crate::strudel_completion_engine::collect_sound_token_problems(&editor.code)
            {
                let (line, col) = Self::byte_to_line_col(&editor.code, prob.byte_start);
                rows.push(ProblemRow {
                    tab_index: idx,
                    file_label: file_label.clone(),
                    severity: prob.severity,
                    message: prob.message,
                    line: Some(line),
                    col: Some(col),
                });
            }
            for prob in
                crate::strudel_completion_engine::collect_function_call_problems(&editor.code)
            {
                let (line, col) = Self::byte_to_line_col(&editor.code, prob.byte_start);
                rows.push(ProblemRow {
                    tab_index: idx,
                    file_label: file_label.clone(),
                    severity: prob.severity,
                    message: prob.message,
                    line: Some(line),
                    col: Some(col),
                });
            }
        }

        ui.horizontal(|ui| {
            ui.heading("Problèmes");
            ui.add_space(8.0);
            let errors = rows.iter().filter(|r| r.severity == "Error").count();
            let warns = rows.iter().filter(|r| r.severity == "Warning").count();
            let infos = rows.iter().filter(|r| r.severity == "Info").count();
            ui.checkbox(&mut self.problems_show_errors, format!("Errors ({errors})"));
            ui.checkbox(
                &mut self.problems_show_warnings,
                format!("Warnings ({warns})"),
            );
            ui.checkbox(&mut self.problems_show_info, format!("Info ({infos})"));
        });
        ui.horizontal(|ui| {
            ui.label("Recherche :");
            ui.add(
                egui::TextEdit::singleline(&mut self.problems_search)
                    .hint_text("fichier / texte d'erreur")
                    .desired_width(280.0),
            );
        });
        ui.separator();

        let search_lc = self.problems_search.to_lowercase();
        let filtered_rows: Vec<ProblemRow> = rows
            .into_iter()
            .filter(|row| {
                severity_enabled(row.severity, self)
                    && (search_lc.is_empty()
                        || row.file_label.to_lowercase().contains(&search_lc)
                        || row.message.to_lowercase().contains(&search_lc))
            })
            .collect();

        if filtered_rows.is_empty() {
            ui.label(
                egui::RichText::new("Aucun problème trouvé avec les filtres actuels.")
                    .color(ui.visuals().weak_text_color()),
            );
            return;
        }

        let mut activate_tab: Option<usize> = None;
        egui::ScrollArea::vertical()
            .id_salt("problems_scroll")
            .show(ui, |ui| {
                use std::collections::BTreeMap;
                let mut groups: BTreeMap<String, Vec<ProblemRow>> = BTreeMap::new();
                for row in filtered_rows {
                    groups.entry(row.file_label.clone()).or_default().push(row);
                }
                for (file, items) in groups {
                    let mut items = items;
                    let sev_rank = |s: &str| match s {
                        "Error" => 0usize,
                        "Warning" => 1usize,
                        "Info" => 2usize,
                        _ => 3usize,
                    };
                    items.sort_by_key(|r| {
                        (
                            r.line.unwrap_or(usize::MAX),
                            r.col.unwrap_or(usize::MAX),
                            sev_rank(r.severity),
                            r.message.to_ascii_lowercase(),
                        )
                    });
                    let err = items.iter().filter(|r| r.severity == "Error").count();
                    let warn = items.iter().filter(|r| r.severity == "Warning").count();
                    let info = items.iter().filter(|r| r.severity == "Info").count();
                    let header = format!("{file}  (E:{err} W:{warn} I:{info})");
                    egui::CollapsingHeader::new(header)
                        .id_salt(ui.id().with("problems_file").with(&file))
                        .default_open(true)
                        .show(ui, |ui| {
                            for (i, row) in items.iter().enumerate() {
                                let where_text = match (row.line, row.col) {
                                    (Some(l), Some(c)) => format!("{}:{}", l, c),
                                    (Some(l), None) => format!("{l}"),
                                    _ => "-".to_string(),
                                };
                                let text =
                                    format!("[{}] {} — {}", row.severity, where_text, row.message);
                                let color = match row.severity {
                                    "Error" => egui::Color32::from_rgb(235, 110, 110),
                                    "Warning" => egui::Color32::from_rgb(240, 200, 120),
                                    _ => ui.visuals().weak_text_color(),
                                };
                                let resp = ui.selectable_label(
                                    false,
                                    egui::RichText::new(text).color(color),
                                );
                                if resp.clicked() {
                                    activate_tab = Some(row.tab_index);
                                }
                                if i + 1 < items.len() {
                                    ui.separator();
                                }
                            }
                        });
                    ui.add_space(4.0);
                    if ui.available_height() <= 0.0 {
                        break;
                    }
                }
            });
        if let Some(idx) = activate_tab {
            self.active_editor_index = idx.min(self.editor_tabs.len().saturating_sub(1));
        }
    }

    fn debugger_content_ui(&mut self, ui: &mut egui::Ui) {
        let actions = ui::debugger_ui(ui, &mut self.debugger);
        for action in actions {
            match action {
                ui::DebuggerAction::Resume => {
                    self.debugger_continue_playback();
                }
                ui::DebuggerAction::Step => {
                    let step_cycles = 1.0;
                    let from = self.current_time;
                    let target = from + step_cycles;
                    self.do_pause();
                    self.playing = false;
                    self.paused = true;
                    self.do_seek(target);
                    self.debugger.paused_at_breakpoint = true;
                    self.debugger.last_hit_message = Some(format!(
                        "Step : {:.3} → {:.3} (Δ {:.3} cycle)",
                        from, target, step_cycles
                    ));
                }
                ui::DebuggerAction::RemoveBreakpoint(id) => {
                    self.debugger.breakpoints.retain(|bp| bp.id != id);
                }
                ui::DebuggerAction::RemoveWatchpoint(id) => {
                    self.debugger.watchpoints.retain(|wp| wp.id != id);
                }
                ui::DebuggerAction::RemoveCatchpoint(id) => {
                    self.debugger.catchpoints.retain(|cp| cp.id != id);
                }
                ui::DebuggerAction::ToggleBreakpoint(id) => {
                    if let Some(bp) = self.debugger.breakpoints.iter_mut().find(|bp| bp.id == id) {
                        bp.enabled = !bp.enabled;
                    }
                }
                ui::DebuggerAction::ToggleWatchpoint(id) => {
                    if let Some(wp) = self.debugger.watchpoints.iter_mut().find(|wp| wp.id == id) {
                        wp.enabled = !wp.enabled;
                    }
                }
                ui::DebuggerAction::ToggleCatchpoint(id) => {
                    if let Some(cp) = self.debugger.catchpoints.iter_mut().find(|cp| cp.id == id) {
                        cp.enabled = !cp.enabled;
                    }
                }
                ui::DebuggerAction::AddBreakpointAt(pos, label) => {
                    self.debugger.add_breakpoint(pos, label);
                }
                ui::DebuggerAction::AddWatchpoint(label) => {
                    self.debugger.add_watchpoint(label);
                }
                ui::DebuggerAction::AddCatchpointError => {
                    self.debugger.add_catchpoint(ui::CatchpointKind::AnyError);
                }
                ui::DebuggerAction::AddCatchpointKeyword(k) => {
                    self.debugger.add_catchpoint(ui::CatchpointKind::Keyword(k));
                }
                ui::DebuggerAction::ClearAllBreakpoints => {
                    self.debugger.breakpoints.clear();
                }
                ui::DebuggerAction::DisableAllBreakpoints => {
                    for bp in &mut self.debugger.breakpoints {
                        bp.enabled = false;
                    }
                }
                ui::DebuggerAction::EnableAllBreakpoints => {
                    for bp in &mut self.debugger.breakpoints {
                        bp.enabled = true;
                    }
                }
            }
        }
    }

    /// Рекурсивная отрисовка дерева разметки. Сплиты рисуются по явным rect — без вертикального/горизонтального
    /// layout — чтобы области занимали ровно выделенное место и правая колонка не «сползала».
    fn layout_ui(&mut self, ui: &mut egui::Ui, node: &mut LayoutNode, path: &[usize]) {
        // Толщина разделителей между панелями.
        const SPLITTER_WIDTH: f32 = 4.0;
        const MIN_PANEL: f32 = 120.0;

        let full = ui.available_rect_before_wrap();
        // Резервируем всю область сразу, чтобы следующий виджет не смещался.
        ui.allocate_exact_size(full.size(), egui::Sense::hover());

        match node {
            LayoutNode::Split {
                horizontal,
                ratio,
                a,
                b,
            } => {
                let total = if *horizontal {
                    full.width().max(200.0)
                } else {
                    full.height().max(200.0)
                };
                let other_dim = if *horizontal {
                    full.height()
                } else {
                    full.width()
                };
                let max_first = (total - MIN_PANEL - SPLITTER_WIDTH).max(MIN_PANEL);
                let first_size = (total * *ratio).clamp(MIN_PANEL, max_first);
                let second_size = (total - first_size - SPLITTER_WIDTH).max(0.0);

                let (first_rect, splitter_rect, second_rect) = if *horizontal {
                    let r1 = egui::Rect::from_min_size(full.min, egui::vec2(first_size, other_dim));
                    let r2 = egui::Rect::from_min_size(
                        egui::pos2(full.min.x + first_size, full.min.y),
                        egui::vec2(SPLITTER_WIDTH, other_dim),
                    );
                    let r3 = egui::Rect::from_min_size(
                        egui::pos2(full.min.x + first_size + SPLITTER_WIDTH, full.min.y),
                        egui::vec2(second_size, other_dim),
                    );
                    (r1, r2, r3)
                } else {
                    let r1 = egui::Rect::from_min_size(full.min, egui::vec2(other_dim, first_size));
                    let r2 = egui::Rect::from_min_size(
                        egui::pos2(full.min.x, full.min.y + first_size),
                        egui::vec2(other_dim, SPLITTER_WIDTH),
                    );
                    let r3 = egui::Rect::from_min_size(
                        egui::pos2(full.min.x, full.min.y + first_size + SPLITTER_WIDTH),
                        egui::vec2(other_dim, second_size),
                    );
                    (r1, r2, r3)
                };

                ui.allocate_ui_at_rect(first_rect, |ui| {
                    self.layout_ui(ui, a, &path_iter(path, 0));
                });

                ui.allocate_ui_at_rect(splitter_rect, |ui| {
                    let r = ui.available_rect_before_wrap();
                    let splitter_id = egui::Id::new(("splitter", path.to_vec()));
                    // Реагируем только на наведение/drag именно на этой полоске.
                    let resp = ui.interact(r, splitter_id, egui::Sense::click_and_drag());
                    let fill = if resp.hovered() || resp.dragged() {
                        // Активный сплиттер — более светло-серый.
                        egui::Color32::from_rgba_premultiplied(180, 180, 180, 180)
                    } else {
                        // Сплиттер по умолчанию — полупрозрачный серый.
                        egui::Color32::from_rgba_premultiplied(140, 140, 140, 160)
                    };
                    ui.painter().rect_filled(r, 0.0, fill);
                    if resp.hovered() || resp.dragged() {
                        ui.output_mut(|o| {
                            o.cursor_icon = if *horizontal {
                                egui::CursorIcon::ResizeHorizontal
                            } else {
                                egui::CursorIcon::ResizeVertical
                            }
                        });
                    }
                    if resp.dragged() && total > 0.0 {
                        let delta = if *horizontal {
                            ui.input(|i| i.pointer.delta().x)
                        } else {
                            ui.input(|i| i.pointer.delta().y)
                        };
                        if delta.abs() > 0.0 {
                            let cur = (total * *ratio).clamp(MIN_PANEL, max_first);
                            let new = (cur + delta).clamp(MIN_PANEL, max_first);
                            *ratio = (new / total).clamp(0.15, 0.85);
                        }
                    }
                });

                ui.allocate_ui_at_rect(second_rect, |ui| {
                    self.layout_ui(ui, b, &path_iter(path, 1));
                });
            }
            LayoutNode::Panel(data) => {
                ui.allocate_ui_at_rect(full, |ui| {
                    self.panel_ui(ui, data, path);
                });
            }
        }
    }

    /// Отрисовка одной панели: вкладки сверху, контент — на всё оставшееся место.
    fn panel_ui(&mut self, ui: &mut egui::Ui, data: &mut PanelData, path: &[usize]) {
        let panel_id = egui::Id::new(("panel", path.to_vec()));
        let full = ui.available_rect_before_wrap();

        // Полностью перекрываем предыдущий контент в области панели тем же цветом, что и фон окна,
        // чтобы текст и элементы из других панелей не «просвечивали» сквозь прозрачные виджеты.
        ui.painter()
            .rect_filled(full, 0.0, ui.visuals().window_fill());

        const TAB_STRIP_H: f32 = 24.0;
        const SEP_H: f32 = 2.0;
        let tabs_rect = egui::Rect::from_min_size(full.min, egui::vec2(full.width(), TAB_STRIP_H));
        let content_rect = egui::Rect::from_min_max(
            egui::pos2(full.min.x, full.min.y + TAB_STRIP_H + SEP_H),
            full.max,
        );

        if content_rect.width() <= 0.0 || content_rect.height() <= 0.0 {
            return;
        }

        let edge_size = (content_rect.width().min(content_rect.height()) * 0.2).clamp(24.0, 120.0);

        // Результаты кликов по вкладкам — заполняются внутри замыканий, применяются снаружи,
        // чтобы исключить проблемы с вложенными мутабельными захватами self.
        let mut tab_activated: Option<ContentId> = None;
        let mut open_samples_settings = false;

        // Полоска вкладок (стиль VS Code)
        ui.allocate_ui_at_rect(tabs_rect, |ui| {
            let bg = ui.visuals().window_fill();
            ui.painter().rect_filled(tabs_rect, 0.0, bg);

            egui::ScrollArea::horizontal()
                .id_salt(panel_id.with("tabs"))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        let active_bg = egui::Color32::from_white_alpha(12);
                        let hover_bg = egui::Color32::from_white_alpha(8);
                        let text_active = egui::Color32::from_gray(220);
                        let text_inactive = egui::Color32::from_gray(140);
                        let separator_color = egui::Color32::from_white_alpha(16);

                        let tabs_snapshot = data.tabs.clone();

                        for (i, &content_id) in tabs_snapshot.iter().enumerate() {
                            let selected = i == data.active;
                            let mut label = content_id.label().to_string();
                            if content_id == ContentId::SamplesSettings {
                                label.push(' ');
                            }

                            let text_width = ui
                                .painter()
                                .layout_no_wrap(
                                    label.clone(),
                                    egui::FontId::proportional(13.0),
                                    egui::Color32::WHITE,
                                )
                                .size()
                                .x;
                            let extra_w = if content_id == ContentId::Samples {
                                20.0
                            } else {
                                0.0
                            };
                            let tab_size = egui::vec2(text_width + 24.0 + extra_w, TAB_STRIP_H);
                            let (rect, _) = ui.allocate_exact_size(tab_size, egui::Sense::hover());
                            // Явный уникальный ID — исключает коллизии между панелями.
                            let resp = ui.interact(
                                rect,
                                panel_id.with(("tab", content_id)),
                                egui::Sense::click_and_drag(),
                            );

                            if selected {
                                ui.painter().rect_filled(rect, 0.0, active_bg);
                            } else if resp.hovered() {
                                ui.painter().rect_filled(rect, 0.0, hover_bg);
                            }

                            let text_color = if selected { text_active } else { text_inactive };
                            ui.painter().text(
                                egui::pos2(rect.center().x - extra_w * 0.25, rect.center().y),
                                egui::Align2::CENTER_CENTER,
                                label,
                                egui::FontId::proportional(13.0),
                                text_color,
                            );

                            // Шестерёнка на вкладке «Семплы» — добавляет/активирует вкладку «Настройки семплов».
                            if content_id == ContentId::Samples {
                                let gear_rect = egui::Rect::from_min_max(
                                    egui::pos2(rect.max.x - extra_w, rect.min.y),
                                    rect.max,
                                );
                                let gear_resp = ui.interact(
                                    gear_rect,
                                    panel_id.with("samples_settings_tab"),
                                    egui::Sense::click(),
                                );
                                ui.painter().text(
                                    gear_rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    "⚙",
                                    egui::FontId::proportional(12.0),
                                    text_color,
                                );
                                if gear_resp.clicked() {
                                    open_samples_settings = true;
                                }
                            }

                            if i + 1 < data.tabs.len() {
                                let sep_x = rect.max.x;
                                let sep_top = rect.min.y + 5.0;
                                let sep_bot = rect.max.y - 5.0;
                                ui.painter().line_segment(
                                    [egui::pos2(sep_x, sep_top), egui::pos2(sep_x, sep_bot)],
                                    egui::Stroke::new(1.0, separator_color),
                                );
                            }

                            if resp.clicked() && i < data.tabs.len() {
                                data.active = i;
                                tab_activated = Some(content_id);
                            }
                            if resp.drag_started() {
                                self.dragging_tab = Some(content_id);
                                resp.dnd_set_drag_payload(content_id.payload_str().to_string());
                            }
                        }
                    });
                });
        });

        // Применяем playback_focus_content снаружи всех замыканий.
        if open_samples_settings {
            if let Some(pos) = data
                .tabs
                .iter()
                .position(|&c| c == ContentId::SamplesSettings)
            {
                data.active = pos;
            } else {
                data.tabs.push(ContentId::SamplesSettings);
                data.active = data.tabs.len() - 1;
            }
            if let Some(&content) = data.tabs.get(data.active) {
                self.playback_focus_content = content;
            }
        } else if let Some(content) = tab_activated {
            self.playback_focus_content = content;
        }

        // Тонкая линия под полоской вкладок
        let sep_y = tabs_rect.max.y;
        ui.painter().line_segment(
            [egui::pos2(full.min.x, sep_y), egui::pos2(full.max.x, sep_y)],
            egui::Stroke::new(1.0, egui::Color32::from_white_alpha(16)),
        );

        // Сначала рисуем основной контент панели на всю доступную область.
        ui.allocate_ui_at_rect(content_rect, |ui| {
            if let Some(content_id) = data.active_content() {
                self.content_ui(ui, content_id);
            }
        });

        // Обработка dnd с лёгкими визуальными подсказками и плавной анимацией:
        // - центральная зона (content_rect.shrink(edge_size)) — добавить вкладку в эту панель (подсветка всей области);
        // - края толщиной edge_size — создать новую область (подсветка будущей половины панели).
        if egui::DragAndDrop::has_any_payload(ui.ctx()) {
            // Позиция указателя в тех же координатах, что и rect'ы панели.
            if let Some(pointer_pos) = ui.ctx().pointer_latest_pos() {
                let inner_rect = content_rect.shrink(edge_size);
                let r = content_rect;
                let edges = [
                    (
                        egui::Rect::from_min_size(r.min, egui::vec2(edge_size, r.height())),
                        true,
                        true,
                    ),
                    (
                        egui::Rect::from_min_max(egui::pos2(r.max.x - edge_size, r.min.y), r.max),
                        true,
                        false,
                    ),
                    (
                        egui::Rect::from_min_size(r.min, egui::vec2(r.width(), edge_size)),
                        false,
                        true,
                    ),
                    (
                        egui::Rect::from_min_max(egui::pos2(r.min.x, r.max.y - edge_size), r.max),
                        false,
                        false,
                    ),
                ];

                // --- Визуальная подсказка (вычисляем целевой прямоугольник) ---
                let mut target_rect: Option<egui::Rect> = None;
                if inner_rect.contains(pointer_pos) {
                    // Подсветка всей панели.
                    target_rect = Some(content_rect);
                } else {
                    for (edge_rect, horizontal, new_first) in edges {
                        if edge_rect.contains(pointer_pos) {
                            // Показываем, на какую половину разобьётся панель.
                            let half_rect = if horizontal {
                                let mid_x = (content_rect.min.x + content_rect.max.x) * 0.5;
                                if new_first {
                                    egui::Rect::from_min_max(
                                        content_rect.min,
                                        egui::pos2(mid_x, content_rect.max.y),
                                    )
                                } else {
                                    egui::Rect::from_min_max(
                                        egui::pos2(mid_x, content_rect.min.y),
                                        content_rect.max,
                                    )
                                }
                            } else {
                                let mid_y = (content_rect.min.y + content_rect.max.y) * 0.5;
                                if new_first {
                                    egui::Rect::from_min_max(
                                        content_rect.min,
                                        egui::pos2(content_rect.max.x, mid_y),
                                    )
                                } else {
                                    egui::Rect::from_min_max(
                                        egui::pos2(content_rect.min.x, mid_y),
                                        content_rect.max,
                                    )
                                }
                            };
                            target_rect = Some(half_rect);
                            break;
                        }
                    }
                }
                let current_panel = path.to_vec();

                if target_rect.is_some() {
                    if self.dnd_preview_panel.as_ref() != Some(&current_panel) {
                        self.dnd_preview_alpha = 0.0;
                        self.dnd_preview_rect = None;
                        self.dnd_preview_panel = Some(current_panel.clone());
                    }
                }

                let is_owner = self.dnd_preview_panel.as_ref() == Some(&current_panel);

                if is_owner {
                    let dt = ui.ctx().input(|i| i.stable_dt).min(0.1);
                    let speed = 22.0;
                    let k = 1.0 - (-speed * dt).exp();

                    let target_alpha = if target_rect.is_some() { 1.0 } else { 0.0 };
                    self.dnd_preview_alpha += (target_alpha - self.dnd_preview_alpha) * k;

                    if let Some(target) = target_rect {
                        self.dnd_preview_rect = Some(match self.dnd_preview_rect {
                            Some(prev) => {
                                let lerp = |a: f32, b: f32| a + (b - a) * k;
                                egui::Rect::from_min_max(
                                    egui::pos2(
                                        lerp(prev.min.x, target.min.x),
                                        lerp(prev.min.y, target.min.y),
                                    ),
                                    egui::pos2(
                                        lerp(prev.max.x, target.max.x),
                                        lerp(prev.max.y, target.max.y),
                                    ),
                                )
                            }
                            None => target,
                        });
                    }

                    if let Some(pr) = self.dnd_preview_rect {
                        let alpha = (self.dnd_preview_alpha * 25.0).clamp(0.0, 25.0) as u8;
                        if alpha > 0 {
                            ui.painter().rect_filled(
                                pr,
                                0.0,
                                egui::Color32::from_white_alpha(alpha),
                            );
                        }
                    }
                }

                // --- Применение действия при отпускании кнопки ---
                // Сначала проверяем, что указатель вообще в зоне этой панели,
                // чтобы не забирать payload чужой панели.
                let pointer_in_panel = content_rect.contains(pointer_pos);
                let pointer_released = ui.input(|i| i.pointer.any_released());
                if pointer_released && pointer_in_panel {
                    if let Some(payload) = egui::DragAndDrop::take_payload::<String>(ui.ctx()) {
                        if let Some(id) = ContentId::from_payload(payload.as_str()) {
                            if inner_rect.contains(pointer_pos) {
                                self.pending_layout_actions
                                    .push(LayoutAction::AddTabToPanel {
                                        content_id: id,
                                        path: path.to_vec(),
                                    });
                                return;
                            }
                            for (edge_rect, horizontal, new_first) in edges {
                                if edge_rect.contains(pointer_pos) {
                                    self.pending_layout_actions.push(LayoutAction::SplitPanel {
                                        path: path.to_vec(),
                                        horizontal,
                                        content_id: id,
                                        new_panel_first: new_first,
                                    });
                                    return;
                                }
                            }
                        }
                    }
                }
            }
        } else {
            // Нет активного drag'n'drop, но могли остаться хвосты анимации — плавно дотягиваем до нуля.
            if self.dnd_preview_alpha > 0.0 || self.dnd_preview_rect.is_some() {
                let dt = ui.ctx().input(|i| i.stable_dt).min(0.1);
                let speed = 28.0;
                let k = 1.0 - (-speed * dt).exp();
                self.dnd_preview_alpha += (0.0 - self.dnd_preview_alpha) * k;
                if self.dnd_preview_alpha <= 0.01 {
                    self.dnd_preview_alpha = 0.0;
                    self.dnd_preview_rect = None;
                    self.dnd_preview_kind = None;
                    self.dnd_preview_panel = None;
                } else if let Some(pr) = self.dnd_preview_rect {
                    let alpha = (self.dnd_preview_alpha * 25.0).clamp(0.0, 25.0) as u8;
                    if alpha > 0 {
                        ui.painter()
                            .rect_filled(pr, 0.0, egui::Color32::from_white_alpha(alpha));
                    }
                }
            }
        }
    }
}

fn compensated_recording_placement(
    started_at_cycle: f64,
    latency_seconds: f64,
    cycles_per_second: f64,
) -> (f64, f64) {
    let compensation_cycles = latency_seconds.max(0.0) * cycles_per_second.max(0.0);
    let compensated_start = (started_at_cycle - compensation_cycles).max(0.0);
    // If compensated start goes before project zero, skip the same amount from source.
    let source_offset = (compensation_cycles - started_at_cycle).max(0.0);
    (compensated_start, source_offset)
}

/// Prise enregistrée (banque `vox` ou `recordings`) : seule catégorie supprimable depuis la palette.
fn is_voice_key(key: &SampleKey) -> bool {
    matches!(key.bank.as_deref(), Some("vox") | Some("recordings"))
}

/// Corbeille des prises supprimées : placée à côté de la racine des samples
/// (`~/.local/share/struidel/trash`) pour ne pas être rescanée comme une banque.
fn sample_trash_dir(sample_root: &Path) -> PathBuf {
    sample_root
        .parent()
        .map(|p| p.join("trash"))
        .unwrap_or_else(|| sample_root.join(".trash"))
}

/// Déplace un fichier vers la corbeille (récupérable) plutôt que de l'effacer.
/// Échoue sans toucher au fichier si le déplacement est impossible.
fn move_to_trash(path: &Path, sample_root: &Path) -> Result<PathBuf, String> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "nom de fichier illisible".to_string())?;
    let bank = path
        .strip_prefix(sample_root)
        .ok()
        .and_then(|rel| rel.parent().and_then(|p| p.file_name()))
        .and_then(|n| n.to_str())
        .unwrap_or("root")
        .to_string();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = sample_trash_dir(sample_root).join(bank);
    std::fs::create_dir_all(&dir).map_err(|e| format!("corbeille: {e}"))?;
    let dest = dir.join(format!("{stamp}-{name}"));
    std::fs::rename(path, &dest).map_err(|e| format!("déplacement vers la corbeille: {e}"))?;
    Ok(dest)
}

/// Source d'un aperçu : fichier disque, ou son généré (synthé / GM) par le moteur natif.
enum PreviewSource {
    Disk(SampleKey),
    Generated(String),
}

#[derive(Clone, Copy, PartialEq)]
enum SoundIcon {
    Add,
    Play,
    Playing,
    Trash,
    Check,
    Cross,
}

/// Rend les widgets inline (`slider(v,min,max)`, `toggler(v)`) trouvés dans le
/// code ; renvoie le code réécrit si l'un est bougé.
fn render_inline_sliders(
    ui: &mut egui::Ui,
    code: &str,
    sliders: &[crate::live_params::InlineWidget],
) -> Option<String> {
    use crate::live_params::InlineWidgetKind;
    if sliders.is_empty() {
        return None;
    }
    ui.separator();
    ui.label(
        egui::RichText::new("widgets du code — slider(v,min,max) · toggler(v)")
            .small()
            .color(ui.visuals().weak_text_color()),
    );
    let mut edit = None;
    for s in sliders {
        let mut v = s.value;
        ui.horizontal(|ui| {
            ui.label(&s.context);
            match s.kind {
                InlineWidgetKind::Slider => {
                    if ui
                        .add(egui::Slider::new(&mut v, s.min..=s.max).fixed_decimals(2))
                        .changed()
                    {
                        edit = Some(crate::live_params::set_inline_widget_value(code, s, v));
                    }
                }
                InlineWidgetKind::Toggler => {
                    let mut on = v >= 0.5;
                    let label = if on { "on" } else { "off" };
                    if ui.checkbox(&mut on, label).changed() {
                        edit = Some(crate::live_params::set_inline_widget_value(
                            code,
                            s,
                            if on { 1.0 } else { 0.0 },
                        ));
                    }
                }
            }
        });
    }
    edit
}

/// Bouton-icône dessiné au pinceau : ne dépend d'aucune police (pas de carrés vides).
fn sound_icon_button(ui: &mut egui::Ui, icon: SoundIcon, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(22.0, 18.0), egui::Sense::click());
    let resp = resp.on_hover_text(tip);
    let (bg, col) = {
        let vis = ui.style().interact(&resp);
        (vis.weak_bg_fill, vis.fg_stroke.color)
    };
    let accent = ui.visuals().hyperlink_color;
    let c = rect.center();
    let v = |x: f32, y: f32| c + egui::vec2(x, y);
    let s = egui::Stroke::new(1.5, col);
    let p = ui.painter();
    p.rect_filled(rect, 3.0, bg);
    match icon {
        SoundIcon::Add => {
            p.line_segment([v(-5.0, 0.0), v(5.0, 0.0)], s);
            p.line_segment([v(0.0, -5.0), v(0.0, 5.0)], s);
        }
        SoundIcon::Cross => {
            p.line_segment([v(-4.0, -4.0), v(4.0, 4.0)], s);
            p.line_segment([v(-4.0, 4.0), v(4.0, -4.0)], s);
        }
        SoundIcon::Check => {
            p.line_segment([v(-5.0, 0.0), v(-1.5, 4.0)], s);
            p.line_segment([v(-1.5, 4.0), v(5.0, -4.0)], s);
        }
        SoundIcon::Play => {
            p.add(egui::Shape::convex_polygon(
                vec![v(-3.5, -5.0), v(-3.5, 5.0), v(5.0, 0.0)],
                col,
                egui::Stroke::NONE,
            ));
        }
        SoundIcon::Playing => {
            // Égaliseur animé : signale qu'un son joue.
            let t = ui.input(|i| i.time) as f32;
            for i in 0..3 {
                let h = 3.0 + 8.0 * (t * 7.0 + i as f32 * 2.1).sin().abs();
                let x = c.x - 4.0 + i as f32 * 4.0;
                p.rect_filled(
                    egui::Rect::from_center_size(egui::pos2(x, c.y), egui::vec2(2.5, h)),
                    1.0,
                    accent,
                );
            }
        }
        SoundIcon::Trash => {
            p.line_segment([v(-5.0, -4.0), v(5.0, -4.0)], s);
            p.line_segment([v(-2.0, -4.0), v(-2.0, -6.0)], s);
            p.line_segment([v(2.0, -4.0), v(2.0, -6.0)], s);
            p.line_segment([v(-2.0, -6.0), v(2.0, -6.0)], s);
            p.rect_stroke(
                egui::Rect::from_min_max(v(-4.0, -2.0), v(4.0, 6.0)),
                1.0,
                s,
                egui::StrokeKind::Middle,
            );
            p.line_segment([v(-1.5, 0.0), v(-1.5, 4.0)], s);
            p.line_segment([v(1.5, 0.0), v(1.5, 4.0)], s);
        }
    }
    resp
}

/// Remplace l'insert du même nom s'il existe, sinon l'ajoute.
fn upsert_insert(inserts: &mut Vec<FxSlot>, slot: FxSlot) {
    let name = slot.name.to_ascii_lowercase();
    if let Some(existing) = inserts
        .iter_mut()
        .find(|s| s.name.to_ascii_lowercase() == name)
    {
        *existing = slot;
    } else {
        inserts.push(slot);
    }
}

/// Lance `pactl` avec un timeout : pendant un bascule de profil Bluetooth,
/// PipeWire peut laisser `pactl` bloqué, ce qui gèlerait le thread UI.
fn run_pactl(args: &[&str]) -> Option<std::process::Output> {
    let mut child = std::process::Command::new("pactl")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Résout le nom de la source PipeWire utilisée pour la capture.
/// `device_name` = nom cpal choisi dans l'UI ; None = source par défaut.
fn resolve_capture_source(device_name: Option<&str>) -> Option<String> {
    match device_name {
        Some(n) if n.starts_with("bluez_") => Some(n.to_string()),
        Some(n) => {
            let needle = n.to_lowercase();
            if needle.contains("default") || needle.contains("pulse") {
                return resolve_capture_source(None);
            }
            let out = run_pactl(&["-f", "json", "list", "sources"])?;
            let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
            for src in v.as_array()? {
                let name = src.get("name").and_then(|x| x.as_str()).unwrap_or("");
                let desc = src
                    .get("properties")
                    .and_then(|p| p.get("device.description"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                if name.to_lowercase().contains(&needle) || desc.to_lowercase().contains(&needle) {
                    return Some(name.to_string());
                }
            }
            None
        }
        None => run_pactl(&["get-default-source"])
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty()),
    }
}

/// Si l'entrée d'enregistrement est un casque Bluetooth, force le profil
/// `headset-head-unit` (même logique que stt_toggle.sh — sinon la capture
/// gèle après ~1 s tant que le casque reste en A2DP).
/// Retourne `(carte, profil_précédent)` à restaurer après l'enregistrement.
fn force_hfp_if_bluetooth_input(device_name: Option<&str>) -> Option<(String, String)> {
    let source = resolve_capture_source(device_name)?;
    let mac = source
        .strip_prefix("bluez_input.")
        .or_else(|| source.strip_prefix("bluez_source."))?;
    let card = format!("bluez_card.{}", mac.replace(':', "_"));

    let out = run_pactl(&["list", "cards"])?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut current = String::from("a2dp-sink");
    let mut in_card = false;
    for line in text.lines() {
        let t = line.trim();
        if let Some(name) = t.strip_prefix("Name:") {
            in_card = name.trim() == card;
        } else if in_card {
            if let Some(p) = t.strip_prefix("Active Profile:") {
                current = p.trim().to_string();
                break;
            }
        }
    }
    if current.contains("headset") {
        return None;
    }
    let ok = run_pactl(&["set-card-profile", &card, "headset-head-unit"])
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        return None;
    }
    eprintln!("[Recording] Bluetooth {card}: {current} → headset-head-unit");
    Some((card, current))
}

/// Restaure le profil Bluetooth sauvegardé avant l'enregistrement.
fn restore_bt_profile(saved: Option<(String, String)>) {
    if let Some((card, profile)) = saved {
        let _ = run_pactl(&["set-card-profile", &card, &profile]);
        eprintln!("[Recording] Bluetooth {card}: profil restauré → {profile}");
    }
}

fn path_iter(path: &[usize], next: usize) -> Vec<usize> {
    let mut p = path.to_vec();
    p.push(next);
    p
}

fn remove_sound_token_from_code(code: &str, sample: &str, begin: f64) -> Option<String> {
    for call in ["sound", "s"] {
        let mut search_from = 0usize;
        while let Some(rel) = code[search_from..].find(call) {
            let call_start = search_from + rel;
            let after_call = call_start + call.len();
            let rest = &code[after_call..];
            let Some(open_rel) = rest.find('(') else {
                break;
            };
            if !rest[..open_rel].trim().is_empty() {
                search_from = after_call;
                continue;
            }

            let open = after_call + open_rel;
            let Some((quote_start, quote)) = first_quote_after_open(code, open) else {
                search_from = open + 1;
                continue;
            };
            if !code[open + 1..quote_start].trim().is_empty() {
                search_from = open + 1;
                continue;
            }
            let Some(quote_end) = find_quote_end(code, quote_start, quote) else {
                return None;
            };
            let inner = &code[quote_start + quote.len_utf8()..quote_end];
            let tokens: Vec<&str> = inner.split_whitespace().collect();
            if tokens.is_empty() {
                search_from = quote_end + quote.len_utf8();
                continue;
            }
            let index = ((begin.fract().rem_euclid(1.0) * tokens.len() as f64).floor() as usize)
                .min(tokens.len().saturating_sub(1));
            if !token_matches_sample(tokens[index], sample) {
                search_from = quote_end + quote.len_utf8();
                continue;
            }

            let mut next_tokens = tokens;
            next_tokens.remove(index);
            let replacement = next_tokens.join(" ");
            let mut out = String::with_capacity(code.len());
            out.push_str(&code[..quote_start + quote.len_utf8()]);
            out.push_str(&replacement);
            out.push_str(&code[quote_end..]);
            return Some(out);
        }
    }
    None
}

fn first_quote_after_open(code: &str, open: usize) -> Option<(usize, char)> {
    code[open + 1..].char_indices().find_map(|(idx, ch)| {
        if ch == '"' || ch == '\'' || ch == '`' {
            Some((open + 1 + idx, ch))
        } else if ch.is_whitespace() {
            None
        } else {
            None
        }
    })
}

fn find_quote_end(code: &str, quote_start: usize, quote: char) -> Option<usize> {
    let mut escaped = false;
    for (idx, ch) in code[quote_start + quote.len_utf8()..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            return Some(quote_start + quote.len_utf8() + idx);
        }
    }
    None
}

fn token_matches_sample(token: &str, sample: &str) -> bool {
    let token = token.trim_matches(|ch: char| {
        matches!(
            ch,
            '[' | ']' | '<' | '>' | '(' | ')' | ',' | '"' | '\'' | '`'
        )
    });
    token == sample
        || token
            .strip_prefix(sample)
            .map(|rest| rest.starts_with('*') || rest.starts_with(':') || rest.starts_with('/'))
            .unwrap_or(false)
}

impl eframe::App for StruidelApp {
    /// Цвет очистки окна (фон всего окна до отрисовки виджетов).
    /// По умолчанию eframe берёт `visuals.panel_fill`, а мы хотим использовать именно `window_fill`,
    /// чтобы окно не становилось прозрачным, даже если у панелей нет собственной заливки.
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        egui::Rgba::from(visuals.window_fill).to_array()
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Heartbeat watchdog : un blocage prolongé alors que l'UI est censée
        // ticker (lecture/enregistrement/init) est relayé en notification.
        self.ui_heartbeat.store(now_millis(), Ordering::Relaxed);
        // Miroir pour sched-tick : le worker ne prend le relais du scheduling
        // audio que si ce heartbeat cesse (fenêtre occluse → update() gelée).
        self.sched.playing.store(self.playing, Ordering::Relaxed);
        self.sched.paused.store(self.paused, Ordering::Relaxed);
        self.sched.reverse.store(self.reverse, Ordering::Relaxed);
        self.sched
            .audio_enabled
            .store(self.settings.audio_enabled, Ordering::Relaxed);
        SchedShared::set_f64(&self.sched.current_time, self.current_time);
        if let Ok(mut at) = self.sched.current_time_at.lock() {
            *at = Instant::now();
        }
        SchedShared::set_f64(&self.sched.cps, self.last_cps);
        SchedShared::set_f64(&self.sched.session_start, self.session_start());
        SchedShared::set_f64(&self.sched.audio_gain, self.settings.audio_gain as f64);
        if let Ok(mut e) = self.sched.engine.lock() {
            *e = self.audio.clone();
        }
        let busy = if self.playing {
            "lecture en cours"
        } else if self.paused {
            "lecture en pause"
        } else if self.audio_recording.is_recording() {
            "enregistrement en cours"
        } else if self.pending_audio_engine.is_some() {
            "initialisation du moteur audio"
        } else if self.pending_recording_finalize.is_some() {
            "finalisation d'enregistrement"
        } else if self.pending_voice_export.is_some() {
            "export voix"
        } else {
            ""
        };
        self.ui_expect_ticks
            .store(!busy.is_empty(), Ordering::Relaxed);
        if let Ok(mut s) = self.ui_busy_label.try_lock() {
            if *s != busy {
                *s = busy.to_string();
            }
        }
        // Diagnostic : gel UI simulé (une fois, pendant l'activité).
        if !busy.is_empty() {
            let stall = self.debug_ui_stall_ms.swap(0, Ordering::Relaxed);
            if stall > 0 {
                thread::sleep(Duration::from_millis(stall));
            }
        }
        self.poll_pending_audio_engine();
        self.poll_dead_audio_stream();
        self.poll_clip_fx_renders();
        self.poll_control_api(ctx);
        self.poll_chain_seed_fetch();
        self.poll_github_import();
        self.poll_ai_pattern();
        self.tick_chain_seed_live();
        // Если drag'n'drop больше не активен, сбрасываем информацию о перетаскиваемой вкладке.
        if !egui::DragAndDrop::has_any_payload(ctx) {
            self.dragging_tab = None;
        }
        if self.samples_loading {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        if self.chain_seed.busy
            || self.ai_pattern.busy
            || self.github_import_rx.is_some()
            || self.chain_seed.live_follow
            || (self.chain_seed.live_morph && self.playing)
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if self.bridge.take_loadsamples_completed() {
            self.reload_samples_at = Some(Instant::now() + Duration::from_millis(500));
            self.samples_loading = false;
            self.samples_loading_started = None;
        }
        if let Some(deadline) = self.reload_samples_at {
            if deadline <= Instant::now() {
                self.reload_samples_at = None;
                self.sample_browser_entries_cache = None;
                self.sample_browser_filtered_rows = None;
                if let Some(engine) = &self.audio {
                    use std::path::Path;
                    engine.reload_samples_async(Path::new(&self.settings.sample_folder));
                }
            }
        }
        if self.reload_samples_after_eval {
            self.reload_samples_after_eval = false;
            let throttle = self
                .last_samples_reload
                .map(|t| t.elapsed() >= Duration::from_secs(2))
                .unwrap_or(true);
            if throttle {
                self.last_samples_reload = Some(Instant::now());
                if let Some(engine) = &self.audio {
                    use std::path::Path;
                    engine.reload_samples_async(Path::new(&self.settings.sample_folder));
                }
            }
        }
        self.poll_sync_engine();
        self.sync_playback_source_from_active_tab();
        // Источник времени:
        //   - native pure-путь: ведём время САМИ, игнорируем bridge.now() полностью.
        //     bridge.evaluate() приходит с большой задержкой (~300 мс), пока bridge не готов он
        //     отдаёт 0 или устаревшее время — это сбивало current_time в начале и schedule_audio_window
        //     планировало одни и те же события 2-3 раза подряд → наложение в начале.
        //   - bridge-путь: слушаем bridge.now() как раньше.
        let native_path_active = self.native_engine.lock().map(|e| e.is_pure()).unwrap_or(false) && !self.reverse;
        if !native_path_active {
            if let Some(t) = self.bridge.now() {
                if self.seek_ignore_now_frames > 0 {
                    self.seek_ignore_now_frames -= 1;
                } else if !(self.playing && self.reverse) {
                    self.last_bridge_now = t;
                    self.last_bridge_instant = Instant::now();
                    if !self.playing {
                        // Когда не играем, JS держит _pausedTime — используем напрямую.
                        self.current_time = t;
                        self.timeline_display_time = t;
                    }
                }
            }
        }
        if self.playing && self.last_cps > 0.0 {
            // В native-режиме ждать первого аудио-окна не нужно — оно отвечает мгновенно.
            if self.awaiting_first_audio_after_start && !native_path_active {
                // Не двигаем время, пока не пришёл первый ответ bridge audio_window.
                self.last_bridge_instant = Instant::now();
            } else {
                let elapsed = self.last_bridge_instant.elapsed().as_secs_f64();
                if self.reverse {
                    self.current_time = (self.last_bridge_now - elapsed * self.last_cps)
                        .max(self.session_start());
                } else {
                    self.current_time = self.last_bridge_now + elapsed * self.last_cps;
                }
            }
        }
        // Во время example: current_time в JS — это позиция внутри примера.
        // Синхронизируем её в отдельную переменную для подсветки токенов внутри диалога,
        // при этом current_time продолжает двигаться, чтобы schedule_audio_window/poll_pending_audio работали.
        if self.example_playback.is_active() {
            self.example_display_time = self.current_time;
        }
        if self.playing
            && self.reverse
            && self.current_time <= self.session_start()
            && !self.transport_state.loop_enabled
        {
            self.current_time = self.session_start();
            self.do_pause();
            self.playing = false;
            self.paused = true;
        }
        // Петля A-B
        if self.playing && self.transport_state.loop_enabled {
            let loop_hit = (!self.reverse && self.current_time >= self.transport_state.loop_end)
                || (self.reverse && self.current_time <= self.transport_state.loop_start);
            if loop_hit {
                let target = if self.reverse {
                    self.transport_state.loop_end
                } else {
                    self.transport_state.loop_start
                };
                // seek уже рестартует JS-планировщик если он играл — не нужно bridge.start()
                self.do_seek(target);
                // После do_seek last_bridge_now уже установлен в target,
                // reset instant чтобы интерполяция не скакала
                self.last_bridge_instant = Instant::now();
            }
        }
        // Реверс: отдельный scheduler с батч-окном/троттлингом seek.
        if self.playing && self.reverse {
            self.maybe_resync_reverse_seek();
        }
        // Проверка брейкпоинтов (не чаще 1 раза в кадр)
        if self.playing && !self.example_playback.is_active() && !self.debugger.paused_at_breakpoint
        {
            let t = self.current_time;
            self.bridge.request_query((t - 1.0).max(0.0), t + 2.0);
            let events_snapshot = self.bridge.query_arc(0.0, 0.0);
            if self
                .debugger
                .check_breakpoints(self.prev_time, self.current_time, &events_snapshot)
            {
                self.do_pause();
                self.playing = false;
                self.paused = true;
            }
            let active_source_keys = if let Some(editor) = self.active_editor() {
                let playing_events: Vec<(String, f64)> = events_snapshot
                    .iter()
                    .filter(|e| e.begin <= t && t < e.end)
                    .filter_map(|e| e.s.as_ref().map(|name| (name.clone(), e.begin.fract())))
                    .collect();
                ui::EditorTab::active_source_keys(
                    &editor.code,
                    self.current_time,
                    self.pattern_document.as_ref(),
                    &playing_events,
                )
            } else {
                std::collections::BTreeSet::new()
            };
            if self
                .debugger
                .track_source_keys(&active_source_keys, self.current_time)
            {
                self.do_pause();
                self.playing = false;
                self.paused = true;
            }
            if self
                .debugger
                .check_watchpoints(&events_snapshot, self.current_time)
            {
                self.do_pause();
                self.playing = false;
                self.paused = true;
            }
            let cp_code_owned = self.active_editor().map(|e| e.code.clone());
            let cp_code = cp_code_owned.as_deref();
            let cp_doc = self.pattern_document.as_ref();
            if let Some(err) = self.bridge.last_error() {
                if self
                    .debugger
                    .check_catchpoints(Some(&err), self.current_time, cp_code, cp_doc)
                {
                    self.do_pause();
                    self.playing = false;
                    self.paused = true;
                }
            } else if self
                .debugger
                .check_catchpoints(None, self.current_time, cp_code, cp_doc)
            {
                self.do_pause();
                self.playing = false;
                self.paused = true;
            }
        }
        self.update_audio_recording_state();
        self.poll_pending_recording_finalize();
        self.poll_pending_voice_export();
        self.sync_audio_mixer_runtime();
        self.update_metronome();
        self.tick_arrangement();
        self.prev_time = self.current_time;
        self.timeline_display_time = self.current_time;
        if self.playing || self.audio_recording.is_recording() || self.pending_recording_finalize.is_some() {
            ctx.request_repaint_after(Duration::from_millis(16)); // 60 fps при воспроизведении
        } else {
            self.debugger.clear_source_line_tracking();
            if self.paused {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }
        if self.playing {
            // Планируем аудио в том числе во время example — example играет через тот же bridge/audio engine.
            self.schedule_audio_window();
        }
        if egui::DragAndDrop::has_any_payload(ctx) || self.dnd_preview_alpha > 0.0 {
            ctx.request_repaint();
        }
        if self.save_as_requested {
            self.save_as_requested = false;
            let default_name = self
                .active_editor()
                .and_then(|e| e.file_path.as_ref())
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .unwrap_or("Untitled.strudel");
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("Strudel", &["strudel"])
                .set_file_name(default_name)
                .save_file()
            {
                self.save_file_as(path);
            }
        }
        // Опрос фонового потока экспорта.
        if self.export_dialog.status == ExportStatus::Running {
            if let Some(rx) = &self.export_dialog.rx {
                if let Ok(result) = rx.try_recv() {
                    self.export_dialog.status = match result {
                        Ok(()) => ExportStatus::Done,
                        Err(e) => ExportStatus::Error(e),
                    };
                    self.export_dialog.rx = None;
                } else {
                    ctx.request_repaint_after(Duration::from_millis(200));
                }
            }
        }

        // Диалог офлайн-экспорта.
        self.show_export_dialog(ctx);

        if let Some(e) = self.bridge.last_error() {
            let (line, column) = self.bridge.last_error_location().unwrap_or((0, 0));
            if let Some(ed) = self.active_editor_mut() {
                ed.eval_error = Some(e);
                ed.eval_error_line = (line > 0).then_some(line);
                ed.eval_error_column = (column > 0).then_some(column);
            }
        }

        // Горячие клавиши (как в @strudel/codemirror: Ctrl/Alt+Enter, Ctrl/Alt+.)
        ctx.input_mut(|i| {
            let run = i.consume_key(egui::Modifiers::CTRL, egui::Key::Enter)
                || i.consume_key(egui::Modifiers::ALT, egui::Key::Enter);
            if run {
                self.run_editor_code();
            }
            let stop = i.consume_key(egui::Modifiers::CTRL, egui::Key::Period)
                || i.consume_key(egui::Modifiers::ALT, egui::Key::Period);
            if stop {
                self.do_pause();
                self.playing = false;
                self.paused = false;
            }
            if self.debugger.paused_at_breakpoint && i.key_pressed(egui::Key::F5) {
                self.debugger_continue_playback();
            }
            if i.consume_key(egui::Modifiers::CTRL, egui::Key::S) {
                if i.modifiers.shift {
                    self.save_as_requested = true;
                } else {
                    let has_path = self
                        .active_editor()
                        .map(|e| e.file_path.is_some())
                        .unwrap_or(false);
                    if has_path {
                        self.save_file();
                    }
                }
            }
            if i.consume_key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT, egui::Key::E) {
                self.export_dialog.open = true;
                if self.export_dialog.duration_cycles.is_empty() {
                    self.export_dialog.duration_cycles = "8".to_string();
                }
                if self.export_dialog.output_path.is_empty() {
                    self.export_dialog.output_path = std::env::var("HOME")
                        .map(|h| std::path::PathBuf::from(h).join("export.wav"))
                        .unwrap_or_else(|_| std::path::PathBuf::from("export.wav"))
                        .to_string_lossy()
                        .to_string();
                }
            }
        });

        // Верхняя панель в стиле VS Code: тёмный фон, пункты File, Edit, Run, …; по клику — контекстное меню
        const MENU_BAR_HEIGHT: f32 = 22.0;
        let menu_bar_fill = egui::Color32::from_gray(45);
        let menu_text = egui::Color32::from_gray(210);
        let menu_hover_bg = egui::Color32::from_white_alpha(25);

        egui::TopBottomPanel::top("menu")
            .exact_height(MENU_BAR_HEIGHT)
            .frame(egui::Frame::none().fill(menu_bar_fill))
            .show(ctx, |ui| {
                ui.set_height(MENU_BAR_HEIGHT);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;

                    fn menu_item(
                        ui: &mut egui::Ui,
                        label: &str,
                        text_color: egui::Color32,
                        hover_bg: egui::Color32,
                    ) -> (egui::Rect, egui::Response) {
                        let padding = egui::vec2(10.0, 2.0);
                        let size = ui
                            .painter()
                            .layout_no_wrap(
                                label.to_string(),
                                egui::FontId::proportional(13.0),
                                text_color,
                            )
                            .size()
                            + padding;
                        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
                        if resp.hovered() || resp.has_focus() {
                            ui.painter().rect_filled(rect, 0.0, hover_bg);
                        }
                        ui.painter().text(
                            rect.left_center() + egui::vec2(padding.x * 0.5, 0.0),
                            egui::Align2::LEFT_CENTER,
                            label,
                            egui::FontId::proportional(13.0),
                            text_color,
                        );
                        (rect, resp)
                    }

                    let (file_rect, file_resp) = menu_item(ui, "File", menu_text, menu_hover_bg);
                    if file_resp.clicked() {
                        let open = self.open_top_menu == Some(TopMenuKind::File);
                        self.open_top_menu = if open { None } else { Some(TopMenuKind::File) };
                        self.menu_dropdown_anchor = if open { None } else { Some(file_rect) };
                    }
                    let (edit_rect, edit_resp) = menu_item(ui, "Edit", menu_text, menu_hover_bg);
                    if edit_resp.clicked() {
                        let open = self.open_top_menu == Some(TopMenuKind::Edit);
                        self.open_top_menu = if open { None } else { Some(TopMenuKind::Edit) };
                        self.menu_dropdown_anchor = if open { None } else { Some(edit_rect) };
                    }
                    let (run_rect, run_resp) = menu_item(ui, "Run", menu_text, menu_hover_bg);
                    if run_resp.clicked() {
                        let open = self.open_top_menu == Some(TopMenuKind::Run);
                        self.open_top_menu = if open { None } else { Some(TopMenuKind::Run) };
                        self.menu_dropdown_anchor = if open { None } else { Some(run_rect) };
                    }
                    let (more_rect, more_resp) = menu_item(ui, "…", menu_text, menu_hover_bg);
                    if more_resp.clicked() {
                        let open = self.open_top_menu == Some(TopMenuKind::More);
                        self.open_top_menu = if open { None } else { Some(TopMenuKind::More) };
                        self.menu_dropdown_anchor = if open { None } else { Some(more_rect) };
                    }
                });
            });

        // Выпадающее контекстное меню по клику на пункт
        if let (Some(kind), Some(anchor)) = (self.open_top_menu, self.menu_dropdown_anchor) {
            const ROW_H: f32 = 22.0;
            const PAD: f32 = 6.0;
            let menu_width = 180.0;
            let menu_pos = egui::pos2(anchor.min.x, anchor.max.y);
            let menu_rect = egui::Rect::from_min_size(menu_pos, egui::vec2(menu_width, 400.0));
            let text_color = egui::Color32::from_gray(220);
            let shortcut_color = egui::Color32::from_gray(140);
            let hover_bg = egui::Color32::from_white_alpha(28);

            fn menu_row(
                ui: &mut egui::Ui,
                _id: egui::Id,
                label: &str,
                shortcut: &str,
                text_color: egui::Color32,
                shortcut_color: egui::Color32,
                hover_bg: egui::Color32,
                enabled: bool,
            ) -> (egui::Rect, egui::Response) {
                let font = egui::FontId::proportional(13.0);
                let _ = ui
                    .painter()
                    .layout_no_wrap(label.to_string(), font.clone(), text_color);
                let _ = ui
                    .painter()
                    .layout_no_wrap(shortcut.to_string(), font, shortcut_color);
                let w = ui.available_width();
                let (rect, resp) =
                    ui.allocate_exact_size(egui::vec2(w, ROW_H), egui::Sense::click());
                let color = if enabled {
                    text_color
                } else {
                    egui::Color32::from_gray(100)
                };
                if resp.hovered() && enabled {
                    ui.painter().rect_filled(rect, 0.0, hover_bg);
                }
                ui.painter().text(
                    rect.left_center() + egui::vec2(PAD, 0.0),
                    egui::Align2::LEFT_CENTER,
                    label,
                    egui::FontId::proportional(13.0),
                    color,
                );
                ui.painter().text(
                    rect.right_center() - egui::vec2(PAD, 0.0),
                    egui::Align2::RIGHT_CENTER,
                    shortcut,
                    egui::FontId::proportional(12.0),
                    shortcut_color,
                );
                (rect, resp)
            }

            egui::Area::new(egui::Id::new("top_menu_dropdown").with(kind))
                .order(egui::Order::Foreground)
                .fixed_pos(menu_pos)
                .constrain(true)
                .show(ctx, |ui| {
                    egui::Frame::menu(ui.style()).show(ui, |ui| {
                        ui.set_min_width(menu_width);
                        ui.spacing_mut().item_spacing.y = 0.0;
                        match kind {
                            TopMenuKind::File => {
                                if menu_row(
                                    ui,
                                    ui.id().with("nw"),
                                    "New Window",
                                    "Ctrl+Shift+N",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    self.close_top_menu();
                                }
                                if menu_row(
                                    ui,
                                    ui.id().with("nf"),
                                    "New File",
                                    "Ctrl+N",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    self.new_file();
                                    self.close_top_menu();
                                }
                                if menu_row(
                                    ui,
                                    ui.id().with("of"),
                                    "Open File…",
                                    "Ctrl+O",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    self.close_top_menu();
                                    if let Some(path) = rfd::FileDialog::new()
                                        .add_filter("Strudel", &["strudel"])
                                        .add_filter("All", &["*"])
                                        .pick_file()
                                    {
                                        if let Ok(contents) = std::fs::read_to_string(&path) {
                                            self.open_file_in_tab(path, contents);
                                        }
                                    }
                                }
                                if menu_row(
                                    ui,
                                    ui.id().with("od"),
                                    "Open Folder…",
                                    "Ctrl+K Ctrl+O",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    self.close_top_menu();
                                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                                        self.open_folder(path);
                                    }
                                }
                                ui.separator();
                                let can_save = self
                                    .active_editor()
                                    .map(|e| e.file_path.is_some())
                                    .unwrap_or(false);
                                if menu_row(
                                    ui,
                                    ui.id().with("sv"),
                                    "Save",
                                    "Ctrl+S",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    can_save,
                                )
                                .1
                                .clicked()
                                {
                                    self.save_file();
                                    self.close_top_menu();
                                }
                                if menu_row(
                                    ui,
                                    ui.id().with("sa"),
                                    "Save As…",
                                    "Ctrl+Shift+S",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    self.close_top_menu();
                                    let default_name = self
                                        .active_editor()
                                        .and_then(|e| e.file_path.as_ref())
                                        .and_then(|p| p.file_name())
                                        .and_then(|n| n.to_str())
                                        .unwrap_or("Untitled.strudel");
                                    if let Some(path) = rfd::FileDialog::new()
                                        .add_filter("Strudel", &["strudel"])
                                        .set_file_name(default_name)
                                        .save_file()
                                    {
                                        self.save_file_as(path);
                                    }
                                }
                                if menu_row(
                                    ui,
                                    ui.id().with("exp"),
                                    "Export to WAV…",
                                    "Ctrl+Shift+E",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    self.close_top_menu();
                                    self.export_dialog.open = true;
                                    if self.export_dialog.duration_cycles.is_empty() {
                                        self.export_dialog.duration_cycles = "8".to_string();
                                    }
                                    if self.export_dialog.output_path.is_empty() {
                                        self.export_dialog.output_path = std::env::var("HOME")
                                            .map(|h| std::path::PathBuf::from(h).join("export.wav"))
                                            .unwrap_or_else(|_| {
                                                std::path::PathBuf::from("export.wav")
                                            })
                                            .to_string_lossy()
                                            .to_string();
                                    }
                                }
                                ui.separator();
                                let (recent_rect, recent_resp) = menu_row(
                                    ui,
                                    ui.id().with("or"),
                                    "Open Recent",
                                    "▶",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                );
                                if recent_resp.hovered() && !self.recent.is_empty() {
                                    self.open_recent_submenu_rect = Some(recent_rect);
                                } else if !recent_resp.hovered() {
                                    let pointer = ctx.pointer_latest_pos();
                                    let in_sub = self
                                        .open_recent_submenu_rect
                                        .map(|r| {
                                            let sub_rect = egui::Rect::from_min_size(
                                                egui::pos2(r.max.x, r.min.y),
                                                egui::vec2(200.0, 200.0),
                                            );
                                            pointer.map(|p| sub_rect.contains(p)).unwrap_or(false)
                                        })
                                        .unwrap_or(false);
                                    if !in_sub {
                                        self.open_recent_submenu_rect = None;
                                    }
                                }
                            }
                            TopMenuKind::Edit => {
                                if menu_row(
                                    ui,
                                    ui.id().with("u"),
                                    "Undo",
                                    "Ctrl+Z",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    self.close_top_menu();
                                }
                                if menu_row(
                                    ui,
                                    ui.id().with("r"),
                                    "Redo",
                                    "Ctrl+Y",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    self.close_top_menu();
                                }
                                ui.separator();
                                if menu_row(
                                    ui,
                                    ui.id().with("ct"),
                                    "Cut",
                                    "Ctrl+X",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    self.close_top_menu();
                                }
                                if menu_row(
                                    ui,
                                    ui.id().with("cp"),
                                    "Copy",
                                    "Ctrl+C",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    self.close_top_menu();
                                }
                                if menu_row(
                                    ui,
                                    ui.id().with("pt"),
                                    "Paste",
                                    "Ctrl+V",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    self.close_top_menu();
                                }
                            }
                            TopMenuKind::Run => {
                                if menu_row(
                                    ui,
                                    ui.id().with("run"),
                                    "Run",
                                    "Ctrl+Enter",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    self.run_editor_code();
                                    self.close_top_menu();
                                }
                                if menu_row(
                                    ui,
                                    ui.id().with("loads"),
                                    "Load samples",
                                    "",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    let _ = self.bridge.load_samples();
                                    self.samples_loading = true;
                                    self.samples_loading_started = Some(Instant::now());
                                    self.close_top_menu();
                                }
                                if menu_row(
                                    ui,
                                    ui.id().with("pa"),
                                    "Pause",
                                    "F6",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    self.do_pause();
                                    self.paused = true;
                                    self.playing = false;
                                    self.close_top_menu();
                                }
                                if menu_row(
                                    ui,
                                    ui.id().with("st"),
                                    "Stop",
                                    "Ctrl+.",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    self.do_pause();
                                    self.playing = false;
                                    self.paused = false;
                                    self.close_top_menu();
                                }
                            }
                            TopMenuKind::More => {
                                if menu_row(
                                    ui,
                                    ui.id().with("problems"),
                                    "Problems",
                                    "",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    let has = self.root_layout.has_content(ContentId::Problems);
                                    if !has {
                                        let path = self.root_layout.first_panel_path();
                                        self.pending_layout_actions.push(
                                            LayoutAction::AddTabToPanel {
                                                content_id: ContentId::Problems,
                                                path,
                                            },
                                        );
                                    }
                                    self.close_top_menu();
                                }
                                if menu_row(
                                    ui,
                                    ui.id().with("dbg"),
                                    "Debugger",
                                    "",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    let has = self.root_layout.has_content(ContentId::Debugger);
                                    if !has {
                                        let path = self.root_layout.first_panel_path();
                                        self.pending_layout_actions.push(
                                            LayoutAction::AddTabToPanel {
                                                content_id: ContentId::Debugger,
                                                path,
                                            },
                                        );
                                    }
                                    self.close_top_menu();
                                }
                                if menu_row(
                                    ui,
                                    ui.id().with("arr"),
                                    "Arrangement",
                                    "",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    let has = self.root_layout.has_content(ContentId::Arrangement);
                                    if !has {
                                        let path = self.root_layout.first_panel_path();
                                        self.pending_layout_actions.push(
                                            LayoutAction::AddTabToPanel {
                                                content_id: ContentId::Arrangement,
                                                path,
                                            },
                                        );
                                    }
                                    self.close_top_menu();
                                }
                if menu_row(
                    ui,
                    ui.id().with("hydra"),
                    "Hydra",
                    "",
                    text_color,
                    shortcut_color,
                    hover_bg,
                    true,
                )
                .1
                .clicked()
                {
                    self.hydra_studio.window_open = true;
                    let has = self.root_layout.has_content(ContentId::Hydra);
                    if !has {
                        let path = self.root_layout.first_panel_path();
                        self.pending_layout_actions.push(LayoutAction::AddTabToPanel {
                            content_id: ContentId::Hydra,
                            path,
                        });
                    }
                    self.close_top_menu();
                }
                if menu_row(
                    ui,
                    ui.id().with("chain_seed"),
                    "Chain Seed",
                    "",
                    text_color,
                    shortcut_color,
                    hover_bg,
                    true,
                )
                .1
                .clicked()
                {
                    let has = self.root_layout.has_content(ContentId::ChainSeed);
                    if !has {
                        let path = self.root_layout.first_panel_path();
                        self.pending_layout_actions.push(
                            LayoutAction::AddTabToPanel {
                                content_id: ContentId::ChainSeed,
                                path,
                            },
                        );
                    }
                    self.close_top_menu();
                }
                if menu_row(
                    ui,
                    ui.id().with("ai_pattern"),
                    "AI Pattern",
                    "",
                    text_color,
                    shortcut_color,
                    hover_bg,
                    true,
                )
                .1
                .clicked()
                {
                    self.ai_pattern.window_open = true;
                    let has = self.root_layout.has_content(ContentId::AiPattern);
                    if !has {
                        let path = self.root_layout.first_panel_path();
                        self.pending_layout_actions.push(LayoutAction::AddTabToPanel {
                            content_id: ContentId::AiPattern,
                            path,
                        });
                    }
                    self.close_top_menu();
                }
                if menu_row(
                    ui,
                    ui.id().with("set"),
                    "Settings",
                    "",
                    text_color,
                    shortcut_color,
                    hover_bg,
                    true,
                )
                .1
                .clicked()
                {
                    self.close_top_menu();
                }
                                if menu_row(
                                    ui,
                                    ui.id().with("ab"),
                                    "About",
                                    "",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                )
                                .1
                                .clicked()
                                {
                                    self.close_top_menu();
                                }
                            }
                        }
                    });
                });

            // Подменю "Open Recent" справа от пункта
            let mut recent_action: Option<(PathBuf, bool)> = None;
            if let Some(recent_rect) = self.open_recent_submenu_rect {
                let sub_w = 200.0;
                let sub_pos = egui::pos2(recent_rect.max.x, recent_rect.min.y);
                egui::Area::new(egui::Id::new("open_recent_submenu"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(sub_pos)
                    .constrain(true)
                    .show(ctx, |ui| {
                        egui::Frame::menu(ui.style()).show(ui, |ui| {
                            ui.set_min_width(sub_w);
                            ui.spacing_mut().item_spacing.y = 0.0;
                            for item in self.recent.iter().take(8) {
                                let label = format!(
                                    "{} {}",
                                    if item.kind == "file" { "📄" } else { "📁" },
                                    item.name
                                );
                                let path = PathBuf::from(&item.path);
                                let is_file = item.kind == "file";
                                let (_r, resp) = menu_row(
                                    ui,
                                    ui.id().with(&item.path),
                                    &label,
                                    "",
                                    text_color,
                                    shortcut_color,
                                    hover_bg,
                                    true,
                                );
                                if resp.clicked() {
                                    recent_action = Some((path, is_file));
                                }
                            }
                        });
                    });
            }

            if let Some((path, is_file)) = recent_action {
                self.close_top_menu();
                if is_file {
                    if let Ok(contents) = std::fs::read_to_string(&path) {
                        self.open_file_in_tab(path, contents);
                    }
                } else {
                    self.open_folder(path);
                }
            }

            let sub_rect = self.open_recent_submenu_rect.map(|r| {
                egui::Rect::from_min_size(egui::pos2(r.max.x, r.min.y), egui::vec2(200.0, 300.0))
            });
            if ctx.input(|i| i.pointer.any_click()) {
                let pos = ctx.pointer_latest_pos();
                let in_menu = pos.map(|p| menu_rect.contains(p)).unwrap_or(false);
                let in_anchor = pos.map(|p| anchor.contains(p)).unwrap_or(false);
                let in_sub = sub_rect
                    .and_then(|sr| pos.map(|p| sr.contains(p)))
                    .unwrap_or(false);
                if !in_menu && !in_anchor && !in_sub {
                    self.close_top_menu();
                }
            }
        }

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            let (file_name, line, column, eval_error_count) = self
                .active_editor()
                .map(|editor| {
                    let (line, column) = editor.cursor_line_column();
                    let eval_error_count = usize::from(editor.eval_error.is_some());
                    (editor.title(), line, column, eval_error_count)
                })
                .unwrap_or_else(|| ("Untitled.strudel".to_string(), 1, 1, 0));
            let parse_error_count = self
                .pattern_document
                .as_ref()
                .map(|doc| doc.diagnostics.len())
                .unwrap_or(0);
            let error_count = parse_error_count + eval_error_count;
            let msg = format!(
                "{} | ligne {}, colonne {} | CPS : {:.1} | erreurs : {}",
                file_name, line, column, self.last_cps, error_count
            );
            ui.label(msg);
        });

        egui::CentralPanel::default()
            // Убираем внутренние отступы центральной области — контент рисуется прямо до краёв окна.
            .frame(egui::Frame::none())
            .show(ctx, |ui| {
                self.apply_pending_layout_actions();
                let rect = ui.available_rect_before_wrap();
                let mut layout = std::mem::take(&mut self.root_layout);
                ui.allocate_ui_at_rect(rect, |ui| {
                    self.layout_ui(ui, &mut layout, &[]);
                });
                self.root_layout = layout;
            });

        self.show_tool_viewports(ctx);

        // Плавающая вкладка, следующая за курсором при перетаскивании
        if let Some(tab_id) = self.dragging_tab {
            if let Some(pos) = ctx.pointer_latest_pos() {
                let label = tab_id.label();
                let font = egui::FontId::proportional(13.0);
                let text_color = egui::Color32::from_gray(200);
                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Tooltip,
                    egui::Id::new("dnd_tab_float"),
                ));
                let galley = painter.layout_no_wrap(label.to_string(), font, text_color);
                let pad = egui::vec2(10.0, 4.0);
                let size = galley.size() + pad * 2.0;
                let rect = egui::Rect::from_min_size(pos + egui::vec2(8.0, 8.0), size);
                painter.rect_filled(rect, 4.0, egui::Color32::from_gray(40));
                painter.rect_stroke(
                    rect,
                    4.0,
                    egui::Stroke::new(1.0, egui::Color32::from_white_alpha(30)),
                    egui::StrokeKind::Middle,
                );
                painter.galley(rect.min + pad, galley, text_color);
            }
        }
    }
}

#[cfg(test)]
mod trash_tests {
    use super::move_to_trash;
    use std::path::PathBuf;

    fn test_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("struidel-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn delete_moves_file_to_trash_outside_samples() {
        let base = test_dir("trash");
        let samples = base.join("samples");
        let vox = samples.join("vox");
        std::fs::create_dir_all(&vox).unwrap();
        let file = vox.join("take.wav");
        std::fs::write(&file, b"RIFF").unwrap();

        let dest = move_to_trash(&file, &samples).unwrap();
        assert!(!file.exists());
        assert!(dest.exists());
        assert!(dest.starts_with(base.join("trash").join("vox")), "{dest:?}");

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn trash_keeps_file_when_move_fails() {
        let base = test_dir("trash-fail");
        let samples = base.join("samples");
        std::fs::create_dir_all(&samples).unwrap();
        // Fichier hors de la racine : parent() de la corbeille reste valide,
        // mais un chemin sans nom de fichier doit échouer proprement.
        let weird = samples.join("vox");
        std::fs::create_dir_all(&weird).unwrap();
        let err = move_to_trash(&weird, &samples);
        // Un dossier peut être renommé : on vérifie juste qu'il n'y a pas de perte
        // silencieuse — soit succès déplacé, soit erreur et dossier intact.
        if err.is_err() {
            assert!(weird.exists());
        }
        std::fs::remove_dir_all(&base).unwrap();
    }
}

#[cfg(test)]
mod stack_insert_tests {
    use super::{extend_stack, whole_stack_call};

    #[test]
    fn empty_code_inserts_bare_expr() {
        assert_eq!(extend_stack("", "s(\"bd\")"), "s(\"bd\")");
        assert_eq!(extend_stack("   \n", "s(\"bd\")"), "s(\"bd\")");
    }

    #[test]
    fn single_expr_is_wrapped_in_stack() {
        assert_eq!(
            extend_stack("s(\"bd\")", "s(\"hh\")"),
            "stack(\n  s(\"bd\"),\n  s(\"hh\")\n)"
        );
        assert_eq!(
            extend_stack("s(\"sawtooth\").lpf(800)", "s(\"hh\")"),
            "stack(\n  s(\"sawtooth\").lpf(800),\n  s(\"hh\")\n)"
        );
    }

    #[test]
    fn existing_stack_gets_new_member() {
        let code = "stack(\n  s(\"bd\"),\n  s(\"hh\")\n)";
        assert_eq!(
            extend_stack(code, "s(\"sd\")"),
            "stack(\n  s(\"bd\"),\n  s(\"hh\"),\n  s(\"sd\"))"
        );
    }

    #[test]
    fn inline_stack_extends_too() {
        assert_eq!(
            extend_stack("stack(s(\"a\"), s(\"b\"))", "s(\"c\")"),
            "stack(s(\"a\"), s(\"b\"),\n  s(\"c\"))"
        );
    }

    #[test]
    fn parens_inside_strings_do_not_close_stack() {
        assert_eq!(
            extend_stack("stack(s(\"a)b\"))", "s(\"c\")"),
            "stack(s(\"a)b\"),\n  s(\"c\"))"
        );
    }

    #[test]
    fn code_after_stack_wraps_whole_thing() {
        // `stack(...) .post()` n'est pas un stack pur → tout est emballé.
        let code = "stack(s(\"a\")).gain(0.5)";
        assert_eq!(
            extend_stack(code, "s(\"c\")"),
            "stack(\n  stack(s(\"a\")).gain(0.5),\n  s(\"c\")\n)"
        );
    }

    #[test]
    fn multi_line_non_stack_is_wrapped_with_indent() {
        let code = "s(\"a\")\n.gain(0.8)";
        assert_eq!(
            extend_stack(code, "s(\"c\")"),
            "stack(\n  s(\"a\")\n  .gain(0.8),\n  s(\"c\")\n)"
        );
    }

    #[test]
    fn whole_stack_call_requires_trailing_nothing() {
        assert_eq!(whole_stack_call("stack(s(\"a\"))"), Some(11 + 1));
        assert_eq!(whole_stack_call("stack(s(\"a\")) x"), None);
        assert_eq!(whole_stack_call("s(\"a\")"), None);
        assert_eq!(whole_stack_call("stack"), None);
        assert_eq!(whole_stack_call("stacking(s(\"a\"))"), None);
    }
}

#[cfg(test)]
mod insert_snippet_tests {
    use super::{insert_snippet, sound_controls_suffix};

    fn cats() -> crate::sample_categories::SampleCategories {
        crate::sample_categories::SampleCategories::new()
    }

    #[test]
    fn controls_off_gives_bare_call() {
        assert_eq!(
            insert_snippet("sawtooth", None, false, &cats()),
            "s(\"sawtooth\")"
        );
        assert_eq!(
            insert_snippet("bd", Some("RolandTR909"), false, &cats()),
            "s(\"bd\").bank(\"RolandTR909\")"
        );
    }

    #[test]
    fn controls_on_adds_widgets_by_kind() {
        // Percussion synthétisée → accordage.
        assert_eq!(
            insert_snippet("synthbd", None, true, &cats()),
            "s(\"synthbd\").gain(slider(0.8,0,1.5)).speed(slider(1,0.5,2)).mask(toggler(1))"
        );
        // Basse → filtre grave.
        assert_eq!(
            insert_snippet("subbass", None, true, &cats()),
            "s(\"subbass\").gain(slider(0.8,0,1.5)).lpf(slider(900,80,4000)).mask(toggler(1))"
        );
        // Synthé/GM → filtre large.
        assert_eq!(
            insert_snippet("gm_lead_sawtooth", None, true, &cats()),
            "s(\"gm_lead_sawtooth\").gain(slider(0.8,0,1.5)).lpf(slider(3000,200,12000)).mask(toggler(1))"
        );
        // Banque voix → gain plus large.
        assert_eq!(
            sound_controls_suffix("take-1", Some("vox"), &cats()),
            ".gain(slider(1,0,3)).mask(toggler(1))"
        );
        // Banque drum sample → accordage.
        assert_eq!(
            sound_controls_suffix("bd", Some("RolandTR909"), &cats()),
            ".gain(slider(0.8,0,1.5)).speed(slider(1,0.5,2)).mask(toggler(1))"
        );
    }
}
