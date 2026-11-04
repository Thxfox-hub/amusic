//! Rendu « baked » des prises voix vers la banque de samples.
//!
//! Le clip sur la timeline garde pitch/fades en paramètres non destructifs ;
//! la copie exportée dans `sample_folder/<banque>/` est rendue avec la chaîne
//! complète (passe-haut → pitch → compresseur → crescendo → absorption → limiteur)
//! pour être directement jouable via `s("nom").bank("banque")`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::audio_clip_engine::{
    apply_fade_curve, load_audio_file, write_wav_float, LoadedAudio,
};
use crate::audio_dsp::process_resample;
use crate::mixer_dsp::db_to_gain;
use crate::project_audio::{FadeCurve, PitchMode, PitchParams, StretchParams};
use crate::settings::VoiceFxSettings;

const LIMITER_CEILING: f32 = 0.95;

/// Rend la prise avec les effets voix et l'écrit dans `bank_root/<banque>/`.
/// Retourne le chemin du WAV produit.
pub fn render_take_to_bank(
    input_path: &Path,
    bank_root: &Path,
    bank: &str,
    vfx: &VoiceFxSettings,
) -> Result<PathBuf, String> {
    let bank = sanitize_bank(bank);
    if bank.is_empty() {
        return Err("export bank is empty".to_string());
    }
    let mut audio = load_audio_file(input_path)?;
    if audio.frames() == 0 {
        return Err(format!("empty take {:?}", input_path));
    }

    if vfx.enabled {
        if vfx.highpass_hz > 0.0 {
            high_pass_buffer(&mut audio, vfx.highpass_hz);
        }
        if vfx.pitch_semitones.abs() > 0.001 {
            pitch_shift(&mut audio, vfx.pitch_semitones);
        }
        if vfx.saturation > 0.001 {
            saturate_buffer(&mut audio, vfx.saturation);
        }
        if vfx.comp_enabled {
            compress_buffer(
                &mut audio,
                vfx.comp_threshold_db,
                vfx.comp_ratio,
                vfx.comp_makeup_db,
            );
        }
        apply_fades(&mut audio, vfx.fade_in_secs, vfx.fade_out_secs);
        limit(&mut audio, LIMITER_CEILING);
    }

    let bank_dir = bank_root.join(&bank);
    fs::create_dir_all(&bank_dir)
        .map_err(|e| format!("create {:?} failed: {e}", bank_dir))?;
    let stem = input_path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("take");
    let out_path = unique_path(&bank_dir, stem, "wav");
    write_wav_float(&out_path, &audio.samples, audio.sample_rate, audio.channels)?;
    Ok(out_path)
}

fn sanitize_bank(bank: &str) -> String {
    bank.trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let candidate = dir.join(format!("{stem}.{ext}"));
    if !candidate.exists() {
        return candidate;
    }
    for n in 2..1000 {
        let candidate = dir.join(format!("{stem}-{n}.{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}-{}.{}", std::process::id(), ext))
}

/// Passe-haut premier ordre, même formule que `FilterState::high_pass`.
fn high_pass_buffer(audio: &mut LoadedAudio, cutoff_hz: f32) {
    let sr = audio.sample_rate.max(1) as f32;
    let channels = audio.channels.max(1) as usize;
    let cutoff = cutoff_hz.clamp(10.0, sr * 0.45);
    let dt = 1.0 / sr;
    let rc = 1.0 / (std::f32::consts::TAU * cutoff);
    let alpha = (dt / (rc + dt)).clamp(0.0, 1.0);
    let hp_alpha = 1.0 - alpha;

    let mut prev_in = vec![0.0_f32; channels];
    let mut prev_out = vec![0.0_f32; channels];
    for frame in audio.samples.chunks_mut(channels) {
        for (ch, sample) in frame.iter_mut().enumerate() {
            let out = hp_alpha * (prev_out[ch] + *sample - prev_in[ch]);
            prev_in[ch] = *sample;
            prev_out[ch] = out;
            *sample = out;
        }
    }
}

/// Pitch en demi-tons via le backend du moteur (mode Elastic : durée conservée,
/// identique à ce que le clip joue en non destructif).
fn pitch_shift(audio: &mut LoadedAudio, semitones: f32) {
    let pitch = PitchParams {
        semitones,
        mode: PitchMode::Elastic,
        ..Default::default()
    };
    let processed = process_resample(
        &audio.samples,
        audio.channels.max(1) as usize,
        audio.sample_rate,
        &StretchParams::default(),
        &pitch,
        true,
    );
    if !processed.is_empty() {
        audio.samples = processed;
    }
}

/// Saturation tanh : `drive` de 0 (off) à 1 (fort). Gain normalisé à
/// `tanh(k)` pour que la pleine échelle reste la pleine échelle — les parties
/// faibles remontent vers le plafond, c'est l'effet recherché.
fn saturate_buffer(audio: &mut LoadedAudio, drive: f32) {
    let drive = drive.clamp(0.0, 1.0);
    if drive <= 0.0 {
        return;
    }
    let k = 1.0 + drive * 9.0; // 1..10
    let norm = k.tanh();
    for sample in audio.samples.iter_mut() {
        *sample = (k * *sample).tanh() / norm;
    }
}

/// Compresseur feed-forward avec enveloppe peak partagée entre canaux.
fn compress_buffer(audio: &mut LoadedAudio, threshold_db: f32, ratio: f32, makeup_db: f32) {
    let sr = audio.sample_rate.max(1) as f32;
    let channels = audio.channels.max(1) as usize;
    let ratio = ratio.max(1.0);
    let attack = (-1.0 / (0.015 * sr)).exp();
    let release = (-1.0 / (0.150 * sr)).exp();
    let mut level = 0.0_f32;

    for frame in audio.samples.chunks_mut(channels) {
        let input = frame
            .iter()
            .fold(0.0_f32, |acc, s| acc.max(s.abs()));
        let coef = if input > level { attack } else { release };
        level = input + (level - input) * coef;

        let level_db = 20.0 * level.max(1.0e-6).log10();
        let gain_db = if level_db > threshold_db {
            threshold_db + (level_db - threshold_db) / ratio - level_db
        } else {
            0.0
        };
        let gain = db_to_gain(gain_db + makeup_db);
        for sample in frame.iter_mut() {
            *sample *= gain;
        }
    }
}

/// Crescendo linéaire au début, absorption exponentielle à la fin
/// (mêmes courbes que les fades de clip).
fn apply_fades(audio: &mut LoadedAudio, fade_in_secs: f32, fade_out_secs: f32) {
    let channels = audio.channels.max(1) as usize;
    let frames = audio.frames();
    if frames == 0 {
        return;
    }
    let sr = audio.sample_rate.max(1) as f64;
    let in_frames = ((fade_in_secs.max(0.0) as f64) * sr) as usize;
    let out_frames = ((fade_out_secs.max(0.0) as f64) * sr) as usize;

    for (i, frame) in audio.samples.chunks_mut(channels).enumerate() {
        let mut env = 1.0_f64;
        if in_frames > 0 && i < in_frames {
            env *= apply_fade_curve(i as f64 / in_frames as f64, FadeCurve::Linear);
        }
        if out_frames > 0 && i >= frames.saturating_sub(out_frames) {
            let remaining = (frames - i) as f64 / out_frames as f64;
            env *= apply_fade_curve(remaining.clamp(0.0, 1.0), FadeCurve::Exponential);
        }
        let env = env.clamp(0.0, 1.0) as f32;
        for sample in frame.iter_mut() {
            *sample *= env;
        }
    }
}

/// Limiteur simple : rabaisse le tout si le pic dépasse le plafond.
fn limit(audio: &mut LoadedAudio, ceiling: f32) {
    let peak = audio
        .samples
        .iter()
        .fold(0.0_f32, |acc, s| acc.max(s.abs()));
    if peak > ceiling && peak > 0.0 {
        let gain = ceiling / peak;
        for sample in &mut audio.samples {
            *sample *= gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compress_reduces_loud_frames() {
        let mut audio = LoadedAudio {
            samples: vec![0.9_f32; 48_000],
            sample_rate: 48_000,
            channels: 1,
        };
        compress_buffer(&mut audio, -22.0, 4.0, 0.0);
        let last = audio.samples.last().copied().unwrap();
        assert!(last < 0.5, "expected compression, got {last}");
    }

    #[test]
    fn fades_shape_ends() {
        let frames = 1000usize;
        let mut audio = LoadedAudio {
            samples: vec![0.5_f32; frames],
            sample_rate: 1000,
            channels: 1,
        };
        apply_fades(&mut audio, 0.2, 0.1);
        assert!(audio.samples[0].abs() < 0.01);
        assert!(audio.samples[frames - 1].abs() < 0.01);
        let mid = audio.samples[frames / 2].abs();
        assert!(mid > 0.4, "middle should be untouched, got {mid}");
    }

    #[test]
    fn render_take_to_bank_writes_processed_wav() {
        let tmp = std::env::temp_dir().join(format!("voicefx-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let sr = 48_000u32;
        let frames = sr as usize;
        let mut samples = Vec::with_capacity(frames);
        for i in 0..frames {
            samples.push((i as f32 * 0.02).sin() * 0.5);
        }
        let input = tmp.join("take-1.wav");
        write_wav_float(&input, &samples, sr, 1).unwrap();

        let vfx = VoiceFxSettings {
            enabled: true,
            pitch_semitones: -2.0,
            comp_enabled: true,
            fade_in_secs: 0.1,
            fade_out_secs: 0.1,
            ..Default::default()
        };
        let out = render_take_to_bank(&input, &tmp, "vox", &vfx).unwrap();
        assert!(out.exists(), "export should write {:?}", out);
        assert_eq!(out.parent().unwrap(), tmp.join("vox").as_path());

        let rendered = load_audio_file(&out).unwrap();
        let diff = rendered.frames().abs_diff(frames);
        assert!(
            diff <= frames / 100 + 64,
            "duration drifted too much: {} vs {}",
            rendered.frames(),
            frames
        );
        assert!(rendered.samples[0].abs() < 0.05, "fade-in should mute start");
        let last = rendered.samples[rendered.frames() - 1].abs();
        assert!(last < 0.05, "fade-out should mute end");

        let out2 = render_take_to_bank(&input, &tmp, "vox", &vfx).unwrap();
        assert_ne!(out, out2, "second export must not overwrite the first");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// La saturation écrase les crêtes vers le plafond normalisé (tanh) et
    /// laisse le silence au silence.
    #[test]
    fn saturation_soft_clips_and_keeps_silence() {
        let mut audio = LoadedAudio {
            samples: vec![0.9_f32, 0.0, -0.9, 0.3],
            sample_rate: 48_000,
            channels: 1,
        };
        saturate_buffer(&mut audio, 0.8);
        // Crêtes poussées contre le plafond (comportement tanh, k=8.2).
        assert!(audio.samples[0] > 0.95 && audio.samples[0] <= 1.0,
            "crête saturée inattendue: {}", audio.samples[0]);
        assert_eq!(audio.samples[1], 0.0, "le silence doit rester le silence");
        assert!(audio.samples[2] < -0.95 && audio.samples[2] >= -1.0);
        // Les parties faibles sont remontées fort (x=0.3 → ~0.985).
        assert!(audio.samples[3] > 0.9, "partie faible non remontée: {}", audio.samples[3]);
    }

    /// drive = 0 → signal inchangé.
    #[test]
    fn saturation_zero_is_noop() {
        let mut audio = LoadedAudio {
            samples: vec![0.5_f32, -0.5],
            sample_rate: 48_000,
            channels: 1,
        };
        saturate_buffer(&mut audio, 0.0);
        assert_eq!(audio.samples, vec![0.5, -0.5]);
    }
}
