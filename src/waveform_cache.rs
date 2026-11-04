use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::project_audio::AudioSource;

const PEAK_VERSION: u32 = 1;
const DEFAULT_PEAK_POINTS: usize = 2048;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaveformPeak {
    pub version: u32,
    pub source_path: PathBuf,
    pub source_modified_unix: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub points_per_channel: usize,
    pub peaks: Vec<PeakPoint>,
    pub transients: Vec<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PeakPoint {
    pub min: f32,
    pub max: f32,
    pub rms: f32,
}

impl Default for PeakPoint {
    fn default() -> Self {
        Self {
            min: 0.0,
            max: 0.0,
            rms: 0.0,
        }
    }
}

impl WaveformPeak {
    pub fn display_envelope(&self) -> Vec<f32> {
        self.peaks
            .iter()
            .map(|point| point.max.abs().max(point.min.abs()).max(point.rms))
            .collect()
    }
}

pub fn peak_path_for_audio(path: &Path) -> PathBuf {
    let mut peak = path.to_path_buf();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("audio");
    peak.set_file_name(format!("{file_name}.peak"));
    peak
}

pub fn load_or_build_peak(path: &Path) -> Result<WaveformPeak, String> {
    let peak_path = peak_path_for_audio(path);
    if let Ok(contents) = fs::read_to_string(&peak_path) {
        if let Ok(peak) = serde_json::from_str::<WaveformPeak>(&contents) {
            if peak_matches_source(&peak, path) {
                return Ok(peak);
            }
        }
    }
    let peak = build_peak_file(path, DEFAULT_PEAK_POINTS)?;
    save_peak(&peak_path, &peak)?;
    Ok(peak)
}

pub fn build_peak_for_source(source: &AudioSource) -> Result<WaveformPeak, String> {
    build_peak_file(&source.path, DEFAULT_PEAK_POINTS)
}

pub fn save_peak(path: &Path, peak: &WaveformPeak) -> Result<(), String> {
    let json = serde_json::to_string(peak).map_err(|e| format!("encode peak failed: {e}"))?;
    fs::write(path, json).map_err(|e| format!("write peak {:?} failed: {e}", path))
}

pub fn build_peak_file(path: &Path, points: usize) -> Result<WaveformPeak, String> {
    let metadata =
        fs::metadata(path).map_err(|e| format!("read metadata {:?} failed: {e}", path))?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);

    let mut reader =
        hound::WavReader::open(path).map_err(|e| format!("open wav {:?} failed: {e}", path))?;
    let spec = reader.spec();
    let channels = spec.channels.max(1) as usize;
    let samples = read_wav_samples(&mut reader)?;
    let frames = samples.len() / channels;
    let target_points = points.max(1).min(frames.max(1));
    let mut peaks = Vec::with_capacity(target_points);

    for i in 0..target_points {
        let start = i * frames / target_points;
        let end = ((i + 1) * frames / target_points).min(frames);
        peaks.push(peak_range(&samples, channels, start, end));
    }

    let transients = detect_transients(&samples, channels, spec.sample_rate, target_points);

    Ok(WaveformPeak {
        version: PEAK_VERSION,
        source_path: path.to_path_buf(),
        source_modified_unix: modified,
        sample_rate: spec.sample_rate,
        channels: spec.channels,
        frames: frames as u64,
        points_per_channel: target_points,
        peaks,
        transients,
    })
}

fn peak_matches_source(peak: &WaveformPeak, path: &Path) -> bool {
    if peak.version != PEAK_VERSION || peak.source_path != path {
        return false;
    }
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    modified == peak.source_modified_unix
}

fn read_wav_samples<R: std::io::Read>(
    reader: &mut hound::WavReader<R>,
) -> Result<Vec<f32>, String> {
    let spec = reader.spec();
    match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .map(|sample| sample.map_err(|e| format!("read float sample failed: {e}")))
            .collect(),
        hound::SampleFormat::Int => {
            let scale = if spec.bits_per_sample == 0 {
                1.0
            } else {
                ((1_i64 << (spec.bits_per_sample.saturating_sub(1) as u32)) - 1) as f32
            };
            reader
                .samples::<i32>()
                .map(|sample| {
                    sample
                        .map(|v| (v as f32 / scale).clamp(-1.0, 1.0))
                        .map_err(|e| format!("read int sample failed: {e}"))
                })
                .collect()
        }
    }
}

fn peak_range(samples: &[f32], channels: usize, start: usize, end: usize) -> PeakPoint {
    if start >= end || samples.is_empty() {
        return PeakPoint::default();
    }
    let mut min = 1.0_f32;
    let mut max = -1.0_f32;
    let mut sum_sq = 0.0_f32;
    let mut count = 0_usize;

    for frame in start..end {
        for ch in 0..channels {
            let Some(sample) = samples.get(frame * channels + ch).copied() else {
                continue;
            };
            min = min.min(sample);
            max = max.max(sample);
            sum_sq += sample * sample;
            count += 1;
        }
    }

    if count == 0 {
        PeakPoint::default()
    } else {
        PeakPoint {
            min,
            max,
            rms: (sum_sq / count as f32).sqrt(),
        }
    }
}

fn detect_transients(
    samples: &[f32],
    channels: usize,
    sample_rate: u32,
    points: usize,
) -> Vec<u64> {
    if samples.is_empty() || channels == 0 || sample_rate == 0 {
        return Vec::new();
    }
    let frames = samples.len() / channels;
    let window = (sample_rate as usize / 200).max(32);
    let mut transients = Vec::new();
    let mut prev_energy = 0.0_f32;
    let mut last_transient = 0_usize;

    for frame in (0..frames).step_by(window) {
        let end = (frame + window).min(frames);
        let point = peak_range(samples, channels, frame, end);
        let energy = point.rms;
        let onset = energy - prev_energy;
        let enough_spacing = frame.saturating_sub(last_transient) > sample_rate as usize / 20;
        if onset > 0.12 && energy > 0.08 && enough_spacing {
            transients.push(frame as u64);
            last_transient = frame;
        }
        prev_energy = prev_energy * 0.85 + energy * 0.15;
    }

    if transients.len() > points {
        transients.truncate(points);
    }
    transients
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_path_keeps_audio_name() {
        assert_eq!(
            peak_path_for_audio(Path::new("/tmp/take.wav")),
            PathBuf::from("/tmp/take.wav.peak")
        );
    }
}
