use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SizedSample;

use crate::project_audio::{AudioSource, InputChannelMode, InputConfig};

const DEFAULT_RING_SAMPLES: usize = 48_000 * 2 * 20;
const WRITER_IDLE_SLEEP: Duration = Duration::from_millis(3);

pub struct RecordingController {
    active: Option<ActiveRecording>,
}

impl Default for RecordingController {
    fn default() -> Self {
        Self { active: None }
    }
}

impl RecordingController {
    pub fn input_devices() -> Vec<String> {
        let host = cpal::default_host();
        host.input_devices()
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|device| device.name().ok())
            .collect()
    }

    pub fn is_recording(&self) -> bool {
        self.active.is_some()
    }

    pub fn meters(&self) -> RecordingMeters {
        self.active
            .as_ref()
            .map(|active| RecordingMeters {
                peak: f32::from_bits(active.peak_bits.load(Ordering::Relaxed)),
                clipped: active.clipped.load(Ordering::Relaxed),
                dropped_samples: active.ring.dropped(),
                frames_written: active.frames_written.load(Ordering::Relaxed),
            })
            .unwrap_or_default()
    }

    pub fn start(&mut self, request: RecordingRequest) -> Result<RecordingSessionStarted, String> {
        if self.active.is_some() {
            return Err("recording is already active".to_string());
        }

        std::fs::create_dir_all(&request.directory).map_err(|e| {
            format!(
                "create recording directory {:?} failed: {e}",
                request.directory
            )
        })?;
        let out_channels = match request.input.channel_mode {
            InputChannelMode::Mono => 1,
            InputChannelMode::Stereo => 2,
        };
        let ring = Arc::new(AudioRingBuffer::new(
            request
                .ring_capacity_samples
                .unwrap_or(DEFAULT_RING_SAMPLES)
                .max(out_channels * 1024),
        ));
        let stop = Arc::new(AtomicBool::new(false));
        let peak_bits = Arc::new(AtomicU32::new(0.0_f32.to_bits()));
        let clipped = Arc::new(AtomicBool::new(false));
        let frames_written = Arc::new(AtomicU64::new(0));
        let session_id = request.session_id;
        let temp_path = request
            .directory
            .join(format!("recording-{session_id}.part.wav"));
        let final_path = request
            .directory
            .join(format!("recording-{session_id}.wav"));
        let sidecar_path = request
            .directory
            .join(format!("recording-{session_id}.json"));

        let host = cpal::default_host();
        let started_input = start_first_working_input_stream(
            &host,
            request.input.device_name.as_deref(),
            request.input.clone(),
            out_channels,
            Arc::clone(&ring),
            Arc::clone(&peak_bits),
            Arc::clone(&clipped),
        )?;
        write_sidecar(
            &sidecar_path,
            &temp_path,
            &final_path,
            started_input.sample_rate,
            out_channels as u16,
        )?;

        let writer_ring = Arc::clone(&ring);
        let writer_stop = Arc::clone(&stop);
        let writer_frames = Arc::clone(&frames_written);
        let writer_temp = temp_path.clone();
        let writer_final = final_path.clone();
        let writer_sidecar = sidecar_path.clone();
        let sample_rate = started_input.sample_rate;
        let writer = thread::spawn(move || {
            writer_thread(
                writer_ring,
                writer_stop,
                writer_frames,
                writer_temp,
                writer_final,
                writer_sidecar,
                sample_rate,
                out_channels as u16,
            )
        });

        let started_sample_rate = started_input.sample_rate;
        self.active = Some(ActiveRecording {
            session_id,
            device_name: started_input.device_name,
            started: Instant::now(),
            temp_path,
            sidecar_path,
            input_latency_frames: started_input.input_latency_frames,
            ring,
            stop,
            peak_bits,
            clipped,
            frames_written,
            _stream: started_input.stream,
            writer: Some(writer),
        });

        Ok(RecordingSessionStarted {
            session_id,
            file_path: final_path,
            sample_rate: started_sample_rate,
            channels: out_channels as u16,
            input_latency_frames: started_input.input_latency_frames,
        })
    }

    pub fn stop(&mut self) -> Result<RecordingSummary, String> {
        let active = self.take_active()?;
        finalize_active_recording(active)
    }

    /// Stop capture without blocking the UI thread; poll the receiver for completion.
    pub fn stop_async(
        &mut self,
    ) -> Result<mpsc::Receiver<Result<RecordingFinalize, String>>, String> {
        let active = self.take_active()?;
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let result = finalize_active_recording(active).and_then(|summary| {
                let peak = crate::waveform_cache::load_or_build_peak(&summary.file_path).ok();
                Ok(RecordingFinalize { summary, peak })
            });
            let _ = tx.send(result);
        });
        Ok(rx)
    }

    fn take_active(&mut self) -> Result<ActiveRecording, String> {
        self.active
            .take()
            .ok_or_else(|| "recording is not active".to_string())
    }

    pub fn recover_incomplete_recordings(directory: &Path) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().map_or(false, |ext| ext == "wav"))
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map_or(false, |name| name.ends_with(".part.wav"))
            })
            .collect()
    }
}

pub struct RecordingRequest {
    pub session_id: u64,
    pub directory: PathBuf,
    pub input: InputConfig,
    pub ring_capacity_samples: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct RecordingSessionStarted {
    pub session_id: u64,
    pub file_path: PathBuf,
    pub sample_rate: u32,
    pub channels: u16,
    pub input_latency_frames: i64,
}

pub struct RecordingFinalize {
    pub summary: RecordingSummary,
    pub peak: Option<crate::waveform_cache::WaveformPeak>,
}

pub struct RecordingSummary {
    pub session_id: u64,
    pub device_name: String,
    pub file_path: PathBuf,
    pub temp_path: PathBuf,
    pub sidecar_path: PathBuf,
    pub duration: Duration,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub input_latency_frames: i64,
    pub dropped_samples: usize,
    pub clipped: bool,
    pub peak: f32,
}

impl RecordingSummary {
    pub fn to_audio_source(&self, source_id: u64, latency_compensation_frames: i64) -> AudioSource {
        AudioSource {
            id: source_id,
            path: self.file_path.clone(),
            sample_rate: self.sample_rate,
            channels: self.channels,
            frames: self.frames,
            created_by_recording: true,
            latency_compensation_frames,
            peak_path: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RecordingMeters {
    pub peak: f32,
    pub clipped: bool,
    pub dropped_samples: usize,
    pub frames_written: u64,
}

struct ActiveRecording {
    session_id: u64,
    device_name: String,
    started: Instant,
    temp_path: PathBuf,
    sidecar_path: PathBuf,
    input_latency_frames: i64,
    ring: Arc<AudioRingBuffer>,
    stop: Arc<AtomicBool>,
    peak_bits: Arc<AtomicU32>,
    clipped: Arc<AtomicBool>,
    frames_written: Arc<AtomicU64>,
    _stream: cpal::Stream,
    writer: Option<thread::JoinHandle<Result<RecordingSummary, String>>>,
}

struct StartedInputStream {
    stream: cpal::Stream,
    device_name: String,
    sample_rate: u32,
    input_latency_frames: i64,
}

pub struct AudioRingBuffer {
    data: Vec<AtomicU32>,
    capacity: usize,
    write: AtomicUsize,
    read: AtomicUsize,
    dropped: AtomicUsize,
}

impl AudioRingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            data: (0..capacity.max(2)).map(|_| AtomicU32::new(0)).collect(),
            capacity: capacity.max(2),
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
        }
    }

    pub fn push(&self, sample: f32) {
        let write = self.write.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        let next = (write + 1) % self.capacity;
        if next == read {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.data[write].store(sample.to_bits(), Ordering::Relaxed);
        self.write.store(next, Ordering::Release);
    }

    pub fn pop(&self) -> Option<f32> {
        let read = self.read.load(Ordering::Relaxed);
        let write = self.write.load(Ordering::Acquire);
        if read == write {
            return None;
        }
        let sample = f32::from_bits(self.data[read].load(Ordering::Relaxed));
        self.read
            .store((read + 1) % self.capacity, Ordering::Release);
        Some(sample)
    }

    pub fn is_empty(&self) -> bool {
        self.read.load(Ordering::Acquire) == self.write.load(Ordering::Acquire)
    }

    pub fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }
}

trait InputSample {
    fn to_f32(self) -> f32;
}

impl InputSample for f32 {
    fn to_f32(self) -> f32 {
        self
    }
}

impl InputSample for i16 {
    fn to_f32(self) -> f32 {
        self as f32 / i16::MAX as f32
    }
}

impl InputSample for u16 {
    fn to_f32(self) -> f32 {
        (self as f32 / u16::MAX as f32) * 2.0 - 1.0
    }
}

fn build_input_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    input_channels: usize,
    out_channels: usize,
    input: InputConfig,
    ring: Arc<AudioRingBuffer>,
    peak_bits: Arc<AtomicU32>,
    clipped: Arc<AtomicBool>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + InputSample + Copy + Send + 'static,
{
    let err_fn = |err| eprintln!("[Recording] input stream error: {err}");
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                process_input(
                    data,
                    input_channels,
                    out_channels,
                    &input,
                    &ring,
                    &peak_bits,
                    &clipped,
                );
            },
            err_fn,
            None,
        )
        .map_err(|e| format!("build input stream failed: {e}"))
}

fn start_first_working_input_stream(
    host: &cpal::Host,
    requested: Option<&str>,
    input: InputConfig,
    out_channels: usize,
    ring: Arc<AudioRingBuffer>,
    peak_bits: Arc<AtomicU32>,
    clipped: Arc<AtomicBool>,
) -> Result<StartedInputStream, String> {
    let mut errors = Vec::new();
    for device in input_device_candidates(host, requested) {
        let device_name = device.name().unwrap_or_else(|_| "input".to_string());
        let supported_config = match device.default_input_config() {
            Ok(config) => config,
            Err(err) => {
                errors.push(format!("{device_name}: default config failed: {err}"));
                continue;
            }
        };
        let config: cpal::StreamConfig = supported_config.clone().config();
        let input_channels = config.channels.max(1) as usize;
        let sample_rate = config.sample_rate;
        let stream_result = match supported_config.sample_format() {
            cpal::SampleFormat::F32 => build_input_stream::<f32>(
                &device,
                &config,
                input_channels,
                out_channels,
                input.clone(),
                Arc::clone(&ring),
                Arc::clone(&peak_bits),
                Arc::clone(&clipped),
            ),
            cpal::SampleFormat::I16 => build_input_stream::<i16>(
                &device,
                &config,
                input_channels,
                out_channels,
                input.clone(),
                Arc::clone(&ring),
                Arc::clone(&peak_bits),
                Arc::clone(&clipped),
            ),
            cpal::SampleFormat::U16 => build_input_stream::<u16>(
                &device,
                &config,
                input_channels,
                out_channels,
                input.clone(),
                Arc::clone(&ring),
                Arc::clone(&peak_bits),
                Arc::clone(&clipped),
            ),
            other => Err(format!("unsupported input sample format: {other:?}")),
        };

        let stream = match stream_result {
            Ok(stream) => stream,
            Err(err) => {
                errors.push(format!("{device_name}: {err}"));
                continue;
            }
        };
        if let Err(err) = stream.play() {
            errors.push(format!("{device_name}: start input stream failed: {err}"));
            continue;
        }
        eprintln!("[Recording] using input device: {device_name}");
        return Ok(StartedInputStream {
            stream,
            device_name,
            sample_rate,
            input_latency_frames: estimate_input_latency_frames(&config),
        });
    }

    Err(format!(
        "no working input device found{}",
        if errors.is_empty() {
            String::new()
        } else {
            format!(": {}", errors.join("; "))
        }
    ))
}

/// PCM ALSA sans horloge (trou noir) : produit du silence à la vitesse du
/// CPU — ne doit jamais être choisi comme entrée d'enregistrement.
fn is_untimed_pcm_name(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    n == "null" || n.starts_with("null:") || n.starts_with("null.")
}

fn input_device_candidates(host: &cpal::Host, requested: Option<&str>) -> Vec<cpal::Device> {
    let mut result = Vec::new();
    let mut seen = Vec::<String>::new();
    let mut push_unique = |device: cpal::Device, result: &mut Vec<cpal::Device>| {
        let name = device.name().unwrap_or_else(|_| "input".to_string());
        if is_untimed_pcm_name(&name) {
            return;
        }
        if seen.iter().any(|seen_name| seen_name == &name) {
            return;
        }
        seen.push(name);
        result.push(device);
    };

    if let Some(requested) = requested {
        if let Ok(devices) = host.input_devices() {
            for device in devices {
                if device.name().ok().as_deref() == Some(requested) {
                    push_unique(device, &mut result);
                    break;
                }
            }
        }
    }

    let devices: Vec<cpal::Device> = host.input_devices().ok().into_iter().flatten().collect();

    if requested.is_none() {
        // Le device par défaut en premier : sous PipeWire il suit la source
        // par défaut (ex. casque BT après forçage HFP), puis les alias
        // pulse/pipewire, puis le reste du matériel.
        if let Some(default) = host.default_input_device() {
            push_unique(default, &mut result);
        }
        for device in &devices {
            let name = device.name().unwrap_or_default().to_lowercase();
            if name.contains("pulse") || name.contains("pipewire") || name.contains("default") {
                push_unique(device.clone(), &mut result);
            }
        }
    }

    if let Some(default) = host.default_input_device() {
        push_unique(default, &mut result);
    }

    for device in devices {
        push_unique(device, &mut result);
    }

    result
}

fn process_input<T: InputSample + Copy>(
    data: &[T],
    input_channels: usize,
    out_channels: usize,
    input: &InputConfig,
    ring: &AudioRingBuffer,
    peak_bits: &AtomicU32,
    clipped: &AtomicBool,
) {
    let mut peak = f32::from_bits(peak_bits.load(Ordering::Relaxed)) * 0.96;
    let channel_a = input.channel_a.min(input_channels.saturating_sub(1));
    let channel_b = input
        .channel_b
        .unwrap_or(channel_a.saturating_add(1))
        .min(input_channels.saturating_sub(1));

    for frame in data.chunks(input_channels.max(1)) {
        let a = frame
            .get(channel_a)
            .copied()
            .map(InputSample::to_f32)
            .unwrap_or(0.0);
        let b = frame
            .get(channel_b)
            .copied()
            .map(InputSample::to_f32)
            .unwrap_or(a);
        let values: [f32; 2] = match input.channel_mode {
            InputChannelMode::Mono => [a, a],
            InputChannelMode::Stereo => [a, b],
        };
        for sample in values.iter().take(out_channels.max(1)) {
            let sample = sample.clamp(-1.0, 1.0);
            peak = peak.max(sample.abs());
            if sample.abs() >= 0.999 {
                clipped.store(true, Ordering::Relaxed);
            }
            ring.push(sample);
        }
    }

    peak_bits.store(peak.to_bits(), Ordering::Relaxed);
}

fn estimate_input_latency_frames(config: &cpal::StreamConfig) -> i64 {
    let buffer_frames = match config.buffer_size {
        cpal::BufferSize::Fixed(frames) => frames.max(1) as i64,
        cpal::BufferSize::Default => 256,
    };
    // Input callback data has already spent roughly one hardware buffer in flight.
    buffer_frames.clamp(0, 16_384)
}

fn finalize_active_recording(mut active: ActiveRecording) -> Result<RecordingSummary, String> {
    active.stop.store(true, Ordering::Release);
    drop(active._stream);
    let writer = active
        .writer
        .take()
        .ok_or_else(|| "recording writer missing".to_string())?;
    let mut summary = writer
        .join()
        .map_err(|_| "recording writer thread panicked".to_string())??;
    summary.session_id = active.session_id;
    summary.device_name = active.device_name;
    summary.duration = active.started.elapsed();
    summary.input_latency_frames = active.input_latency_frames;
    summary.dropped_samples = active.ring.dropped();
    summary.clipped = active.clipped.load(Ordering::Relaxed);
    summary.peak = f32::from_bits(active.peak_bits.load(Ordering::Relaxed));
    summary.temp_path = active.temp_path;
    summary.sidecar_path = active.sidecar_path;
    Ok(summary)
}

fn writer_thread(
    ring: Arc<AudioRingBuffer>,
    stop: Arc<AtomicBool>,
    frames_written: Arc<AtomicU64>,
    temp_path: PathBuf,
    final_path: PathBuf,
    sidecar_path: PathBuf,
    sample_rate: u32,
    channels: u16,
) -> Result<RecordingSummary, String> {
    let spec = hound::WavSpec {
        channels: channels.max(1),
        sample_rate: sample_rate.max(1),
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(&temp_path, spec)
        .map_err(|e| format!("create recording wav {:?} failed: {e}", temp_path))?;
    let mut samples_written = 0_u64;

    while !stop.load(Ordering::Acquire) || !ring.is_empty() {
        let mut wrote = false;
        while let Some(sample) = ring.pop() {
            writer
                .write_sample(sample.clamp(-1.0, 1.0))
                .map_err(|e| format!("write recording sample failed: {e}"))?;
            samples_written = samples_written.saturating_add(1);
            frames_written.store(samples_written / channels.max(1) as u64, Ordering::Relaxed);
            wrote = true;
        }
        if !wrote {
            thread::sleep(WRITER_IDLE_SLEEP);
        }
    }

    writer
        .finalize()
        .map_err(|e| format!("finalize recording failed: {e}"))?;
    let frames = samples_written / channels.max(1) as u64;
    if frames == 0 {
        let _ = std::fs::remove_file(&temp_path);
        let _ = std::fs::remove_file(&sidecar_path);
        return Err(
            "recording captured 0 frames (check input device / input level)".to_string(),
        );
    }
    std::fs::rename(&temp_path, &final_path)
        .map_err(|e| format!("finalize recording rename failed: {e}"))?;
    let _ = std::fs::remove_file(&sidecar_path);

    Ok(RecordingSummary {
        session_id: 0,
        device_name: String::new(),
        file_path: final_path,
        temp_path,
        sidecar_path,
        duration: Duration::ZERO,
        sample_rate,
        channels,
        frames,
        input_latency_frames: 0,
        dropped_samples: ring.dropped(),
        clipped: false,
        peak: 0.0,
    })
}

fn write_sidecar(
    sidecar_path: &Path,
    temp_path: &Path,
    final_path: &Path,
    sample_rate: u32,
    channels: u16,
) -> Result<(), String> {
    let payload = serde_json::json!({
        "temp_path": temp_path,
        "final_path": final_path,
        "sample_rate": sample_rate,
        "channels": channels,
        "format": "wav-float32",
    });
    std::fs::write(sidecar_path, payload.to_string())
        .map_err(|e| format!("write recording sidecar {:?} failed: {e}", sidecar_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_preserves_order() {
        let ring = AudioRingBuffer::new(8);
        ring.push(0.25);
        ring.push(0.5);
        assert_eq!(ring.pop(), Some(0.25));
        assert_eq!(ring.pop(), Some(0.5));
        assert_eq!(ring.pop(), None);
    }
}
