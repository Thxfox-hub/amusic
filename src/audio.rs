use std::collections::{HashMap, HashSet};
use std::f32::consts::TAU;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use arc_swap::ArcSwap;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample as CpalSample, SizedSample};
use crossbeam_channel::{bounded, Receiver, Sender};

use crate::mixer_dsp::{
    apply_channel_sample, apply_master_options, db_to_gain, BuiltInFx, InsertProcessor,
    MeterAccumulator,
};
use crate::project_audio::{
    AudioSourceId, AutomationLane, AutomationMode, AutomationParameter, FadeCurve, MasterOptions,
    MeterState, MixerChannel, MixerChannelId, OutputTarget, PanLaw, ProjectAudioState,
};
use crate::sample_loader::{
    default_sample_root, load_sample_library_merged, load_wav_sample, sample_roots, Sample,
    SampleKey, SampleLibrary,
};
use crate::soundfont;

const MAX_VOICES: usize = 512;
const AUDIO_COMMAND_CAPACITY: usize = 4096;
const MAX_LIVE_DAW_CHANNELS: usize = 64;

/// Размер буфера cpal в сэмплах. На 48 kHz: 1024/48000 ≈ 21 мс аудио-латенции.
/// Раньше было 8192 (170 мс) — из-за этого короткие войсы (duration < 170 мс)
/// проскакивали и не играли — render thread входил в callback уже после их end_time.
const AUDIO_BUFFER_SIZE: u32 = 1024;

/// Лук-ахед для планирования voice'ов (в секундах). Гарантирует, что start_time
/// войса будет в будущем относительно момента drain'а в render thread.
///
/// На холодном старте первый `schedule_event` блокируется на mutex'е библиотеки
/// сэмплов (фоновая загрузка держит его), + jitter cpal callback'а + латенция mpsc channel'а.
/// Без достаточного look-ahead первый из коротких войсов (~125-170 мс) «съедается»: render's
/// `t` уже > end_time к моменту появления voice'а в render state.
///
/// 60 мс — баланс между «незаметностью при Play» и «хватит на jitter callback'а +
/// латенцию mpsc + cpal-буфер 1024 сэмпла». Сэмплы прогреваются заранее
/// через prewarm_samples, поэтому больших блокировок на дисковом I/O нет.
const SCHEDULE_LOOK_AHEAD_SECS: f32 = 0.060;

/// Find a GM soundfont in the given directory. Предпочтительно FluidR3_GM / FluidR3 GM (точный GM).
fn find_soundfont_in_dir(root: &Path) -> Option<PathBuf> {
    for name in [
        "FluidR3_GM.sf2",
        "FluidR3 GM.sf2",
        "FluidR3_GS.sf2",
        "FluidR3 GS.sf2",
    ] {
        let p = root.join(name);
        if p.exists() {
            return Some(p);
        }
    }
    let entries = std::fs::read_dir(root).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().map_or(false, |e| e == "sf2") {
            return Some(p);
        }
    }
    None
}

/// Описание аудио-события из Strudel в циклах.
///
/// Это «сырой» ивент, приходящий из моста. Преобразование в конкретный
/// `Voice` (выбор файла, питч, панорама, envelope) делает `AudioEngine`.
#[derive(Clone)]
pub struct AudioEvent {
    pub start_cycle: f64,
    pub end_cycle: f64,
    pub midi_note: Option<f64>,
    pub gain: Option<f64>,
    /// Скорость циклов в секунду (cps) на момент события.
    pub cps: f64,
    /// Имя семпла (s("bd"), "bd:0" и т.п.).
    pub s: Option<String>,
    /// Имя банка (подкаталог в папке семплов), опционально.
    pub bank: Option<String>,
    /// Вариант/индекс семпла (bd_0, bd_1, ...).
    pub n: Option<f64>,
    /// Панорама (-1..1).
    pub pan: Option<f64>,
    /// Контролы огибающей/эффектов (MVP — игнорируются, но протокол уже есть).
    pub attack: Option<f64>,
    pub decay: Option<f64>,
    pub sustain: Option<f64>,
    pub release: Option<f64>,
    pub room: Option<f64>,
    pub shape: Option<f64>,
    pub cut: Option<f64>,
    /// Vitesse de lecture (`.speed()`) : 2 = une octave au-dessus, 0.5 = une octave en dessous.
    pub speed: Option<f64>,
    /// Fréquence de coupure du passe-bas (`.lpf()`) en Hz ; absent/0 = pas de filtre.
    pub cutoff: Option<f64>,
}

/// Параметры для отложенного рендера GM-ноты (вне блокировки state).
struct GmRenderParams {
    soundfont: Arc<rustysynth::SoundFont>,
    program: u8,
    midi_note: u8,
    velocity: u8,
    duration_secs: f32,
    sample_rate: u32,
    start_time: f32,
    end_time: f32,
    gain: f32,
    pan: f32,
    attack: f32,
    decay: f32,
    sustain: f32,
    release: f32,
    room: f32,
    cut: f32,
    shape: f32,
}

struct Voice {
    sample: Arc<Sample>,
    position: f32,
    rate: f32,
    gain: f32,
    pan: f32,
    start_time: f64,
    end_time: f64,
    frames: usize,
    /// Огибающая и эффекты (0 = по умолчанию / без эффекта).
    pub attack: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
    pub room: f32,
    pub cut: f32,
    pub shape: f32,
    /// Passe-bas par voix (Hz) ; 0 = inactif. `lp_state` = mémoire du filtre un pôle.
    pub cutoff_hz: f32,
    pub lp_state: f32,
}

/// Параметры эффектов по умолчанию (0 = без огибающей/реверба).
fn ev_attack(ev: &AudioEvent) -> f32 {
    ev.attack.unwrap_or(0.0) as f32
}
fn ev_decay(ev: &AudioEvent) -> f32 {
    ev.decay.unwrap_or(0.0) as f32
}
fn ev_sustain(ev: &AudioEvent) -> f32 {
    ev.sustain.unwrap_or(1.0) as f32
}
fn ev_release(ev: &AudioEvent) -> f32 {
    ev.release.unwrap_or(0.0) as f32
}
fn ev_room(ev: &AudioEvent) -> f32 {
    ev.room.unwrap_or(0.0) as f32
}
fn ev_cut(ev: &AudioEvent) -> f32 {
    ev.cut.unwrap_or(0.0) as f32
}
fn ev_shape(ev: &AudioEvent) -> f32 {
    ev.shape.unwrap_or(0.0) as f32
}

enum AudioCommand {
    AddVoice(Voice),
    Clear,
}

struct AudioRenderState {
    sample_rate: f32,
    sample_clock: u64,
    voices: Vec<Voice>,
    master_processor: InsertProcessor,
    daw_processors: Vec<InsertProcessor>,
    /// Простой реверб: два кольцевых буфера (L/R), задержка ~40 ms.
    reverb_buf_l: Vec<f32>,
    reverb_buf_r: Vec<f32>,
    reverb_pos: usize,
}

impl AudioRenderState {
    fn new(sample_rate: f32) -> Self {
        let reverb_len = (0.04 * sample_rate).ceil() as usize;
        Self {
            sample_rate,
            sample_clock: 0,
            voices: Vec::with_capacity(MAX_VOICES),
            master_processor: InsertProcessor::default(),
            daw_processors: (0..MAX_LIVE_DAW_CHANNELS)
                .map(|_| InsertProcessor::default())
                .collect(),
            reverb_buf_l: vec![0.0; reverb_len],
            reverb_buf_r: vec![0.0; reverb_len],
            reverb_pos: 0,
        }
    }

    fn drain_commands(&mut self, command_rx: &Receiver<AudioCommand>) {
        while let Ok(command) = command_rx.try_recv() {
            match command {
                AudioCommand::AddVoice(voice) => {
                    if self.voices.len() >= self.voices.capacity() {
                        // Saturé : on sacrifie la voix la plus ancienne plutôt
                        // que la nouvelle (sinon le clic/événement courant est muet).
                        self.voices.remove(0);
                    }
                    self.voices.push(voice);
                }
                AudioCommand::Clear => self.voices.clear(),
            }
        }
    }
}

struct AudioControlState {
    library: SampleLibrary,
    soundfont: Option<Arc<rustysynth::SoundFont>>,
    project_sources: HashMap<AudioSourceId, Arc<Sample>>,
    missing_logged: HashSet<SampleKey>,
    broken_project_sources: HashSet<AudioSourceId>,
}

impl AudioControlState {
    fn new() -> Self {
        Self {
            library: SampleLibrary::new(),
            soundfont: None,
            project_sources: HashMap::new(),
            missing_logged: HashSet::new(),
            broken_project_sources: HashSet::new(),
        }
    }
}

pub struct AudioEngine {
    control: Arc<Mutex<AudioControlState>>,
    command_tx: Sender<AudioCommand>,
    sample_clock: Arc<AtomicU64>,
    sample_rate: f32,
    mixer: Arc<ArcSwap<LiveMixerSnapshot>>,
    daw: Arc<ArcSwap<LiveDawSnapshot>>,
    daw_transport: Arc<ArcSwap<LiveTransportSnapshot>>,
    master_meter: Arc<MeterAtomics>,
    daw_meters: Arc<Vec<MeterAtomics>>,
    _stream: cpal::Stream,
}

#[derive(Clone)]
struct LiveMixerSnapshot {
    master_volume_db: f32,
    master_pan: f32,
    master_pan_law: PanLaw,
    master_inserts: Vec<BuiltInFx>,
    master_options: MasterOptions,
}

impl Default for LiveMixerSnapshot {
    fn default() -> Self {
        Self {
            master_volume_db: 0.0,
            master_pan: 0.0,
            master_pan_law: PanLaw::Minus3Db,
            master_inserts: Vec::new(),
            master_options: MasterOptions::default(),
        }
    }
}

impl LiveMixerSnapshot {
    fn from_project(project: &ProjectAudioState) -> Self {
        Self {
            master_volume_db: project.master.volume_db,
            master_pan: project.master.pan,
            master_pan_law: project.master.pan_law,
            master_inserts: project
                .master
                .inserts
                .iter()
                .filter_map(BuiltInFx::from_slot)
                .collect(),
            master_options: project.master_options,
        }
    }
}

#[derive(Clone, Copy)]
struct LiveTransportSnapshot {
    playing: bool,
    reverse: bool,
    sync_cycle: f64,
    sync_sample_clock: u64,
    cycles_per_second: f64,
}

impl Default for LiveTransportSnapshot {
    fn default() -> Self {
        Self {
            playing: false,
            reverse: false,
            sync_cycle: 0.0,
            sync_sample_clock: 0,
            cycles_per_second: 1.0,
        }
    }
}

#[derive(Clone, Default)]
struct LiveDawSnapshot {
    tracks: Vec<LiveDawTrack>,
    channels: Vec<LiveDawChannel>,
    routing_order: Vec<usize>,
    patterns_channel_index: Option<usize>,
}

#[derive(Clone)]
struct LiveDawTrack {
    channel_index: usize,
    clips: Vec<LiveDawClip>,
}

#[derive(Clone)]
struct LiveDawClip {
    source: Arc<Sample>,
    timeline_position: f64,
    source_offset_seconds: f64,
    length_cycles: f64,
    gain: f32,
    pan: f32,
    fade_in_cycles: f64,
    fade_in_curve: FadeCurve,
    fade_out_cycles: f64,
    fade_out_curve: FadeCurve,
    source_rate_factor: f64,
}

#[derive(Clone)]
struct LiveDawChannel {
    id: MixerChannelId,
    volume_db: f32,
    pan: f32,
    pan_law: PanLaw,
    muted: bool,
    automation: Vec<AutomationLane>,
    inserts: Vec<BuiltInFx>,
    sends: Vec<LiveDawSend>,
    output: LiveDawOutput,
}

#[derive(Clone, Copy)]
struct LiveDawSend {
    send_index: usize,
    target_index: usize,
    level_db: f32,
    pre_fader: bool,
}

#[derive(Clone, Copy)]
enum LiveDawOutput {
    Master,
    Bus(usize),
    None,
}

struct MeterAtomics {
    peak_bits: AtomicU32,
    rms_bits: AtomicU32,
    lufs_bits: AtomicU32,
    clipped: AtomicBool,
    clip_hold_blocks: AtomicU32,
}

impl Default for MeterAtomics {
    fn default() -> Self {
        Self {
            peak_bits: AtomicU32::new(0.0_f32.to_bits()),
            rms_bits: AtomicU32::new(0.0_f32.to_bits()),
            lufs_bits: AtomicU32::new((-90.0_f32).to_bits()),
            clipped: AtomicBool::new(false),
            clip_hold_blocks: AtomicU32::new(0),
        }
    }
}

impl MeterAtomics {
    fn load(&self) -> MeterState {
        MeterState {
            peak: f32::from_bits(self.peak_bits.load(Ordering::Relaxed)),
            rms: f32::from_bits(self.rms_bits.load(Ordering::Relaxed)),
            lufs_short: f32::from_bits(self.lufs_bits.load(Ordering::Relaxed)),
            clipped: self.clipped.load(Ordering::Relaxed),
            clip_hold_blocks: self.clip_hold_blocks.load(Ordering::Relaxed),
        }
    }

    fn store(&self, meter: MeterState) {
        self.peak_bits
            .store(meter.peak.to_bits(), Ordering::Relaxed);
        self.rms_bits.store(meter.rms.to_bits(), Ordering::Relaxed);
        self.lufs_bits
            .store(meter.lufs_short.to_bits(), Ordering::Relaxed);
        self.clipped.store(meter.clipped, Ordering::Relaxed);
        self.clip_hold_blocks
            .store(meter.clip_hold_blocks, Ordering::Relaxed);
    }
}

impl AudioEngine {
    pub fn new() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "No default audio output device".to_string())?;
        let supported_config = device
            .default_output_config()
            .map_err(|e| format!("Failed to get default output config: {e}"))?;

        let mut stream_config: cpal::StreamConfig = supported_config.clone().config();
        // Маленький буфер нужен, чтобы короткие voice'ы (~125 мс) не проскакивали
        // между моментом измерения `now_seconds()` в UI и drain'ом в render callback.
        stream_config.buffer_size = cpal::BufferSize::Fixed(AUDIO_BUFFER_SIZE);
        let sample_rate = stream_config.sample_rate as f32;
        let channels = stream_config.channels as usize;

        // Стартуем с пустой библиотекой; загрузка сэмплов в фоне, чтобы окно и звук запускались сразу.
        let control = Arc::new(Mutex::new(AudioControlState::new()));
        let control_clone = Arc::clone(&control);
        let (command_tx, command_rx) = bounded(AUDIO_COMMAND_CAPACITY);
        let sample_clock = Arc::new(AtomicU64::new(0));
        let mixer = Arc::new(ArcSwap::from_pointee(LiveMixerSnapshot::default()));
        let daw = Arc::new(ArcSwap::from_pointee(LiveDawSnapshot::default()));
        let daw_transport = Arc::new(ArcSwap::from_pointee(LiveTransportSnapshot::default()));
        let master_meter = Arc::new(MeterAtomics::default());
        let daw_meters = Arc::new(
            (0..MAX_LIVE_DAW_CHANNELS)
                .map(|_| MeterAtomics::default())
                .collect::<Vec<_>>(),
        );
        let roots = sample_roots(&default_sample_root());
        let soundfont_path = find_soundfont_in_dir(&default_sample_root());
        thread::spawn(move || {
            eprintln!("[AudioEngine] loading sample library from {:?} ...", roots);
            match load_sample_library_merged(&roots) {
                Ok(lib) => {
                    let count = lib.len();
                    if let Ok(mut s) = control_clone.lock() {
                        s.library = lib;
                    }
                    eprintln!(
                        "[AudioEngine] sample library loaded ({} roots, {} entries)",
                        roots.len(),
                        count
                    );
                }
                Err(e) => {
                    eprintln!("[AudioEngine] failed to load samples: {}", e);
                }
            }
            if let Some(path) = soundfont_path {
                if let Some(sf) = soundfont::load_soundfont(&path) {
                    if let Ok(mut s) = control_clone.lock() {
                        s.soundfont = Some(sf);
                        eprintln!("[AudioEngine] GM soundfont loaded from {:?}", path);
                    }
                } else {
                    eprintln!("[AudioEngine] GM: failed to load soundfont from {:?}", path);
                }
            } else {
                eprintln!("[AudioEngine] GM: place FluidR3_GM.sf2 or FluidR3 GS.sf2 in strudel_files for gm_piano, gm_epiano1, etc.");
            }
        });

        let stream = match supported_config.sample_format() {
            cpal::SampleFormat::F32 => Self::build_output_stream::<f32>(
                &device,
                &stream_config,
                channels,
                sample_rate,
                command_rx.clone(),
                Arc::clone(&sample_clock),
                Arc::clone(&mixer),
                Arc::clone(&daw),
                Arc::clone(&daw_transport),
                Arc::clone(&master_meter),
                Arc::clone(&daw_meters),
            )?,
            cpal::SampleFormat::I16 => Self::build_output_stream::<i16>(
                &device,
                &stream_config,
                channels,
                sample_rate,
                command_rx.clone(),
                Arc::clone(&sample_clock),
                Arc::clone(&mixer),
                Arc::clone(&daw),
                Arc::clone(&daw_transport),
                Arc::clone(&master_meter),
                Arc::clone(&daw_meters),
            )?,
            cpal::SampleFormat::U16 => Self::build_output_stream::<u16>(
                &device,
                &stream_config,
                channels,
                sample_rate,
                command_rx,
                Arc::clone(&sample_clock),
                Arc::clone(&mixer),
                Arc::clone(&daw),
                Arc::clone(&daw_transport),
                Arc::clone(&master_meter),
                Arc::clone(&daw_meters),
            )?,
            other => return Err(format!("Unsupported sample format: {other:?}")),
        };

        stream
            .play()
            .map_err(|e| format!("Failed to start audio stream: {e}"))?;

        Ok(Self {
            control,
            command_tx,
            sample_clock,
            sample_rate,
            mixer,
            daw,
            daw_transport,
            master_meter,
            daw_meters,
            _stream: stream,
        })
    }

    fn build_output_stream<T>(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        channels: usize,
        sample_rate: f32,
        command_rx: Receiver<AudioCommand>,
        sample_clock: Arc<AtomicU64>,
        mixer: Arc<ArcSwap<LiveMixerSnapshot>>,
        daw: Arc<ArcSwap<LiveDawSnapshot>>,
        daw_transport: Arc<ArcSwap<LiveTransportSnapshot>>,
        master_meter: Arc<MeterAtomics>,
        daw_meters: Arc<Vec<MeterAtomics>>,
    ) -> Result<cpal::Stream, String>
    where
        T: CpalSample + SizedSample + FromSample<f32>,
    {
        let err_count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let err_fn = move |err| {
            // Limite le spam stderr : un device audio qui disparaît (BT off, etc.)
            // produit des POLLERR en boucle — sans plafond le log remplit /tmp.
            let n = err_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 50 || n % 10_000 == 0 {
                eprintln!("[AudioEngine] stream error: {err} (x{})", n + 1);
            }
            if n >= 50 {
                // Device mort : backoff progressif (≤50 ms) pour ne pas spinner
                // le thread ALSA à 100 % CPU en attendant la recréation de l'engine.
                let backoff = std::cmp::min(50, 1 + n / 2_000);
                std::thread::sleep(std::time::Duration::from_millis(backoff));
            }
        };
        let mut render_state = AudioRenderState::new(sample_rate);
        device
            .build_output_stream(
                config,
                move |data: &mut [T], _| {
                    Self::write_data(
                        data,
                        channels,
                        &mut render_state,
                        &command_rx,
                        &sample_clock,
                        &mixer,
                        &daw,
                        &daw_transport,
                        &master_meter,
                        &daw_meters,
                    );
                },
                err_fn,
                None,
            )
            .map_err(|e| format!("Failed to build output stream: {e}"))
    }

    fn write_data<T>(
        output: &mut [T],
        channels: usize,
        render_state: &mut AudioRenderState,
        command_rx: &Receiver<AudioCommand>,
        sample_clock: &Arc<AtomicU64>,
        mixer: &Arc<ArcSwap<LiveMixerSnapshot>>,
        daw: &Arc<ArcSwap<LiveDawSnapshot>>,
        daw_transport: &Arc<ArcSwap<LiveTransportSnapshot>>,
        master_meter: &Arc<MeterAtomics>,
        daw_meters: &Arc<Vec<MeterAtomics>>,
    ) where
        T: CpalSample + SizedSample + FromSample<f32>,
    {
        render_state.drain_commands(command_rx);
        let sr = render_state.sample_rate;
        let snapshot = mixer.load();
        let daw_snapshot = daw.load();
        let transport = daw_transport.load();
        let mut meter = MeterAccumulator::default();
        let mut daw_meter_acc = [MeterAccumulator::default(); MAX_LIVE_DAW_CHANNELS];
        let previous_hold = master_meter.clip_hold_blocks.load(Ordering::Relaxed);

        for frame in output.chunks_mut(channels) {
            let t = render_state.sample_clock as f32 / sr;
            render_state.sample_clock = render_state.sample_clock.wrapping_add(1);

            // Удаляем отыгравшие голоса.
            render_state
                .voices
                .retain(|v| t < v.end_time as f32 && v.position < v.frames as f32);

            let mut left = 0.0f32;
            let mut right = 0.0f32;
            let mut reverb_send_l = 0.0f32;
            let mut reverb_send_r = 0.0f32;
            let rlen = render_state.reverb_buf_l.len();
            let rdelay = rlen.min((0.04 * sr) as usize);

            for v in &mut render_state.voices {
                if t < v.start_time as f32 || t >= v.end_time as f32 {
                    continue;
                }
                if v.position >= v.frames as f32 {
                    continue;
                }

                let frames = v.frames;
                let channels_in = v.sample.channels as usize;
                let pos = v.position.max(0.0).min(frames.saturating_sub(1) as f32);
                let base = pos.floor() as usize;
                let frac = pos - base as f32;
                let next = (base + 1).min(frames.saturating_sub(1));

                let mut mono = 0.0f32;
                for ch in 0..channels_in {
                    let i0 = base * channels_in + ch;
                    let i1 = next * channels_in + ch;
                    let s0 = v.sample.data[i0];
                    let s1 = v.sample.data[i1];
                    mono += s0 + (s1 - s0) * frac;
                }
                mono /= channels_in as f32;
                let shape = v.shape.clamp(0.0, 1.0);
                if shape > 0.0 {
                    let drive = 1.0 + shape * 12.0;
                    mono = (mono * drive).tanh() / drive;
                }
                let cut = v.cut.clamp(0.0, 1.0);
                if cut > 0.0 {
                    mono *= 1.0 - cut * 0.5;
                }
                if v.cutoff_hz > 0.0 {
                    let alpha = crate::mixer_dsp::one_pole_alpha(sr, v.cutoff_hz);
                    v.lp_state += alpha * (mono - v.lp_state);
                    mono = v.lp_state;
                }

                let env = envelope(
                    v.attack,
                    v.decay,
                    v.sustain,
                    v.release,
                    v.start_time,
                    v.end_time,
                    t,
                );
                let gain = v.gain * env;
                let pan = v.pan.clamp(-1.0, 1.0);
                let pan_l = (1.0 - pan) * 0.5;
                let pan_r = (1.0 + pan) * 0.5;
                let dry_l = mono * gain * pan_l;
                let dry_r = mono * gain * pan_r;
                let room = v.room.clamp(0.0, 1.0);

                left += dry_l * (1.0 - room);
                right += dry_r * (1.0 - room);
                reverb_send_l += dry_l * room;
                reverb_send_r += dry_r * room;

                v.position += v.rate;
            }

            let rp = render_state.reverb_pos;
            if rlen > 0 {
                left += render_state.reverb_buf_l[rp];
                right += render_state.reverb_buf_r[rp];
                let w = (rp + rdelay) % rlen;
                render_state.reverb_buf_l[w] = render_state.reverb_buf_l[w] * 0.6 + reverb_send_l;
                render_state.reverb_buf_r[w] = render_state.reverb_buf_r[w] * 0.6 + reverb_send_r;
                render_state.reverb_pos = (rp + 1) % rlen;
            }

            if transport.playing || left != 0.0 || right != 0.0 {
                let cycle = transport_cycle_at(&transport, render_state.sample_clock, sr);
                let (daw_l, daw_r) = render_live_daw_frame(
                    &daw_snapshot,
                    &mut render_state.daw_processors,
                    cycle,
                    transport.cycles_per_second,
                    sr,
                    transport.playing,
                    left,
                    right,
                    &mut daw_meter_acc,
                );
                left = daw_l;
                right = daw_r;
            }

            render_state.master_processor.process_pair(
                &snapshot.master_inserts,
                sr,
                &mut left,
                &mut right,
            );
            apply_channel_sample(
                &mut left,
                &mut right,
                snapshot.master_volume_db,
                snapshot.master_pan,
                snapshot.master_pan_law,
            );
            apply_master_options(&mut left, &mut right, snapshot.master_options);

            // Мягкий soft-clip оставляем как последний safety net для live path.
            let l = soft_clip(left);
            let r = soft_clip(right);
            meter.add_pair(l, r);

            if channels == 1 {
                let mono = (l + r) * 0.5;
                let value: T = T::from_sample(mono);
                for ch in frame.iter_mut() {
                    *ch = value;
                }
            } else {
                let mut it = frame.iter_mut();
                if let Some(out_l) = it.next() {
                    *out_l = T::from_sample(l);
                }
                if let Some(out_r) = it.next() {
                    *out_r = T::from_sample(r);
                }
                for ch in it {
                    *ch = T::from_sample((l + r) * 0.5);
                }
            }
        }
        sample_clock.store(render_state.sample_clock, Ordering::Relaxed);
        master_meter.store(meter.finish(previous_hold));
        for (index, acc) in daw_meter_acc
            .iter_mut()
            .enumerate()
            .take(daw_snapshot.channels.len())
        {
            if let Some(atomics) = daw_meters.get(index) {
                let previous_hold = atomics.clip_hold_blocks.load(Ordering::Relaxed);
                atomics.store(acc.finish(previous_hold));
            }
        }
    }

    pub fn update_mixer_snapshot(&self, project: &ProjectAudioState) {
        self.mixer
            .store(Arc::new(LiveMixerSnapshot::from_project(project)));
    }

    pub fn update_daw_transport(
        &self,
        playing: bool,
        current_cycle: f64,
        cycles_per_second: f64,
        reverse: bool,
    ) {
        self.daw_transport.store(Arc::new(LiveTransportSnapshot {
            playing,
            reverse,
            sync_cycle: current_cycle.max(0.0),
            sync_sample_clock: self.sample_clock.load(Ordering::Relaxed),
            cycles_per_second: cycles_per_second.max(0.000_001),
        }));
    }

    pub fn update_daw_snapshot(&self, project: &ProjectAudioState) {
        match self.build_live_daw_snapshot(project) {
            Ok(snapshot) => self.daw.store(Arc::new(snapshot)),
            Err(err) => eprintln!("[AudioEngine] live DAW snapshot failed: {err}"),
        }
    }

    fn build_live_daw_snapshot(
        &self,
        project: &ProjectAudioState,
    ) -> Result<LiveDawSnapshot, String> {
        let mut channel_ids = Vec::<MixerChannelId>::new();
        channel_ids.extend(project.tracks.iter().map(|track| track.channel.id));
        channel_ids.extend(project.group_buses.iter().map(|bus| bus.id));
        channel_ids.extend(project.return_buses.iter().map(|bus| bus.id));
        if channel_ids.len() > MAX_LIVE_DAW_CHANNELS {
            return Err(format!(
                "too many live DAW channels: {} > {}",
                channel_ids.len(),
                MAX_LIVE_DAW_CHANNELS
            ));
        }
        let channel_index: HashMap<MixerChannelId, usize> = channel_ids
            .iter()
            .enumerate()
            .map(|(index, id)| (*id, index))
            .collect();

        let mut channels = Vec::with_capacity(channel_ids.len());
        for id in &channel_ids {
            let Some(channel) = project.mixer_channel(*id) else {
                continue;
            };
            channels.push(snapshot_channel(project, channel, &channel_index));
        }

        let mut control = self
            .control
            .lock()
            .map_err(|_| "control state lock poisoned".to_string())?;
        let mut tracks = Vec::new();
        for track in &project.tracks {
            let Some(&channel_index) = channel_index.get(&track.channel.id) else {
                continue;
            };
            if !project.effective_audible(track.channel.id).audible {
                continue;
            }
            let mut clips = Vec::new();
            for clip in track.clips.iter().filter(|clip| !clip.muted) {
                let Some(source_meta) = project.pool.sources.get(&clip.source_file_id) else {
                    continue;
                };
                if !control.project_sources.contains_key(&clip.source_file_id) {
                    match load_wav_sample(&source_meta.path) {
                        Ok(source) => {
                            control.project_sources.insert(clip.source_file_id, source);
                        }
                        Err(err) => {
                            if control
                                .broken_project_sources
                                .insert(clip.source_file_id)
                            {
                                eprintln!(
                                    "[AudioEngine] failed to load project source {:?}: {err}",
                                    source_meta.path
                                );
                            }
                            continue;
                        }
                    }
                }
                let Some(source) = control.project_sources.get(&clip.source_file_id).cloned()
                else {
                    continue;
                };
                clips.push(LiveDawClip {
                    source,
                    timeline_position: clip.timeline_position,
                    source_offset_seconds: clip.source_offset,
                    length_cycles: clip.length,
                    gain: clip.gain,
                    pan: clip.pan,
                    fade_in_cycles: clip.fade_in.length,
                    fade_in_curve: clip.fade_in.curve,
                    fade_out_cycles: clip.fade_out.length,
                    fade_out_curve: clip.fade_out.curve,
                    source_rate_factor: clip.stretch.effective_source_rate()
                        * clip.pitch.ratio() as f64,
                });
            }
            tracks.push(LiveDawTrack {
                channel_index,
                clips,
            });
        }
        drop(control);

        let mut routing_order = project
            .routing_order()
            .map_err(|err| format!("routing graph invalid: {err:?}"))?;
        routing_order.reverse();
        let routing_order = routing_order
            .into_iter()
            .filter_map(|id| channel_index.get(&id).copied())
            .filter(|index| {
                let id = channel_ids[*index];
                project.tracks.iter().all(|track| track.channel.id != id)
            })
            .collect();

        Ok(LiveDawSnapshot {
            tracks,
            channels,
            routing_order,
            patterns_channel_index: project
                .patterns_bus_id
                .and_then(|id| channel_index.get(&id).copied()),
        })
    }

    pub fn master_meter(&self) -> MeterState {
        self.master_meter.load()
    }

    pub fn daw_meters(&self) -> Vec<(MixerChannelId, MeterState)> {
        let snapshot = self.daw.load();
        snapshot
            .channels
            .iter()
            .enumerate()
            .filter_map(|(index, channel)| {
                self.daw_meters
                    .get(index)
                    .map(|meter| (channel.id, meter.load()))
            })
            .collect()
    }

    fn now_seconds(&self) -> f32 {
        self.sample_clock.load(Ordering::Relaxed) as f32 / self.sample_rate.max(1.0)
    }

    fn enqueue_voice(&self, voice: Voice) {
        if self.command_tx.try_send(AudioCommand::AddVoice(voice)).is_err() {
            // File pleine ou flux audio mort : la voix est perdue. Journal
            // borné (le scheduling par motif peut échouer en rafale).
            static DROPPED: AtomicU64 = AtomicU64::new(0);
            let n = DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 8 || n % 1024 == 0 {
                eprintln!("[AudioEngine] voix perdue: file de commandes pleine ou flux inactif ({n} au total)");
            }
        }
    }

    fn voice_from_sample(
        &self,
        sample: Arc<Sample>,
        start_time: f32,
        end_time: f32,
        gain: f32,
        pan: f32,
        attack: f32,
        decay: f32,
        sustain: f32,
        release: f32,
        room: f32,
        cut: f32,
        shape: f32,
    ) -> Option<Voice> {
        let channels_in = sample.channels as usize;
        let frames = if channels_in == 0 {
            0
        } else {
            sample.data.len() / channels_in
        };
        if frames == 0 {
            return None;
        }
        Some(Voice {
            rate: sample.sample_rate as f32 / self.sample_rate.max(1.0),
            sample,
            position: 0.0,
            gain,
            pan,
            start_time: start_time as f64,
            end_time: end_time as f64,
            frames,
            attack,
            decay,
            sustain,
            release,
            room,
            cut,
            shape,
            cutoff_hz: 0.0,
            lp_state: 0.0,
        })
    }

    pub fn schedule_event(&self, event: AudioEvent) {
        if event.end_cycle <= 0.0 {
            return;
        }
        let cps = if event.cps > 0.0 { event.cps } else { 1.0 };
        let now = self.now_seconds();
        let start_offset = (event.start_cycle as f32) / (cps as f32);
        let duration_cycles = (event.end_cycle - event.start_cycle) as f32;
        let duration_secs = duration_cycles / (cps as f32);
        let start_time = now + SCHEDULE_LOOK_AHEAD_SECS + start_offset.max(0.0);
        let end_time = start_time + duration_secs;
        if end_time <= start_time || duration_secs <= 0.0 {
            return;
        }

        let Some(name) = event.s.clone() else {
            return;
        };
        let key = SampleKey {
            bank: event.bank.clone(),
            name,
        };
        let variant_index = event.n.map(|v| v.max(0.0).floor() as usize);
        let gain = event.gain.unwrap_or(0.8) as f32;
        let pan = event.pan.unwrap_or(0.0) as f32;

        if key.bank.is_none() && SYNTH_NAMES.contains(&key.name.as_str()) {
            let freq = midi_to_freq(event.midi_note.unwrap_or(69.0) as f32);
            let duration = duration_secs.clamp(0.05, 8.0);
            if let Some(sample) = synth_sample(&key.name, self.sample_rate as u32, freq, duration) {
                if let Some(voice) = self.voice_from_sample(
                    Arc::new(sample),
                    start_time,
                    end_time,
                    gain,
                    pan,
                    ev_attack(&event),
                    ev_decay(&event),
                    ev_sustain(&event),
                    ev_release(&event),
                    ev_room(&event),
                    ev_cut(&event),
                    ev_shape(&event),
                ) {
                    self.enqueue_voice(with_cutoff(with_speed(voice, event.speed), event.cutoff));
                }
            }
            return;
        }

        let cached = self
            .control
            .lock()
            .ok()
            .and_then(|state| state.library.get_cached(&key, variant_index));
        if let Some(sample) = cached {
            let end_time = sample_end_time(start_time, end_time, &sample);
            if let Some(voice) = self.voice_from_sample(
                sample,
                start_time,
                end_time,
                gain,
                pan,
                ev_attack(&event),
                ev_decay(&event),
                ev_sustain(&event),
                ev_release(&event),
                ev_room(&event),
                ev_cut(&event),
                ev_shape(&event),
            ) {
                self.enqueue_voice(with_cutoff(with_speed(voice, event.speed), event.cutoff));
            }
            return;
        }

        if key.bank.is_none() && key.name.starts_with("gm_") && key.name.len() > 3 {
            let params = self.control.lock().ok().and_then(|state| {
                let program = soundfont::gm_program_for_name(&key.name)?;
                let soundfont = state.soundfont.as_ref().cloned()?;
                Some(GmRenderParams {
                    soundfont,
                    program,
                    midi_note: event
                        .midi_note
                        .map(|m| m.clamp(0.0, 127.0) as u8)
                        .unwrap_or(60),
                    velocity: (event.gain.unwrap_or(0.8) * 127.0).clamp(0.0, 127.0) as u8,
                    duration_secs: (end_time - start_time).max(0.05).min(8.0),
                    sample_rate: self.sample_rate as u32,
                    start_time,
                    end_time,
                    gain,
                    pan,
                    attack: ev_attack(&event),
                    decay: ev_decay(&event),
                    sustain: ev_sustain(&event),
                    release: ev_release(&event),
                    room: ev_room(&event),
                    cut: ev_cut(&event),
                    shape: ev_shape(&event),
                })
            });
            if let Some(params) = params {
                if let Some(gm_sample) = soundfont::render_gm_note(
                    &params.soundfont,
                    params.program,
                    params.midi_note,
                    params.velocity,
                    params.duration_secs,
                    params.sample_rate,
                ) {
                    if let Some(voice) = self.voice_from_sample(
                        Arc::new(gm_sample),
                        params.start_time,
                        params.end_time,
                        params.gain,
                        params.pan,
                        params.attack,
                        params.decay,
                        params.sustain,
                        params.release,
                        params.room,
                        params.cut,
                        params.shape,
                    ) {
                        self.enqueue_voice(with_cutoff(with_speed(voice, event.speed), event.cutoff));
                    }
                }
                return;
            }
        }

        let path = {
            let Ok(mut state) = self.control.lock() else {
                return;
            };
            match state.library.get_path(&key, variant_index) {
                Some(path) => path,
                None => {
                    if state.missing_logged.insert(key.clone()) {
                        eprintln!(
                            "[AudioEngine] sample not found: bank={:?} name={:?} (variant {:?})",
                            key.bank, key.name, variant_index
                        );
                    }
                    return;
                }
            }
        };
        let sample = match load_wav_sample(&path) {
            Ok(sample) => sample,
            Err(e) => {
                eprintln!("[AudioEngine] failed to load {:?}: {}", path, e);
                return;
            }
        };
        if let Ok(state) = self.control.lock() {
            state
                .library
                .insert_cached(&key, variant_index, Arc::clone(&sample));
        }
        let end_time = sample_end_time(start_time, end_time, &sample);
        if let Some(voice) = self.voice_from_sample(
            sample,
            start_time,
            end_time,
            gain,
            pan,
            ev_attack(&event),
            ev_decay(&event),
            ev_sustain(&event),
            ev_release(&event),
            ev_room(&event),
            ev_cut(&event),
            ev_shape(&event),
        ) {
            self.enqueue_voice(with_cutoff(with_speed(voice, event.speed), event.cutoff));
        }
    }

    pub fn clear(&self) {
        let _ = self.command_tx.try_send(AudioCommand::Clear);
    }

    /// Короткий метрономный клик без зависимости от библиотеки семплов.
    pub fn play_metronome_click(&self, accent: bool, gain: f32) {
        let now = self.now_seconds();
        let dur = if accent { 0.055 } else { 0.035 };
        let freq = if accent { 1760.0 } else { 1175.0 };
        let g = gain.clamp(0.0, 1.0);
        let frames = (dur * self.sample_rate) as usize;
        if frames == 0 {
            return;
        }
        let mut data = Vec::with_capacity(frames);
        for n in 0..frames {
            let t = n as f32 / self.sample_rate;
            let env = (1.0 - (t / dur)).max(0.0).powf(4.0);
            data.push((t * freq * TAU).sin() * env);
        }
        if let Some(voice) = self.voice_from_sample(
            Arc::new(Sample {
                data: Arc::new(data),
                sample_rate: self.sample_rate as u32,
                channels: 1,
            }),
            now,
            now + dur,
            g,
            0.0,
            0.0,
            0.0,
            1.0,
            0.0,
            0.0,
            0.0,
            0.0,
        ) {
            self.enqueue_voice(voice);
        }
    }

    /// Перезагрузить библиотеку семплов из основной папки и кэша (.last_samples_cache).
    /// Блокирует поток вызова (тяжёлые I/O).
    pub fn reload_samples(&self, main_root: &Path) {
        let roots = sample_roots(main_root);
        match load_sample_library_merged(&roots) {
            Ok(lib) => {
                if let Ok(mut state) = self.control.lock() {
                    eprintln!(
                        "[AudioEngine] reloaded sample library ({} roots, {} entries)",
                        roots.len(),
                        lib.len()
                    );
                    state.library = lib;
                    state.missing_logged.clear();
                    if let Some(sf_path) = find_soundfont_in_dir(main_root) {
                        if let Some(sf) = soundfont::load_soundfont(&sf_path) {
                            state.soundfont = Some(sf);
                            eprintln!("[AudioEngine] GM soundfont loaded from {:?}", sf_path);
                        }
                    }
                }
                self.clear();
            }
            Err(e) => {
                eprintln!("[AudioEngine] failed to reload sample library: {}", e);
            }
        }
    }

    /// Downsampled waveform preview для отрисовки в таймлайне. None если семпл не найден или GM.
    pub fn get_waveform_preview(
        &self,
        key: &SampleKey,
        variant: Option<usize>,
    ) -> Option<std::sync::Arc<[f32]>> {
        self.control
            .lock()
            .ok()?
            .library
            .get_waveform_preview(key, variant)
    }

    /// Список семплов (ключ + число вариантов) для браузера.
    pub fn list_sample_entries(&self) -> Vec<(SampleKey, usize)> {
        self.control
            .lock()
            .map(|s| s.library.list_entries())
            .unwrap_or_default()
    }

    /// Возвращает закэшированный семпл по ключу (для офлайн-рендера).
    pub fn lookup_sample(
        &self,
        key: &SampleKey,
        variant: Option<usize>,
    ) -> Option<Arc<crate::sample_loader::Sample>> {
        self.control
            .lock()
            .ok()
            .and_then(|s| s.library.get_cached(key, variant))
    }

    /// Возвращает путь к файлу семпла по ключу (для офлайн-рендера).
    pub fn lookup_sample_path(
        &self,
        key: &SampleKey,
        variant: Option<usize>,
    ) -> Option<std::path::PathBuf> {
        self.control
            .lock()
            .ok()
            .and_then(|s| s.library.get_path(key, variant))
    }

    /// Проиграть семпл один раз (превью в браузере). Вызывать из main thread.
    /// Joue l'aperçu d'un sample ; renvoie sa durée lue (plafonnée à 5 s), `None` si introuvable.
    pub fn play_sample_preview(&self, key: &SampleKey, variant: Option<usize>) -> Option<f32> {
        let variant_index = variant.unwrap_or(0);
        let sample = self
            .control
            .lock()
            .ok()
            .and_then(|state| state.library.lookup(key, Some(variant_index)));
        let Some(sample) = sample else {
            eprintln!("[Sons] aperçu: sample introuvable {:?}", key);
            return None;
        };
        let now = self.now_seconds();
        let channels_in = sample.channels as usize;
        let frames = if channels_in == 0 {
            0
        } else {
            sample.data.len() / channels_in
        };
        if frames == 0 {
            eprintln!("[Sons] aperçu: sample vide {:?}", key);
            return None;
        }
        let duration_secs = (frames as f32 / sample.sample_rate as f32).min(5.0);
        self.enqueue_preview(sample, duration_secs);
        Some(duration_secs)
    }

    /// Aperçu d'un son généré : synthé Strudel (sine, saw, noise…) ou GM (si soundfont chargée).
    /// Joué comme une voix native, indépendamment du motif en cours.
    pub fn play_generated_preview(&self, name: &str) -> Option<f32> {
        const DURATION: f32 = 1.5;
        let sample = if name.starts_with("gm_") {
            let soundfont = self.control.lock().ok()?.soundfont.clone()?;
            let program = soundfont::gm_program_for_name(name)?;
            Arc::new(soundfont::render_gm_note(
                &soundfont,
                program,
                60,
                100,
                DURATION,
                self.sample_rate as u32,
            )?)
        } else {
            Arc::new(synth_sample(name, self.sample_rate as u32, midi_to_freq(69.0), DURATION)?)
        };
        self.enqueue_preview(sample, DURATION);
        Some(DURATION)
    }

    fn enqueue_preview(&self, sample: Arc<Sample>, duration_secs: f32) {
        let now = self.now_seconds();
        if let Some(voice) = self.voice_from_sample(
            sample,
            now,
            now + duration_secs,
            0.8,
            0.0,
            0.0,
            0.0,
            1.0,
            0.01,
            0.0,
            0.0,
            0.0,
        ) {
            self.enqueue_voice(voice);
        }
    }

    /// То же, что reload_samples, но выполняется в фоне — не блокирует UI и не вызывает buffer underrun.
    pub fn reload_samples_async(&self, main_root: &Path) {
        let control = Arc::clone(&self.control);
        let command_tx = self.command_tx.clone();
        let root = main_root.to_path_buf();
        thread::spawn(move || {
            let roots = sample_roots(&root);
            match load_sample_library_merged(&roots) {
                Ok(lib) => {
                    if let Ok(mut s) = control.lock() {
                        eprintln!(
                            "[AudioEngine] reloaded sample library ({} roots, {} entries)",
                            roots.len(),
                            lib.len()
                        );
                        s.library = lib;
                        s.missing_logged.clear();
                        if let Some(sf_path) = find_soundfont_in_dir(&root) {
                            if let Some(sf) = soundfont::load_soundfont(&sf_path) {
                                s.soundfont = Some(sf);
                                eprintln!("[AudioEngine] GM soundfont loaded from {:?}", sf_path);
                            }
                        }
                    }
                    // НЕ слать AudioCommand::Clear: это убивает все уже запланированные voice'ы,
                    // в том числе первые удары только что запущенного паттерна.
                    // Старые voice'ы держат Arc<Sample> на бывшую библиотеку и доиграют без проблем.
                }
                Err(e) => {
                    eprintln!("[AudioEngine] failed to reload sample library: {}", e);
                }
            }
        });
    }

    /// Прогреть кэш сэмплов: в фоне загружаем WAV с диска и кладём в `library`,
    /// чтобы первый `schedule_event` не блокировался на I/O и не терял первый voice.
    /// `samples`: список (bank, name, variant) для предварительногй загрузки.
    pub fn preload_samples(&self, samples: Vec<(Option<String>, String, Option<usize>)>) {
        if samples.is_empty() {
            return;
        }
        let control = Arc::clone(&self.control);
        thread::spawn(move || {
            for (bank, name, variant) in samples {
                let key = SampleKey { bank, name };
                // Уже закэширован? Проверяем быстро под lock'ом.
                let (already_cached, path) = match control.lock() {
                    Ok(state) => (
                        state.library.get_cached(&key, variant).is_some(),
                        state.library.get_path(&key, variant),
                    ),
                    Err(_) => continue,
                };
                if already_cached {
                    continue;
                }
                let Some(path) = path else {
                    continue; // библиотека ещё не загружена или сэмпла нет — пропускаем
                };
                let sample = match load_wav_sample(&path) {
                    Ok(sample) => sample,
                    Err(e) => {
                        eprintln!("[AudioEngine] preload échoué pour {:?}: {}", path, e);
                        continue;
                    }
                };
                if let Ok(state) = control.lock() {
                    state.library.insert_cached(&key, variant, sample);
                }
            }
        });
    }

    /// Проиграть короткий тестовый тон с заданной громкостью (0.0..=1.0).
    /// Реализовано как on-the-fly сгенерированный семпл.
    /// Синхронный прогрев: блокирует вызывающий поток, пока все WAV не окажутся в кэше.
    /// Используется ПЕРЕД Play, чтобы первый schedule_event гарантированно попал по
    /// быстрой ветви (get_cached → Some) и первый удар не «съедался» retain'ом.
    pub fn prewarm_samples_blocking(&self, requests: Vec<(Option<String>, String, Option<usize>)>) {
        for (bank, name, variant) in requests {
            let key = SampleKey { bank, name };
            let already = self
                .control
                .lock()
                .ok()
                .and_then(|s| s.library.get_cached(&key, variant))
                .is_some();
            if already {
                continue;
            }
            let path = self
                .control
                .lock()
                .ok()
                .and_then(|s| s.library.get_path(&key, variant));
            let Some(path) = path else { continue };
            let sample = match load_wav_sample(&path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[AudioEngine] prewarm failed for {:?}: {}", path, e);
                    continue;
                }
            };
            if let Ok(state) = self.control.lock() {
                state.library.insert_cached(&key, variant, sample);
            }
        }
    }

    /// Прогреть кэш сэмплов в фоновом потоке (не блокирует UI).
    /// Используется при редактировании кода, чтобы к Play кэш был уже подготовлен.
    pub fn prewarm_samples(&self, requests: Vec<(Option<String>, String, Option<usize>)>) {
        if requests.is_empty() {
            return;
        }
        let control = Arc::clone(&self.control);
        thread::spawn(move || {
            for (bank, name, variant) in requests {
                let key = SampleKey { bank, name };
                let already = {
                    let Ok(state) = control.lock() else { continue };
                    state.library.get_cached(&key, variant).is_some()
                };
                if already {
                    continue;
                }
                let path = {
                    let Ok(state) = control.lock() else { continue };
                    state.library.get_path(&key, variant)
                };
                let Some(path) = path else { continue };
                // Дисковый I/O вне lock'а.
                let sample = match load_wav_sample(&path) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[AudioEngine] prewarm failed for {:?}: {}", path, e);
                        continue;
                    }
                };
                if let Ok(state) = control.lock() {
                    state.library.insert_cached(&key, variant, sample);
                }
            }
        });
    }

    pub fn play_test_tone(&self, gain: f32) {
        let now = self.now_seconds();
        let dur = 0.3f32;
        let g = gain.clamp(0.0, 1.0);
        let frames = (dur * self.sample_rate) as usize;
        if frames == 0 {
            return;
        }

        let mut data = Vec::with_capacity(frames);
        for n in 0..frames {
            let t = n as f32 / self.sample_rate;
            let v = (t * 440.0 * TAU).sin();
            data.push(v);
        }

        if let Some(voice) = self.voice_from_sample(
            Arc::new(Sample {
                data: Arc::new(data),
                sample_rate: self.sample_rate as u32,
                channels: 1,
            }),
            now,
            now + dur,
            g,
            0.0,
            0.0,
            0.0,
            1.0,
            0.0,
            0.0,
            0.0,
            0.0,
        ) {
            self.enqueue_voice(voice);
        }
    }
}

fn snapshot_channel(
    project: &ProjectAudioState,
    channel: &MixerChannel,
    channel_index: &HashMap<MixerChannelId, usize>,
) -> LiveDawChannel {
    let audible = project.effective_audible(channel.id).audible;
    let sends = channel
        .sends
        .iter()
        .enumerate()
        .filter(|(_, send)| send.enabled)
        .filter_map(|(send_index, send)| {
            Some(LiveDawSend {
                send_index,
                target_index: *channel_index.get(&send.target_bus_id)?,
                level_db: send.level_db,
                pre_fader: send.pre_fader,
            })
        })
        .collect();
    let output = match channel.output_target {
        OutputTarget::Master => LiveDawOutput::Master,
        OutputTarget::Bus(target_id) => channel_index
            .get(&target_id)
            .copied()
            .map(LiveDawOutput::Bus)
            .unwrap_or(LiveDawOutput::None),
        OutputTarget::None => LiveDawOutput::None,
    };
    LiveDawChannel {
        id: channel.id,
        volume_db: channel.volume_db,
        pan: channel.pan,
        pan_law: channel.pan_law,
        muted: channel.mute || !audible,
        automation: channel.automation.clone(),
        inserts: channel
            .inserts
            .iter()
            .filter_map(BuiltInFx::from_slot)
            .collect(),
        sends,
        output,
    }
}

fn transport_cycle_at(
    transport: &LiveTransportSnapshot,
    sample_clock: u64,
    sample_rate: f32,
) -> f64 {
    let elapsed_samples = sample_clock.saturating_sub(transport.sync_sample_clock) as f64;
    let delta_cycles =
        elapsed_samples / sample_rate.max(1.0) as f64 * transport.cycles_per_second.max(0.000_001);
    if transport.reverse {
        (transport.sync_cycle - delta_cycles).max(0.0)
    } else {
        transport.sync_cycle + delta_cycles
    }
}

fn render_live_daw_frame(
    snapshot: &LiveDawSnapshot,
    processors: &mut [InsertProcessor],
    cycle: f64,
    cycles_per_second: f64,
    sample_rate: f32,
    sample_clips: bool,
    input_l: f32,
    input_r: f32,
    meter_acc: &mut [MeterAccumulator; MAX_LIVE_DAW_CHANNELS],
) -> (f32, f32) {
    if snapshot.channels.is_empty() {
        return (input_l, input_r);
    }
    let len = snapshot.channels.len().min(MAX_LIVE_DAW_CHANNELS);
    let mut channel_l = [0.0_f32; MAX_LIVE_DAW_CHANNELS];
    let mut channel_r = [0.0_f32; MAX_LIVE_DAW_CHANNELS];
    let mut master_l = 0.0_f32;
    let mut master_r = 0.0_f32;

    if let Some(channel_index) = snapshot.patterns_channel_index {
        if channel_index < len {
            channel_l[channel_index] += input_l;
            channel_r[channel_index] += input_r;
        } else {
            master_l += input_l;
            master_r += input_r;
        }
    } else if let Some(track) = snapshot.tracks.first() {
        if track.channel_index < len {
            channel_l[track.channel_index] += input_l;
            channel_r[track.channel_index] += input_r;
        } else {
            master_l += input_l;
            master_r += input_r;
        }
    } else {
        master_l += input_l;
        master_r += input_r;
    }

    if sample_clips {
        for track in &snapshot.tracks {
            if track.channel_index >= len {
                continue;
            }
            for clip in &track.clips {
                if cycle < clip.timeline_position
                    || cycle >= clip.timeline_position + clip.length_cycles
                {
                    continue;
                }
                let (mut left, mut right) = sample_live_clip(clip, cycle, cycles_per_second);
                if left == 0.0 && right == 0.0 {
                    continue;
                }
                let env = live_clip_envelope(clip, cycle);
                let pan = clip.pan.clamp(-1.0, 1.0);
                let left_gain = (1.0 - pan) * 0.5;
                let right_gain = (1.0 + pan) * 0.5;
                left *= clip.gain * env * left_gain;
                right *= clip.gain * env * right_gain;
                channel_l[track.channel_index] += left;
                channel_r[track.channel_index] += right;
            }
        }
    }

    for track in &snapshot.tracks {
        if track.channel_index < len {
            process_live_channel(
                track.channel_index,
                snapshot,
                processors,
                cycle,
                sample_rate,
                &mut channel_l,
                &mut channel_r,
                &mut master_l,
                &mut master_r,
                meter_acc,
            );
        }
    }

    for &channel_index in &snapshot.routing_order {
        if channel_index < len {
            process_live_channel(
                channel_index,
                snapshot,
                processors,
                cycle,
                sample_rate,
                &mut channel_l,
                &mut channel_r,
                &mut master_l,
                &mut master_r,
                meter_acc,
            );
        }
    }

    (master_l, master_r)
}

fn process_live_channel(
    channel_index: usize,
    snapshot: &LiveDawSnapshot,
    processors: &mut [InsertProcessor],
    cycle: f64,
    sample_rate: f32,
    channel_l: &mut [f32; MAX_LIVE_DAW_CHANNELS],
    channel_r: &mut [f32; MAX_LIVE_DAW_CHANNELS],
    master_l: &mut f32,
    master_r: &mut f32,
    meter_acc: &mut [MeterAccumulator; MAX_LIVE_DAW_CHANNELS],
) {
    let Some(channel) = snapshot.channels.get(channel_index) else {
        return;
    };
    let volume_db = automated_volume_db(channel, cycle);
    let pan = automated_pan(channel, cycle);
    let muted = automated_mute(channel, cycle);
    let pre_l = channel_l[channel_index];
    let pre_r = channel_r[channel_index];
    channel_l[channel_index] = 0.0;
    channel_r[channel_index] = 0.0;
    if muted {
        return;
    }
    let mut post_l = pre_l;
    let mut post_r = pre_r;
    if let Some(processor) = processors.get_mut(channel_index) {
        processor.process_pair(&channel.inserts, sample_rate, &mut post_l, &mut post_r);
    }
    apply_channel_sample(&mut post_l, &mut post_r, volume_db, pan, channel.pan_law);
    meter_acc[channel_index].add_pair(post_l, post_r);

    for send in &channel.sends {
        if send.target_index >= MAX_LIVE_DAW_CHANNELS {
            continue;
        }
        let send_gain = db_to_gain(automated_send_level_db(channel, cycle, send));
        let (send_l, send_r) = if send.pre_fader {
            (pre_l, pre_r)
        } else {
            (post_l, post_r)
        };
        channel_l[send.target_index] += send_l * send_gain;
        channel_r[send.target_index] += send_r * send_gain;
    }

    match channel.output {
        LiveDawOutput::Master => {
            *master_l += post_l;
            *master_r += post_r;
        }
        LiveDawOutput::Bus(target_index) if target_index < MAX_LIVE_DAW_CHANNELS => {
            channel_l[target_index] += post_l;
            channel_r[target_index] += post_r;
        }
        _ => {}
    }
}

fn automated_volume_db(channel: &LiveDawChannel, cycle: f64) -> f32 {
    automation_value_at(&channel.automation, &AutomationParameter::Volume, cycle)
        .unwrap_or(channel.volume_db)
}

fn automated_pan(channel: &LiveDawChannel, cycle: f64) -> f32 {
    automation_value_at(&channel.automation, &AutomationParameter::Pan, cycle)
        .unwrap_or(channel.pan)
}

fn automated_mute(channel: &LiveDawChannel, cycle: f64) -> bool {
    let lane_mute = automation_value_at(&channel.automation, &AutomationParameter::Mute, cycle)
        .map(|value| value >= 0.5)
        .unwrap_or(false);
    channel.muted || lane_mute
}

fn automated_send_level_db(channel: &LiveDawChannel, cycle: f64, send: &LiveDawSend) -> f32 {
    let parameter = AutomationParameter::SendLevel {
        send_index: send.send_index,
    };
    automation_value_at(&channel.automation, &parameter, cycle).unwrap_or(send.level_db)
}

fn automation_value_at(
    lanes: &[AutomationLane],
    parameter: &AutomationParameter,
    cycle: f64,
) -> Option<f32> {
    let lane = lanes
        .iter()
        .find(|lane| lane.parameter == *parameter && lane.mode != AutomationMode::Write)?;
    interpolate_automation_points(&lane.points, cycle)
}

fn interpolate_automation_points(
    points: &[crate::project_audio::AutomationPoint],
    cycle: f64,
) -> Option<f32> {
    let first = points.first()?;
    if cycle <= first.cycle {
        return Some(first.value);
    }
    for window in points.windows(2) {
        let a = window[0];
        let b = window[1];
        if cycle <= b.cycle {
            let span = (b.cycle - a.cycle).max(f64::EPSILON);
            let t = ((cycle - a.cycle) / span).clamp(0.0, 1.0) as f32;
            return Some(a.value + (b.value - a.value) * t);
        }
    }
    points.last().map(|point| point.value)
}

fn sample_live_clip(clip: &LiveDawClip, cycle: f64, cycles_per_second: f64) -> (f32, f32) {
    let channels = clip.source.channels.max(1) as usize;
    let frames = clip.source.data.len() / channels;
    if frames == 0 {
        return (0.0, 0.0);
    }
    let source_seconds = clip.source_offset_seconds
        + ((cycle - clip.timeline_position) / cycles_per_second.max(0.000_001))
            * clip.source_rate_factor.max(0.000_001);
    let frame_pos = source_seconds * clip.source.sample_rate.max(1) as f64;
    if frame_pos < 0.0 || frame_pos >= frames.saturating_sub(1) as f64 {
        return (0.0, 0.0);
    }
    let base = frame_pos.floor() as usize;
    let frac = (frame_pos - base as f64) as f32;
    let next = (base + 1).min(frames.saturating_sub(1));
    let sample = |frame: usize, ch: usize| -> f32 {
        clip.source
            .data
            .get(frame * channels + ch.min(channels - 1))
            .copied()
            .unwrap_or(0.0)
    };
    if channels == 1 {
        let mono = sample(base, 0) + (sample(next, 0) - sample(base, 0)) * frac;
        (mono, mono)
    } else {
        let l = sample(base, 0) + (sample(next, 0) - sample(base, 0)) * frac;
        let r = sample(base, 1) + (sample(next, 1) - sample(base, 1)) * frac;
        (l, r)
    }
}

fn live_clip_envelope(clip: &LiveDawClip, cycle: f64) -> f32 {
    let local = (cycle - clip.timeline_position).clamp(0.0, clip.length_cycles.max(0.0));
    let fade_in = if clip.fade_in_cycles <= 0.0 {
        1.0
    } else {
        apply_live_fade(
            (local / clip.fade_in_cycles.max(0.000_001)).clamp(0.0, 1.0),
            clip.fade_in_curve,
        )
    };
    let fade_out_start = (clip.length_cycles - clip.fade_out_cycles).max(0.0);
    let fade_out = if clip.fade_out_cycles <= 0.0 || local <= fade_out_start {
        1.0
    } else {
        apply_live_fade(
            ((clip.length_cycles - local) / clip.fade_out_cycles.max(0.000_001)).clamp(0.0, 1.0),
            clip.fade_out_curve,
        )
    };
    (fade_in * fade_out).clamp(0.0, 1.0) as f32
}

fn apply_live_fade(x: f64, curve: FadeCurve) -> f64 {
    let x = x.clamp(0.0, 1.0);
    match curve {
        FadeCurve::Linear => x,
        FadeCurve::EqualPower => (x * std::f64::consts::FRAC_PI_2).sin(),
        FadeCurve::Exponential => x * x,
        FadeCurve::SCurve => x * x * (3.0 - 2.0 * x),
    }
}

/// Noms des synthés natifs — synchronisé avec `live_params::SYNTH_NAMES`.
const SYNTH_NAMES: &[&str] = &[
    "sine", "cosine", "sawtooth", "saw", "square", "triangle", "tri", "pink", "white", "brown", "crackle",
    // Basses / leads / nappes techno
    "supersaw", "hoover", "pulse", "subbass", "fmsine",
    // Percussions synthétisées (type 808/909)
    "synthbd", "synthsd", "synthhh", "synthoh", "synthcp",
];

/// Un fichier joue sur toute sa durée (comme Strudel), pas seulement sur celle de l'événement.
/// Plafonné à `MAX_SAMPLE_SECS` pour qu'une prise très longue ne sature pas les voix.
const MAX_SAMPLE_SECS: f32 = 30.0;

fn sample_end_time(start_time: f32, event_end: f32, sample: &Sample) -> f32 {
    let frames = sample.data.len() / sample.channels.max(1) as usize;
    let secs = frames as f32 / sample.sample_rate.max(1) as f32;
    event_end.max(start_time + secs.min(MAX_SAMPLE_SECS))
}

/// Applique `.speed()` à une voix : la vitesse de lecture multiplie le taux de lecture.
fn with_speed(mut voice: Voice, speed: Option<f64>) -> Voice {
    if let Some(s) = speed {
        if s.is_finite() && s > 0.0 {
            voice.rate *= s as f32;
        }
    }
    voice
}

/// Applique `.lpf(…)` (cutoff en Hz) à une voix : filtre passe-bas un pôle ; absent/0 = aucun.
fn with_cutoff(mut voice: Voice, cutoff: Option<f64>) -> Voice {
    voice.cutoff_hz = cutoff.filter(|c| c.is_finite() && *c > 0.0).unwrap_or(0.0) as f32;
    voice
}

/// Fréquence (Hz) d'une note MIDI, A4 (69) = 440 Hz.
fn midi_to_freq(midi: f32) -> f32 {
    440.0 * 2f32.powf((midi - 69.0) / 12.0)
}

/// Bruit blanc xorshift — partagé par les synthés bruités et les percussions.
fn xorshift_u32(seed: &mut u32) -> f32 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 5;
    (*seed as f32 / u32::MAX as f32) * 2.0 - 1.0
}

/// Normalise une percussion générée : plafond de crête, micro-fade d'entrée
/// (1 ms, préserve le transient) et fade de sortie (50 ms) — mêmes bornes que
/// les synthés tonaux pour le gain staging.
fn finalize_drum(mut data: Vec<f32>, sr: f32, cap: f32) -> Vec<f32> {
    let n = data.len();
    let peak = data.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    if peak > cap {
        let k = cap / peak;
        for v in data.iter_mut() {
            *v *= k;
        }
    }
    let fade_in = ((0.001 * sr) as usize).min(n);
    for (i, v) in data.iter_mut().enumerate().take(fade_in) {
        *v *= i as f32 / fade_in.max(1) as f32;
    }
    let fade_out = ((0.05 * sr) as usize).min(n);
    for i in 0..fade_out {
        data[n - 1 - i] *= i as f32 / fade_out as f32;
    }
    data
}

/// Kick type 909 : sinus dont la hauteur chute de ~156 à 46 Hz + clic d'attaque.
/// `freq` ignoré volontairement : la percussion a son accordage propre
/// (`.speed()` reste le moyen de l'altérer).
fn drum_kick(sample_rate: u32, secs: f32) -> Sample {
    let sr = sample_rate.max(1) as f32;
    let n = (secs.min(2.0) * sr) as usize;
    let mut seed: u32 = 0x9E37_5A01;
    let mut phase = 0.0f32;
    let mut data = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / sr;
        phase += (46.0 + 110.0 * (-t / 0.03).exp()) / sr;
        let body = (phase * TAU).sin() * (-t / 0.16).exp();
        let click = if t < 0.002 {
            xorshift_u32(&mut seed) * 0.6 * (1.0 - t / 0.002)
        } else {
            0.0
        };
        data.push((body * 0.95 + click * 0.4).clamp(-1.0, 1.0));
    }
    Sample { data: Arc::new(finalize_drum(data, sr, 0.28)), sample_rate, channels: 1 }
}

/// Caisse claire type 909 : corps 180+335 Hz (triangles) + bruit, ~0.25 s.
fn drum_snare(sample_rate: u32, secs: f32) -> Sample {
    let sr = sample_rate.max(1) as f32;
    let n = (secs.min(2.0) * sr) as usize;
    let mut seed: u32 = 0x51ED_270B;
    let mut data = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / sr;
        let body = ((t * 180.0).fract() - 0.5).abs() * 4.0 - 1.0;
        let body2 = ((t * 335.0).fract() - 0.5).abs() * 4.0 - 1.0;
        let tone = (body * 0.55 + body2 * 0.30) * (-t / 0.10).exp();
        let snap = if t < 0.008 { xorshift_u32(&mut seed) * 0.5 } else { 0.0 };
        let noise = xorshift_u32(&mut seed) * 0.8 * (-t / 0.11).exp();
        data.push((tone + noise + snap * (-t / 0.01).exp()).clamp(-1.0, 1.0) * 0.85);
    }
    Sample { data: Arc::new(finalize_drum(data, sr, 0.28)), sample_rate, channels: 1 }
}

/// Charleston type 808 : 6 oscillateurs carrés métalliques. `open` = queue longue.
fn drum_hat(sample_rate: u32, secs: f32, open: bool) -> Sample {
    const OSC: [f32; 6] = [263.0, 400.0, 421.0, 474.0, 587.0, 845.0];
    let sr = sample_rate.max(1) as f32;
    let n = (secs.min(2.0) * sr) as usize;
    let decay = if open { 0.35 } else { 0.045 };
    let mut data = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / sr;
        let mut acc = 0.0;
        for &f in &OSC {
            acc += if (t * f * 1.5).fract() < 0.5 { 1.0 } else { -1.0 };
        }
        data.push((acc / OSC.len() as f32) * (-t / decay).exp() * 0.55);
    }
    Sample { data: Arc::new(finalize_drum(data, sr, 0.28)), sample_rate, channels: 1 }
}

/// Clap type 909 : quatre rafales de bruit à ~11 ms + queue.
fn drum_clap(sample_rate: u32, secs: f32) -> Sample {
    let sr = sample_rate.max(1) as f32;
    let n = (secs.min(2.0) * sr) as usize;
    let mut seed: u32 = 0xC0FF_EE01;
    let mut data = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / sr;
        let mut env = 0.0f32;
        for k in 0..4 {
            let tk = t - k as f32 * 0.011;
            if tk >= 0.0 {
                env += (-tk / 0.006).exp();
            }
        }
        if t >= 0.033 {
            env += 0.7 * (-(t - 0.033) / 0.16).exp();
        }
        data.push((xorshift_u32(&mut seed) * env.min(1.2) * 0.7).clamp(-1.0, 1.0));
    }
    Sample { data: Arc::new(finalize_drum(data, sr, 0.28)), sample_rate, channels: 1 }
}

/// Synthé Strudel mono (ondes à `freq` Hz, bruits pour pink/white/brown/crackle), fondu en entrée/sortie.
fn synth_sample(name: &str, sample_rate: u32, freq: f32, duration_secs: f32) -> Option<Sample> {
    match name {
        "synthbd" => return Some(drum_kick(sample_rate, duration_secs)),
        "synthsd" => return Some(drum_snare(sample_rate, duration_secs)),
        "synthhh" => return Some(drum_hat(sample_rate, duration_secs, false)),
        "synthoh" => return Some(drum_hat(sample_rate, duration_secs, true)),
        "synthcp" => return Some(drum_clap(sample_rate, duration_secs)),
        _ => {}
    }
    let kind = *SYNTH_NAMES.iter().find(|k| **k == name)?;
    let sr = sample_rate.max(1) as f32;
    let n = (duration_secs * sr) as usize;
    let fade_in = (0.005 * sr) as usize;
    let fade_out = (0.06 * sr) as usize;
    let mut seed: u32 = 0x2545_F491;
    let mut white = move || xorshift_u32(&mut seed);
    let (mut p1, mut p2, mut p3, mut brown, mut crackle_amp, mut crackle_pos) =
        (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32, 0usize);
    let mut data = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / sr;
        let phase = (t * freq).fract();
        let v = match kind {
            "sine" => (t * freq * TAU).sin(),
            "cosine" => (t * freq * TAU).cos(),
            "sawtooth" | "saw" => 2.0 * phase - 1.0,
            "square" => {
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            "triangle" | "tri" => 4.0 * (phase - 0.5).abs() - 1.0,
            // 7 dents de scie désaccordées — le lead trance/techno classique.
            "supersaw" => {
                const DETUNE: [f32; 7] =
                    [-0.025, -0.017, -0.008, 0.0, 0.008, 0.017, 0.025];
                let mut acc = 0.0;
                for &d in &DETUNE {
                    acc += 2.0 * (t * freq * (1.0 + d)).fract() - 1.0;
                }
                acc / DETUNE.len() as f32
            }
            // Mentasm-ish : saws désaccordées + PWM qui s'ouvre sur la durée.
            "hoover" => {
                const DETUNE: [f32; 5] = [-0.03, -0.015, 0.0, 0.015, 0.03];
                let mut acc = 0.0;
                for &d in &DETUNE {
                    acc += 2.0 * (t * freq * (1.0 + d)).fract() - 1.0;
                }
                let w = 0.5 + 0.42 * (t / duration_secs).min(1.0);
                let sq = if phase < w { 1.0 } else { -1.0 };
                (acc / DETUNE.len() as f32) * 0.7 + sq * 0.3
            }
            // Onde pulsée étroite — sonorité roseau, bonne pour stabs techno.
            "pulse" => {
                if phase < 0.2 {
                    1.0
                } else {
                    -1.0
                }
            }
            // Sous-basse : sinus + octave en dessous — le sub techno.
            "subbass" => {
                let w = t * freq * TAU;
                w.sin() * 0.6 + (w * 0.5).sin() * 0.4
            }
            // FM 2 opérateurs, indice qui retombe — basse/lead brillant type DX.
            "fmsine" => {
                let w = t * freq * TAU;
                let idx = 0.4 + 5.0 * (-4.0 * t / duration_secs).exp();
                (w + idx * w.sin()).sin()
            }
            "white" => white(),
            "pink" => {
                let w = white();
                p1 = 0.99765 * p1 + w * 0.0990460;
                p2 = 0.96300 * p2 + w * 0.2965164;
                p3 = 0.57000 * p3 + w * 1.0526913;
                (p1 + p2 + p3 + w * 0.1848) * 0.11
            }
            "brown" => {
                brown = (brown + 0.02 * white()).clamp(-1.0, 1.0);
                brown
            }
            _ => {
                if crackle_pos == 0 && white().abs() > 0.992 {
                    crackle_amp = white();
                    crackle_pos = (0.004 * sr) as usize;
                }
                if crackle_pos > 0 {
                    crackle_pos -= 1;
                    crackle_amp * (crackle_pos as f32 / (0.004 * sr)).max(0.0)
                } else {
                    0.0
                }
            }
        };
        let mut amp = 0.3 * v;
        if i < fade_in {
            amp *= i as f32 / fade_in as f32;
        }
        if i + fade_out > n {
            amp *= (n - i) as f32 / fade_out as f32;
        }
        data.push(amp);
    }
    Some(Sample {
        data: Arc::new(data),
        sample_rate,
        channels: 1,
    })
}

/// Огибающая ADSR: возвращает множитель 0..1 по времени t (сек).
fn envelope(
    attack: f32,
    decay: f32,
    sustain: f32,
    release: f32,
    start: f64,
    end: f64,
    t: f32,
) -> f32 {
    let t = t as f64;
    let start = start;
    let end = end;
    if t < start || t > end {
        return 0.0;
    }
    let rel_start = end - release.max(0.001) as f64;
    if t >= rel_start {
        return (sustain as f64 * (end - t) / release.max(0.001) as f64) as f32;
    }
    let dt = t - start;
    if attack > 0.0 && dt < attack as f64 {
        return (dt / attack as f64) as f32;
    }
    let att_end = start + attack as f64;
    if decay > 0.0 && t < att_end + decay as f64 {
        let d = (t - att_end) / decay as f64;
        return (1.0 - (1.0 - sustain as f64) * d.min(1.0)) as f32;
    }
    sustain
}

fn soft_clip(x: f32) -> f32 {
    // Простое мягкое клипирование: x / (1 + |x|)
    let a = x.abs();
    if a < 1e-6 {
        x
    } else {
        x / (1.0 + a)
    }
}

#[cfg(test)]
mod preview_tests {
    use super::{sample_end_time, synth_sample, Sample, SYNTH_NAMES};
    use std::sync::Arc;

    #[test]
    fn file_samples_play_their_full_length() {
        let s = Sample { data: Arc::new(vec![0.0; 48_000 * 2]), sample_rate: 48_000, channels: 2 };
        assert_eq!(sample_end_time(10.0, 10.5, &s), 11.0);
        assert_eq!(sample_end_time(10.0, 12.0, &s), 12.0);
        let long = Sample { data: Arc::new(vec![0.0; 48_000 * 120]), sample_rate: 48_000, channels: 1 };
        assert_eq!(sample_end_time(0.0, 0.5, &long), 30.0);
    }

    #[test]
    fn synth_previews_are_audible_and_bounded() {
        for name in SYNTH_NAMES {
            let s = synth_sample(name, 48_000, 440.0, 1.5).expect(name);
            assert_eq!(s.data.len(), 72_000, "{name}");
            let peak = s.data.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(peak > 0.01 && peak <= 0.3 + 1e-6, "{name} peak {peak}");
            assert!(s.data[0].abs() < 1e-3, "{name} must fade in");
            assert!(s.data[s.data.len() - 1].abs() < 1e-3, "{name} must fade out");
        }
    }

    #[test]
    fn unknown_synth_has_no_preview() {
        assert!(synth_sample("gm_piano", 48_000, 440.0, 1.5).is_none());
        assert!(synth_sample("nope", 48_000, 440.0, 1.5).is_none());
    }
}

#[cfg(test)]
mod speed_tests {
    use super::*;

    fn voice(speed_rate: f32) -> Voice {
        Voice {
            sample: Arc::new(Sample { data: Arc::new(vec![0.0; 4]), sample_rate: 44_100, channels: 1 }),
            rate: speed_rate,
            position: 0.0,
            gain: 1.0,
            pan: 0.0,
            start_time: 0.0,
            end_time: 1.0,
            frames: 4,
            attack: 0.0,
            decay: 0.0,
            sustain: 1.0,
            release: 0.0,
            room: 0.0,
            cut: 0.0,
            shape: 0.0,
            cutoff_hz: 0.0,
            lp_state: 0.0,
        }
    }

    #[test]
    fn speed_scales_playback_rate_only_when_valid() {
        assert_eq!(with_speed(voice(1.0), Some(2.0)).rate, 2.0);
        assert_eq!(with_speed(voice(1.0), Some(0.5)).rate, 0.5);
        assert_eq!(with_speed(voice(1.0), Some(-1.0)).rate, 1.0);
        assert_eq!(with_speed(voice(1.0), None).rate, 1.0);
    }

    #[test]
    fn full_voice_list_drops_oldest_not_newest() {
        let mut state = AudioRenderState::new(48_000.0);
        for _ in 0..MAX_VOICES {
            state.voices.push(voice(1.0));
        }
        let (tx, rx) = bounded::<AudioCommand>(4);
        tx.try_send(AudioCommand::AddVoice(voice(7.0))).unwrap();
        state.drain_commands(&rx);
        assert_eq!(state.voices.len(), MAX_VOICES);
        assert_eq!(state.voices.last().unwrap().rate, 7.0);
    }
}
