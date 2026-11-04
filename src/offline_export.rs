//! Офлайн-рендер strudel-паттернов в WAV без реального аудио-потока.
//!
//! Пайплайн:
//!   1. Главный поток: опросить bridge за [0, duration] несколькими вызовами audio_window.
//!   2. Главный поток: разрешить пути/данные семплов через AudioEngine.
//!   3. Фоновый поток: загрузить семплы, смешать в PCM-буфер, записать WAV.

use crate::audio::AudioEngine;
use crate::audio_clip_engine::write_wav_with_options;
use crate::project_audio::MasterOptions;
use crate::sample_loader::{load_wav_sample, Sample, SampleKey};
use crate::strudel_bridge::{StrudelAudioEvent, StrudelBridge};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

// ── Конфигурация ───────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ExportConfig {
    pub duration_cycles: f64,
    pub sample_rate: u32,
    pub channels: u16,
    pub master_gain: f32,
    pub master_options: MasterOptions,
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            duration_cycles: 8.0,
            sample_rate: 48_000,
            channels: 2,
            master_gain: 1.0,
            master_options: MasterOptions::default(),
        }
    }
}

// ── Внутренние структуры для передачи в поток ─────────────────────────────────

struct ResolvedEvent {
    start_sec: f64,
    /// Аудиоданные: либо уже загружены из кэша, либо нужно загрузить из пути.
    data: ResolvedSample,
    gain_l: f32,
    gain_r: f32,
}

enum ResolvedSample {
    Cached(Arc<Sample>),
    Path(PathBuf),
}

// ── Сбор событий из bridge ────────────────────────────────────────────────────

/// Опрашивает bridge чанками по 4 цикла и собирает все события за [0, duration_cycles].
/// Возвращает (события, cps).
pub fn collect_events(
    bridge: &mut StrudelBridge,
    duration_cycles: f64,
) -> Result<(Vec<StrudelAudioEvent>, f64), String> {
    let chunk = 4.0_f64;
    let mut all_events = Vec::new();
    let mut cps = 1.0_f64;
    let mut pos = 0.0_f64;
    while pos < duration_cycles {
        let to = (pos + chunk).min(duration_cycles);
        let result = bridge.audio_window(pos, to)?;
        if result.cps > 0.0 {
            cps = result.cps;
        }
        // Убираем дубликаты на границах чанков.
        let prev_end = pos;
        for ev in result.events {
            if ev.begin >= prev_end || all_events.is_empty() {
                all_events.push(ev);
            }
        }
        pos = to;
    }
    Ok((all_events, cps))
}

// ── Разрешение семплов ────────────────────────────────────────────────────────

fn resolve_events(
    events: &[StrudelAudioEvent],
    cps: f64,
    audio: &AudioEngine,
    config: &ExportConfig,
) -> Vec<ResolvedEvent> {
    let mut resolved = Vec::with_capacity(events.len());
    for ev in events {
        let Some(name) = ev.s.as_ref() else { continue };
        // GM-синтез пропускаем — сложно рендерить оффлайн без soundfont контекста.
        if name.starts_with("gm_") {
            continue;
        }
        let key = SampleKey {
            bank: ev.bank.clone(),
            name: name.clone(),
        };
        let variant = ev.n.map(|v| v.max(0.0).floor() as usize);

        let gain = ev.gain.unwrap_or(0.8) as f32 * config.master_gain;
        let pan = ev.pan.unwrap_or(0.0) as f32;
        let gain_l = gain * (1.0 - pan.max(0.0));
        let gain_r = gain * (1.0 + pan.min(0.0));

        let start_sec = ev.begin / cps.max(1e-6);

        // Пробуем сначала кэш (уже загружен AudioEngine), потом путь из индекса.
        let data = if let Some(sample) = audio.lookup_sample(&key, variant) {
            resolved.push(ResolvedEvent {
                start_sec,
                data: ResolvedSample::Cached(sample),
                gain_l,
                gain_r,
            });
            continue;
        } else if let Some(path) = audio.lookup_sample_path(&key, variant) {
            ResolvedSample::Path(path)
        } else {
            continue;
        };

        resolved.push(ResolvedEvent {
            start_sec,
            data,
            gain_l,
            gain_r,
        });
    }
    resolved
}

// ── Рендер в буфер ────────────────────────────────────────────────────────────

fn render_resolved(resolved: Vec<ResolvedEvent>, config: &ExportConfig, cps: f64) -> Vec<f32> {
    let sr = config.sample_rate as f64;
    let duration_secs = config.duration_cycles / cps.max(1e-6);
    let total_frames = (duration_secs * sr) as usize;
    let ch_out = config.channels.max(1) as usize;
    let mut buf = vec![0.0f32; total_frames * ch_out];

    for ev in resolved {
        let start_frame = (ev.start_sec * sr) as usize;
        if start_frame >= total_frames {
            continue;
        }

        let sample: Arc<Sample> = match ev.data {
            ResolvedSample::Cached(s) => s,
            ResolvedSample::Path(path) => match load_wav_sample(&path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[OfflineExport] failed to load {:?}: {e}", path);
                    continue;
                }
            },
        };

        let src_ch = sample.channels.max(1) as usize;
        let src_frames = sample.data.len() / src_ch;
        if src_frames == 0 {
            continue;
        }

        let rate = sample.sample_rate as f64 / sr;
        let max_out = ((src_frames as f64 / rate) as usize + 1).min(total_frames - start_frame);

        for out_frame in 0..max_out {
            let out_pos = start_frame + out_frame;
            if out_pos >= total_frames {
                break;
            }
            let src_pos = out_frame as f64 * rate;
            let src_idx = src_pos.floor() as usize;
            if src_idx >= src_frames {
                break;
            }
            let frac = (src_pos - src_idx as f64) as f32;
            let next = (src_idx + 1).min(src_frames - 1);

            let get = |frame: usize, ch: usize| -> f32 {
                let i = frame * src_ch + ch.min(src_ch - 1);
                sample.data.get(i).copied().unwrap_or(0.0)
            };

            let l_cur = get(src_idx, 0);
            let l_next = get(next, 0);
            let l = l_cur + (l_next - l_cur) * frac;

            if ch_out >= 2 {
                let r_cur = if src_ch > 1 { get(src_idx, 1) } else { l_cur };
                let r_next = if src_ch > 1 { get(next, 1) } else { l_next };
                let r = r_cur + (r_next - r_cur) * frac;
                buf[out_pos * 2] += l * ev.gain_l;
                buf[out_pos * 2 + 1] += r * ev.gain_r;
            } else {
                buf[out_pos] += l * ((ev.gain_l + ev.gain_r) * 0.5);
            }
        }
    }

    // Лёгкий лимитер: предотвращает клиппинг от наложения нескольких дорожек.
    let peak = buf.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
    if peak > 1.0 {
        let scale = 0.99 / peak;
        for s in &mut buf {
            *s *= scale;
        }
    }

    buf
}

// ── Публичный API ─────────────────────────────────────────────────────────────

/// Запускает экспорт в фоновом потоке. Возвращает Receiver для опроса результата.
/// Вызывается с главного потока, который уже собрал события и имеет доступ к AudioEngine.
pub fn start_export(
    events: Vec<StrudelAudioEvent>,
    cps: f64,
    output_path: PathBuf,
    config: ExportConfig,
    audio: &Arc<AudioEngine>,
) -> mpsc::Receiver<Result<(), String>> {
    let (tx, rx) = mpsc::channel();
    let audio = Arc::clone(audio);

    thread::spawn(move || {
        let resolved = resolve_events(&events, cps, &audio, &config);
        let buf = render_resolved(resolved, &config, cps);
        let result = write_wav_with_options(
            &output_path,
            &buf,
            config.sample_rate,
            config.channels,
            config.master_options,
        );
        let _ = tx.send(result);
    });

    rx
}
