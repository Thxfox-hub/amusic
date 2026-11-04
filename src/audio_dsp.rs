use crate::project_audio::{PitchMode, PitchParams, StretchParams};

pub trait StretchPitchBackend: Send + Sync {
    fn name(&self) -> &'static str;

    fn process_preview(
        &self,
        input: &[f32],
        channels: usize,
        sample_rate: u32,
        stretch: &StretchParams,
        pitch: &PitchParams,
    ) -> Vec<f32>;

    fn process_offline_hq(
        &self,
        input: &[f32],
        channels: usize,
        sample_rate: u32,
        stretch: &StretchParams,
        pitch: &PitchParams,
    ) -> Vec<f32>;
}

#[derive(Default)]
pub struct BuiltInStretchPitch;

impl StretchPitchBackend for BuiltInStretchPitch {
    fn name(&self) -> &'static str {
        "built-in-resampler"
    }

    fn process_preview(
        &self,
        input: &[f32],
        channels: usize,
        sample_rate: u32,
        stretch: &StretchParams,
        pitch: &PitchParams,
    ) -> Vec<f32> {
        process_resample(input, channels, sample_rate, stretch, pitch, false)
    }

    fn process_offline_hq(
        &self,
        input: &[f32],
        channels: usize,
        sample_rate: u32,
        stretch: &StretchParams,
        pitch: &PitchParams,
    ) -> Vec<f32> {
        process_resample(input, channels, sample_rate, stretch, pitch, true)
    }
}

pub fn default_backend() -> BuiltInStretchPitch {
    BuiltInStretchPitch
}

pub fn process_resample(
    input: &[f32],
    channels: usize,
    sample_rate: u32,
    stretch: &StretchParams,
    pitch: &PitchParams,
    high_quality: bool,
) -> Vec<f32> {
    if input.is_empty() || channels == 0 {
        return Vec::new();
    }

    let pitch_ratio = pitch.ratio().max(0.01) as f64;
    let stretch_ratio = stretch.ratio.max(0.01);

    // Mode Elastic : vrai phase-vocoder (signalsmith-stretch) — pitch et stretch
    // en une seule passe, durée préservée, qualité bien au-dessus du
    // rééchantillonnage naïf ci-dessous qui reste en filet de secours.
    if matches!(pitch.mode, PitchMode::Elastic)
        && ((pitch_ratio - 1.0).abs() > 0.000_1 || (stretch_ratio - 1.0).abs() > 0.000_1)
    {
        if let Some(out) = signalsmith_process(
            input,
            channels,
            sample_rate,
            stretch_ratio,
            pitch,
            stretch.preserve_formants,
        ) {
            return out;
        }
    }

    let source_step = match pitch.mode {
        // Varispeed : +12 demi-tons = lecture 2× plus rapide (pas 2× plus
        // lente — le facteur pitch multiplie le pas, le stretch le divise).
        PitchMode::Tape => pitch_ratio / stretch_ratio,
        PitchMode::Elastic => 1.0 / stretch_ratio,
    };
    let first_pass = resample_linear(input, channels, source_step, high_quality);

    if matches!(pitch.mode, PitchMode::Elastic) && (pitch_ratio - 1.0).abs() > 0.000_1 {
        // Fallback naïf si le backend phase-vocoder a échoué.
        let pitched = resample_linear(&first_pass, channels, 1.0 / pitch_ratio, high_quality);
        let restore = pitched.len().max(channels) as f64 / first_pass.len().max(channels) as f64;
        resample_linear(&pitched, channels, restore, high_quality)
    } else {
        first_pass
    }
}

/// Pitch-shift + time-stretch en une passe via signalsmith-stretch.
/// `stretch_ratio` détermine la longueur de sortie (durée × ratio) ;
/// `pitch` transpose en demi-tons sans changer cette durée.
/// Retourne `None` si le moteur refuse (buffer trop court, ratio extrême) —
/// l'appelant repasse alors au rééchantillonneur naïf.
fn signalsmith_process(
    input: &[f32],
    channels: usize,
    sample_rate: u32,
    stretch_ratio: f64,
    pitch: &PitchParams,
    preserve_formants: bool,
) -> Option<Vec<f32>> {
    let frames = input.len() / channels;
    if frames == 0 {
        return None;
    }
    let out_frames = ((frames as f64) * stretch_ratio.max(0.05))
        .round()
        .max(1.0) as usize;
    let mut out = vec![0.0_f32; out_frames * channels];

    let sr = sample_rate.clamp(8_000, 192_000);
    let mut stretcher = signalsmith_stretch::Stretch::preset_default(channels as u32, sr);
    let semis = pitch.semitones + pitch.cents / 100.0;
    if semis.abs() > 0.001 {
        stretcher.set_transpose_factor_semitones(semis, Some(8_000.0 / sr as f32));
    }
    if pitch.formant_correction || preserve_formants {
        stretcher.set_formant_factor_semitones(0.0, true);
    }
    if stretcher.exact(input, &mut out) {
        Some(out)
    } else {
        None
    }
}

fn resample_linear(
    input: &[f32],
    channels: usize,
    source_step: f64,
    high_quality: bool,
) -> Vec<f32> {
    let frames = input.len() / channels;
    if frames == 0 {
        return Vec::new();
    }
    let out_frames = ((frames as f64) / source_step.max(0.000_001))
        .ceil()
        .max(1.0) as usize;
    let mut out = vec![0.0_f32; out_frames * channels];
    let taps = if high_quality { 4 } else { 1 };

    for out_frame in 0..out_frames {
        let source_pos = out_frame as f64 * source_step;
        let base = source_pos.floor() as isize;
        let frac = (source_pos - base as f64) as f32;
        for ch in 0..channels {
            let sample = if taps == 1 {
                sample_linear(input, channels, frames, base, frac, ch)
            } else {
                sample_cubic(input, channels, frames, base, frac, ch)
            };
            out[out_frame * channels + ch] = sample;
        }
    }

    out
}

fn sample_linear(
    input: &[f32],
    channels: usize,
    frames: usize,
    base: isize,
    frac: f32,
    ch: usize,
) -> f32 {
    let a = sample_at(input, channels, frames, base, ch);
    let b = sample_at(input, channels, frames, base + 1, ch);
    a + (b - a) * frac
}

fn sample_cubic(
    input: &[f32],
    channels: usize,
    frames: usize,
    base: isize,
    frac: f32,
    ch: usize,
) -> f32 {
    let p0 = sample_at(input, channels, frames, base - 1, ch);
    let p1 = sample_at(input, channels, frames, base, ch);
    let p2 = sample_at(input, channels, frames, base + 1, ch);
    let p3 = sample_at(input, channels, frames, base + 2, ch);
    let a = -0.5 * p0 + 1.5 * p1 - 1.5 * p2 + 0.5 * p3;
    let b = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
    let c = -0.5 * p0 + 0.5 * p2;
    let d = p1;
    (((a * frac) + b) * frac + c) * frac + d
}

fn sample_at(input: &[f32], channels: usize, frames: usize, frame: isize, ch: usize) -> f32 {
    let frame = frame.clamp(0, frames.saturating_sub(1) as isize) as usize;
    input.get(frame * channels + ch).copied().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project_audio::{PitchParams, StretchParams};

    #[test]
    fn stretch_ratio_changes_duration() {
        let input = vec![0.0_f32; 100];
        let stretch = StretchParams {
            ratio: 2.0,
            ..Default::default()
        };
        let out = process_resample(&input, 1, 44_100, &stretch, &PitchParams::default(), false);
        assert!(out.len() > input.len());
    }

    fn sine_frames(frames: usize, freq: f32, sample_rate: u32) -> Vec<f32> {
        (0..frames)
            .map(|i| {
                (i as f32 / sample_rate as f32 * freq * 2.0 * std::f32::consts::PI).sin() * 0.5
            })
            .collect()
    }

    fn zero_crossings(data: &[f32]) -> usize {
        data.windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count()
    }

    /// Mode Elastic : le pitch monte d'une octave, la durée reste identique
    /// (signalsmith-stretch). Le test compte les passages à zéro : une octave
    /// = ×2 de fréquence, donc ×2 de croisements.
    #[test]
    fn elastic_pitch_shifts_one_octave_same_duration() {
        let sr = 44_100u32;
        let input = sine_frames(sr as usize, 220.0, sr); // 1 s de La 220 Hz
        let pitch = PitchParams {
            semitones: 12.0,
            mode: PitchMode::Elastic,
            ..Default::default()
        };
        let out = process_resample(&input, 1, sr, &StretchParams::default(), &pitch, true);

        // Durée conservée (±5 % de marge pour le moteur).
        let ratio_len = out.len() as f64 / input.len() as f64;
        assert!(
            (0.95..=1.05).contains(&ratio_len),
            "durée non conservée: {ratio_len}"
        );

        let in_zc = zero_crossings(&input);
        let out_zc = zero_crossings(&out);
        let zc_ratio = out_zc as f64 / in_zc.max(1) as f64;
        assert!(
            (1.7..=2.3).contains(&zc_ratio),
            "ratio de fréquence attendu ~2, obtenu {zc_ratio} ({in_zc} -> {out_zc})"
        );
    }

    /// Stretch + pitch en une passe : ×2 de durée ET +7 demi-tons.
    #[test]
    fn elastic_stretch_and_pitch_one_pass() {
        let sr = 44_100u32;
        let input = sine_frames(sr as usize / 2, 220.0, sr); // 0,5 s
        let pitch = PitchParams {
            semitones: 7.0,
            mode: PitchMode::Elastic,
            ..Default::default()
        };
        let stretch = StretchParams {
            ratio: 2.0,
            ..Default::default()
        };
        let out = process_resample(&input, 1, sr, &stretch, &pitch, true);

        let ratio_len = out.len() as f64 / input.len() as f64;
        assert!(
            (1.9..=2.1).contains(&ratio_len),
            "stretch ratio attendu ~2, obtenu {ratio_len}"
        );
        // La fréquence perçue doit avoir monté : +7 demi-tons ≈ ×1.5.
        let in_zc = zero_crossings(&input);
        let out_zc = zero_crossings(&out);
        let expected = 2.0 * 1.498; // durée ×2 ET fréquence ×~1.5
        let zc_ratio = out_zc as f64 / in_zc.max(1) as f64;
        assert!(
            (expected * 0.85..=expected * 1.15).contains(&zc_ratio),
            "zc ratio attendu ~{expected:.2}, obtenu {zc_ratio}"
        );
    }

    /// Mode Tape : varispeed — le pitch change la durée (comportement voulu,
    /// pas de signalsmith).
    #[test]
    fn tape_pitch_changes_duration() {
        let sr = 44_100u32;
        let input = sine_frames(sr as usize / 2, 220.0, sr);
        let pitch = PitchParams {
            semitones: 12.0,
            mode: PitchMode::Tape,
            ..Default::default()
        };
        let out = process_resample(&input, 1, sr, &StretchParams::default(), &pitch, true);
        // +12 demi-tons en Tape : lecture ×2 plus vite → moitié de frames.
        let ratio_len = out.len() as f64 / input.len() as f64;
        assert!(
            (0.45..=0.55).contains(&ratio_len),
            "tape devrait raccourcir d'une octave: {ratio_len}"
        );
    }
}
