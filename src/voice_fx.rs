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
use crate::project_audio::{
    ClipFxParam, ClipVoiceFx, FadeCurve, PitchMode, PitchParams, StretchParams,
};
use crate::settings::VoiceFxSettings;

const LIMITER_CEILING: f32 = 0.95;

/// Rend la chaîne FX « lourde » d'un clip (panneau personnalisé) sur la source
/// déjà chargée : gate → passe-haut → compresseur → saturation → bitcrush →
/// rampes de pitch → limiteur. Pitch/fades/volume restent hors de ce rendu —
/// ils vivent en paramètres non destructifs du clip.
/// Seuls les paramètres *affichés* dans le panneau s'appliquent : un effet
/// non ajouté par l'utilisateur reste en bypass même si sa valeur est non nulle.
pub fn render_clip_voice_fx(audio: &mut LoadedAudio, fx: &ClipVoiceFx) {
    let sr = audio.sample_rate.max(1) as f32;
    if fx.param_enabled(ClipFxParam::NoiseGate) && fx.gate_db > -79.9 {
        noise_gate_buffer(audio, fx.gate_db);
    }
    if fx.param_enabled(ClipFxParam::Highpass) && fx.highpass_hz > 0.0 {
        high_pass_buffer(audio, fx.highpass_hz);
    }
    if (fx.param_enabled(ClipFxParam::CompThreshold)
        || fx.param_enabled(ClipFxParam::CompRatio))
        && fx.comp_ratio > 1.0
        && fx.comp_threshold_db < -0.05
    {
        compress_buffer(audio, fx.comp_threshold_db, fx.comp_ratio, 0.0);
    }
    if fx.param_enabled(ClipFxParam::Saturation) && fx.saturation_db > 0.05 {
        saturate_buffer_db(audio, fx.saturation_db);
    }
    if (fx.param_enabled(ClipFxParam::BitcrushRate)
        || fx.param_enabled(ClipFxParam::BitcrushBits))
        && fx.bitcrush_rate_hz > 0.0
        && fx.bitcrush_rate_hz < sr * 0.5
        && fx.bitcrush_bits > 0.0
        && fx.bitcrush_bits < 24.0
    {
        bitcrush_buffer(audio, fx.bitcrush_rate_hz, fx.bitcrush_bits);
    }
    let rise = fx.param_enabled(ClipFxParam::PitchRise) && fx.pitch_rise_semitones.abs() > 0.01;
    let fall = fx.param_enabled(ClipFxParam::PitchFall) && fx.pitch_fall_semitones.abs() > 0.01;
    if rise || fall {
        pitch_ramp_buffer(
            audio,
            if rise { fx.pitch_rise_semitones } else { 0.0 },
            if fall { fx.pitch_fall_semitones } else { 0.0 },
        );
    }
    limit(audio, LIMITER_CEILING);
}

/// Noise gate / expander : les trames sous le seuil sont atténuées vers 0
/// (enveloppe attack rapide, release ~80 ms partagée entre canaux).
fn noise_gate_buffer(audio: &mut LoadedAudio, threshold_db: f32) {
    let sr = audio.sample_rate.max(1) as f32;
    let channels = audio.channels.max(1) as usize;
    let attack = (-1.0 / (0.005 * sr)).exp();
    let release = (-1.0 / (0.080 * sr)).exp();
    let mut level = 0.0_f32;
    for frame in audio.samples.chunks_mut(channels) {
        let input = frame.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()));
        let coef = if input > level { attack } else { release };
        level = input + (level - input) * coef;
        let level_db = 20.0 * level.max(1.0e-6).log10();
        // Porte douce : -12 dB de sous-pente sous le seuil, coupe franche à -24.
        let gain_db = if level_db >= threshold_db {
            0.0
        } else {
            ((threshold_db - level_db) * 2.0).min(48.0)
        };
        let gain = db_to_gain(-gain_db);
        for sample in frame.iter_mut() {
            *sample *= gain;
        }
    }
}

/// Bitcrush : décimation sample-and-hold à `rate_hz` + quantification à `bits`
/// niveaux — le grain « lo-fi » classique.
fn bitcrush_buffer(audio: &mut LoadedAudio, rate_hz: f32, bits: f32) {
    let sr = audio.sample_rate.max(1) as f32;
    let channels = audio.channels.max(1) as usize;
    let hold = (sr / rate_hz.max(1.0)).max(1.0);
    let levels = 2.0_f32.powi(bits.round() as i32 - 1).max(1.0);
    let mut hold_pos = 0.0_f32;
    let mut held = vec![0.0_f32; channels];
    for frame in audio.samples.chunks_mut(channels) {
        if hold_pos <= 0.0 {
            hold_pos += hold;
            held.copy_from_slice(frame);
        }
        hold_pos -= 1.0;
        for (ch, sample) in frame.iter_mut().enumerate() {
            *sample = (held[ch] * levels).round() / levels;
        }
    }
}

/// Saturation en dB : k = gain linéaire du dB demandé, `tanh(k·x)/tanh(k)`.
fn saturate_buffer_db(audio: &mut LoadedAudio, db: f32) {
    let k = db_to_gain(db).max(1.0);
    let norm = k.tanh();
    for sample in audio.samples.iter_mut() {
        *sample = (k * *sample).tanh() / norm;
    }
}

/// Rampes de pitch (style bande) : le taux de lecture monte/descend en
/// demi-tons sur les ~15 premiers/derniers % du clip. La durée change
/// légèrement — c'est l'effet recherché.
fn pitch_ramp_buffer(audio: &mut LoadedAudio, rise_st: f32, fall_st: f32) {
    let channels = audio.channels.max(1) as usize;
    let frames = audio.frames();
    if frames < 8 || channels == 0 {
        return;
    }
    let ramp = (frames / 7).max(1); // ~15% du clip
    // offset en demi-tons pour la frame i
    let offset = |i: usize| -> f64 {
        let mut st = 0.0_f64;
        if rise_st.abs() > 0.01 && i < ramp {
            st += rise_st as f64 * (1.0 - i as f64 / ramp as f64);
        }
        if fall_st.abs() > 0.01 && i >= frames - ramp {
            st += fall_st as f64 * ((i - (frames - ramp)) as f64 / ramp as f64);
        }
        st
    };
    // position source lue = intégrale du taux 2^(st/12), normalisée pour
    // couvrir toute la source (durée conservée, pitch variable, pas de perte).
    let mut src_pos = vec![0.0_f64; frames + 1];
    let mut pos = 0.0_f64;
    for i in 0..frames {
        src_pos[i] = pos;
        pos += 2.0_f64.powf(offset(i) / 12.0);
    }
    src_pos[frames] = pos;
    let scale = (frames - 1) as f64 / pos.max(1.0);
    let mut out = vec![0.0_f32; frames * channels];
    for i in 0..frames {
        let p = (src_pos[i] * scale).min((frames - 2) as f64).max(0.0);
        let i0 = p.floor() as usize;
        let frac = (p - i0 as f64) as f32;
        for ch in 0..channels {
            let a = audio.samples[i0 * channels + ch];
            let b = audio.samples[(i0 + 1) * channels + ch];
            out[i * channels + ch] = a + (b - a) * frac;
        }
    }
    audio.samples = out;
}

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

    #[test]
    fn bitcrush_quantizes_and_holds() {
        // rampe douce de 100 échantillons à 1000 Hz, crush à 200 Hz / 3 bits
        let samples: Vec<f32> = (0..100).map(|i| i as f32 / 100.0 - 0.5).collect();
        let mut audio = LoadedAudio {
            samples: samples.clone(),
            sample_rate: 1000,
            channels: 1,
        };
        bitcrush_buffer(&mut audio, 200.0, 3.0);
        // quantification 3 bits → niveaux multiples de 1/4
        for s in &audio.samples {
            let q = (s * 4.0).round() / 4.0;
            assert!((s - q).abs() < 1e-6, "niveau non quantifié: {s}");
        }
        // sample-and-hold à 200 Hz (hold=5) → paliers de 5 échantillons
        assert_eq!(audio.samples[0], audio.samples[1]);
        assert_eq!(audio.samples[0], audio.samples[4]);
    }

    #[test]
    fn noise_gate_silences_quiet_frames() {
        // 500 trames fortes puis 500 faibles (-40 dB) sous un seuil à -20 dB
        let mut samples = vec![0.8_f32; 500];
        samples.extend(vec![0.008_f32; 500]);
        let mut audio = LoadedAudio {
            samples,
            sample_rate: 1000,
            channels: 1,
        };
        noise_gate_buffer(&mut audio, -20.0);
        assert!(audio.samples[400].abs() > 0.7, "fort atténué à tort");
        assert!(
            audio.samples[999].abs() < 0.004,
            "faible non gatée: {}",
            audio.samples[999]
        );
    }

    #[test]
    fn pitch_ramp_preserves_length_and_morphs_start() {
        // sinusoïde : rampe montante change le début, durée identique
        let frames = 1000usize;
        let mut audio = LoadedAudio {
            samples: (0..frames).map(|i| (i as f32 * 0.1).sin()).collect(),
            sample_rate: 1000,
            channels: 1,
        };
        let before: Vec<f32> = audio.samples.clone();
        pitch_ramp_buffer(&mut audio, 12.0, 0.0);
        assert_eq!(audio.samples.len(), before.len());
        let start_changed = (0..50)
            .any(|i| (audio.samples[i] - before[i]).abs() > 1e-4);
        assert!(start_changed, "la rampe n'a pas modifié le début");
        // la queue reste un contenu audible de même nature (la compensation de
        // durée décale la lecture, mais ne tronque ni ne coupe rien)
        let tail_max = audio.samples[frames - 50..]
            .iter()
            .fold(0.0_f32, |acc, s| acc.max(s.abs()));
        assert!(tail_max > 0.5, "queue muette ou tronquée: {tail_max}");
    }

    #[test]
    fn render_clip_voice_fx_chain_runs() {
        let mut audio = LoadedAudio {
            samples: (0..4800).map(|i| (i as f32 * 0.05).sin() * 0.5).collect(),
            sample_rate: 48_000,
            channels: 1,
        };
        let fx = ClipVoiceFx {
            ui_params: crate::project_audio::CLIP_FX_PARAMS_ALL.to_vec(),
            ..ClipVoiceFx::default()
        };
        render_clip_voice_fx(&mut audio, &fx);
        assert_eq!(audio.frames(), 4800);
        assert!(audio.samples.iter().all(|s| s.abs() <= 0.96));
    }
}
