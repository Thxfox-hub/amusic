//! Onglet « Vue » : visualisations du son sous différentes formes —
//! Rythme (grille du pattern en cycles), Onde (waveform des samples du
//! code), Spectre (FFT live de la sortie master).

use std::collections::BTreeMap;
use std::sync::Arc;

use egui::{pos2, Color32, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use crate::audio::AudioEngine;
use crate::pattern_ir::{PatternDocument, PatternValue};
use crate::sample_loader::SampleKey;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum SoundViewMode {
    #[default]
    Rhythm,
    Waveform,
    Spectrum,
}

impl SoundViewMode {
    const ALL: [SoundViewMode; 3] = [
        SoundViewMode::Rhythm,
        SoundViewMode::Waveform,
        SoundViewMode::Spectrum,
    ];

    pub fn label(self) -> &'static str {
        match self {
            SoundViewMode::Rhythm => "Rythme",
            SoundViewMode::Waveform => "Onde",
            SoundViewMode::Spectrum => "Spectre",
        }
    }
}

#[derive(Default)]
pub struct SoundViewState {
    pub mode: SoundViewMode,
    fft: Option<Arc<dyn Fft<f32>>>,
    fft_buf: Vec<Complex<f32>>,
}

impl SoundViewState {
    pub fn new() -> Self {
        let mut state = Self::default();
        state.mode = SoundViewMode::Rhythm;
        state.fft = Some(FftPlanner::<f32>::new().plan_fft_forward(2048));
        state
    }
}

/// `name[:n[:gain]]` — même tolérance que le parseur natif.
fn parse_sample_token(token: &str) -> (String, Option<f64>) {
    let mut parts = token.split(':');
    let name = parts.next().unwrap_or("").to_string();
    let n = parts.next().and_then(|s| s.parse::<f64>().ok());
    (name, n)
}

/// HSV→RGB manuel (ecolor 0.33 n'a plus `Color32::from_hsv`).
fn hsv(h: f32, s: f32, v: f32) -> Color32 {
    let h = (h - h.floor()) * 6.0;
    let (r, g, b) = if h < 1.0 {
        (v, v * (1.0 - s * (1.0 - h)), v * (1.0 - s))
    } else if h < 2.0 {
        (v * (1.0 - s * (h - 1.0)), v, v * (1.0 - s))
    } else if h < 3.0 {
        (v * (1.0 - s), v, v * (1.0 - s * (3.0 - h)))
    } else if h < 4.0 {
        (v * (1.0 - s), v * (1.0 - s * (h - 3.0)), v)
    } else if h < 5.0 {
        (v * (1.0 - s * (5.0 - h)), v, v)
    } else {
        (v, v * (1.0 - s * (h - 5.0)), v * (1.0 - s))
    };
    let c = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_rgb(c(r), c(g), c(b))
}

fn name_color(name: &str) -> Color32 {
    // Hash FNV-1a → teinte stable par nom de son.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    hsv(((h % 360) as f32) / 360.0, 0.65, 0.95)
}

pub fn sound_view_ui(
    ui: &mut Ui,
    state: &mut SoundViewState,
    doc: Option<&PatternDocument>,
    current_cycle: f64,
    playing: bool,
    audio: Option<&Arc<AudioEngine>>,
) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Vue").strong());
        for &m in &SoundViewMode::ALL {
            ui.selectable_value(&mut state.mode, m, m.label());
        }
        ui.label(
            RichText::new(if playing { "●" } else { "○" })
                .color(if playing { Color32::from_rgb(80, 220, 100) } else { Color32::GRAY }),
        );
    });
    ui.separator();

    match state.mode {
        SoundViewMode::Rhythm => rhythm_view(ui, doc, current_cycle, playing),
        SoundViewMode::Waveform => waveform_view(ui, doc, audio),
        SoundViewMode::Spectrum => spectrum_view(ui, state, audio),
    }
}

/// Grille cycles × temps : chaque événement du pattern = un bloc coloré.
/// 4 lignes : 2 cycles passés, cycle courant, 1 à venir — playhead en ligne.
fn rhythm_view(
    ui: &mut Ui,
    doc: Option<&PatternDocument>,
    current_cycle: f64,
    playing: bool,
) {
    let Some(doc) = doc else {
        ui.label("Aucun code analysé.");
        return;
    };
    if doc.ir.events.is_empty() {
        ui.label("Le pattern ne contient pas d'événements (vide ou non analysable).");
        return;
    }

    let cur = if current_cycle.is_finite() && current_cycle > 0.0 {
        current_cycle.floor()
    } else {
        0.0
    };
    let c_start = (cur - 1.0).max(0.0);
    let rows = 4usize;

    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), 22.0 * rows as f32 + 8.0),
        Sense::hover(),
    );
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, Color32::from_gray(18));

    // Période de boucle par pattern : les begin/end sont en temps de pattern
    // (ex. s("bd hh") → events dans [0,1), rejoués chaque cycle ; .fast(2)
    // → période 0.5). P = max(end) des events du pattern.
    let mut periods: BTreeMap<&str, f64> = BTreeMap::new();
    for ev in &doc.ir.events {
        let slot = periods.entry(ev.pattern_id.as_str()).or_insert(0.0);
        *slot = slot.max(ev.end);
    }

    // Bank par pattern_id pour l'infobulle.
    let banks: BTreeMap<&str, Option<&str>> = doc
        .ir
        .patterns
        .iter()
        .map(|p| (p.id.as_str(), p.bank.as_deref()))
        .collect();

    let row_h = 22.0;
    let inner = Rect::from_min_size(
        pos2(rect.min.x + 4.0, rect.min.y + 4.0),
        Vec2::new(rect.width() - 8.0, rect.height() - 8.0),
    );

    let cell_w = inner.width();
    for row in 0..rows {
        let cycle = c_start + row as f64;
        let y = inner.min.y + row as f32 * row_h;
        // Ligne de cycle
        painter.hline(
            inner.min.x..=inner.max.x,
            y + row_h - 1.0,
            Stroke::new(1.0, Color32::from_white_alpha(20)),
        );
        // Label du numéro de cycle
        painter.text(
            pos2(rect.min.x + 2.0, y + 8.0),
            egui::Align2::LEFT_CENTER,
            format!("{}", cycle as i64),
            egui::FontId::proportional(9.0),
            Color32::from_gray(120),
        );

        for ev in &doc.ir.events {
            let p = periods
                .get(ev.pattern_id.as_str())
                .copied()
                .unwrap_or(1.0)
                .max(1e-6);
            // Instances [b+kP, e+kP) intersectant [cycle, cycle+1) :
            // k ∈ ((cycle-e)/P, (cycle+1-b)/P)
            let k_lo = ((cycle - ev.end) / p).floor() as i64 + 1;
            let k_hi = ((cycle + 1.0 - ev.begin) / p).ceil() as i64 - 1;
            // Garde-fou : période pathologique → au plus 64 instances/ligne.
            let (k_lo, k_hi) = if k_hi - k_lo > 64 {
                (k_lo, k_lo + 64)
            } else {
                (k_lo, k_hi)
            };
            for k in k_lo..=k_hi {
                let b = ev.begin + k as f64 * p;
                let e = ev.end + k as f64 * p;
                let b = b.max(cycle);
                let e = e.min(cycle + 1.0);
                if e <= b {
                    continue;
                }
                let x0 = inner.min.x + ((b - cycle) as f32) * cell_w;
                let x1 = inner.min.x + ((e - cycle) as f32) * cell_w;
                let w = (x1 - x0).max(3.0);
                let is_note = matches!(ev.value, PatternValue::Note(_));
                let is_rest = matches!(ev.value, PatternValue::Rest);
                let label = ev.sample().unwrap_or("·");
                let bank = banks.get(ev.pattern_id.as_str()).copied().flatten();
                let tip = match bank {
                    Some(bk) => format!("{label}  [{bk}]"),
                    None => label.to_string(),
                };
                let color = if is_rest {
                    Color32::from_white_alpha(30)
                } else if is_note {
                    Color32::from_rgb(90, 160, 255)
                } else {
                    name_color(label)
                };
                let r = Rect::from_min_size(
                    pos2(x0, y + 3.0),
                    Vec2::new(w.min(inner.max.x - x0), row_h - 6.0),
                );
                painter.rect_filled(r, 3.0, color);
                // Nom du son si la case est assez large
                if w > 26.0 && !is_rest {
                    painter.text(
                        pos2(x0 + 4.0, y + row_h * 0.5),
                        egui::Align2::LEFT_CENTER,
                        label,
                        egui::FontId::proportional(9.0),
                        Color32::BLACK,
                    );
                }
                // Zone de survol → infobulle
                let hit = Rect::from_min_size(pos2(x0, y), Vec2::new(w, row_h));
                let resp = ui.interact(
                    hit,
                    ui.id().with(("rhythm", ev.id.as_str(), cycle as i64, k)),
                    Sense::hover(),
                );
                if resp.hovered() {
                    resp.clone().on_hover_text(tip);
                }
            }
        }
    }

    // Playhead : position dans le cycle courant
    if playing {
        let frac = (current_cycle - cur).clamp(0.0, 1.0) as f32;
        let row_idx = (cur - c_start) as usize; // cycle courant = c_start+1
        if row_idx < rows {
            let y = inner.min.y + row_idx as f32 * row_h;
            let x = inner.min.x + frac * cell_w;
            painter.vline(x, y..=(y + row_h), Stroke::new(2.0, Color32::WHITE));
        }
    }
    ui.label(
        RichText::new("blocs = déclenchements dans le cycle · blanc = playhead")
            .small()
            .color(Color32::GRAY),
    );
}

/// Waveforms RMS des samples cités dans le code (via la librairie audio).
fn waveform_view(ui: &mut Ui, doc: Option<&PatternDocument>, audio: Option<&Arc<AudioEngine>>) {
    let Some(doc) = doc else {
        ui.label("Aucun code analysé.");
        return;
    };
    let Some(engine) = audio else {
        ui.label("Moteur audio inactif — active l'audio pour voir les ondes.");
        return;
    };

    // Samples distincts cités, dans l'ordre d'apparition (max 6).
    let mut seen: Vec<(Option<String>, String, Option<usize>)> = Vec::new();
    for ev in &doc.ir.events {
        let Some(raw) = ev.sample() else { continue };
        let (name, n) = parse_sample_token(raw);
        if name.is_empty() || seen.iter().any(|(_, nm, _)| nm == &name) {
            continue;
        }
        let bank = doc
            .ir
            .patterns
            .iter()
            .find(|p| p.id.as_str() == ev.pattern_id.as_str())
            .and_then(|p| p.bank.clone());
        seen.push((bank, name, n.map(|v| v.max(0.0) as usize)));
        if seen.len() >= 6 {
            break;
        }
    }
    if seen.is_empty() {
        ui.label("Aucun sample dans le code (notes/synths non affichés).");
        return;
    }

    let row_h = 64.0;
    let width = ui.available_width();
    ui.allocate_ui_with_layout(
        Vec2::new(width, row_h * seen.len() as f32 + 4.0),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            for (bank, name, variant) in &seen {
                let key = SampleKey {
                    bank: bank.clone(),
                    name: name.clone(),
                };
                ui.horizontal(|ui| {
                    ui.set_min_width(110.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(name).small().strong());
                        if let Some(b) = bank {
                            ui.label(RichText::new(b).small().color(Color32::GRAY));
                        }
                    });
                    let (rect, _) = ui.allocate_exact_size(
                        Vec2::new(width - 130.0, row_h),
                        Sense::hover(),
                    );
                    let painter = ui.painter();
                    painter.rect_filled(rect, 3.0, Color32::from_gray(20));
                    match engine.get_waveform_preview(&key, *variant) {
                        Some(peak) if !peak.is_empty() => {
                            let max_v = peak.iter().copied().fold(0.0f32, f32::max).max(1e-4);
                            let n = peak.len();
                            let pts: Vec<Pos2> = peak
                                .iter()
                                .enumerate()
                                .map(|(i, &v)| {
                                    let x = rect.min.x + (i as f32 / n.max(1) as f32) * rect.width();
                                    let h = (v / max_v).clamp(0.0, 1.0) * rect.height() * 0.9;
                                    pos2(x, rect.center().y - h * 0.5)
                                })
                                .collect();
                            // Miroir symétrique pour une silhouette de forme d'onde.
                            let mut poly = pts.clone();
                            poly.extend(pts.iter().rev().map(|p| pos2(p.x, rect.center().y * 2.0 - p.y)));
                            painter.add(egui::Shape::convex_polygon(
                                poly,
                                name_color(name),
                                Stroke::NONE,
                            ));
                            painter.hline(
                                rect.min.x..=rect.max.x,
                                rect.center().y,
                                Stroke::new(1.0, Color32::from_white_alpha(30)),
                            );
                        }
                        _ => {
                            painter.text(
                                rect.center(),
                                egui::Align2::CENTER_CENTER,
                                "indisponible",
                                egui::FontId::proportional(11.0),
                                Color32::GRAY,
                            );
                        }
                    }
                });
            }
        },
    );
}

/// Spectre FFT live de la sortie master (tap post-soft-clip).
fn spectrum_view(ui: &mut Ui, state: &mut SoundViewState, audio: Option<&Arc<AudioEngine>>) {
    let Some(engine) = audio else {
        ui.label("Moteur audio inactif — active l'audio pour voir le spectre.");
        return;
    };
    let tap = engine.spectrum_tap();
    // Fenêtre glissante non destructive : les 2048 plus récents post-master.
    let samples = tap.recent(2048);
    if samples.len() < 2048 {
        ui.label("Pas assez de signal — joue quelque chose.");
        return;
    }

    // Fenêtrage Hann + FFT.
    let n = samples.len();
    state.fft_buf.clear();
    state.fft_buf.reserve(n);
    for (i, &s) in samples.iter().enumerate() {
        let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (n - 1) as f32).cos();
        state.fft_buf.push(Complex::new(s * w, 0.0));
    }
    if let Some(fft) = &state.fft {
        fft.process(&mut state.fft_buf);
    }
    let buf = &state.fft_buf;

    // ~48 barres en échelle logarithmique 40 Hz → Nyquist.
    let sr = engine.sample_rate() as f32;
    let nyq = sr * 0.5;
    let bars = 48usize;
    let f_min = 40.0f32;
    let f_max = nyq.min(20_000.0);
    let bin_hz = sr / n as f32;
    let mut mags = Vec::with_capacity(bars);
    for b in 0..bars {
        let f0 = f_min * (f_max / f_min).powf(b as f32 / bars as f32);
        let f1 = f_min * (f_max / f_min).powf((b + 1) as f32 / bars as f32);
        let i0 = (f0 / bin_hz).floor().max(1.0) as usize;
        let i1 = ((f1 / bin_hz).ceil() as usize).min(n / 2);
        let mut m = 0.0f32;
        for bin in i0..=i1 {
            if let Some(c) = buf.get(bin) {
                m = m.max(c.norm());
            }
        }
        mags.push(m);
    }
    let peak = mags.iter().copied().fold(0.0f32, f32::max).max(1e-6);

    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 160.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 4.0, Color32::from_gray(18));
    let bar_w = rect.width() / bars as f32;
    for (i, &m) in mags.iter().enumerate() {
        let h = (m / peak).powf(0.6).clamp(0.02, 1.0) * rect.height();
        let x = rect.min.x + i as f32 * bar_w;
        let t = i as f32 / bars as f32;
        let col = hsv(0.55 - 0.45 * t, 0.7, 0.95);
        painter.rect_filled(
            Rect::from_min_size(pos2(x + 1.0, rect.max.y - h), Vec2::new((bar_w - 2.0).max(1.0), h)),
            1.5,
            col,
        );
    }
    // Repères de fréquence
    for (hz, label) in [(100.0, "100"), (1000.0, "1k"), (10000.0, "10k")] {
        if hz > f_min && hz < f_max {
            let t = ((hz / f_min).ln() / (f_max / f_min).ln()) as f32;
            let x = rect.min.x + t * rect.width();
            painter.vline(x, rect.min.y..=rect.max.y, Stroke::new(1.0, Color32::from_white_alpha(25)));
            painter.text(
                pos2(x + 2.0, rect.max.y - 12.0),
                egui::Align2::LEFT_BOTTOM,
                label,
                egui::FontId::proportional(9.0),
                Color32::GRAY,
            );
        }
    }
    ui.label(
        RichText::new("FFT 2048 pts · échelle log 40 Hz–20 kHz · mono post-master")
            .small()
            .color(Color32::GRAY),
    );
}
