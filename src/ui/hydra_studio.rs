//! Standalone Hydra studio: edit sketch → Eval → Visual Stage (browser).
//! Independent from Chain Seed / NFT / AI.

use crate::visual_stage::VisualStage;
use egui::{RichText, ScrollArea, Ui};

#[derive(Debug, Clone)]
pub enum HydraAction {
    OpenStage,
    Eval,
    LoadPreset(usize),
    /// Generate unusual sketch from last Chain Seed (NFT/block).
    FromChainSeed,
}

#[derive(Debug, Clone)]
pub struct HydraStudioState {
    pub code: String,
    pub status: String,
    pub follow_music: bool,
    pub window_open: bool,
    pub preset_idx: usize,
}

impl Default for HydraStudioState {
    fn default() -> Self {
        Self {
            code: DEFAULT_SKETCH.into(),
            status: "Idle — Open Stage, then Eval".into(),
            follow_music: true,
            window_open: false,
            preset_idx: 0,
        }
    }
}

pub struct HydraPreset {
    pub name: &'static str,
    pub code: &'static str,
}

pub const HYDRA_PRESETS: &[HydraPreset] = &[
    HydraPreset {
        name: "blank osc",
        code: DEFAULT_SKETCH,
    },
    HydraPreset {
        name: "music buses",
        code: r#"// Bus helpers: F MD FB TH CA … (live from Strudel hits)
osc(() => F(), () => -SYNC(), () => 0.15 + MD() * 0.4)
  .colorama(() => CA())
  .modulate(noise(() => MR()), () => MD())
  .blend(src(o0), () => Math.min(0.92, FB()))
  .contrast(() => CT())
  .invert(() => INV())
  .thresh(() => TH(), 0.15)
  .out()
"#,
    },
    HydraPreset {
        name: "feedback drift",
        code: r#"osc(18, 0.09, 0.8)
  .color(-1.2, -1.4, -1.1)
  .rotate(0.1, 0.15)
  .modulate(shape(4).scale(1.2).repeat(3, 3).modulate(o0, 0.05), 0.5)
  .blend(src(o0), 0.85)
  .out()
"#,
    },
    HydraPreset {
        name: "kaleid voronoi",
        code: r#"voronoi(60, 0.1, 0.01)
  .modulateScale(osc(3).rotate(() => time * 0.1), 0.4)
  .modulateKaleid(osc(4), 4)
  .scale(0.8)
  .colorama(0.25)
  .out()
"#,
    },
    HydraPreset {
        name: "shape grid",
        code: r#"shape(4, 0.2, 0.01)
  .repeat(4, 4)
  .scale(1.1)
  .scrollX(0.1, 0.05)
  .modulate(noise(2), 0.2)
  .diff(osc(6, 0.1, 0.8).rotate(0.5))
  .out()
"#,
    },
];

pub const DEFAULT_SKETCH: &str = r#"// Hydra sketch — edit freely, press Eval
// Music-reactive helpers (optional): F, MD, FB, TH, CA, SIDES…
osc(12, 0.1, 0.8)
  .colorama(0.2)
  .modulate(noise(2), 0.15)
  .out()
"#;

pub fn hydra_studio_ui(
    ui: &mut Ui,
    state: &mut HydraStudioState,
    stage: &VisualStage,
    has_chain_seed: bool,
) -> Vec<HydraAction> {
    let mut actions = Vec::new();

    ui.heading("Hydra");
    ui.label(
        RichText::new(
            "Edit freely, or generate an unusual sketch from Chain Seed / NFT entropy.",
        )
        .weak()
        .small(),
    );
    ui.add_space(6.0);

    ui.horizontal(|ui| {
        if ui.button("Open Stage").clicked() {
            actions.push(HydraAction::OpenStage);
        }
        if ui
            .add_enabled(!state.code.trim().is_empty(), egui::Button::new("Eval"))
            .on_hover_text("Send sketch to open Stage window")
            .clicked()
        {
            actions.push(HydraAction::Eval);
        }
        if ui
            .add_enabled(has_chain_seed, egui::Button::new("From Chain Seed"))
            .on_hover_text("Unusual Hydra topology derived from last NFT/block seed")
            .clicked()
        {
            actions.push(HydraAction::FromChainSeed);
        }
        let clients = stage.client_count();
        let status = if !stage.is_running() {
            "stage off".into()
        } else if clients == 0 {
            format!("listening · {}", stage.stage_url())
        } else {
            format!("{clients} stage client(s)")
        };
        ui.label(RichText::new(status).small().weak());
    });

    ui.horizontal(|ui| {
        ui.checkbox(&mut state.follow_music, "Music → buses");
        ui.label(
            RichText::new("(Strudel hits still rewire F/MD/FB when sketch uses them)")
                .small()
                .weak(),
        );
    });

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label("Preset");
        egui::ComboBox::from_id_salt("hydra_preset")
            .selected_text(HYDRA_PRESETS[state.preset_idx.min(HYDRA_PRESETS.len() - 1)].name)
            .show_ui(ui, |ui| {
                for (i, p) in HYDRA_PRESETS.iter().enumerate() {
                    if ui
                        .selectable_value(&mut state.preset_idx, i, p.name)
                        .clicked()
                    {
                        actions.push(HydraAction::LoadPreset(i));
                    }
                }
            });
        if ui.small_button("Load").clicked() {
            actions.push(HydraAction::LoadPreset(state.preset_idx));
        }
    });

    ui.add_space(6.0);
    ui.label(RichText::new(&state.status).small());
    ui.add_space(4.0);

    ScrollArea::vertical()
        .id_salt("hydra_studio_code")
        .max_height(ui.available_height().max(200.0))
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut state.code)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .desired_rows(18),
            );
        });

    actions
}
