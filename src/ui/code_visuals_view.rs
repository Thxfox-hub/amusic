//! Bande de visuels sous l'éditeur — équivalent struIDEl des `._scope()`,
//! `._spectrum()`, `._punchcard()`, `._pianoroll()` de strudel.cc.
//! Chaque directive du code = un mini-canvas labellisé, mis à jour en live.

use std::collections::BTreeMap;
use std::sync::Arc;

use egui::{pos2, Color32, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use crate::code_visuals::{parse_visual_directives, VisualKind, VisualsData};
use crate::pattern_ir::{PatternDocument, PatternValue};

/// Hauteur de la bande quand au moins une directive est présente.
pub const VISUAL_STRIP_H: f32 = 148.0;

thread_local! {
    static FFT_PLAN: Arc<dyn Fft<f32>> =
        FftPlanner::<f32>::new().plan_fft_forward(2048);
}

fn fft_plan() -> Arc<dyn Fft<f32>> {
    FFT_PLAN.with(|p| Arc::clone(p))
}

/// Rend la bande sous l'éditeur. `data` = None → messages « en attente ».
pub fn visuals_strip_ui(ui: &mut Ui, code: &str, data: Option<&VisualsData>) {
    let directives = parse_visual_directives(code);
    if directives.is_empty() {
        return;
    }
    let strip_h = ui.available_height();
    let cols = directives.len().min(3).max(1);
    let cell_w = (ui.available_width() / cols as f32).max(160.0);
    let pad = 4.0;
    let label_h = 14.0;
    let canvas_h = (strip_h - label_h - pad).max(40.0);

    ui.horizontal_wrapped(|ui| {
        for d in &directives {
            ui.vertical(|ui| {
                ui.set_min_width(cell_w);
                ui.set_min_height(strip_h);
                ui.label(
                    RichText::new(format!("L{} · {}", d.line + 1, d.kind.label()))
                        .small()
                        .color(Color32::GRAY),
                );
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(cell_w - pad, canvas_h), Sense::hover());
                draw_visual(ui.painter(), rect, d.kind, data);
            });
        }
    });
}

fn draw_visual(painter: &egui::Painter, rect: Rect, kind: VisualKind, data: Option<&VisualsData>) {
    painter.rect_filled(rect, 4.0, Color32::from_gray(20));
    let Some(data) = data else {
        center_text(painter, rect, "audio inactif");
        return;
    };
    match kind {
        VisualKind::Scope => draw_scope(painter, rect, data),
        VisualKind::Spectrum => draw_spectrum(painter, rect, data),
        VisualKind::Punchcard => draw_punchcard(painter, rect, data),
        VisualKind::Pianoroll => draw_pianoroll(painter, rect, data),
    }
}

fn center_text(painter: &egui::Painter, rect: Rect, text: &str) {
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(11.0),
        Color32::GRAY,
    );
}

// ---------- scope : forme d'onde live ----------

fn draw_scope(painter: &egui::Painter, rect: Rect, data: &VisualsData) {
    let Some(tap) = &data.spectrum else {
        center_text(painter, rect, "pas de tap audio");
        return;
    };
    let samples = tap.recent(1024);
    if samples.len() < 32 {
        center_text(painter, rect, "joue pour voir l'onde");
        return;
    }
    let mid = rect.center().y;
    let amp = rect.height() * 0.45;
    let n = samples.len();
    let pts: Vec<Pos2> = samples
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            pos2(
                rect.min.x + (i as f32 / (n - 1).max(1) as f32) * rect.width(),
                mid - s.clamp(-1.0, 1.0) * amp,
            )
        })
        .collect();
    painter.add(egui::Shape::line(
        pts,
        Stroke::new(1.5_f32, Color32::from_rgb(120, 230, 140)),
    ));
    painter.hline(
        rect.min.x..=rect.max.x,
        mid,
        Stroke::new(1.0_f32, Color32::from_white_alpha(30)),
    );
}

// ---------- spectrum : barres FFT ----------

fn draw_spectrum(painter: &egui::Painter, rect: Rect, data: &VisualsData) {
    let Some(tap) = &data.spectrum else {
        center_text(painter, rect, "pas de tap audio");
        return;
    };
    let samples = tap.recent(2048);
    if samples.len() < 2048 {
        center_text(painter, rect, "signal insuffisant");
        return;
    }
    let n = samples.len();
    let mut buf: Vec<Complex<f32>> = Vec::with_capacity(n);
    for (i, &s) in samples.iter().enumerate() {
        let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (n - 1) as f32).cos();
        buf.push(Complex::new(s * w, 0.0));
    }
    fft_plan().process(&mut buf);

    let sr = data.sample_rate_hz.max(1.0);
    let bars = 36usize;
    let f_min = 40.0f32;
    let f_max = 20_000.0f32.min(sr * 0.5);
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
    let bar_w = rect.width() / bars as f32;
    for (i, &m) in mags.iter().enumerate() {
        let h = (m / peak).powf(0.6).clamp(0.02, 1.0) * rect.height();
        let x = rect.min.x + i as f32 * bar_w;
        let t = i as f32 / bars as f32;
        let col = hsv_local(0.55 - 0.45 * t, 0.7, 0.95);
        painter.rect_filled(
            Rect::from_min_size(
                pos2(x + 1.0, rect.max.y - h),
                Vec2::new((bar_w - 2.0).max(1.0), h),
            ),
            1.5,
            col,
        );
    }
}

fn hsv_local(h: f32, s: f32, v: f32) -> Color32 {
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

// ---------- punchcard / pianoroll : grilles d'events ----------

/// Période de boucle par pattern (max(end)) — même sémantique que l'onglet Vue.
fn pattern_periods(doc: &PatternDocument) -> BTreeMap<String, f64> {
    let mut periods: BTreeMap<String, f64> = BTreeMap::new();
    for ev in &doc.ir.events {
        let slot = periods
            .entry(ev.pattern_id.as_str().to_string())
            .or_insert(0.0);
        *slot = slot.max(ev.end);
    }
    periods
}

/// Instances [b+kP, e+kP) de l'event intersectant [cycle, cycle+1),
/// retournées en fractions (début, fin) relatives au cycle.
fn instances_in_cycle(ev_begin: f64, ev_end: f64, period: f64, cycle: f64) -> Vec<(f32, f32)> {
    let p = period.max(1e-6);
    let k_lo = ((cycle - ev_end) / p).floor() as i64 + 1;
    let k_hi = ((cycle + 1.0 - ev_begin) / p).ceil() as i64 - 1;
    let (k_lo, k_hi) = if k_hi - k_lo > 64 {
        (k_lo, k_lo + 64)
    } else {
        (k_lo, k_hi)
    };
    let mut out = Vec::new();
    for k in k_lo..=k_hi {
        let b = (ev_begin + k as f64 * p).max(cycle) - cycle;
        let e = (ev_end + k as f64 * p).min(cycle + 1.0) - cycle;
        if e > b {
            out.push((b as f32, e as f32));
        }
    }
    out
}

const GRID_CYCLES: i64 = 4;

fn draw_punchcard(painter: &egui::Painter, rect: Rect, data: &VisualsData) {
    let Some(doc) = data.doc else {
        center_text(painter, rect, "pas de code analysé");
        return;
    };
    // Lignes = noms de samples distincts (ordre d'apparition, max 8).
    let mut rows: Vec<String> = Vec::new();
    for ev in &doc.ir.events {
        if let Some(s) = ev.sample() {
            let name = s.split(':').next().unwrap_or(s).to_string();
            if !name.is_empty() && !rows.contains(&name) {
                rows.push(name);
            }
        }
        if rows.len() >= 8 {
            break;
        }
    }
    if rows.is_empty() {
        center_text(painter, rect, "aucun sample dans le code");
        return;
    }
    draw_event_grid(painter, rect, doc, data, &rows, |ev| {
        ev.sample()
            .and_then(|s| s.split(':').next())
            .map(|s| s.to_string())
    });
}

fn draw_pianoroll(painter: &egui::Painter, rect: Rect, data: &VisualsData) {
    let Some(doc) = data.doc else {
        center_text(painter, rect, "pas de code analysé");
        return;
    };
    // Lignes = notes distinctes triées par hauteur (aigu en haut).
    let mut rows: Vec<(String, i32)> = Vec::new();
    for ev in &doc.ir.events {
        if let PatternValue::Note(n) = &ev.value {
            if let Some(midi) = note_to_midi(n) {
                if !rows.iter().any(|(s, _)| s == n) {
                    rows.push((n.clone(), midi));
                }
            }
        }
    }
    if rows.is_empty() {
        center_text(painter, rect, "aucune note dans le code");
        return;
    }
    // Aigu en haut : tri par hauteur décroissante.
    rows.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
    draw_event_grid(
        painter,
        rect,
        doc,
        data,
        &rows.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>(),
        |ev| match &ev.value {
            PatternValue::Note(n) => Some(n.clone()),
            _ => None,
        },
    );
}

/// note "c3" / "eb4" / "fs5" → midi. None si non parsable.
fn note_to_midi(name: &str) -> Option<i32> {
    let b = name.trim().as_bytes();
    if b.len() < 2 {
        return None;
    }
    let semi = match b[0].to_ascii_lowercase() {
        b'c' => 0,
        b'd' => 2,
        b'e' => 4,
        b'f' => 5,
        b'g' => 7,
        b'a' => 9,
        b'b' => 11,
        _ => return None,
    };
    let mut i = 1;
    let mut acc: i32 = 0;
    if i < b.len() && (b[i] == b'#' || b[i] == b'b') {
        acc = if b[i] == b'#' { 1 } else { -1 };
        i += 1;
    }
    // octave : chiffres restants (peut être négatif "c-1")
    let oct_str = std::str::from_utf8(&b[i..]).ok()?;
    let octave: i32 = oct_str.parse().ok()?;
    Some((octave + 1) * 12 + semi + acc)
}

/// Grille partagée punchcard/pianoroll : lignes = `rows` (labels),
/// chaque event échantillonné par `row_of` dessiné en bloc.
fn draw_event_grid<F>(
    painter: &egui::Painter,
    rect: Rect,
    doc: &PatternDocument,
    data: &VisualsData,
    rows: &[String],
    row_of: F,
) where
    F: Fn(&crate::pattern_ir::PatternEvent) -> Option<String>,
{
    let periods = pattern_periods(doc);
    let cur = if data.current_cycle.is_finite() && data.current_cycle > 0.0 {
        data.current_cycle.floor()
    } else {
        0.0
    };
    let c_start = (cur + 1.0 - GRID_CYCLES as f64).max(0.0);
    let label_w = 52.0f32.min(rect.width() * 0.25);
    let grid = Rect::from_min_size(
        pos2(rect.min.x + label_w, rect.min.y),
        Vec2::new(rect.width() - label_w, rect.height()),
    );
    let row_h = grid.height() / rows.len() as f32;

    painter.rect_filled(rect, 4.0, Color32::from_gray(18));
    for (ri, name) in rows.iter().enumerate() {
        let y = grid.min.y + ri as f32 * row_h;
        painter.text(
            pos2(rect.min.x + 4.0, y + row_h * 0.5),
            egui::Align2::LEFT_CENTER,
            name,
            egui::FontId::proportional(9.0),
            Color32::from_gray(140),
        );
        painter.hline(
            grid.min.x..=grid.max.x,
            y + row_h - 1.0,
            Stroke::new(1.0_f32, Color32::from_white_alpha(15)),
        );
    }

    // Une colonne = un cycle ; dedans, x = fraction du cycle.
    let cell_w = grid.width() / GRID_CYCLES as f32;
    for ci in 0..GRID_CYCLES {
        let abs_cycle = c_start + ci as f64;
        // Séparateur de cycle
        let sx = grid.min.x + ci as f32 * cell_w;
        painter.vline(
            sx,
            grid.min.y..=grid.max.y,
            Stroke::new(1.0_f32, Color32::from_white_alpha(15)),
        );
        for ev in &doc.ir.events {
            let Some(row_name) = row_of(ev) else { continue };
            let Some(ri) = rows.iter().position(|r| *r == row_name) else {
                continue;
            };
            let p = periods
                .get(ev.pattern_id.as_str())
                .copied()
                .unwrap_or(1.0)
                .max(1e-6);
            for (b, e) in instances_in_cycle(ev.begin, ev.end, p, abs_cycle) {
                let x0 = grid.min.x + (ci as f32 + b) * cell_w;
                let x1 = grid.min.x + (ci as f32 + e) * cell_w;
                let y = grid.min.y + ri as f32 * row_h + 2.0;
                let h = row_h - 4.0;
                let is_note = matches!(ev.value, PatternValue::Note(_));
                let col = if is_note {
                    Color32::from_rgb(90, 160, 255)
                } else {
                    name_hash_color(&row_name)
                };
                painter.rect_filled(
                    Rect::from_min_size(pos2(x0, y), Vec2::new((x1 - x0).max(2.0), h)),
                    2.0,
                    col,
                );
            }
        }
    }

    // Playhead dans la colonne du cycle courant, à la fraction écoulée.
    if data.playing {
        let frac = (data.current_cycle - cur).clamp(0.0, 1.0) as f32;
        let ci = (cur - c_start).max(0.0) as usize;
        if ci < GRID_CYCLES as usize {
            let x = grid.min.x + (ci as f32 + frac) * cell_w;
            painter.vline(
                x,
                grid.min.y..=grid.max.y,
                Stroke::new(1.5_f32, Color32::WHITE),
            );
        }
    }
}

fn name_hash_color(name: &str) -> Color32 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    hsv_local(((h % 360) as f32) / 360.0, 0.65, 0.95)
}
