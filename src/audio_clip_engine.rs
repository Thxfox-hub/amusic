use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::audio_dsp::{default_backend, StretchPitchBackend};
use crate::mixer_dsp::{
    apply_channel_sample, apply_master_options, db_to_gain, BuiltInFx, InsertProcessor,
};
use crate::project_audio::{
    AudioClip, AudioSource, AudioSourceId, AudioTrack, AutomationLane, AutomationMode,
    AutomationParameter, ExportBitDepth, FadeCurve, MasterOptions, MixerChannel, MixerChannelId,
    OutputTarget, ProjectAudioState, StretchMode, WarpMarker,
};

#[derive(Debug, Clone)]
pub struct RenderConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub start_cycle: f64,
    pub end_cycle: f64,
    pub cycles_per_second: f64,
    pub realtime_preview: bool,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            channels: 2,
            start_cycle: 0.0,
            end_cycle: 4.0,
            cycles_per_second: 1.0,
            realtime_preview: true,
        }
    }
}

pub struct ClipRenderEngine {
    backend: Arc<dyn StretchPitchBackend>,
}

impl Default for ClipRenderEngine {
    fn default() -> Self {
        Self {
            backend: Arc::new(default_backend()),
        }
    }
}

impl ClipRenderEngine {
    pub fn new(backend: Arc<dyn StretchPitchBackend>) -> Self {
        Self { backend }
    }

    pub fn render_project(
        &self,
        project: &ProjectAudioState,
        config: &RenderConfig,
    ) -> Result<Vec<f32>, String> {
        let out_channels = config.channels.max(1) as usize;
        let frames = render_frames(config);
        let mut channel_buffers = HashMap::<MixerChannelId, Vec<f32>>::new();
        let mut source_cache = HashMap::<AudioSourceId, LoadedAudio>::new();

        for track in &project.tracks {
            let audible = project.effective_audible(track.channel.id);
            if !audible.audible {
                continue;
            }
            let mut input = vec![0.0_f32; frames * out_channels];
            self.render_track(
                track,
                &project.pool.sources,
                &mut source_cache,
                config,
                &mut input,
            )?;
            let pre_fader = input.clone();
            let post_fader = apply_channel_strip(
                &input,
                out_channels,
                &track.channel,
                config.start_cycle,
                config.cycles_per_second,
                config.sample_rate,
                None,
            );
            for (send_index, send) in track
                .channel
                .sends
                .iter()
                .enumerate()
                .filter(|(_, send)| send.enabled)
            {
                let source = if send.pre_fader {
                    &pre_fader
                } else {
                    &post_fader
                };
                mix_send_automated(
                    channel_buffers
                        .entry(send.target_bus_id)
                        .or_insert_with(|| vec![0.0_f32; frames * out_channels]),
                    source,
                    out_channels,
                    &track.channel.automation,
                    send.level_db,
                    config.start_cycle,
                    config.cycles_per_second,
                    config.sample_rate,
                    send_index,
                );
            }
            match track.channel.output_target {
                OutputTarget::Master => mix_scaled(
                    channel_buffers
                        .entry(project.master.id)
                        .or_insert_with(|| vec![0.0_f32; frames * out_channels]),
                    &post_fader,
                    1.0,
                ),
                OutputTarget::Bus(bus_id) => mix_scaled(
                    channel_buffers
                        .entry(bus_id)
                        .or_insert_with(|| vec![0.0_f32; frames * out_channels]),
                    &post_fader,
                    1.0,
                ),
                OutputTarget::None => {}
            }
        }

        let order = project
            .routing_order()
            .map_err(|err| format!("routing graph invalid: {err:?}"))?;
        for channel_id in order {
            if channel_id == project.master.id
                || project
                    .tracks
                    .iter()
                    .any(|track| track.channel.id == channel_id)
            {
                continue;
            }
            let Some(channel) = project.mixer_channel(channel_id) else {
                continue;
            };
            let audible = project.effective_audible(channel_id);
            if !audible.audible {
                channel_buffers.remove(&channel_id);
                continue;
            }
            let input = channel_buffers
                .remove(&channel_id)
                .unwrap_or_else(|| vec![0.0_f32; frames * out_channels]);
            let pre_fader = input.clone();
            let post_fader = apply_channel_strip(
                &input,
                out_channels,
                channel,
                config.start_cycle,
                config.cycles_per_second,
                config.sample_rate,
                None,
            );
            for (send_index, send) in channel
                .sends
                .iter()
                .enumerate()
                .filter(|(_, send)| send.enabled)
            {
                let source = if send.pre_fader {
                    &pre_fader
                } else {
                    &post_fader
                };
                mix_send_automated(
                    channel_buffers
                        .entry(send.target_bus_id)
                        .or_insert_with(|| vec![0.0_f32; frames * out_channels]),
                    source,
                    out_channels,
                    &channel.automation,
                    send.level_db,
                    config.start_cycle,
                    config.cycles_per_second,
                    config.sample_rate,
                    send_index,
                );
            }
            match channel.output_target {
                OutputTarget::Master => mix_scaled(
                    channel_buffers
                        .entry(project.master.id)
                        .or_insert_with(|| vec![0.0_f32; frames * out_channels]),
                    &post_fader,
                    1.0,
                ),
                OutputTarget::Bus(bus_id) => mix_scaled(
                    channel_buffers
                        .entry(bus_id)
                        .or_insert_with(|| vec![0.0_f32; frames * out_channels]),
                    &post_fader,
                    1.0,
                ),
                OutputTarget::None => {}
            }
        }

        let master_in = channel_buffers
            .remove(&project.master.id)
            .unwrap_or_else(|| vec![0.0_f32; frames * out_channels]);
        Ok(apply_channel_strip(
            &master_in,
            out_channels,
            &project.master,
            config.start_cycle,
            config.cycles_per_second,
            config.sample_rate,
            Some(project.master_options),
        ))
    }

    fn render_track(
        &self,
        track: &AudioTrack,
        sources: &HashMap<AudioSourceId, AudioSource>,
        source_cache: &mut HashMap<AudioSourceId, LoadedAudio>,
        config: &RenderConfig,
        output: &mut [f32],
    ) -> Result<(), String> {
        for clip in &track.clips {
            if clip.muted
                || clip.end() <= config.start_cycle
                || clip.timeline_position >= config.end_cycle
            {
                continue;
            }
            let source = sources
                .get(&clip.source_file_id)
                .ok_or_else(|| format!("missing audio source {}", clip.source_file_id))?;
            if !source_cache.contains_key(&source.id) {
                source_cache.insert(source.id, load_audio_file(&source.path)?);
            }
            let loaded = source_cache
                .get(&source.id)
                .ok_or_else(|| format!("source {} failed to load", source.id))?;
            self.mix_clip(clip, loaded, config, output);
        }
        Ok(())
    }

    fn mix_clip(
        &self,
        clip: &AudioClip,
        source: &LoadedAudio,
        config: &RenderConfig,
        output: &mut [f32],
    ) {
        let out_channels = config.channels.max(1) as usize;
        let source_channels = source.channels.max(1) as usize;
        let cps = config.cycles_per_second.max(0.000_001);
        let out_sr = config.sample_rate.max(1) as f64;
        let clip_start_sec = clip.timeline_position / cps;
        let render_start_sec = config.start_cycle / cps;
        let clip_offset_frames = ((clip_start_sec - render_start_sec) * out_sr).round() as isize;
        let clip_len_frames = ((clip.length / cps) * out_sr).ceil().max(1.0) as usize;
        let source_start_frames = (clip.source_offset * source.sample_rate as f64).round() as usize;
        let source_needed_frames = ((clip.length * clip.stretch.effective_source_rate() / cps)
            * source.sample_rate as f64)
            .ceil()
            .max(1.0) as usize;
        let source_total = source.frames();
        let source_end = (source_start_frames + source_needed_frames).min(source_total);
        if source_start_frames >= source_end {
            return;
        }

        let mut source_slice =
            Vec::with_capacity((source_end - source_start_frames) * source_channels);
        source_slice.extend_from_slice(
            &source.samples[source_start_frames * source_channels..source_end * source_channels],
        );

        let use_segment_warp =
            matches!(clip.stretch.mode, StretchMode::Musical) && !clip.warp_markers.is_empty();
        let warp_segments = if use_segment_warp {
            Some(build_warp_segments(
                clip,
                &clip.warp_markers,
                clip_len_frames,
                source_needed_frames,
                source.sample_rate as f64,
                cps,
                source_total.saturating_sub(source_start_frames),
            ))
        } else {
            None
        };

        let processed = if use_segment_warp {
            Vec::new()
        } else if config.realtime_preview {
            self.backend.process_preview(
                &source_slice,
                source_channels,
                source.sample_rate,
                &clip.stretch,
                &clip.pitch,
            )
        } else {
            self.backend.process_offline_hq(
                &source_slice,
                source_channels,
                source.sample_rate,
                &clip.stretch,
                &clip.pitch,
            )
        };

        let mut segment_index = 0usize;
        for local_frame in 0..clip_len_frames {
            let out_frame = clip_offset_frames + local_frame as isize;
            if out_frame < 0 {
                continue;
            }
            let out_frame = out_frame as usize;
            if out_frame * out_channels >= output.len() {
                break;
            }
            let mono = if let Some(segments) = &warp_segments {
                let source_local_frame =
                    warp_source_frame(local_frame, segments, &mut segment_index);
                sample_mono_linear(
                    &source_slice,
                    source_channels,
                    source_slice.len() / source_channels,
                    source_local_frame,
                )
            } else {
                let processed_frame = map_frame(
                    local_frame,
                    clip_len_frames,
                    processed.len() / source_channels,
                );
                frame_to_mono(&processed, source_channels, processed_frame)
            };
            let env = fade_envelope(clip, local_frame, clip_len_frames);
            let gain = clip.gain * env;
            let pan = clip.pan.clamp(-1.0, 1.0);
            let left_gain = if out_channels == 1 {
                1.0
            } else {
                (1.0 - pan) * 0.5
            };
            let right_gain = if out_channels == 1 {
                1.0
            } else {
                (1.0 + pan) * 0.5
            };

            if out_channels == 1 {
                output[out_frame] += mono * gain;
            } else {
                let out_idx = out_frame * out_channels;
                output[out_idx] += mono * gain * left_gain;
                output[out_idx + 1] += mono * gain * right_gain;
                for ch in 2..out_channels {
                    output[out_idx + ch] += mono * gain * 0.5;
                }
            }
        }
    }
}

pub fn render_frames(config: &RenderConfig) -> usize {
    let seconds = ((config.end_cycle - config.start_cycle).max(0.0)
        / config.cycles_per_second.max(0.000_001))
    .max(0.0);
    (seconds * config.sample_rate.max(1) as f64).ceil() as usize
}

fn apply_channel_strip(
    input: &[f32],
    channels: usize,
    channel: &MixerChannel,
    start_cycle: f64,
    cycles_per_second: f64,
    sample_rate: u32,
    master_options: Option<MasterOptions>,
) -> Vec<f32> {
    let mut out = input.to_vec();
    let fx: Vec<BuiltInFx> = channel
        .inserts
        .iter()
        .filter_map(BuiltInFx::from_slot)
        .collect();
    let mut processor = InsertProcessor::new(&fx);
    let channels = channels.max(1);
    for (frame_index, frame) in out.chunks_mut(channels).enumerate() {
        let cycle = frame_cycle(
            start_cycle,
            frame_index,
            sample_rate,
            cycles_per_second.max(0.000_001),
        );
        if channel_mute_at(channel, cycle) {
            for sample in frame.iter_mut() {
                *sample = 0.0;
            }
            continue;
        }
        let volume_db = channel_volume_at(channel, cycle);
        let pan = channel_pan_at(channel, cycle);
        if channels == 1 {
            let mut left = frame[0];
            let mut right = frame[0];
            processor.process_pair(&fx, sample_rate as f32, &mut left, &mut right);
            apply_channel_sample(&mut left, &mut right, volume_db, pan, channel.pan_law);
            if let Some(options) = master_options {
                apply_master_options(&mut left, &mut right, options);
            }
            frame[0] = (left + right) * 0.5;
        } else {
            let mut left = frame[0];
            let mut right = frame[1];
            processor.process_pair(&fx, sample_rate as f32, &mut left, &mut right);
            apply_channel_sample(&mut left, &mut right, volume_db, pan, channel.pan_law);
            if let Some(options) = master_options {
                apply_master_options(&mut left, &mut right, options);
            }
            frame[0] = left;
            frame[1] = right;
            for sample in frame.iter_mut().skip(2) {
                *sample *= db_to_gain(volume_db);
            }
        }
    }
    out
}

fn mix_send_automated(
    dst: &mut [f32],
    src: &[f32],
    channels: usize,
    automation: &[AutomationLane],
    base_level_db: f32,
    start_cycle: f64,
    cycles_per_second: f64,
    sample_rate: u32,
    send_index: usize,
) {
    let channels = channels.max(1);
    for (frame_index, (dst_frame, src_frame)) in dst
        .chunks_mut(channels)
        .zip(src.chunks(channels))
        .enumerate()
    {
        let cycle = frame_cycle(
            start_cycle,
            frame_index,
            sample_rate,
            cycles_per_second.max(0.000_001),
        );
        let gain =
            db_to_gain(send_level_at(automation, send_index, cycle).unwrap_or(base_level_db));
        for (dst_sample, src_sample) in dst_frame.iter_mut().zip(src_frame.iter()) {
            *dst_sample += *src_sample * gain;
        }
    }
}

fn frame_cycle(start_cycle: f64, frame_index: usize, sample_rate: u32, cps: f64) -> f64 {
    start_cycle + frame_index as f64 * cps / sample_rate.max(1) as f64
}

fn channel_volume_at(channel: &MixerChannel, cycle: f64) -> f32 {
    automation_value_at(&channel.automation, &AutomationParameter::Volume, cycle)
        .unwrap_or(channel.volume_db)
}

fn channel_pan_at(channel: &MixerChannel, cycle: f64) -> f32 {
    automation_value_at(&channel.automation, &AutomationParameter::Pan, cycle)
        .unwrap_or(channel.pan)
}

fn channel_mute_at(channel: &MixerChannel, cycle: f64) -> bool {
    channel.mute
        || automation_value_at(&channel.automation, &AutomationParameter::Mute, cycle)
            .map(|value| value >= 0.5)
            .unwrap_or(false)
}

fn send_level_at(automation: &[AutomationLane], send_index: usize, cycle: f64) -> Option<f32> {
    automation_value_at(
        automation,
        &AutomationParameter::SendLevel { send_index },
        cycle,
    )
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

fn mix_scaled(dst: &mut [f32], src: &[f32], gain: f32) {
    for (dst, src) in dst.iter_mut().zip(src.iter()) {
        *dst += *src * gain;
    }
}

pub fn fade_envelope(clip: &AudioClip, frame: usize, total_frames: usize) -> f32 {
    if total_frames == 0 {
        return 0.0;
    }
    let total = total_frames as f64;
    let pos = frame as f64 / total;
    let fade_in = if clip.fade_in.length <= 0.0 {
        1.0
    } else {
        let fade_ratio = (clip.fade_in.length / clip.length.max(0.000_001)).clamp(0.0, 1.0);
        if pos >= fade_ratio {
            1.0
        } else {
            apply_fade_curve((pos / fade_ratio).clamp(0.0, 1.0), clip.fade_in.curve)
        }
    };
    let fade_out = if clip.fade_out.length <= 0.0 {
        1.0
    } else {
        let fade_ratio = (clip.fade_out.length / clip.length.max(0.000_001)).clamp(0.0, 1.0);
        let out_start = 1.0 - fade_ratio;
        if pos <= out_start {
            1.0
        } else {
            apply_fade_curve(
                ((1.0 - pos) / fade_ratio).clamp(0.0, 1.0),
                clip.fade_out.curve,
            )
        }
    };
    (fade_in * fade_out).clamp(0.0, 1.0) as f32
}

pub fn apply_fade_curve(x: f64, curve: FadeCurve) -> f64 {
    let x = x.clamp(0.0, 1.0);
    match curve {
        FadeCurve::Linear => x,
        FadeCurve::EqualPower => (x * std::f64::consts::FRAC_PI_2).sin(),
        FadeCurve::Exponential => x * x,
        FadeCurve::SCurve => x * x * (3.0 - 2.0 * x),
    }
}

pub fn write_wav_float(
    path: &Path,
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: channels.max(1),
        sample_rate: sample_rate.max(1),
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|e| format!("create wav {:?} failed: {e}", path))?;
    for sample in samples {
        writer
            .write_sample(sample.clamp(-1.0, 1.0))
            .map_err(|e| format!("write wav sample failed: {e}"))?;
    }
    writer
        .finalize()
        .map_err(|e| format!("finalize wav failed: {e}"))
}

pub fn write_wav_with_options(
    path: &Path,
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
    options: MasterOptions,
) -> Result<(), String> {
    match options.export_bit_depth {
        ExportBitDepth::Float32 => write_wav_float(path, samples, sample_rate, channels),
        ExportBitDepth::Int24 => write_wav_int(
            path,
            samples,
            sample_rate,
            channels,
            24,
            options.dither_enabled,
        ),
        ExportBitDepth::Int16 => write_wav_int(
            path,
            samples,
            sample_rate,
            channels,
            16,
            options.dither_enabled,
        ),
    }
}

fn write_wav_int(
    path: &Path,
    samples: &[f32],
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    dither: bool,
) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: channels.max(1),
        sample_rate: sample_rate.max(1),
        bits_per_sample,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|e| format!("create wav {:?} failed: {e}", path))?;
    let peak = ((1_i64 << (bits_per_sample - 1)) - 1) as f32;
    let lsb = 1.0 / peak.max(1.0);
    let mut noise = DitherNoise::default();
    for sample in samples {
        let dither_noise = if dither { noise.tpdf() * lsb } else { 0.0 };
        let quantized = ((sample + dither_noise).clamp(-1.0, 1.0) * peak).round() as i32;
        writer
            .write_sample(quantized)
            .map_err(|e| format!("write wav sample failed: {e}"))?;
    }
    writer
        .finalize()
        .map_err(|e| format!("finalize wav failed: {e}"))
}

struct DitherNoise {
    state: u32,
}

impl Default for DitherNoise {
    fn default() -> Self {
        Self { state: 0x1234_abcd }
    }
}

impl DitherNoise {
    fn tpdf(&mut self) -> f32 {
        self.uniform() - self.uniform()
    }

    fn uniform(&mut self) -> f32 {
        self.state = self
            .state
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        ((self.state >> 8) as f32 / 0x00ff_ffff as f32).clamp(0.0, 1.0)
    }
}

#[derive(Clone)]
pub struct LoadedAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
}

impl LoadedAudio {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }
}

pub fn load_audio_file(path: &Path) -> Result<LoadedAudio, String> {
    let mut reader =
        hound::WavReader::open(path).map_err(|e| format!("open wav {:?} failed: {e}", path))?;
    let spec = reader.spec();
    let samples = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .map(|sample| sample.map_err(|e| format!("read float sample failed: {e}")))
            .collect::<Result<Vec<_>, _>>()?,
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
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    Ok(LoadedAudio {
        samples,
        sample_rate: spec.sample_rate,
        channels: spec.channels.max(1),
    })
}

fn map_frame(out_frame: usize, out_frames: usize, input_frames: usize) -> usize {
    if input_frames <= 1 || out_frames <= 1 {
        return 0;
    }
    let ratio = out_frame as f64 / (out_frames - 1) as f64;
    (ratio * (input_frames - 1) as f64).round() as usize
}

fn frame_to_mono(samples: &[f32], channels: usize, frame: usize) -> f32 {
    if channels == 0 {
        return 0.0;
    }
    let mut sum = 0.0;
    for ch in 0..channels {
        sum += samples.get(frame * channels + ch).copied().unwrap_or(0.0);
    }
    sum / channels as f32
}

#[derive(Debug, Clone, Copy)]
struct WarpSegment {
    t0: f64,
    t1: f64,
    s0: f64,
    s1: f64,
}

fn build_warp_segments(
    clip: &AudioClip,
    markers: &[WarpMarker],
    clip_len_frames: usize,
    default_source_frames: usize,
    source_sample_rate: f64,
    cycles_per_second: f64,
    source_available_frames: usize,
) -> Vec<WarpSegment> {
    let clip_max = clip_len_frames.saturating_sub(1) as f64;
    let source_max = source_available_frames.saturating_sub(1) as f64;
    let default_source_max = default_source_frames.saturating_sub(1) as f64;
    let clip_length_sec = (clip.length / cycles_per_second.max(0.000_001)).max(0.000_001);

    let mut points: Vec<(f64, f64)> = vec![(0.0, 0.0)];
    for marker in markers {
        let local_cycles = marker.timeline_position - clip.timeline_position;
        let local_seconds = local_cycles / cycles_per_second.max(0.000_001);
        let t = ((local_seconds / clip_length_sec) * clip_len_frames.max(1) as f64)
            .clamp(0.0, clip_max);
        let s = (marker.source_position * source_sample_rate).clamp(0.0, source_max);
        points.push((t, s));
    }
    points.push((clip_max, default_source_max.min(source_max)));
    points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut dedup = Vec::with_capacity(points.len());
    for point in points {
        if dedup
            .last()
            .map(|last: &(f64, f64)| (last.0 - point.0).abs() < 0.5)
            .unwrap_or(false)
        {
            continue;
        }
        dedup.push(point);
    }
    if dedup.len() < 2 {
        return vec![WarpSegment {
            t0: 0.0,
            t1: clip_max.max(1.0),
            s0: 0.0,
            s1: default_source_max.min(source_max).max(1.0),
        }];
    }

    let mut segments = Vec::with_capacity(dedup.len().saturating_sub(1));
    for pair in dedup.windows(2) {
        let (t0, s0) = pair[0];
        let (t1, s1) = pair[1];
        if t1 <= t0 {
            continue;
        }
        segments.push(WarpSegment { t0, t1, s0, s1 });
    }
    if segments.is_empty() {
        segments.push(WarpSegment {
            t0: 0.0,
            t1: clip_max.max(1.0),
            s0: 0.0,
            s1: default_source_max.min(source_max).max(1.0),
        });
    }
    segments
}

fn warp_source_frame(
    local_frame: usize,
    segments: &[WarpSegment],
    segment_index: &mut usize,
) -> f64 {
    if segments.is_empty() {
        return 0.0;
    }
    let t = local_frame as f64;
    while *segment_index + 1 < segments.len() && t > segments[*segment_index].t1 {
        *segment_index += 1;
    }
    let seg = segments[*segment_index];
    let seg_len = (seg.t1 - seg.t0).max(0.000_001);
    let ratio = ((t - seg.t0) / seg_len).clamp(0.0, 1.0);
    seg.s0 + (seg.s1 - seg.s0) * ratio
}

fn sample_mono_linear(samples: &[f32], channels: usize, frames: usize, frame: f64) -> f32 {
    if channels == 0 || frames == 0 {
        return 0.0;
    }
    let frame = frame.clamp(0.0, frames.saturating_sub(1) as f64);
    let base = frame.floor() as usize;
    let next = (base + 1).min(frames.saturating_sub(1));
    let frac = (frame - base as f64) as f32;

    let mut a = 0.0f32;
    let mut b = 0.0f32;
    for ch in 0..channels {
        a += samples.get(base * channels + ch).copied().unwrap_or(0.0);
        b += samples.get(next * channels + ch).copied().unwrap_or(0.0);
    }
    let inv = 1.0 / channels as f32;
    let a = a * inv;
    let b = b * inv;
    a + (b - a) * frac
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_audio::{
        AudioClip, Fade, FadeCurve, PitchParams, StretchMode, StretchParams, WarpMarker,
    };

    #[test]
    fn equal_power_fade_reaches_ends() {
        assert!((apply_fade_curve(0.0, FadeCurve::EqualPower) - 0.0).abs() < 0.000_001);
        assert!((apply_fade_curve(1.0, FadeCurve::EqualPower) - 1.0).abs() < 0.000_001);
    }

    #[test]
    fn clip_fade_envelope_drops_at_edges() {
        let clip = AudioClip {
            id: 1,
            source_file_id: 1,
            timeline_position: 0.0,
            source_offset: 0.0,
            length: 10.0,
            gain: 1.0,
            pan: 0.0,
            fade_in: Fade {
                length: 1.0,
                curve: FadeCurve::Linear,
            },
            fade_out: Fade {
                length: 1.0,
                curve: FadeCurve::Linear,
            },
            stretch: StretchParams::default(),
            pitch: PitchParams::default(),
            warp_markers: Vec::new(),
            take_lanes: Vec::new(),
            selected_take: None,
            muted: false,
            name: None,
        };
        assert!(fade_envelope(&clip, 0, 100) < 0.1);
        assert!(fade_envelope(&clip, 50, 100) > 0.9);
        assert!(fade_envelope(&clip, 99, 100) < 0.2);
    }

    #[test]
    fn warp_segments_map_piecewise_source_positions() {
        let clip = AudioClip {
            id: 1,
            source_file_id: 1,
            timeline_position: 10.0,
            source_offset: 0.0,
            length: 4.0,
            gain: 1.0,
            pan: 0.0,
            fade_in: Fade::default(),
            fade_out: Fade::default(),
            stretch: StretchParams {
                mode: StretchMode::Musical,
                ratio: 1.0,
                ..Default::default()
            },
            pitch: PitchParams::default(),
            warp_markers: vec![
                WarpMarker {
                    timeline_position: 11.0,
                    source_position: 0.5,
                },
                WarpMarker {
                    timeline_position: 13.0,
                    source_position: 2.0,
                },
            ],
            take_lanes: Vec::new(),
            selected_take: None,
            muted: false,
            name: None,
        };
        let segments = build_warp_segments(&clip, &clip.warp_markers, 400, 400, 100.0, 1.0, 1_000);
        let mut seg_idx = 0usize;
        let first = warp_source_frame(50, &segments, &mut seg_idx);
        let middle = warp_source_frame(200, &segments, &mut seg_idx);
        let late = warp_source_frame(350, &segments, &mut seg_idx);
        assert!(first < middle);
        assert!(middle < late);
    }
}
