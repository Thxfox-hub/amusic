use crate::project_audio::{FxSlot, MasterOptions, MeterState, PanLaw};

const CLIP_HOLD_BLOCKS: u32 = 90;
const MAX_INSERT_PROCESSORS: usize = 16;

#[derive(Debug, Clone, Copy)]
pub enum BuiltInFxKind {
    Gain,
    Saturation,
    LowPass,
    HighPass,
    Reverb,
    Compressor,
}

#[derive(Debug, Clone, Copy)]
pub struct BuiltInFx {
    pub kind: BuiltInFxKind,
    pub enabled: bool,
    pub amount: f32,
    pub cutoff_hz: f32,
    pub mix: f32,
    pub decay: f32,
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_db: f32,
}

impl BuiltInFx {
    pub fn from_slot(slot: &FxSlot) -> Option<Self> {
        let name = slot.name.trim().to_ascii_lowercase();
        let kind = if name.contains("gain") || name.contains("trim") {
            BuiltInFxKind::Gain
        } else if name.contains("sat") || name.contains("drive") || name.contains("clip") {
            BuiltInFxKind::Saturation
        } else if name.contains("lowpass") || name.contains("low-pass") || name.contains("lpf") {
            BuiltInFxKind::LowPass
        } else if name.contains("highpass") || name.contains("high-pass") || name.contains("hpf") {
            BuiltInFxKind::HighPass
        } else if name.contains("reverb") || name.contains("room") {
            BuiltInFxKind::Reverb
        } else if name.contains("compress") || name.contains("comp") {
            BuiltInFxKind::Compressor
        } else {
            return None;
        };
        Some(Self {
            kind,
            enabled: slot.enabled,
            amount: slot.params.get("amount").copied().unwrap_or_else(|| {
                slot.params
                    .get("gain_db")
                    .copied()
                    .or_else(|| slot.params.get("drive").copied())
                    .unwrap_or(0.0)
            }),
            cutoff_hz: slot.params.get("cutoff_hz").copied().unwrap_or(match kind {
                BuiltInFxKind::LowPass => 18_000.0,
                BuiltInFxKind::HighPass => 30.0,
                _ => 0.0,
            }),
            mix: slot.params.get("mix").copied().unwrap_or(0.35),
            decay: slot.params.get("decay").copied().unwrap_or(0.65),
            threshold_db: slot.params.get("threshold_db").copied().unwrap_or(-22.0),
            ratio: slot
                .params
                .get("ratio")
                .copied()
                .unwrap_or(3.0)
                .max(1.0),
            attack_ms: slot.params.get("attack_ms").copied().unwrap_or(15.0),
            release_ms: slot.params.get("release_ms").copied().unwrap_or(150.0),
            makeup_db: slot.params.get("makeup_db").copied().unwrap_or(0.0),
        })
    }
}

#[derive(Debug, Clone)]
pub struct InsertProcessor {
    filters: [FilterState; MAX_INSERT_PROCESSORS],
    reverbs: [ReverbState; MAX_INSERT_PROCESSORS],
    compressors: [CompressorState; MAX_INSERT_PROCESSORS],
    active_len: usize,
}

impl InsertProcessor {
    pub fn new(fx: &[BuiltInFx]) -> Self {
        Self {
            filters: [FilterState::default(); MAX_INSERT_PROCESSORS],
            reverbs: std::array::from_fn(|_| ReverbState::new()),
            compressors: [CompressorState::default(); MAX_INSERT_PROCESSORS],
            active_len: fx.len().min(MAX_INSERT_PROCESSORS),
        }
    }

    pub fn sync_shape(&mut self, fx: &[BuiltInFx]) {
        let len = fx.len().min(MAX_INSERT_PROCESSORS);
        if self.active_len != len {
            self.filters = [FilterState::default(); MAX_INSERT_PROCESSORS];
            self.reverbs = std::array::from_fn(|_| ReverbState::new());
            self.compressors = [CompressorState::default(); MAX_INSERT_PROCESSORS];
            self.active_len = len;
        }
    }

    pub fn process_pair(
        &mut self,
        fx: &[BuiltInFx],
        sample_rate: f32,
        left: &mut f32,
        right: &mut f32,
    ) {
        self.sync_shape(fx);
        for (index, fx) in fx.iter().take(self.active_len).enumerate() {
            if !fx.enabled {
                continue;
            }
            match fx.kind {
                BuiltInFxKind::Gain => {
                    let gain = db_to_gain(fx.amount);
                    *left *= gain;
                    *right *= gain;
                }
                BuiltInFxKind::Saturation => {
                    let drive = db_to_gain(fx.amount.max(0.0));
                    *left = (*left * drive).tanh() / drive.max(1.0);
                    *right = (*right * drive).tanh() / drive.max(1.0);
                }
                BuiltInFxKind::LowPass => {
                    self.filters[index].low_pass(sample_rate, fx.cutoff_hz, left, right);
                }
                BuiltInFxKind::HighPass => {
                    self.filters[index].high_pass(sample_rate, fx.cutoff_hz, left, right);
                }
                BuiltInFxKind::Reverb => {
                    self.reverbs[index].process_pair(left, right, fx.mix, fx.decay);
                }
                BuiltInFxKind::Compressor => {
                    self.compressors[index].process_pair(
                        sample_rate,
                        fx.threshold_db,
                        fx.ratio,
                        fx.attack_ms,
                        fx.release_ms,
                        fx.makeup_db,
                        left,
                        right,
                    );
                }
            }
        }
    }
}

impl Default for InsertProcessor {
    fn default() -> Self {
        Self {
            filters: [FilterState::default(); MAX_INSERT_PROCESSORS],
            reverbs: std::array::from_fn(|_| ReverbState::new()),
            compressors: [CompressorState::default(); MAX_INSERT_PROCESSORS],
            active_len: 0,
        }
    }
}

/// Compresseur feed-forward simple : suiveur d'enveloppe peak stéréo-lié,
/// attaque/relâche en ms, seuil/ratio en dB, makeup appliqué en sortie.
#[derive(Debug, Clone, Copy, Default)]
struct CompressorState {
    level: f32,
    coef_key: (u32, u32, u32),
    attack_coef: f32,
    release_coef: f32,
}

impl CompressorState {
    fn coefs(
        &mut self,
        sample_rate: f32,
        attack_ms: f32,
        release_ms: f32,
    ) -> (f32, f32) {
        let key = (
            sample_rate as u32,
            attack_ms as u32,
            release_ms as u32,
        );
        if key != self.coef_key {
            self.coef_key = key;
            let sr = sample_rate.max(1.0);
            self.attack_coef = (-1.0 / (attack_ms.max(0.1) * 0.001 * sr)).exp();
            self.release_coef = (-1.0 / (release_ms.max(1.0) * 0.001 * sr)).exp();
        }
        (self.attack_coef, self.release_coef)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn process_pair(
        &mut self,
        sample_rate: f32,
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        makeup_db: f32,
        left: &mut f32,
        right: &mut f32,
    ) {
        let (attack, release) = self.coefs(sample_rate, attack_ms, release_ms);
        let input = left.abs().max(right.abs());
        let coef = if input > self.level { attack } else { release };
        self.level = input + (self.level - input) * coef;

        let level_db = 20.0 * self.level.max(1.0e-6).log10();
        let gain_db = if level_db > threshold_db {
            threshold_db + (level_db - threshold_db) / ratio.max(1.0) - level_db
        } else {
            0.0
        };
        let gain = db_to_gain(gain_db + makeup_db);
        *left *= gain;
        *right *= gain;
    }
}

#[derive(Debug, Clone)]
struct ReverbState {
    delay_l: Vec<f32>,
    delay_r: Vec<f32>,
    pos: usize,
}

impl ReverbState {
    fn new() -> Self {
        Self {
            delay_l: vec![0.0; 24_000],
            delay_r: vec![0.0; 24_000],
            pos: 0,
        }
    }

    fn process_pair(&mut self, left: &mut f32, right: &mut f32, mix: f32, decay: f32) {
        let len = self.delay_l.len();
        if len < 4 {
            return;
        }
        // Taps diffus espacés dans le buffer (≈ 23 ms … 417 ms à 48 kHz,
        // somme des gains = 1.0 comme avant) : une queue de reverb plutôt
        // que les 3 échos distincts des anciens taps len/4, len/2, 3len/4.
        const TAPS: [(usize, f32); 8] = [
            (1_103, 0.16),
            (2_411, 0.15),
            (3_773, 0.14),
            (5_231, 0.13),
            (7_919, 0.12),
            (11_039, 0.11),
            (15_071, 0.10),
            (19_999, 0.09),
        ];
        let read = |buf: &[f32], offset: usize| {
            let idx = (self.pos + len - offset.min(len - 1)) % len;
            buf[idx]
        };
        let mut wet_l = 0.0;
        let mut wet_r = 0.0;
        for (off, g) in TAPS {
            wet_l += read(&self.delay_l, off) * g;
            wet_r += read(&self.delay_r, off) * g;
        }
        let feedback = decay.clamp(0.0, 0.92);
        let dry_l = *left;
        let dry_r = *right;
        self.delay_l[self.pos] = dry_l + wet_l * feedback;
        self.delay_r[self.pos] = dry_r + wet_r * feedback;
        self.pos = (self.pos + 1) % len;
        let wet_mix = mix.clamp(0.0, 1.0);
        *left = dry_l * (1.0 - wet_mix) + wet_l * wet_mix;
        *right = dry_r * (1.0 - wet_mix) + wet_r * wet_mix;
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct FilterState {
    low_l: f32,
    low_r: f32,
    high_prev_in_l: f32,
    high_prev_in_r: f32,
    high_prev_out_l: f32,
    high_prev_out_r: f32,
}

impl FilterState {
    fn low_pass(&mut self, sample_rate: f32, cutoff_hz: f32, left: &mut f32, right: &mut f32) {
        let alpha = one_pole_alpha(sample_rate, cutoff_hz);
        self.low_l += alpha * (*left - self.low_l);
        self.low_r += alpha * (*right - self.low_r);
        *left = self.low_l;
        *right = self.low_r;
    }

    fn high_pass(&mut self, sample_rate: f32, cutoff_hz: f32, left: &mut f32, right: &mut f32) {
        let alpha = one_pole_alpha(sample_rate, cutoff_hz);
        let hp_alpha = 1.0 - alpha;
        let out_l = hp_alpha * (self.high_prev_out_l + *left - self.high_prev_in_l);
        let out_r = hp_alpha * (self.high_prev_out_r + *right - self.high_prev_in_r);
        self.high_prev_in_l = *left;
        self.high_prev_in_r = *right;
        self.high_prev_out_l = out_l;
        self.high_prev_out_r = out_r;
        *left = out_l;
        *right = out_r;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MeterAccumulator {
    peak: f32,
    sum_squares: f64,
    frames: usize,
    clipped: bool,
    hold: u32,
}

impl Default for MeterAccumulator {
    fn default() -> Self {
        Self {
            peak: 0.0,
            sum_squares: 0.0,
            frames: 0,
            clipped: false,
            hold: 0,
        }
    }
}

impl MeterAccumulator {
    pub fn add_pair(&mut self, left: f32, right: f32) {
        let peak = left.abs().max(right.abs());
        self.peak = self.peak.max(peak);
        self.sum_squares += ((left as f64 * left as f64) + (right as f64 * right as f64)) * 0.5;
        self.frames += 1;
        if peak >= 1.0 {
            self.clipped = true;
            self.hold = CLIP_HOLD_BLOCKS;
        }
    }

    pub fn finish(&mut self, previous_hold: u32) -> MeterState {
        let rms = if self.frames == 0 {
            0.0
        } else {
            (self.sum_squares / self.frames as f64).sqrt() as f32
        };
        let lufs_short = if rms <= 0.000_000_1 {
            -90.0
        } else {
            20.0 * rms.log10() - 0.691
        };
        let hold = if self.clipped {
            self.hold
        } else {
            previous_hold.saturating_sub(1)
        };
        MeterState {
            peak: self.peak,
            rms,
            lufs_short,
            clipped: hold > 0,
            clip_hold_blocks: hold,
        }
    }
}

pub fn apply_channel_sample(
    left: &mut f32,
    right: &mut f32,
    volume_db: f32,
    pan: f32,
    pan_law: PanLaw,
) {
    let gain = db_to_gain(volume_db);
    let pan = pan.clamp(-1.0, 1.0);
    let center = db_to_gain(pan_law.center_gain_db());
    *left *= gain * pan_gain(pan, true, center);
    *right *= gain * pan_gain(pan, false, center);
}

pub fn apply_master_options(left: &mut f32, right: &mut f32, options: MasterOptions) {
    if options.mono_monitor {
        let mono = (*left + *right) * 0.5;
        *left = mono;
        *right = mono;
    }
    if options.dim_enabled {
        let dim = db_to_gain(options.dim_db);
        *left *= dim;
        *right *= dim;
    }
    if options.limiter_enabled {
        *left = hard_limiter(*left);
        *right = hard_limiter(*right);
    }
}

pub fn db_to_gain(db: f32) -> f32 {
    if db <= -90.0 {
        0.0
    } else {
        10.0_f32.powf(db / 20.0)
    }
}

fn pan_gain(pan: f32, left: bool, center_gain: f32) -> f32 {
    if pan.abs() < 0.000_1 {
        return center_gain;
    }
    let normalized = ((pan + 1.0) * 0.5).clamp(0.0, 1.0);
    let angle = normalized * std::f32::consts::FRAC_PI_2;
    if left {
        angle.cos()
    } else {
        angle.sin()
    }
}

fn hard_limiter(sample: f32) -> f32 {
    sample.clamp(-1.0, 1.0)
}

pub(crate) fn one_pole_alpha(sample_rate: f32, cutoff_hz: f32) -> f32 {
    let cutoff = cutoff_hz.clamp(10.0, sample_rate * 0.45);
    let dt = 1.0 / sample_rate.max(1.0);
    let rc = 1.0 / (std::f32::consts::TAU * cutoff);
    (dt / (rc + dt)).clamp(0.0, 1.0)
}
